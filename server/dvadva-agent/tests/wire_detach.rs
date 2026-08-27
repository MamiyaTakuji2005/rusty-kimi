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
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

use dvadva_agent::live::Registry;
use dvadva_agent::wire::WIRE_PROTOCOL_VERSION;
use dvadva_agent::wire::listener::{ListenOptions, TOKEN_FILE_NAME, bind};
use dvadva_agent::wire::server::WireServer;

use wire_agent_fixture::{
    ARRIVES, Fixture, TestClient, asking_agent, event_type, is_answer_to, request_type,
    talking_agent, working_agent,
};

/// Short enough that the lifetime tests finish, long enough that a loaded
/// machine does not reap an agent mid-handshake.
const IMPATIENT: Duration = Duration::from_secs(2);

/// A listening agent, and what a client needs to reach it.
struct Listening {
    addr: SocketAddr,
    token: String,
    agent: Fixture,
    registry: Registry,
    /// The serving task. Its completion *is* the end of the agent, which is
    /// what the lifetime tests below wait on.
    serving: tokio::task::JoinHandle<()>,
    _tmp: TempDir,
}

impl Listening {
    /// Put a scripted agent on a loopback port, exactly as `--listen` does.
    async fn start(agent: Fixture) -> Self {
        Self::start_with(agent, None).await
    }

    async fn start_with(agent: Fixture, idle_timeout: Option<Duration>) -> Self {
        let tmp = TempDir::new().expect("temp dir");
        let registry = Registry::at(tmp.path().join("live"));
        let listening = bind(ListenOptions {
            addr: "127.0.0.1:0".parse().expect("addr"),
            token_file: tmp.path().join(TOKEN_FILE_NAME),
            // The test harness owns this process's stdin; only the real
            // binary hands it to a client.
            inherit_stdio: false,
            // And a test agent must not advertise itself to the frontends
            // running on the machine the test runs on.
            registry_dir: Some(registry.dir().to_path_buf()),
            idle_timeout,
        })
        .await
        .expect("bind");

        let addr = listening.addr();
        let token = listening.token().to_string();
        let server: Arc<WireServer> = Arc::clone(&agent.server);
        let serving = tokio::spawn(async move {
            let _ = listening.serve(server).await;
        });

        Self {
            addr,
            token,
            agent,
            registry,
            serving,
            _tmp: tmp,
        }
    }

    /// Wait for the agent to stop of its own accord, and say whether it did.
    async fn stops_within(&mut self, patience: Duration) -> bool {
        tokio::time::timeout(patience, &mut self.serving)
            .await
            .is_ok()
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

/// Wait for the listening agent to appear in the registry it was given.
/// Registration happens on the serving task, a moment after `bind` returns.
async fn wait_for_listing(registry: &Registry) -> dvadva_agent::live::LiveSession {
    for _ in 0..100 {
        if let Some(entry) = registry.list().await.into_iter().next() {
            return entry;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "the agent never listed itself in {}",
        registry.dir().display()
    );
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

#[tokio::test(flavor = "multi_thread")]
async fn a_listening_agent_lists_itself_and_can_be_reached_from_the_listing_alone() {
    // The supervisor's path: something that did not spawn this agent, and
    // never saw its announce line, finds it and attaches with nothing but
    // what the registry says.
    let listening = Listening::start(talking_agent()).await;

    let entry = wait_for_listing(&listening.registry).await;
    assert_eq!(entry.addr, listening.addr.to_string());
    assert_eq!(entry.pid, std::process::id());
    assert_eq!(entry.protocol_version, WIRE_PROTOCOL_VERSION);

    let token = tokio::fs::read_to_string(&entry.token_file)
        .await
        .expect("the registry names a token file that exists");
    let socket = TcpStream::connect(entry.addr.parse::<SocketAddr>().expect("addr"))
        .await
        .expect("connect to the listed address");
    let mut client = TestClient::over(socket);
    client
        .send_raw(&format!(
            "{{\"auth\":\"{}\",\"client\":\"a supervisor\"}}",
            token.trim()
        ))
        .await;
    let hello = client.next_frame(ARRIVES).await.expect("a handshake reply");

    assert_eq!(hello.get("auth"), Some(&json!("ok")), "denied: {hello}");
    assert_eq!(hello.get("session"), Some(&json!(entry.session)));
}

// ------------------------------------------- how a detached agent ever ends

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_nobody_ever_attached_to_stops_by_itself() {
    // The accumulation case: a supervisor started an agent, whoever asked
    // for it never arrived, and nothing else was ever going to end it.
    let mut listening = Listening::start_with(talking_agent(), Some(IMPATIENT)).await;
    wait_for_listing(&listening.registry).await;

    assert!(
        listening.stops_within(ARRIVES).await,
        "an idle agent should stop on its own"
    );
    assert!(
        listening.registry.list().await.is_empty(),
        "and take its registry entry with it"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_that_is_working_is_never_idle_however_alone_it_is() {
    // The opposite case, and the one that makes detaching worth anything:
    // the client left mid-turn, and the turn is the point.
    let mut listening = Listening::start_with(working_agent(), Some(IMPATIENT)).await;
    let release = Arc::clone(&listening.agent.release);

    let mut client = listening.attach().await;
    client.initialize().await;
    client.call("prompt", json!({"user_input": "go"})).await;
    client
        .wait_for("the tool call to start", |frame| {
            event_type(frame) == Some("ToolCall")
        })
        .await;
    drop(client);

    // Well past the timeout, with nobody watching and the tool still running.
    tokio::time::sleep(IMPATIENT * 3).await;
    assert!(
        !listening.serving.is_finished(),
        "a working agent must not be reaped for having no audience"
    );

    // And once the work is done, the same silence does end it.
    release.notify_one();
    assert!(
        listening.stops_within(ARRIVES).await,
        "an agent that finished its turn alone should then stop"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_parked_with_nobody_left_to_answer_counts_as_idle() {
    // A turn waiting on an approval is waiting on a person. With nothing
    // attached there is no person, so this is the one shape of "busy" that
    // would otherwise strand an agent forever.
    let mut listening = Listening::start_with(asking_agent(), Some(IMPATIENT)).await;

    let mut client = listening.attach().await;
    client.initialize().await;
    client.call("prompt", json!({"user_input": "go"})).await;
    client
        .wait_for("the approval request", |frame| {
            request_type(frame) == Some("ApprovalRequest")
        })
        .await;
    drop(client);

    assert!(
        listening.stops_within(ARRIVES).await,
        "an agent parked on a question nobody is left to answer should stop"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_attached_client_can_ask_the_agent_to_stop() {
    // The explicit end, for a frontend two daemons away that has no way to
    // send this process a signal.
    let mut listening = Listening::start(talking_agent()).await;
    wait_for_listing(&listening.registry).await;

    let mut client = listening.attach().await;
    client.initialize().await;
    let id = client.call("shutdown", json!({})).await;
    let ack = client
        .wait_for("the shutdown ack", |frame| is_answer_to(frame, &id))
        .await;
    assert_eq!(ack.pointer("/result/status"), Some(&json!("stopping")));

    assert!(
        listening.stops_within(ARRIVES).await,
        "the agent should stop when asked"
    );
    assert!(listening.registry.list().await.is_empty());

    // The ack came first and the stream ended after it, in that order: a
    // client must be able to tell "it is going" from "it fell over".
    assert!(
        client.reader.read_u8().await.is_err(),
        "the connection should end once the session does"
    );
}

// ------------------------------------------------ what a returning client needs

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_names_the_session_a_client_landed_on() {
    // A client that means to come back has to know what to come back *to*,
    // and only the agent can say — a fresh session's id is minted here.
    let listening = Listening::start(talking_agent()).await;

    let mut client = listening.attach().await;
    let result = client.initialize().await;

    assert_eq!(
        result.pointer("/result/session").and_then(Value::as_str),
        Some(listening.agent.server.session_id().as_str())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_catching_up_is_told_whether_the_agent_is_busy() {
    // Replay is the past tense. A client that attaches mid-turn replays a
    // `TurnBegin` with no end and cannot tell that from an interrupted one,
    // so it has to be told the present tense outright.
    let listening = Listening::start(working_agent()).await;
    let release = Arc::clone(&listening.agent.release);

    let mut worker = listening.attach().await;
    worker.initialize().await;
    let turn = worker.call("prompt", json!({"user_input": "go"})).await;
    worker
        .wait_for("the tool call to start", |frame| {
            event_type(frame) == Some("ToolCall")
        })
        .await;

    let mut newcomer = listening.attach().await;
    newcomer.initialize().await;
    let replay = newcomer.call("replay", json!({})).await;
    let caught_up = newcomer
        .wait_for("the replay result", |frame| is_answer_to(frame, &replay))
        .await;
    assert_eq!(
        caught_up.pointer("/result/turn_running"),
        Some(&json!(true)),
        "a client joining a working agent must not be told it is ready"
    );

    // Waiting for the `prompt` answer rather than for the `TurnEnd` event:
    // the event is emitted from inside the turn, the answer after the
    // session has released the turn slot, and the slot is what this flag
    // reports.
    release.notify_one();
    worker
        .wait_for("the end of the turn", |frame| is_answer_to(frame, &turn))
        .await;

    // And the same question, asked of a quiet agent, answers the other way.
    let mut later = listening.attach().await;
    later.initialize().await;
    let replay = later.call("replay", json!({})).await;
    let caught_up = later
        .wait_for("the replay result", |frame| is_answer_to(frame, &replay))
        .await;
    assert_eq!(
        caught_up.pointer("/result/turn_running"),
        Some(&json!(false))
    );
}
