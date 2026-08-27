//! Detach without dying: one agent, over a real loopback socket, outliving
//! the clients that come and go on it.
//!
//! Everything here goes through `TcpStream` rather than a duplex pair on
//! purpose. Half the point of the listening transport is what happens when a
//! *socket* drops — a frontend killed, a laptop closed, an `ssh -L` cut — and
//! an in-process pipe cannot fail the way a socket does.

mod wire_agent_fixture;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpStream;

use dvadva_agent::wire::WIRE_PROTOCOL_VERSION;
use dvadva_agent::wire::listener::{ListenOptions, TOKEN_FILE_NAME, bind};
use dvadva_agent::wire::server::WireServer;

use wire_agent_fixture::{
    ARRIVES, Fixture, TestClient, asking_agent, event_type, is_answer_to, request_type,
    talking_agent,
};

/// A listening agent, and what a client needs to reach it.
struct Listening {
    addr: SocketAddr,
    token: String,
    agent: Fixture,
    _tmp: TempDir,
}

impl Listening {
    /// Put a scripted agent on a loopback port, exactly as `--listen` does.
    async fn start(agent: Fixture) -> Self {
        let tmp = TempDir::new().expect("temp dir");
        let listening = bind(ListenOptions {
            addr: "127.0.0.1:0".parse().expect("addr"),
            token_file: tmp.path().join(TOKEN_FILE_NAME),
            // The test harness owns this process's stdin; only the real
            // binary hands it to a client.
            inherit_stdio: false,
        })
        .await
        .expect("bind");

        let addr = listening.addr();
        let token = listening.token().to_string();
        let server: Arc<WireServer> = Arc::clone(&agent.server);
        tokio::spawn(async move {
            let _ = listening.serve(server).await;
        });

        Self {
            addr,
            token,
            agent,
            _tmp: tmp,
        }
    }

    /// Attach the way a frontend does: connect, present the token, go.
    async fn attach(&self) -> TestClient<TcpStream> {
        let mut client = self.connect().await;
        client
            .send_raw(&format!(
                "{{\"auth\":\"{}\",\"client\":\"test\"}}",
                self.token
            ))
            .await;
        let hello = client.next_frame(ARRIVES).await.expect("a handshake reply");
        assert_eq!(hello.get("auth"), Some(&json!("ok")), "denied: {hello}");
        client
    }

    async fn connect(&self) -> TestClient<TcpStream> {
        let socket = TcpStream::connect(self.addr).await.expect("connect");
        TestClient::over(socket)
    }
}

/// Run a whole turn and wait for its result.
async fn prompt(client: &mut TestClient<TcpStream>, text: &str) -> Value {
    let id = client.call("prompt", json!({"user_input": text})).await;
    client
        .wait_for("the prompt result", |frame| is_answer_to(frame, &id))
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_drops_its_socket_leaves_the_agent_running() {
    let listening = Listening::start(talking_agent()).await;

    let mut first = listening.attach().await;
    first.initialize().await;
    let finished = prompt(&mut first, "hi").await;
    assert_eq!(finished.pointer("/result/status"), Some(&json!("finished")));

    // Not a polite half-close: the whole socket goes, the way a killed
    // frontend's does. Over stdio this is the end of the process.
    drop(first);
    tokio::time::sleep(Duration::from_millis(100)).await;

    // The same agent is still there, and it is the *same* one: a fresh
    // process would have replayed an empty file.
    let mut second = listening.attach().await;
    second.initialize().await;
    let replay = second.call("replay", json!({})).await;
    let caught_up = second
        .wait_for("the replay result", |frame| is_answer_to(frame, &replay))
        .await;
    assert_eq!(
        caught_up.pointer("/result/status"),
        Some(&json!("finished"))
    );
    assert!(
        caught_up
            .pointer("/result/events")
            .and_then(Value::as_u64)
            .is_some_and(|events| events > 0),
        "the turn the first client ran should still be in the transcript: {caught_up}"
    );

    // And it still works, rather than merely still existing.
    let again = prompt(&mut second, "again").await;
    assert_eq!(again.pointer("/result/status"), Some(&json!("finished")));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_parked_on_an_approval_survives_the_client_that_started_it() {
    let listening = Listening::start(asking_agent()).await;

    let mut first = listening.attach().await;
    first.initialize().await;
    let started = first.call("prompt", json!({"user_input": "go"})).await;
    let request = first
        .wait_for("the approval request", |frame| {
            request_type(frame) == Some("ApprovalRequest")
        })
        .await;
    let request_id = request
        .get("id")
        .and_then(Value::as_str)
        .expect("a request id")
        .to_string();

    // The client that asked the question walks away before answering it.
    // The turn is the session's, not the connection's, so it waits.
    let _ = started;
    drop(first);
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut second = listening.attach().await;
    second.initialize().await;
    let replay = second.call("replay", json!({})).await;
    second
        .wait_for("the replay result", |frame| is_answer_to(frame, &replay))
        .await;

    // Still open, so it is handed over armed rather than replayed as history.
    let handed_over = second
        .wait_for("the re-armed approval request", |frame| {
            request_type(frame) == Some("ApprovalRequest")
        })
        .await;
    assert_eq!(
        handed_over.get("id").and_then(Value::as_str),
        Some(request_id.as_str()),
        "the newcomer should inherit the same question, not a new one"
    );

    second
        .send(json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {"request_id": request_id, "response": "approve"},
        }))
        .await;

    // Nobody is left to receive the `prompt` answer — its asker is gone — but
    // the turn ending is a session fact and reaches whoever is attached.
    second
        .wait_for("the end of the turn", |frame| {
            event_type(frame) == Some("TurnEnd")
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_without_the_token_never_reaches_the_wire() {
    let listening = Listening::start(talking_agent()).await;

    let mut stranger = listening.connect().await;
    stranger.send_raw("{\"auth\":\"not-the-token\"}").await;
    let denial = stranger.next_frame(ARRIVES).await.expect("an answer");
    assert_eq!(denial.get("auth"), Some(&json!("denied")));
    assert_eq!(denial.get("error"), Some(&json!("invalid token")));

    // Refused means refused: the wire server never sees the connection, so a
    // `prompt` sent anyway gets nothing back. `initialize` is not the gate —
    // nothing obliges a client to send it first.
    stranger
        .send(json!({"jsonrpc": "2.0", "id": "1", "method": "prompt",
                         "params": {"user_input": "run something"}}))
        .await;
    assert!(
        stranger.drain().await.is_empty(),
        "a refused connection must not be served"
    );

    // And the refusal cost the agent nothing.
    let mut client = listening.attach().await;
    let result = client.initialize().await;
    assert_eq!(
        result.pointer("/result/protocol_version").unwrap(),
        WIRE_PROTOCOL_VERSION
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_may_pipeline_its_first_call_behind_the_handshake() {
    let listening = Listening::start(talking_agent()).await;
    let mut client = listening.connect().await;

    // One write, two lines. The handshake reader must leave the second one
    // for the wire server instead of swallowing it with the first.
    client
        .send_raw(&format!(
            "{{\"auth\":\"{}\"}}\n{{\"jsonrpc\":\"2.0\",\"id\":\"1\",\"method\":\"initialize\",\"params\":{{\"protocol_version\":\"{WIRE_PROTOCOL_VERSION}\"}}}}",
            listening.token
        ))
        .await;

    let hello = client.next_frame(ARRIVES).await.expect("a handshake reply");
    assert_eq!(hello.get("auth"), Some(&json!("ok")));
    assert_eq!(
        hello.get("session").and_then(Value::as_str),
        Some(listening.agent.server.session_id().as_str())
    );

    let result = client
        .wait_for("the initialize result", |frame| is_answer_to(frame, "1"))
        .await;
    assert_eq!(
        result.pointer("/result/capabilities/multi_client"),
        Some(&json!(true))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn two_sockets_share_one_agent_and_one_leaving_does_not_silence_the_other() {
    let listening = Listening::start(talking_agent()).await;

    let mut watcher = listening.attach().await;
    watcher.initialize().await;
    let mut leaver = listening.attach().await;
    leaver.initialize().await;
    watcher.drain().await;

    drop(leaver);
    tokio::time::sleep(Duration::from_millis(100)).await;

    let finished = prompt(&mut watcher, "hi").await;
    assert_eq!(finished.pointer("/result/status"), Some(&json!("finished")));
}
