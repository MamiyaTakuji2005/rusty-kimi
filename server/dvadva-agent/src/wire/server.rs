use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use kosong::chat_provider::ChatProviderError;
use kosong::tooling::tool_error;

use crate::constant::{NAME, VERSION};
use crate::soul::kimisoul::KimiSoul;
use crate::soul::{LLMNotSet, LLMNotSupported, MaxStepsReached, RunCancelled, Soul, run_soul};
use crate::utils::{Queue, QueueShutDown};
use crate::wire::fanout::{ConnId, Fanout};
use crate::wire::{
    ApprovalRequest, ApprovalResponse, ToolCallRequest, ToolResult, Wire, WireMessage,
    now_timestamp, out_of_turn_events,
};

use crate::wire::jsonrpc::{
    InitializeParams, JsonRpcErrorObject, JsonRpcErrorResponse, JsonRpcErrorResponseNullableId,
    JsonRpcMessage, JsonRpcSuccessResponse, PromptParams, build_event_message,
    build_request_message, error_codes, statuses,
};
use crate::wire::protocol::{WIRE_PROTOCOL_VERSION, check_peer};

/// Staging room for the reader, not a limit on anything.
///
/// `read_until` grows its own output, so a line longer than this still
/// arrives whole — the capacity only decides how many reads that takes. It
/// used to be 100 MB, which was harmless when the one connection was stdio
/// and is not once every attached client allocates one.
const READ_BUFFER_CAPACITY: usize = 64 * 1024;

#[derive(Clone)]
enum PendingRequest {
    Approval(ApprovalRequest),
    ToolCall(ToolCallRequest),
}

impl PendingRequest {
    fn id(&self) -> &str {
        match self {
            PendingRequest::Approval(req) => &req.id,
            PendingRequest::ToolCall(req) => &req.id,
        }
    }

    fn to_wire_message(&self) -> WireMessage {
        match self {
            PendingRequest::Approval(req) => WireMessage::ApprovalRequest(req.clone()),
            PendingRequest::ToolCall(req) => WireMessage::ToolCallRequest(req.clone()),
        }
    }
}

/// Everything one session owns, shared by every client attached to it.
///
/// The split from [`Connection`] is the whole of "many clients, one agent":
/// what is here is a fact about the session (one turn at a time, one set of
/// open approvals, one toolset), and what is on a `Connection` is a fact
/// about one client (has it initialized, is it still catching up).
struct SessionCore {
    soul: Arc<KimiSoul>,
    fanout: Fanout,
    /// Reverse-RPC awaiting a client's answer, keyed by the *agent's* request
    /// id. Those are minted here and globally unique, so several clients can
    /// share one map without collision — unlike the ids clients mint for
    /// their own calls, which are unique only per connection.
    pending: tokio::sync::Mutex<HashMap<String, PendingRequest>>,
    /// The turn in flight, if any. Session-wide on purpose: one soul runs one
    /// turn at a time no matter how many clients are watching.
    cancel_token: tokio::sync::Mutex<Option<CancellationToken>>,
    /// Which client registered each external tool. Only that client can
    /// service a call to it, so the registration leaves with it.
    tool_owner: tokio::sync::Mutex<HashMap<String, ConnId>>,
}

pub struct WireServer {
    core: Arc<SessionCore>,
}

pub type WireOverStdio = WireServer;

impl WireServer {
    pub fn new(soul: Arc<KimiSoul>) -> Self {
        Self {
            core: Arc::new(SessionCore {
                soul,
                fanout: Fanout::new(),
                pending: tokio::sync::Mutex::new(HashMap::new()),
                cancel_token: tokio::sync::Mutex::new(None),
                tool_owner: tokio::sync::Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Serve one client over stdio, and end the session when it goes.
    ///
    /// The pipe *is* the lifetime here, deliberately: this is the one-shot
    /// path, where the frontend spawned the process and closing stdin is how
    /// it says it is done. [`crate::wire::listener::serve_detachable`] is the
    /// other binding, where a client leaving is a detach instead.
    pub async fn serve(&mut self) -> anyhow::Result<()> {
        info!("Starting Wire server on stdio");
        let out_of_turn_task = self.spawn_background();

        let stdin = tokio::io::stdin();
        let stdout = tokio::io::stdout();
        let result = self.serve_connection(stdin, stdout).await;

        info!("stdin closed, Wire server exiting");
        self.shutdown().await;
        out_of_turn_task.abort();
        result
    }

    /// Start the work the session does with no client asking: draining
    /// subagents that outlive the turn that spawned them. Once per process,
    /// by whichever transport is running.
    pub fn spawn_background(&self) -> tokio::task::JoinHandle<()> {
        Arc::clone(&self.core).spawn_out_of_turn_drain()
    }

    /// End the session: every open request loses, the turn stops, every
    /// client is cut loose. Called when the *process* is done, which on the
    /// listening transport is no longer the same event as a client leaving.
    pub async fn shutdown(&self) {
        self.core.shutdown().await;
    }

    /// Which session this agent is, for a banner or a handshake reply. A
    /// process hosts exactly one, and that is a constraint rather than an
    /// accident: `app.rs` chdirs the whole process into the session's work
    /// directory.
    pub fn session_id(&self) -> String {
        self.core.soul.runtime().session.id.clone()
    }

    /// Which directory this session works in, for a listing that wants to
    /// say *which project* is running without opening the session.
    pub fn session_work_dir(&self) -> String {
        self.core
            .soul
            .runtime()
            .session
            .work_dir
            .as_path()
            .to_string_lossy()
            .into_owned()
    }

    /// Serve one attached client until its stream ends.
    ///
    /// Stdio is one caller; a listener would be another, and the tests are a
    /// third (a `tokio::io::duplex` pair per client). Nothing below this line
    /// knows which it is, which is the point: attaching a second client is
    /// calling this a second time.
    pub async fn serve_connection<R, W>(&self, reader: R, writer: W) -> anyhow::Result<()>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (id, out) = self.core.fanout.attach();
        info!(
            "{id} attached ({} client(s) on this session)",
            self.core.fanout.len()
        );
        let write_task = tokio::spawn(write_loop(id, out, writer));

        let mut conn = Connection::new(id, Arc::clone(&self.core));
        let result = conn.read_loop(reader).await;

        conn.close().await;
        let _ = write_task.await;
        info!("{id} detached ({} client(s) left)", self.core.fanout.len());
        result
    }
}

impl SessionCore {
    /// Background subagents keep streaming after the turn that spawned them
    /// has ended, and their `Wire` dies with that turn. Drain what they hand
    /// off here for as long as the process lives, recording it in the
    /// session's wire file too so a later replay still shows the subagent.
    fn spawn_out_of_turn_drain(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        let out_of_turn = out_of_turn_events();
        let wire_file = self.soul.runtime().session.wire_file();
        tokio::spawn(async move {
            while let Ok(msg) = out_of_turn.get().await {
                if let Err(err) = wire_file.append_message(&msg, Some(now_timestamp())).await {
                    error!("Failed to record out-of-turn wire message: {}", err);
                }
                let out = build_event_message(msg);
                self.fanout
                    .broadcast(serde_json::to_value(&out).unwrap_or(Value::Null));
            }
        })
    }

    /// Snapshot the requests already awaiting an answer *and* start staging
    /// this connection's live traffic, as one step.
    ///
    /// The two have to be taken together. A request raised between them would
    /// be both in the snapshot and in the staged buffer, and the attaching
    /// client would be shown the same approval dialog twice — which is why
    /// `request_approval` publishes while still holding this lock.
    async fn begin_catch_up(&self, id: ConnId) -> Vec<PendingRequest> {
        let pending = self.pending.lock().await;
        self.fanout.begin_catch_up(id);
        pending.values().cloned().collect()
    }

    async fn broadcast_event(&self, msg: WireMessage) {
        let out = build_event_message(msg);
        self.fanout
            .broadcast(serde_json::to_value(&out).unwrap_or(Value::Null));
    }

    fn send_error(&self, id: ConnId, rpc_id: String, code: i64, message: impl Into<String>) {
        let response = JsonRpcErrorResponse {
            jsonrpc: "2.0",
            id: rpc_id,
            error: JsonRpcErrorObject {
                code,
                message: message.into(),
                data: None,
            },
        };
        self.fanout
            .send_to(id, serde_json::to_value(&response).unwrap_or(Value::Null));
    }

    fn send_error_nullable(
        &self,
        id: ConnId,
        code: i64,
        message: impl Into<String>,
        rpc_id: Option<String>,
    ) {
        let response = JsonRpcErrorResponseNullableId {
            jsonrpc: "2.0",
            id: rpc_id,
            error: JsonRpcErrorObject {
                code,
                message: message.into(),
                data: None,
            },
        };
        self.fanout
            .send_to(id, serde_json::to_value(&response).unwrap_or(Value::Null));
    }

    /// Tear the session down: every open request loses, the turn stops, every
    /// client is cut loose. Tied to the end of the *process*, not to the end
    /// of a connection — over stdio those coincide, and on the listening
    /// transport they deliberately do not.
    async fn shutdown(&self) {
        let pending = {
            let mut pending = self.pending.lock().await;
            std::mem::take(&mut *pending)
        };
        for (_, request) in pending {
            match request {
                PendingRequest::Approval(req) => {
                    req.resolve(crate::wire::ApprovalResponseKind::Reject);
                }
                PendingRequest::ToolCall(req) => {
                    let return_value = tool_error(
                        "",
                        "Wire connection closed before tool result was received.",
                        "Wire closed",
                    );
                    req.resolve(return_value);
                }
            }
        }

        if let Some(token) = self.cancel_token.lock().await.take() {
            token.cancel();
        }

        self.fanout.shutdown();
    }
}

/// One attached client, for as long as it is attached.
struct Connection {
    id: ConnId,
    core: Arc<SessionCore>,
    /// Per connection, not per session: a second client attaching mid-turn is
    /// the entire point, so `initialize` is only refused to a client that has
    /// already done it.
    initialized: bool,
    /// The catch-up this client has in flight, so its own `cancel` can stop
    /// it. Replay runs as its own task precisely so that `cancel` can still
    /// be read while a long history streams out.
    replay_cancel: Arc<tokio::sync::Mutex<Option<CancellationToken>>>,
    /// That task, so the connection can wait for it. A client that has
    /// finished *sending* has not necessarily finished *reading*.
    replay_task: Option<tokio::task::JoinHandle<()>>,
}

impl Connection {
    fn new(id: ConnId, core: Arc<SessionCore>) -> Self {
        Self {
            id,
            core,
            initialized: false,
            replay_cancel: Arc::new(tokio::sync::Mutex::new(None)),
            replay_task: None,
        }
    }

    async fn read_loop<R>(&mut self, reader: R) -> anyhow::Result<()>
    where
        R: AsyncRead + Unpin,
    {
        let mut reader = BufReader::with_capacity(READ_BUFFER_CAPACITY, reader);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            let n = reader.read_until(b'\n', &mut buf).await?;
            if n == 0 {
                break;
            }
            let line = String::from_utf8_lossy(&buf);
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let msg_json: Value = match serde_json::from_str(line) {
                Ok(value) => value,
                Err(_) => {
                    error!("Invalid JSON line: {}", line);
                    self.core.send_error_nullable(
                        self.id,
                        error_codes::PARSE_ERROR,
                        "Invalid JSON format",
                        None,
                    );
                    continue;
                }
            };
            let response_hint = msg_json.get("method").is_none() && msg_json.get("id").is_some();
            let msg: JsonRpcMessage = match serde_json::from_value(msg_json.clone()) {
                Ok(msg) => msg,
                Err(err) => {
                    if response_hint {
                        error!("Invalid JSON-RPC response: {:?}", err);
                    } else {
                        error!("Invalid JSON-RPC message: {:?}", err);
                    }
                    let (code, message) = if response_hint {
                        (error_codes::INVALID_REQUEST, "Invalid response")
                    } else {
                        (error_codes::INVALID_REQUEST, "Invalid request")
                    };
                    self.core.send_error_nullable(self.id, code, message, None);
                    continue;
                }
            };

            if let Some(version) = &msg.jsonrpc {
                if version != "2.0" {
                    self.core.send_error_nullable(
                        self.id,
                        error_codes::INVALID_REQUEST,
                        "Invalid request",
                        None,
                    );
                    continue;
                }
            }

            if msg.is_response() {
                if msg.result.is_none() && msg.error.is_none() {
                    self.core.send_error_nullable(
                        self.id,
                        error_codes::INVALID_REQUEST,
                        "Invalid response",
                        None,
                    );
                    continue;
                }
                self.handle_response(&msg).await;
                continue;
            }

            let method = match msg.method.as_deref() {
                Some(method) => method.to_string(),
                None => {
                    error!("Invalid JSON-RPC inbound message: {:?}", msg);
                    if let Some(id) = msg.id.clone() {
                        self.core.send_error(
                            self.id,
                            id,
                            error_codes::METHOD_NOT_FOUND,
                            "Unexpected method received: None",
                        );
                    }
                    continue;
                }
            };

            match method.as_str() {
                "initialize" => self.handle_initialize(msg).await,
                "prompt" => self.handle_prompt(msg).await,
                "cancel" => self.handle_cancel(msg).await,
                "replay" => self.handle_replay(msg).await,
                "steer" => self.handle_steer(msg).await,
                _ => {
                    if let Some(id) = msg.id.clone() {
                        self.core.send_error(
                            self.id,
                            id,
                            error_codes::METHOD_NOT_FOUND,
                            format!("Unexpected method received: {method}"),
                        );
                    }
                }
            }
        }
        Ok(())
    }

    /// This client is leaving. The session is not: its turn keeps running,
    /// its open approvals stay open for whoever else is attached, and only
    /// what belongs to this client goes away with it.
    async fn close(&mut self) {
        // Let a catch-up finish rather than cancelling it. The read half
        // closing means the client stopped talking, not that it stopped
        // listening — a script that pipes its requests in and reads the
        // answers out is half-closed by design, and cancelling here would
        // swallow the replay result it is waiting for. Detaching below shuts
        // the outbound queue, and the writer drains what is already in it.
        if let Some(task) = self.replay_task.take() {
            let _ = task.await;
        }
        self.release_external_tools().await;
        self.core.fanout.detach(self.id);
    }

    async fn handle_initialize(&mut self, msg: JsonRpcMessage) {
        let Some(id) = msg.id.clone() else {
            return;
        };
        // Deliberately *not* gated on a turn being in progress: attaching to a
        // working agent is the feature. What is gated is doing it twice on one
        // connection, which would re-register the client's external tools.
        if self.initialized {
            self.core.send_error(
                self.id,
                id,
                error_codes::INVALID_STATE,
                "This connection is already initialized",
            );
            return;
        }
        let params: InitializeParams = match msg
            .params
            .clone()
            .and_then(|params| serde_json::from_value(params).ok())
        {
            Some(params) => params,
            None => {
                self.core.send_error(
                    self.id,
                    id,
                    error_codes::INVALID_PARAMS,
                    "Invalid parameters for method `initialize`",
                );
                return;
            }
        };

        // The version gate, before anything is mutated: a client we cannot
        // talk to must not get its external tools registered on the way to
        // being refused, and this error is the only part of the exchange it
        // is guaranteed to understand.
        let peer = match check_peer(&params.protocol_version) {
            Ok(peer) => peer,
            Err(err) => {
                warn!("Refusing client: {err}");
                self.core.send_error(
                    self.id,
                    id,
                    error_codes::PROTOCOL_VERSION_MISMATCH,
                    err.to_string(),
                );
                return;
            }
        };
        info!(
            "{} speaks wire protocol {peer}; this build speaks {}",
            self.id, WIRE_PROTOCOL_VERSION
        );

        let (accepted, rejected) = self.register_external_tools(params.external_tools).await;

        let slash_commands: Vec<Value> = self
            .core
            .soul
            .available_slash_commands()
            .into_iter()
            .map(|cmd| {
                json!({
                    "name": cmd.name,
                    "description": cmd.description,
                    "aliases": cmd.aliases,
                })
            })
            .collect();

        let mut result = json!({
            "protocol_version": WIRE_PROTOCOL_VERSION,
            "server": {
                "name": NAME,
                "version": VERSION,
                "model": self.core.soul.model_name(),
            },
            "slash_commands": slash_commands,
            // What this build can do beyond the shapes a 1.0 client knows.
            // Additive by construction: a client that does not read it sees
            // exactly the session it would have seen before.
            "capabilities": {
                "multi_client": true,
            },
        });
        if !accepted.is_empty() || !rejected.is_empty() {
            result["external_tools"] = json!({
                "accepted": accepted,
                "rejected": rejected,
            });
        }

        self.initialized = true;
        let response = JsonRpcSuccessResponse {
            jsonrpc: "2.0",
            id,
            result,
        };
        self.core.fanout.send_to(
            self.id,
            serde_json::to_value(response).unwrap_or(Value::Null),
        );

        // Hand over the model and context figures immediately, to this client
        // alone — it is catch-up, not news. A fresh session has no wire
        // history to replay, so without this a client's status bar stays
        // blank until the first step completes.
        let event = build_event_message(WireMessage::StatusUpdate(
            self.core.soul.current_status_update(),
        ));
        self.core
            .fanout
            .send_to(self.id, serde_json::to_value(&event).unwrap_or(Value::Null));
    }

    /// Register this client's external tools, claiming each name for it.
    ///
    /// A tool call is answered by reverse-RPC to whoever registered the tool,
    /// so two clients cannot own one name: the second offer is refused rather
    /// than silently shadowing the first. The claim is released in `close`,
    /// because a registration nobody can service would hang the next turn
    /// that calls it.
    async fn register_external_tools(
        &self,
        external_tools: Option<Vec<crate::wire::jsonrpc::ExternalTool>>,
    ) -> (Vec<String>, Vec<Value>) {
        let mut accepted = Vec::new();
        let mut rejected = Vec::new();
        let Some(external_tools) = external_tools else {
            return (accepted, rejected);
        };
        let mut owners = self.core.tool_owner.lock().await;
        let mut toolset = self.core.soul.agent().toolset.lock().await;
        for tool in external_tools {
            if toolset.has_builtin_tool(&tool.name) {
                rejected.push(json!({"name": tool.name, "reason": "conflicts with builtin tool"}));
                continue;
            }
            if let Some(owner) = owners.get(&tool.name) {
                if *owner != self.id {
                    rejected.push(json!({
                        "name": tool.name,
                        "reason": "already registered by another attached client",
                    }));
                    continue;
                }
            }
            match toolset.register_external_tool(&tool.name, &tool.description, tool.parameters) {
                Ok(()) => {
                    owners.insert(tool.name.clone(), self.id);
                    accepted.push(tool.name);
                }
                Err(reason) => rejected.push(json!({"name": tool.name, "reason": reason})),
            }
        }
        (accepted, rejected)
    }

    async fn release_external_tools(&self) {
        let mut owners = self.core.tool_owner.lock().await;
        let mine: Vec<String> = owners
            .iter()
            .filter(|(_, owner)| **owner == self.id)
            .map(|(name, _)| name.clone())
            .collect();
        if mine.is_empty() {
            return;
        }
        let mut toolset = self.core.soul.agent().toolset.lock().await;
        for name in mine {
            toolset.unregister_external_tool(&name);
            owners.remove(&name);
            debug!("{} took its external tool `{name}` with it", self.id);
        }
    }

    async fn handle_prompt(&mut self, msg: JsonRpcMessage) {
        let Some(id) = msg.id.clone() else {
            return;
        };
        if self.core.cancel_token.lock().await.is_some() {
            self.core.send_error(
                self.id,
                id,
                error_codes::INVALID_STATE,
                "An agent turn is already in progress",
            );
            return;
        }
        let params: PromptParams = match msg
            .params
            .clone()
            .and_then(|params| serde_json::from_value(params).ok())
        {
            Some(params) => params,
            None => {
                self.core.send_error(
                    self.id,
                    id,
                    error_codes::INVALID_PARAMS,
                    "Invalid parameters for method `prompt`",
                );
                return;
            }
        };

        let cancel_token = CancellationToken::new();
        let core = Arc::clone(&self.core);
        *core.cancel_token.lock().await = Some(cancel_token.clone());

        let conn = self.id;
        let soul = Arc::clone(&core.soul);
        let wire_file = Some(core.soul.runtime().session.wire_file());

        tokio::spawn(async move {
            let core_for_stream = Arc::clone(&core);
            let run_handle = tokio::task::spawn_blocking(move || {
                let handle = tokio::runtime::Handle::current();
                handle.block_on(run_soul(
                    soul.as_ref(),
                    params.user_input,
                    move |wire| stream_wire_messages(Arc::clone(&core_for_stream), wire),
                    cancel_token,
                    wire_file,
                ))
            });
            let run_result = match run_handle.await {
                Ok(result) => result,
                Err(err) => Err(anyhow::anyhow!("Wire run task failed: {err}")),
            };

            *core.cancel_token.lock().await = None;

            // The turn is a session fact, but its *result* answers one
            // client's `prompt`, whose id means nothing to the others.
            let response = match turn_outcome(id, run_result) {
                Ok(success) => serde_json::to_value(success).unwrap_or(Value::Null),
                Err(failure) => serde_json::to_value(failure).unwrap_or(Value::Null),
            };
            core.fanout.send_to(conn, response);
        });
    }

    async fn handle_replay(&mut self, msg: JsonRpcMessage) {
        let Some(id) = msg.id.clone() else {
            return;
        };
        // Deliberately *not* gated on a turn being in progress: replaying
        // into a working agent is exactly what a client attaching mid-turn
        // does. It is gated on this client not already replaying.
        let cancel_token = CancellationToken::new();
        {
            let mut slot = self.replay_cancel.lock().await;
            if slot.is_some() {
                self.core.send_error(
                    self.id,
                    id,
                    error_codes::INVALID_STATE,
                    "This connection is already replaying",
                );
                return;
            }
            *slot = Some(cancel_token.clone());
        }

        // As its own task, so `cancel` can still be read while a long history
        // streams out. Live traffic is staged meanwhile and released below,
        // so the past and the present cannot interleave.
        let core = Arc::clone(&self.core);
        let conn = self.id;
        let replay_cancel = Arc::clone(&self.replay_cancel);
        self.replay_task = Some(tokio::spawn(async move {
            let caught_up = replay_to(&core, conn, cancel_token).await;
            *replay_cancel.lock().await = None;

            let status = if caught_up.cancelled {
                statuses::CANCELLED
            } else {
                statuses::FINISHED
            };
            let response = JsonRpcSuccessResponse {
                jsonrpc: "2.0",
                id,
                result: json!({
                    "status": status,
                    "events": caught_up.events,
                    "requests": caught_up.requests,
                }),
            };
            core.fanout
                .send_to(conn, serde_json::to_value(response).unwrap_or(Value::Null));

            // Only now, once the client has been told the replay ended, are
            // the still-open requests handed over. Both frontends render a
            // request that arrives while they are replaying and deliberately
            // do not arm it, so sending these any earlier would file a live
            // approval as history. Handing them over last is what lets a
            // client attach to a parked agent and actually answer it.
            if !caught_up.cancelled {
                rearm_open_requests(&core, conn, caught_up.still_open).await;
            }
        }));
    }

    async fn handle_steer(&mut self, msg: JsonRpcMessage) {
        let Some(id) = msg.id.clone() else {
            return;
        };
        // Steering only makes sense during an in-progress turn.
        if self.core.cancel_token.lock().await.is_none() {
            self.core.send_error(
                self.id,
                id,
                error_codes::INVALID_STATE,
                "No agent turn is in progress",
            );
            return;
        }
        let params: PromptParams = match msg
            .params
            .clone()
            .and_then(|params| serde_json::from_value(params).ok())
        {
            Some(params) => params,
            None => {
                self.core.send_error(
                    self.id,
                    id,
                    error_codes::INVALID_PARAMS,
                    "Invalid parameters for method `steer`",
                );
                return;
            }
        };
        self.core.soul.steer(params.user_input);
        let response = JsonRpcSuccessResponse {
            jsonrpc: "2.0",
            id,
            result: json!({"status": statuses::STEERED}),
        };
        self.core.fanout.send_to(
            self.id,
            serde_json::to_value(response).unwrap_or(Value::Null),
        );
    }

    async fn handle_cancel(&mut self, msg: JsonRpcMessage) {
        let Some(id) = msg.id.clone() else {
            return;
        };
        // This client's own catch-up first: a `cancel` sent while a long
        // history is streaming means "stop showing me this", not "stop the
        // agent", and with several clients attached the difference matters.
        if let Some(token) = self.replay_cancel.lock().await.as_ref() {
            token.cancel();
            self.core.fanout.send_to(
                self.id,
                serde_json::to_value(JsonRpcSuccessResponse {
                    jsonrpc: "2.0",
                    id,
                    result: json!({}),
                })
                .unwrap_or(Value::Null),
            );
            return;
        }
        let guard = self.core.cancel_token.lock().await;
        let Some(token) = guard.as_ref() else {
            self.core.send_error(
                self.id,
                id,
                error_codes::INVALID_STATE,
                "No agent turn is in progress",
            );
            return;
        };
        token.cancel();
        let response = JsonRpcSuccessResponse {
            jsonrpc: "2.0",
            id,
            result: json!({}),
        };
        self.core.fanout.send_to(
            self.id,
            serde_json::to_value(response).unwrap_or(Value::Null),
        );
    }

    async fn handle_response(&mut self, msg: &JsonRpcMessage) {
        let Some(id) = msg.id.clone() else {
            return;
        };
        let request = {
            let mut pending = self.core.pending.lock().await;
            pending.remove(&id)
        };
        let Some(request) = request else {
            // With several clients attached, the same approval dialog is on
            // every screen and the first answer wins. The losers arrive to
            // find nothing pending. That is the arbitration working, not a
            // fault, so it is a debug line rather than an error.
            debug!(
                "{} answered request id={id}, which was already resolved; ignoring",
                self.id
            );
            return;
        };

        match request {
            PendingRequest::Approval(req) => {
                if msg.error.is_some() {
                    req.resolve(crate::wire::ApprovalResponseKind::Reject);
                    return;
                }
                let result: ApprovalResponse = match msg
                    .result
                    .clone()
                    .and_then(|value| serde_json::from_value(value).ok())
                {
                    Some(result) => result,
                    None => {
                        error!(
                            "Invalid response result for request id={}: missing result",
                            id
                        );
                        req.resolve(crate::wire::ApprovalResponseKind::Reject);
                        return;
                    }
                };
                if result.request_id != req.id {
                    warn!(
                        "Approval response id mismatch: request={}, response={}",
                        req.id, result.request_id
                    );
                }
                req.resolve(result.response);
            }
            PendingRequest::ToolCall(req) => {
                if let Some(error) = &msg.error {
                    let return_value = tool_error("", error.message.clone(), "External tool error");
                    req.resolve(return_value);
                    return;
                }
                let tool_result: ToolResult = match msg
                    .result
                    .clone()
                    .and_then(|value| serde_json::from_value(value).ok())
                {
                    Some(result) => result,
                    None => {
                        error!("Invalid tool result for request id={}: missing result", id);
                        let return_value = tool_error(
                            "",
                            "Invalid tool result payload from client.",
                            "Invalid tool result",
                        );
                        req.resolve(return_value);
                        return;
                    }
                };
                if tool_result.tool_call_id != req.id {
                    warn!(
                        "Tool result id mismatch: request={}, result={}",
                        req.id, tool_result.tool_call_id
                    );
                }
                req.resolve(tool_result.return_value);
            }
        }
    }
}

/// What a catch-up covered, and what is left for the caller to hand over.
struct CaughtUp {
    cancelled: bool,
    events: u64,
    requests: u64,
    /// The requests that were already awaiting an answer when the catch-up
    /// began. Skipped by the file walk, because they are not history yet.
    still_open: Vec<PendingRequest>,
}

/// Stream one client the session's recorded past, then release whatever
/// happened while that was streaming.
async fn replay_to(
    core: &Arc<SessionCore>,
    conn: ConnId,
    cancel_token: CancellationToken,
) -> CaughtUp {
    use futures::StreamExt;

    // Snapshot the open requests and start staging live traffic together:
    // anything raised from here on is staged, so nothing is both replayed
    // from the file and delivered live.
    let still_open = core.begin_catch_up(conn).await;
    let open_ids: Vec<String> = still_open.iter().map(|req| req.id().to_string()).collect();

    let wire_file = core.soul.runtime().session.wire_file();
    let mut events: u64 = 0;
    let mut requests: u64 = 0;

    // iter_records() no-ops if the file is missing, so no existence check needed.
    let mut records = wire_file.iter_records();
    while let Some(record) = records.next().await {
        if cancel_token.is_cancelled() {
            break;
        }
        let wire_msg = match record.to_wire_message() {
            Ok(wire_msg) => wire_msg,
            Err(err) => {
                error!(
                    error = ?err,
                    "Failed to deserialize wire record for replay: {}",
                    wire_file.path().display()
                );
                continue;
            }
        };
        // Replayed requests are read-only: re-emit for display, but do NOT
        // register them as pending — they were answered in the past. The
        // exception is a request that is still open right now, which is
        // skipped here and handed over live below, armed.
        let out = match wire_msg {
            WireMessage::ApprovalRequest(req) => {
                if open_ids.contains(&req.id) {
                    continue;
                }
                requests += 1;
                build_request_message(req.id.clone(), WireMessage::ApprovalRequest(req))
            }
            WireMessage::ToolCallRequest(req) => {
                if open_ids.contains(&req.id) {
                    continue;
                }
                requests += 1;
                build_request_message(req.id.clone(), WireMessage::ToolCallRequest(req))
            }
            other => {
                events += 1;
                let event = build_event_message(other);
                core.fanout
                    .send_to(conn, serde_json::to_value(&event).unwrap_or(Value::Null));
                continue;
            }
        };
        core.fanout
            .send_to(conn, serde_json::to_value(&out).unwrap_or(Value::Null));
    }

    let cancelled = cancel_token.is_cancelled();

    // Emit a fresh StatusUpdate after replay so the client receives the
    // current model's max_context_tokens (and recomputed ratio), overriding
    // any stale values that came from the old wire.jsonl. This fixes the
    // "wrong percentage on resume" and "stale max context" display bugs.
    if !cancelled {
        let event =
            build_event_message(WireMessage::StatusUpdate(core.soul.current_status_update()));
        core.fanout
            .send_to(conn, serde_json::to_value(&event).unwrap_or(Value::Null));
    }

    // Go live: release what was staged while the file streamed out.
    core.fanout.end_catch_up(conn);

    CaughtUp {
        cancelled,
        events,
        requests,
        still_open,
    }
}

/// Hand a freshly caught-up client the requests that are still waiting, so it
/// can answer them rather than watch a dialog that decides nothing.
async fn rearm_open_requests(core: &Arc<SessionCore>, conn: ConnId, open: Vec<PendingRequest>) {
    for request in open {
        // It may have been answered by another client while we were catching
        // up, in which case the resolution event already went out.
        if !core.pending.lock().await.contains_key(request.id()) {
            continue;
        }
        let out = build_request_message(request.id().to_string(), request.to_wire_message());
        core.fanout
            .send_to(conn, serde_json::to_value(&out).unwrap_or(Value::Null));
    }
}

fn turn_outcome(
    id: String,
    run_result: anyhow::Result<()>,
) -> Result<JsonRpcSuccessResponse, JsonRpcErrorResponse> {
    let err = match run_result {
        Ok(()) => {
            return Ok(JsonRpcSuccessResponse {
                jsonrpc: "2.0",
                id,
                result: json!({"status": statuses::FINISHED}),
            });
        }
        Err(err) => err,
    };
    if err.is::<LLMNotSet>() {
        return Err(JsonRpcErrorResponse {
            jsonrpc: "2.0",
            id,
            error: JsonRpcErrorObject {
                code: error_codes::LLM_NOT_SET,
                message: "LLM is not set".to_string(),
                data: None,
            },
        });
    }
    if err.is::<LLMNotSupported>() {
        return Err(JsonRpcErrorResponse {
            jsonrpc: "2.0",
            id,
            error: JsonRpcErrorObject {
                code: error_codes::LLM_NOT_SUPPORTED,
                message: err.to_string(),
                data: None,
            },
        });
    }
    if err.is::<ChatProviderError>() {
        return Err(JsonRpcErrorResponse {
            jsonrpc: "2.0",
            id,
            error: JsonRpcErrorObject {
                code: error_codes::CHAT_PROVIDER_ERROR,
                message: err.to_string(),
                data: None,
            },
        });
    }
    if let Some(MaxStepsReached { n_steps }) = err.downcast_ref::<MaxStepsReached>() {
        return Ok(JsonRpcSuccessResponse {
            jsonrpc: "2.0",
            id,
            result: json!({
                "status": statuses::MAX_STEPS_REACHED,
                "steps": n_steps,
            }),
        });
    }
    if err.is::<RunCancelled>() {
        return Ok(JsonRpcSuccessResponse {
            jsonrpc: "2.0",
            id,
            result: json!({"status": statuses::CANCELLED}),
        });
    }
    Err(JsonRpcErrorResponse {
        jsonrpc: "2.0",
        id,
        error: JsonRpcErrorObject {
            code: error_codes::INTERNAL_ERROR,
            message: err.to_string(),
            data: None,
        },
    })
}

async fn write_loop<W>(id: ConnId, queue: Queue<Value>, mut writer: W)
where
    W: AsyncWrite + Unpin,
{
    loop {
        let msg = match queue.get().await {
            Ok(msg) => msg,
            Err(_) => {
                debug!("{id} send queue shut down, stopping its write loop");
                break;
            }
        };
        let line = match serde_json::to_string(&msg) {
            Ok(line) => line,
            Err(err) => {
                error!("{id} write loop error: {:?}", err);
                continue;
            }
        };
        if let Err(err) = writer.write_all(line.as_bytes()).await {
            error!("{id} write loop error: {:?}", err);
            break;
        }
        if let Err(err) = writer.write_all(b"\n").await {
            error!("{id} write loop error: {:?}", err);
            break;
        }
        let _ = writer.flush().await;
    }
}

async fn stream_wire_messages(
    core: Arc<SessionCore>,
    wire: Arc<Wire>,
) -> Result<(), QueueShutDown> {
    let ui_side = wire.ui_side(false);
    loop {
        let msg = ui_side.receive().await?;
        match msg {
            WireMessage::ApprovalRequest(request) => {
                request_approval(&core, request).await;
            }
            WireMessage::ToolCallRequest(request) => {
                request_tool_call(&core, request).await;
            }
            other => core.broadcast_event(other).await,
        }
    }
}

/// Ask every attached client, and take the first answer.
///
/// The registration and the broadcast happen under one lock so that a client
/// attaching in between cannot see the request twice — once in the snapshot
/// its catch-up takes, once in the traffic that catch-up stages.
async fn request_approval(core: &Arc<SessionCore>, request: ApprovalRequest) {
    let msg_id = request.id.clone();
    {
        let mut pending = core.pending.lock().await;
        pending.insert(msg_id.clone(), PendingRequest::Approval(request.clone()));
        let out = build_request_message(msg_id, WireMessage::ApprovalRequest(request.clone()));
        core.fanout
            .broadcast(serde_json::to_value(out).unwrap_or(Value::Null));
    }
    let _ = request.wait().await;
}

async fn request_tool_call(core: &Arc<SessionCore>, request: ToolCallRequest) {
    let msg_id = request.id.clone();
    {
        let mut pending = core.pending.lock().await;
        pending.insert(msg_id.clone(), PendingRequest::ToolCall(request.clone()));
        let out = build_request_message(msg_id, WireMessage::ToolCallRequest(request.clone()));
        core.fanout
            .broadcast(serde_json::to_value(out).unwrap_or(Value::Null));
    }
    let _ = request.wait().await;
}
