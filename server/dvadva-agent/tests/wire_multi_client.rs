//! Several frontends attached to one live agent.
//!
//! The transport here is a `tokio::io::duplex` pair per client, which is the
//! whole reason `WireServer::serve_connection` takes a reader and a writer
//! rather than reaching for stdio: attaching a second client is calling it a
//! second time, and a test can do that as easily as a listener will.

mod tool_test_utils;

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};

use dvadva_agent::llm::LLM;
use dvadva_agent::soul::agent::Agent;
use dvadva_agent::soul::approval::Approval;
use dvadva_agent::soul::context::Context;
use dvadva_agent::soul::kimisoul::KimiSoul;
use dvadva_agent::soul::toolset::KimiToolset;
use dvadva_agent::wire::WIRE_PROTOCOL_VERSION;
use dvadva_agent::wire::server::WireServer;
use kosong::chat_provider::{
    ChatProvider, ChatProviderError, StreamedMessage, ThinkingEffort, TokenUsage,
};
use kosong::message::{ContentPart, Message, StreamedMessagePart, TextPart, ToolCall};
use kosong::tooling::{CallableTool2, Tool, ToolReturnValue, tool_error, tool_ok};
use schemars::JsonSchema;

use tool_test_utils::RuntimeFixture;

/// How long to wait for something that should be coming.
const ARRIVES: Duration = Duration::from_secs(10);
/// How long to wait before concluding that nothing more is coming.
const SILENCE: Duration = Duration::from_millis(300);

// ---------------------------------------------------------------- the agent

struct ScriptedStream {
    parts: VecDeque<StreamedMessagePart>,
}

#[async_trait]
impl StreamedMessage for ScriptedStream {
    async fn next_part(&mut self) -> Result<Option<StreamedMessagePart>, ChatProviderError> {
        Ok(self.parts.pop_front())
    }

    fn id(&self) -> Option<String> {
        Some("scripted".to_string())
    }

    fn usage(&self) -> Option<TokenUsage> {
        None
    }
}

/// Replies with each scripted turn in order, then repeats the last one.
struct ScriptedProvider {
    turns: Vec<Vec<StreamedMessagePart>>,
    index: AtomicUsize,
}

#[async_trait]
impl ChatProvider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted"
    }

    fn model_name(&self) -> &str {
        "scripted"
    }

    fn thinking_effort(&self) -> Option<ThinkingEffort> {
        None
    }

    async fn generate(
        &self,
        _system_prompt: &str,
        _tools: &[Tool],
        _history: &[Message],
    ) -> Result<Box<dyn StreamedMessage>, ChatProviderError> {
        let index = self.index.fetch_add(1, Ordering::SeqCst);
        let turn = self.turns[std::cmp::min(index, self.turns.len() - 1)].clone();
        Ok(Box::new(ScriptedStream { parts: turn.into() }))
    }

    fn with_thinking(&self, _effort: ThinkingEffort) -> Box<dyn ChatProvider> {
        Box::new(ScriptedProvider {
            turns: self.turns.clone(),
            index: AtomicUsize::new(self.index.load(Ordering::SeqCst)),
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct NoParams {}

/// A tool that parks the turn on an approval, so a test can attach a client
/// to an agent that is waiting for an answer.
struct AsksFirst {
    approval: Arc<Approval>,
}

#[async_trait]
impl CallableTool2 for AsksFirst {
    type Params = NoParams;

    fn name(&self) -> &str {
        "asks_first"
    }

    fn description(&self) -> &str {
        "Asks before doing anything."
    }

    async fn call_typed(&self, _params: NoParams) -> ToolReturnValue {
        match self
            .approval
            .request("asks_first", "test:act", "May I?", None)
            .await
        {
            Ok(true) => tool_ok("approved", "approved", ""),
            Ok(false) => tool_ok("rejected", "rejected", ""),
            Err(err) => tool_error("", err.to_string(), ""),
        }
    }
}

struct Fixture {
    server: Arc<WireServer>,
    _fixture: RuntimeFixture,
    _tmp: TempDir,
}

/// An agent whose one scripted turn just talks.
fn talking_agent() -> Fixture {
    build(
        vec![vec![StreamedMessagePart::from(ContentPart::Text(
            TextPart::new("hello back"),
        ))]],
        true,
    )
}

/// An agent whose first turn calls a tool that needs approval, and whose
/// second turn (after the answer) just talks.
fn asking_agent() -> Fixture {
    let mut call = ToolCall::new("call-1", "asks_first");
    call.function.arguments = Some("{}".to_string());
    build(
        vec![
            vec![StreamedMessagePart::from(call)],
            vec![StreamedMessagePart::from(ContentPart::Text(TextPart::new(
                "done",
            )))],
        ],
        false,
    )
}

fn build(turns: Vec<Vec<StreamedMessagePart>>, yolo: bool) -> Fixture {
    let fixture = RuntimeFixture::new();
    let mut runtime = fixture.runtime.clone();
    runtime.approval = Arc::new(Approval::new(yolo));
    let llm = LLM {
        chat_provider: Box::new(ScriptedProvider {
            turns,
            index: AtomicUsize::new(0),
        }),
        max_context_size: 100_000,
        capabilities: HashSet::new(),
        model_config: None,
        provider_config: None,
    };
    *runtime.llm.try_write().expect("llm lock uncontended") = Some(Arc::new(llm));

    let mut toolset = KimiToolset::new();
    toolset.add(Arc::new(AsksFirst {
        approval: runtime.approval.clone(),
    }));

    let tmp = TempDir::new().expect("temp dir");
    let agent = Agent {
        name: "Test Agent".to_string(),
        system_prompt: "Test system prompt.".to_string(),
        toolset: Arc::new(tokio::sync::Mutex::new(toolset)),
        runtime,
    };
    let soul = KimiSoul::new(agent, Context::new(tmp.path().join("history.jsonl")));
    Fixture {
        server: Arc::new(WireServer::new(Arc::new(soul))),
        _fixture: fixture,
        _tmp: tmp,
    }
}

// --------------------------------------------------------------- the client

struct TestClient {
    reader: BufReader<ReadHalf<DuplexStream>>,
    writer: Option<WriteHalf<DuplexStream>>,
    next_id: u64,
}

impl TestClient {
    fn attach(server: &Arc<WireServer>) -> Self {
        let (theirs, mine) = tokio::io::duplex(4 << 20);
        let (server_read, server_write) = tokio::io::split(theirs);
        let server = Arc::clone(server);
        tokio::spawn(async move {
            let _ = server.serve_connection(server_read, server_write).await;
        });
        let (read, write) = tokio::io::split(mine);
        Self {
            reader: BufReader::new(read),
            writer: Some(write),
            next_id: 0,
        }
    }

    /// Every client mints its own ids from 1, exactly as `WireClient` does.
    /// Two clients therefore both call their first request "1", which is the
    /// point: nothing may route by a client-minted id.
    async fn call(&mut self, method: &str, params: Value) -> String {
        self.next_id += 1;
        let id = self.next_id.to_string();
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await;
        id
    }

    async fn send(&mut self, frame: Value) {
        let mut line = serde_json::to_string(&frame).expect("serialize frame");
        line.push('\n');
        let writer = self.writer.as_mut().expect("still attached");
        writer.write_all(line.as_bytes()).await.expect("write");
        writer.flush().await.expect("flush");
    }

    async fn next_frame(&mut self, patience: Duration) -> Option<Value> {
        let mut line = String::new();
        match tokio::time::timeout(patience, self.reader.read_line(&mut line)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => None,
            Ok(Ok(_)) => Some(serde_json::from_str(&line).expect("valid JSON frame")),
        }
    }

    async fn wait_for(&mut self, what: &str, mut pred: impl FnMut(&Value) -> bool) -> Value {
        let mut seen = Vec::new();
        while let Some(frame) = self.next_frame(ARRIVES).await {
            if pred(&frame) {
                return frame;
            }
            seen.push(frame);
        }
        panic!("never saw {what}; got {seen:#?}");
    }

    /// Everything already in flight, up to a beat of silence.
    async fn drain(&mut self) -> Vec<Value> {
        let mut frames = Vec::new();
        while let Some(frame) = self.next_frame(SILENCE).await {
            frames.push(frame);
        }
        frames
    }

    async fn initialize(&mut self) -> Value {
        let id = self
            .call(
                "initialize",
                json!({"protocol_version": WIRE_PROTOCOL_VERSION}),
            )
            .await;
        self.wait_for("the initialize result", |frame| is_answer_to(frame, &id))
            .await
    }

    /// Close the stream, the way a frontend quitting does.
    async fn detach(mut self) {
        drop(self.writer.take());
        // Give the server's read loop its EOF before the test looks again.
        while self.next_frame(SILENCE).await.is_some() {}
    }
}

fn is_answer_to(frame: &Value, id: &str) -> bool {
    frame.get("id").and_then(Value::as_str) == Some(id) && frame.get("method").is_none()
}

fn is_event(frame: &Value) -> bool {
    frame.get("method").and_then(Value::as_str) == Some("event")
}

fn event_type(frame: &Value) -> Option<&str> {
    frame.pointer("/params/type").and_then(Value::as_str)
}

fn request_type(frame: &Value) -> Option<&str> {
    if frame.get("method").and_then(Value::as_str) != Some("request") {
        return None;
    }
    frame.pointer("/params/type").and_then(Value::as_str)
}

// ----------------------------------------------------------------- the tests

#[tokio::test(flavor = "multi_thread")]
async fn two_clients_initialize_on_one_agent_without_their_ids_colliding() {
    let agent = talking_agent();
    let mut first = TestClient::attach(&agent.server);
    let mut second = TestClient::attach(&agent.server);

    let first_result = first.initialize().await;
    let second_result = second.initialize().await;

    for result in [&first_result, &second_result] {
        assert_eq!(
            result.pointer("/result/protocol_version").unwrap(),
            WIRE_PROTOCOL_VERSION
        );
        assert_eq!(
            result.pointer("/result/capabilities/multi_client"),
            Some(&json!(true)),
            "1.3 advertises that a second client is welcome"
        );
    }

    // Both minted the id "1". Each must have been answered exactly once: a
    // shared outbound queue would have handed one client both answers.
    for client in [&mut first, &mut second] {
        let extra: Vec<Value> = client
            .drain()
            .await
            .into_iter()
            .filter(|frame| is_answer_to(frame, "1"))
            .collect();
        assert!(
            extra.is_empty(),
            "a client received another client's answer: {extra:#?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_is_broadcast_but_its_result_goes_only_to_the_client_that_asked() {
    let agent = talking_agent();
    let mut asker = TestClient::attach(&agent.server);
    let mut watcher = TestClient::attach(&agent.server);
    asker.initialize().await;
    watcher.initialize().await;
    watcher.drain().await;

    let prompt = asker.call("prompt", json!({"user_input": "hi"})).await;
    asker
        .wait_for("the prompt result", |frame| is_answer_to(frame, &prompt))
        .await;

    let seen = watcher.drain().await;
    let kinds: Vec<&str> = seen.iter().filter_map(event_type).collect();
    assert!(
        kinds.contains(&"TurnBegin") && kinds.contains(&"TurnEnd"),
        "the watcher should have seen the whole turn; saw {kinds:?}"
    );
    let answers: Vec<&Value> = seen
        .iter()
        .filter(|frame| frame.get("method").is_none())
        .collect();
    assert!(
        answers.is_empty(),
        "the watcher was sent someone else's answer: {answers:#?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_can_attach_to_a_parked_agent_and_answer_the_approval() {
    let agent = asking_agent();
    let mut first = TestClient::attach(&agent.server);
    first.initialize().await;

    let prompt = first.call("prompt", json!({"user_input": "go"})).await;
    let request = first
        .wait_for("the approval request", |frame| {
            request_type(frame) == Some("ApprovalRequest")
        })
        .await;
    let request_id = request
        .get("id")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();

    // A second frontend arrives while the agent is parked. Neither the
    // initialize nor the replay may be refused for "a turn is in progress":
    // that state is exactly the one worth attaching to.
    let mut second = TestClient::attach(&agent.server);
    second.initialize().await;
    let replay = second.call("replay", json!({})).await;
    second
        .wait_for("the replay result", |frame| is_answer_to(frame, &replay))
        .await;

    // And the request is handed over live, after the replay, so the newcomer
    // can actually answer it instead of just seeing a dead dialog.
    let handed_over = second
        .wait_for("the re-armed approval request", |frame| {
            request_type(frame) == Some("ApprovalRequest")
        })
        .await;
    assert_eq!(
        handed_over.get("id").and_then(Value::as_str),
        Some(request_id.as_str())
    );

    second
        .send(json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {"request_id": request_id, "response": "approve"},
        }))
        .await;

    // The first client never answered, so its dialog has to come down on the
    // resolution event rather than hang there deciding nothing.
    let dismissal = first
        .wait_for("the approval resolution", |frame| {
            is_event(frame) && event_type(frame) == Some("ApprovalResponse")
        })
        .await;
    assert_eq!(
        dismissal.pointer("/params/payload/request_id"),
        Some(&json!(request_id))
    );

    let finished = first
        .wait_for("the prompt result", |frame| is_answer_to(frame, &prompt))
        .await;
    assert_eq!(finished.pointer("/result/status"), Some(&json!("finished")));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_stops_talking_still_gets_its_catch_up() {
    let agent = talking_agent();
    let mut client = TestClient::attach(&agent.server);
    client.initialize().await;
    let replay = client.call("replay", json!({})).await;

    // Half-close, the way a script that pipes its requests in and reads the
    // answers out does. Closing the read half means the client stopped
    // talking, not that it stopped listening, so its catch-up must finish.
    drop(client.writer.take());

    let answer = client
        .wait_for("the replay result", |frame| is_answer_to(frame, &replay))
        .await;
    assert_eq!(answer.pointer("/result/status"), Some(&json!("finished")));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_external_tool_name_belongs_to_one_client_and_leaves_with_it() {
    let agent = talking_agent();
    let tool = json!([{
        "name": "peek",
        "description": "Ask the frontend.",
        "parameters": {"type": "object"},
    }]);

    let mut owner = TestClient::attach(&agent.server);
    let accepted = owner
        .call(
            "initialize",
            json!({"protocol_version": WIRE_PROTOCOL_VERSION, "external_tools": tool}),
        )
        .await;
    let accepted = owner
        .wait_for("the initialize result", |frame| {
            is_answer_to(frame, &accepted)
        })
        .await;
    assert_eq!(
        accepted.pointer("/result/external_tools/accepted"),
        Some(&json!(["peek"]))
    );

    // A second client offering the same name is refused rather than silently
    // shadowing the first: only the registrant can service a call to it.
    let mut rival = TestClient::attach(&agent.server);
    let refused = rival
        .call(
            "initialize",
            json!({"protocol_version": WIRE_PROTOCOL_VERSION, "external_tools": tool}),
        )
        .await;
    let refused = rival
        .wait_for("the initialize result", |frame| {
            is_answer_to(frame, &refused)
        })
        .await;
    assert_eq!(
        refused.pointer("/result/external_tools/rejected/0/name"),
        Some(&json!("peek"))
    );
    assert!(
        refused
            .pointer("/result/external_tools/accepted")
            .and_then(Value::as_array)
            .is_none_or(|accepted| accepted.is_empty())
    );

    // The claim is released when its client goes: a registration nobody can
    // service would hang the next turn that called it.
    owner.detach().await;
    rival.detach().await;

    let mut heir = TestClient::attach(&agent.server);
    let inherited = heir
        .call(
            "initialize",
            json!({"protocol_version": WIRE_PROTOCOL_VERSION, "external_tools": tool}),
        )
        .await;
    let inherited = heir
        .wait_for("the initialize result", |frame| {
            is_answer_to(frame, &inherited)
        })
        .await;
    assert_eq!(
        inherited.pointer("/result/external_tools/accepted"),
        Some(&json!(["peek"])),
        "the name should be free once its owner detached"
    );
}
