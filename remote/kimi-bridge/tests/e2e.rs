//! Loopback end-to-end tests: the remote and local daemons on real TCP
//! sockets, with the mock agent (`tests/mock_agent.rs`) standing in for
//! `kimi-agent`. These pin the whole contract: frame handling, argument
//! passing, opaque relay, and close propagation in both directions.

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::net::TcpStream;

use kimi_bridge::local_daemon;
use kimi_bridge::proto::{self, Reply, Request};
use kimi_bridge::remote_daemon;

/// Path of the mock agent binary (cargo exposes every bin of the package).
fn mock_agent() -> String {
    env!("CARGO_BIN_EXE_kimi-bridge-mock-agent").to_string()
}

/// Start the remote daemon on an ephemeral loopback port.
async fn remote_on(agent_bin: &str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let agent_bin = agent_bin.to_string();
    tokio::spawn(async move {
        let _ = remote_daemon::serve(listener, agent_bin).await;
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
    // exit-marker line arrives first, then the end of the stream).
    send_line(&mut writer, "die").await;
    assert_eq!(read_line(&mut reader).await, "MOCK-AGENT-EOF");
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
    assert_eof(&mut reader).await;
}

#[tokio::test]
async fn spawn_failure_surfaces_as_error_frame() {
    let port = remote_on("/nonexistent/kimi-agent-binary").await;
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
