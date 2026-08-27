//! A scripted agent and a client to point at it, shared by the wire tests.
//!
//! The transport is the variable here. `WireServer::serve_connection` takes a
//! reader and a writer rather than reaching for stdio, so a test can attach a
//! client over a `tokio::io::duplex` pair (`wire_multi_client`) or over a real
//! loopback socket (`wire_detach`) with the same code on the other side. That
//! is the same seam the listening transport uses in production, which is why
//! it is worth testing through both.

#![allow(dead_code)]

// Resolved against tests/ either way: this file is both a module of the
// wire test binaries and, by cargo convention, a test binary of its own.
#[path = "tool_test_utils.rs"]
mod tool_test_utils;

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, DuplexStream, ReadHalf,
    WriteHalf,
};

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
pub const ARRIVES: Duration = Duration::from_secs(10);
/// How long to wait before concluding that nothing more is coming.
pub const SILENCE: Duration = Duration::from_millis(300);

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

/// A tool that blocks until the test releases it, so a turn can be held in
/// the *working* state for exactly as long as a test needs. A sleep would
/// have been a race with whatever the test is timing; this is a handshake.
struct WaitsToBeReleased {
    release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl CallableTool2 for WaitsToBeReleased {
    type Params = NoParams;

    fn name(&self) -> &str {
        "waits"
    }

    fn description(&self) -> &str {
        "Does nothing, slowly."
    }

    async fn call_typed(&self, _params: NoParams) -> ToolReturnValue {
        self.release.notified().await;
        tool_ok("done waiting", "done waiting", "")
    }
}

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

pub struct Fixture {
    pub server: Arc<WireServer>,
    /// Lets the `waits` tool finish, ending a turn a test has been holding
    /// open (`notify_one`, so releasing before the tool gets there is not a
    /// race). Ignored by every agent that does not call it.
    pub release: Arc<tokio::sync::Notify>,
    _fixture: RuntimeFixture,
    _tmp: TempDir,
}

/// An agent whose one scripted turn just talks.
pub fn talking_agent() -> Fixture {
    build(
        vec![vec![StreamedMessagePart::from(ContentPart::Text(
            TextPart::new("hello back"),
        ))]],
        true,
    )
}

/// An agent whose first turn calls a tool that needs approval, and whose
/// second turn (after the answer) just talks.
pub fn asking_agent() -> Fixture {
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

/// An agent whose first turn calls a tool that does not return until the
/// test says so, and whose second turn just talks. For anything that has to
/// observe an agent while it is genuinely working.
pub fn working_agent() -> Fixture {
    let mut call = ToolCall::new("call-1", "waits");
    call.function.arguments = Some("{}".to_string());
    build(
        vec![
            vec![StreamedMessagePart::from(call)],
            vec![StreamedMessagePart::from(ContentPart::Text(TextPart::new(
                "done",
            )))],
        ],
        true,
    )
}

pub fn build(turns: Vec<Vec<StreamedMessagePart>>, yolo: bool) -> Fixture {
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

    let release = Arc::new(tokio::sync::Notify::new());
    let mut toolset = KimiToolset::new();
    toolset.add(Arc::new(AsksFirst {
        approval: runtime.approval.clone(),
    }));
    toolset.add(Arc::new(WaitsToBeReleased {
        release: Arc::clone(&release),
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
        release,
        _fixture: fixture,
        _tmp: tmp,
    }
}

// --------------------------------------------------------------- the client

pub struct TestClient<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    pub reader: BufReader<ReadHalf<S>>,
    pub writer: Option<WriteHalf<S>>,
    next_id: u64,
}

impl<S> TestClient<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Speak the wire over any stream. The server's side of it is somebody
    /// else's problem: a duplex partner, or a socket a listener accepted.
    pub fn over(stream: S) -> Self {
        let (read, write) = tokio::io::split(stream);
        Self {
            reader: BufReader::new(read),
            writer: Some(write),
            next_id: 0,
        }
    }

    /// Every client mints its own ids from 1, exactly as `WireClient` does.
    /// Two clients therefore both call their first request "1", which is the
    /// point: nothing may route by a client-minted id.
    pub async fn call(&mut self, method: &str, params: Value) -> String {
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

    pub async fn send(&mut self, frame: Value) {
        let mut line = serde_json::to_string(&frame).expect("serialize frame");
        line.push('\n');
        let writer = self.writer.as_mut().expect("still attached");
        writer.write_all(line.as_bytes()).await.expect("write");
        writer.flush().await.expect("flush");
    }

    /// Send a raw line, for the handshakes that are not wire messages.
    pub async fn send_raw(&mut self, line: &str) {
        let writer = self.writer.as_mut().expect("still attached");
        writer.write_all(line.as_bytes()).await.expect("write");
        writer.write_all(b"\n").await.expect("write");
        writer.flush().await.expect("flush");
    }

    pub async fn next_frame(&mut self, patience: Duration) -> Option<Value> {
        let mut line = String::new();
        match tokio::time::timeout(patience, self.reader.read_line(&mut line)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => None,
            Ok(Ok(_)) => Some(serde_json::from_str(&line).expect("valid JSON frame")),
        }
    }

    pub async fn wait_for(&mut self, what: &str, mut pred: impl FnMut(&Value) -> bool) -> Value {
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
    pub async fn drain(&mut self) -> Vec<Value> {
        let mut frames = Vec::new();
        while let Some(frame) = self.next_frame(SILENCE).await {
            frames.push(frame);
        }
        frames
    }

    pub async fn initialize(&mut self) -> Value {
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
    pub async fn detach(mut self) {
        drop(self.writer.take());
        // Give the server's read loop its EOF before the test looks again.
        while self.next_frame(SILENCE).await.is_some() {}
    }
}

impl TestClient<DuplexStream> {
    /// Attach in-process, with no transport in between.
    pub fn attach(server: &Arc<WireServer>) -> Self {
        let (theirs, mine) = tokio::io::duplex(4 << 20);
        let (server_read, server_write) = tokio::io::split(theirs);
        let server = Arc::clone(server);
        tokio::spawn(async move {
            let _ = server.serve_connection(server_read, server_write).await;
        });
        Self::over(mine)
    }
}

pub fn is_answer_to(frame: &Value, id: &str) -> bool {
    frame.get("id").and_then(Value::as_str) == Some(id) && frame.get("method").is_none()
}

pub fn is_event(frame: &Value) -> bool {
    frame.get("method").and_then(Value::as_str) == Some("event")
}

pub fn event_type(frame: &Value) -> Option<&str> {
    frame.pointer("/params/type").and_then(Value::as_str)
}

pub fn request_type(frame: &Value) -> Option<&str> {
    if frame.get("method").and_then(Value::as_str) != Some("request") {
        return None;
    }
    frame.pointer("/params/type").and_then(Value::as_str)
}
