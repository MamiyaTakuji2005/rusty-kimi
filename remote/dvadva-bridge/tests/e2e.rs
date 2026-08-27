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
