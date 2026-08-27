//! Loopback end-to-end tests: the remote and local daemons on real TCP
//! sockets, with the mock agent (`tests/mock_agent.rs`) standing in for
//! `dvadva-agent`. These pin the whole contract: frame handling, argument
//! passing, opaque relay, and close propagation in both directions.

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::net::TcpStream;

use dvadva_bridge::local_daemon;
use dvadva_bridge::proto::{self, Reply, Request};
use dvadva_bridge::remote_daemon;

/// Path of the mock agent binary (cargo exposes every bin of the package).
fn mock_agent() -> String {
    env!("CARGO_BIN_EXE_dvadva-bridge-mock-agent").to_string()
}

/// Start the remote daemon on an ephemeral loopback port.
async fn remote_on(agent_bin: &str) -> u16 {
    remote_with(remote_daemon::Config::new(agent_bin)).await
}

/// Same, with an explicit daemon config (default work dir, …).
async fn remote_with(config: remote_daemon::Config) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = remote_daemon::serve(listener, config).await;
    });
    port
}

/// Start the local daemon on an ephemeral loopback port, upstream given.
async fn local_up(upstream: u16) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let upstream = format!("127.0.0.1:{upstream}");
    tokio::spawn(async move {
        let _ = local_daemon::serve(listener, upstream).await;
    });
    port
}

/// The test-side connection: split halves over one tokio TcpStream.
type Conn = (
    tokio::net::tcp::OwnedWriteHalf,
    BufReader<tokio::net::tcp::OwnedReadHalf>,
);

/// Connect, send a header, and return the ack/reply frame (one line).
async fn connect_and_send(port: u16, header: &Request) -> (Conn, Reply) {
    let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let (rd, mut writer) = stream.into_split();
    let mut reader = BufReader::new(rd);
    let frame = proto::encode(header);
    writer.write_all(frame.as_bytes()).await.unwrap();
    writer.write_all(b"\n").await.unwrap();
    writer.flush().await.unwrap();
    let reply = read_reply(&mut reader).await;
    ((writer, reader), reply)
}

/// Read one bridge frame from the reader and parse it as a `Reply`.
async fn read_reply(reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>) -> Reply {
    let mut line = String::new();
    let n = reader.read_line(&mut line).await.unwrap();
    assert!(n > 0, "connection closed before a reply frame");
    proto::decode(line.trim_end()).expect("reply frame decodes")
}

/// Send one raw line (with newline) and flush.
async fn send_line(writer: &mut tokio::net::tcp::OwnedWriteHalf, line: &str) {
    writer.write_all(line.as_bytes()).await.unwrap();
    writer.write_all(b"\n").await.unwrap();
    writer.flush().await.unwrap();
}

/// Read one relayed line; EOF is an error here (the caller expected data).
async fn read_line(reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>) -> String {
    let mut line = String::new();
    let n = reader.read_line(&mut line).await.unwrap();
    assert!(n > 0, "unexpected EOF while waiting for a relayed line");
    line.trim_end().to_string()
}

/// Read the daemon's exit trailer: the final `BRIDGE1` frame that carries
/// the agent's exit status and stderr tail, appended after the agent's own
/// output ends.
async fn read_trailer(reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>) -> String {
    let line = read_line(reader).await;
    let reply: Reply = proto::decode(&line).expect("trailer decodes as a bridge frame");
    assert!(!reply.ok, "the exit trailer reports the agent is gone");
    reply.error.expect("the trailer carries a reason")
}

/// Assert the reader hits EOF (the peer closed its write side).
async fn assert_eof(reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>) {
    let mut byte = [0u8; 1];
    let n = reader.read(&mut byte).await.unwrap();
    assert_eq!(n, 0, "expected EOF, got a byte");
}

/// A supervising daemon with a share directory of its own, so the registry
/// it reads and the agents it starts never touch the machine's real one.
async fn supervisor() -> (u16, tempfile::TempDir) {
    let share = tempfile::tempdir().expect("temp share dir");
    let config =
        remote_daemon::Config::new(mock_agent()).with_share_dir(Some(share.path().to_path_buf()));
    (remote_with(config).await, share)
}

/// Attach, and return the connection plus the session the ack names.
async fn attach(port: u16, session: Option<&str>) -> (Conn, String) {
    attach_with(port, session, vec![]).await
}

/// Same, for a caller that also has agent arguments to pass.
async fn attach_with(port: u16, session: Option<&str>, args: Vec<String>) -> (Conn, String) {
    let (conn, reply) = connect_and_send(
        port,
        &Request::Attach {
            session: session.map(str::to_string),
            args,
        },
    )
    .await;
    assert!(reply.ok, "attach failed: {reply:?}");
    let named = reply.session.expect("an attach ack names its session");
    (conn, named)
}

/// Ask the agent on the other end of a relay for one line.
async fn ask(conn: &mut Conn, line: &str) -> String {
    send_line(&mut conn.0, line).await;
    read_line(&mut conn.1).await
}

// --- remote daemon ---------------------------------------------------------

#[tokio::test]
async fn spawn_replies_ack_and_relays_opaquely() {
    let port = remote_on(&mock_agent()).await;
    let ((mut writer, mut reader), ack) =
        connect_and_send(port, &Request::Spawn { args: vec![] }).await;
    assert_eq!(ack, Reply::spawn_ok());

    // Relay is opaque: whatever bytes the agent writes come back verbatim.
    send_line(&mut writer, "say hello-through-the-bridge").await;
    assert_eq!(read_line(&mut reader).await, "hello-through-the-bridge");

    send_line(
        &mut writer,
        r#"{"jsonrpc":"2.0","id":"x","method":"whatever"}"#,
    )
    .await;
    assert_eq!(
        read_line(&mut reader).await,
        r#"{"jsonrpc":"2.0","id":"x","method":"whatever"}"#
    );
}

#[tokio::test]
async fn spawn_args_reach_the_agent() {
    let port = remote_on(&mock_agent()).await;
    let ((mut writer, mut reader), ack) = connect_and_send(
        port,
        &Request::Spawn {
            args: vec![
                "-w".into(),
                "/srv/proj".into(),
                "--session".into(),
                "abc".into(),
            ],
        },
    )
    .await;
    assert!(ack.ok);

    send_line(&mut writer, "argv").await;
    assert_eq!(
        read_line(&mut reader).await,
        "-w\u{1f}/srv/proj\u{1f}--session\u{1f}abc"
    );
}

#[tokio::test]
async fn agent_exit_closes_the_socket() {
    let port = remote_on(&mock_agent()).await;
    let ((mut writer, mut reader), _ack) =
        connect_and_send(port, &Request::Spawn { args: vec![] }).await;

    // `die` makes the mock agent exit without replying; the daemon must
    // observe stdout EOF and half-close our read side (the agent's
    // exit-marker line arrives first, then the trailer, then the end of the
    // stream).
    send_line(&mut writer, "die").await;
    assert_eq!(read_line(&mut reader).await, "MOCK-AGENT-EOF");
    let reason = read_trailer(&mut reader).await;
    assert!(reason.starts_with("agent exited:"), "{reason}");
    assert_eof(&mut reader).await;
}

#[tokio::test]
async fn exit_trailer_carries_the_agent_stderr_tail() {
    let port = remote_on(&mock_agent()).await;
    let ((mut writer, mut reader), _ack) =
        connect_and_send(port, &Request::Spawn { args: vec![] }).await;

    // The whole point of the trailer: a remote agent that dies on startup
    // must reach the frontend with its reason attached, the way a locally
    // spawned agent does through its stderr tail.
    send_line(&mut writer, "fail could not open work dir").await;
    let reason = read_trailer(&mut reader).await;
    assert!(reason.contains("agent exited:"), "{reason}");
    assert!(reason.contains("could not open work dir"), "{reason}");
    assert_eof(&mut reader).await;
}

#[tokio::test]
async fn client_half_close_reaches_agent_stdin() {
    let port = remote_on(&mock_agent()).await;
    let ((mut writer, mut reader), _ack) =
        connect_and_send(port, &Request::Spawn { args: vec![] }).await;

    // Half-close our write side (the graceful "please exit" a frontend
    // sends): the agent must see stdin EOF, emit its marker, and exit.
    writer.shutdown().await.unwrap();
    assert_eq!(read_line(&mut reader).await, "MOCK-AGENT-EOF");
    let reason = read_trailer(&mut reader).await;
    assert!(reason.starts_with("agent exited:"), "{reason}");
    assert_eof(&mut reader).await;
}

#[tokio::test]
async fn default_work_dir_fills_in_for_args_that_name_none() {
    let port = remote_with(
        remote_daemon::Config::new(mock_agent()).with_default_work_dir(Some("/srv/home".into())),
    )
    .await;
    let ((mut writer, mut reader), ack) = connect_and_send(
        port,
        &Request::Spawn {
            args: vec!["--session".into(), "abc".into()],
        },
    )
    .await;
    assert!(ack.ok);

    // A frontend on another OS sends no -w at all; the daemon supplies one
    // that exists on *its* machine.
    send_line(&mut writer, "argv").await;
    assert_eq!(
        read_line(&mut reader).await,
        "-w\u{1f}/srv/home\u{1f}--session\u{1f}abc"
    );
}

#[tokio::test]
async fn a_callers_work_dir_beats_the_daemon_default() {
    let port = remote_with(
        remote_daemon::Config::new(mock_agent()).with_default_work_dir(Some("/srv/home".into())),
    )
    .await;
    let ((mut writer, mut reader), _ack) = connect_and_send(
        port,
        &Request::Spawn {
            args: vec!["-w".into(), "/srv/proj".into()],
        },
    )
    .await;

    send_line(&mut writer, "argv").await;
    assert_eq!(read_line(&mut reader).await, "-w\u{1f}/srv/proj");
}

#[tokio::test]
async fn spawn_failure_surfaces_as_error_frame() {
    let port = remote_on("/nonexistent/dvadva-agent-binary").await;
    let ((_writer, mut reader), reply) =
        connect_and_send(port, &Request::Spawn { args: vec![] }).await;
    assert!(!reply.ok, "expected a failure reply: {reply:?}");
    assert!(reply.error.unwrap().contains("failed to spawn agent"));
    assert_eof(&mut reader).await;
}

#[tokio::test]
async fn list_sessions_replies_ok_then_closes() {
    let port = remote_on(&mock_agent()).await;
    let ((_writer, mut reader), reply) = connect_and_send(port, &Request::ListSessions).await;

    // Read-only against the real ~/.kimi (possibly empty on CI): the shape
    // is the contract, the contents are the machine's.
    assert!(reply.ok, "list_sessions failed: {reply:?}");
    assert!(
        reply.sessions.is_some(),
        "sessions field missing: {reply:?}"
    );
    assert_eof(&mut reader).await;
}

#[tokio::test]
async fn version_answers_without_spawning_anything() {
    // The liveness probe behind a UI's connection light: it must answer even
    // when no agent binary exists, because it never spawns one.
    let port = remote_on("/nonexistent/dvadva-agent-binary").await;
    let ((_writer, mut reader), reply) = connect_and_send(port, &Request::Version).await;
    assert!(reply.ok, "version failed: {reply:?}");
    assert_eq!(reply.version.as_deref(), Some(env!("CARGO_PKG_VERSION")));
    // The build and the frame protocol are separate answers, and the probe
    // that drives a UI's connection light reads both.
    assert_eq!(
        reply.proto.as_deref(),
        Some(dvadva_bridge::proto::BRIDGE_PROTOCOL_VERSION)
    );
    assert_eof(&mut reader).await;
}

#[tokio::test]
async fn garbage_header_is_rejected_with_an_error_frame() {
    let port = remote_on(&mock_agent()).await;
    let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let (rd, mut writer) = stream.into_split();
    let mut reader = BufReader::new(rd);
    send_line(&mut writer, "GET / HTTP/1.1").await;

    let reply = read_reply(&mut reader).await;
    assert!(!reply.ok);
    assert!(reply.error.unwrap().contains("BRIDGE1"));
    assert_eof(&mut reader).await;
}

// --- local daemon ----------------------------------------------------------

#[tokio::test]
async fn local_forwards_spawn_relay_both_ways() {
    let upstream = remote_on(&mock_agent()).await;
    let port = local_up(upstream).await;

    let ((mut writer, mut reader), ack) =
        connect_and_send(port, &Request::Spawn { args: vec![] }).await;
    assert_eq!(ack, Reply::spawn_ok(), "the upstream ack must be forwarded");

    send_line(&mut writer, "say through-two-hops").await;
    assert_eq!(read_line(&mut reader).await, "through-two-hops");
}

#[tokio::test]
async fn local_forwards_list_sessions() {
    let upstream = remote_on(&mock_agent()).await;
    let port = local_up(upstream).await;

    let ((_writer, mut reader), reply) = connect_and_send(port, &Request::ListSessions).await;
    assert!(reply.ok, "forwarded listing failed: {reply:?}");
    assert!(reply.sessions.is_some());
    assert_eof(&mut reader).await;
}

#[tokio::test]
async fn local_forwards_version() {
    let upstream = remote_on(&mock_agent()).await;
    let port = local_up(upstream).await;

    let ((_writer, mut reader), reply) = connect_and_send(port, &Request::Version).await;
    assert!(reply.ok, "forwarded version failed: {reply:?}");
    assert_eq!(reply.version.as_deref(), Some(env!("CARGO_PKG_VERSION")));
    assert_eof(&mut reader).await;
}

#[tokio::test]
async fn local_reports_unreachable_upstream() {
    // An upstream port with no listener.
    let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_port = dead.local_addr().unwrap().port();
    drop(dead);

    let port = local_up(dead_port).await;
    let ((_writer, mut reader), reply) =
        connect_and_send(port, &Request::Spawn { args: vec![] }).await;
    assert!(!reply.ok);
    assert!(reply.error.unwrap().contains("failed to reach upstream"));
    assert_eof(&mut reader).await;
}

// --- the supervisor --------------------------------------------------------

#[tokio::test]
async fn attach_starts_an_agent_and_names_the_session_it_got() {
    let (port, _share) = supervisor().await;
    let (mut conn, session) = attach(port, None).await;
    assert!(!session.is_empty(), "the ack must name a session");

    // The relay is as opaque as the spawn path's.
    assert_eq!(ask(&mut conn, "say hello").await, "hello");

    // And the agent was told to listen, on top of whatever it was given.
    let argv = ask(&mut conn, "argv").await;
    assert!(argv.split('\u{1f}').any(|arg| arg == "--listen"), "{argv}");

    send_line(&mut conn.0, "stop").await;
}

#[tokio::test]
async fn a_client_that_leaves_does_not_take_the_agent_with_it() {
    // The phase in one test: attach, walk away, come back, find the same
    // process still holding the session.
    let (port, _share) = supervisor().await;
    let (mut first, session) = attach(port, None).await;
    let pid = ask(&mut first, "pid").await;

    // Not a half-close: the whole socket goes, the way a killed frontend's
    // does. On the spawn path this is the end of the agent.
    drop(first);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let (mut second, again) = attach(port, Some(&session)).await;
    assert_eq!(again, session, "the same session, not a new one");
    assert_eq!(
        ask(&mut second, "pid").await,
        pid,
        "a second attach must reach the same process, not start another"
    );

    send_line(&mut second.0, "stop").await;
}

#[tokio::test]
async fn an_agent_that_never_listens_is_reported_with_its_log() {
    // The diagnosis the exit trailer exists for, arriving earlier: on the
    // supervised path there is no relay to append it to, so it has to be
    // the ack itself.
    let (port, _share) = supervisor().await;
    let ((_conn, mut reader), reply) = connect_and_send(
        port,
        &Request::Attach {
            session: None,
            args: vec!["--fail-to-start".into(), "no work dir over here".into()],
        },
    )
    .await;

    assert!(!reply.ok, "a failed start must not be acknowledged");
    let error = reply.error.expect("a reason");
    assert!(
        error.contains("exited before it started listening"),
        "{error}"
    );
    assert!(error.contains("no work dir over here"), "{error}");
    assert_eof(&mut reader).await;
}

#[tokio::test]
async fn an_agent_that_falls_over_ends_the_relay_with_a_trailer() {
    let (port, _share) = supervisor().await;
    let (mut conn, session) = attach(port, None).await;

    send_line(&mut conn.0, "crash").await;
    let reason = read_trailer(&mut conn.1).await;

    assert!(reason.contains(&session), "{reason}");
    assert!(reason.contains("closed the connection"), "{reason}");
    // The log of an agent this connection started is quotable, so the
    // client gets the same diagnosis a local one leaves in its stderr tail.
    assert!(reason.contains("falling over on request"), "{reason}");
    assert_eof(&mut conn.1).await;
}

#[tokio::test]
async fn a_stale_registry_entry_does_not_fail_the_attach() {
    // The killed-agent case: something is listed for this session, and it
    // is not there any more. The request can still be served by starting a
    // live one, so it is.
    let (port, _share) = supervisor().await;
    let session = "a-session-that-outlives-its-agent";
    // A cold resume names the session twice and means two different things
    // by it: the registry key here, and the agent's own `--session` over
    // there. The daemon does not invent the second from the first.
    let args = vec!["--session".to_string(), session.to_string()];

    let (mut first, named) = attach_with(port, Some(session), args.clone()).await;
    assert_eq!(named, session, "the agent kept the id it was given");
    let pid = ask(&mut first, "pid").await;
    send_line(&mut first.0, "stop").await;
    drop(first);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let (mut second, again) = attach_with(port, Some(session), args).await;
    assert_eq!(again, session);
    assert_ne!(
        ask(&mut second, "pid").await,
        pid,
        "the dead agent's entry must not be attached to"
    );

    send_line(&mut second.0, "stop").await;
}

#[tokio::test]
async fn list_sessions_says_which_ones_are_live() {
    let (port, _share) = supervisor().await;
    let (mut conn, session) = attach(port, None).await;

    let ((_writer, mut reader), reply) = connect_and_send(port, &Request::ListSessions).await;
    assert!(reply.ok, "listing failed: {reply:?}");
    let sessions = reply.sessions.expect("a listing");
    let listed = sessions
        .iter()
        .find(|entry| entry.id == session)
        .unwrap_or_else(|| panic!("the live session is missing from {sessions:?}"));
    assert!(listed.live, "a session with an agent on it must say so");
    assert_eof(&mut reader).await;

    send_line(&mut conn.0, "stop").await;
}

#[tokio::test]
async fn a_client_that_says_goodbye_gets_no_trailer() {
    // How the daemon's two endings are told apart from the outside: a
    // detach ends the stream and says nothing, because nothing died. Only
    // an agent going away earns last words.
    let (port, _share) = supervisor().await;
    let (mut conn, session) = attach(port, None).await;
    assert_eq!(ask(&mut conn, "say still here").await, "still here");

    conn.0.shutdown().await.unwrap();
    assert_eof(&mut conn.1).await;

    // And the agent it left behind is still the one holding the session.
    let (mut second, again) = attach(port, Some(&session)).await;
    assert_eq!(again, session);
    assert_eq!(ask(&mut second, "say back").await, "back");
    send_line(&mut second.0, "stop").await;
}
