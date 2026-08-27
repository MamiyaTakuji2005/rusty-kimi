//! The agent-side state machine: one dvadva-agent conversation over a
//! [`WireClient`], mirroring inkvizitor's `Session` minus every UI concern.
//!
//! Lifecycle: `initialize` → `replay` (history events fold into the
//! transcript) → ready. A user line sends `prompt` (or `steer` mid-turn);
//! approval reverse-requests are collected for the UI to answer; `Esc`
//! cancels.

use std::sync::Arc;
use std::sync::mpsc::Receiver;

use serde_json::{Value, json};

use dvadva_agent::wire::protocol::WIRE_PROTOCOL_VERSION;
use dvadva_agent::wire::{ApprovalResponse, ApprovalResponseKind, WireMessage};

use wire_client::launch::{AgentLaunch, session_arg};
use wire_client::remotes::Remote;
use wire_client::transcript::{ApprovalInfo, Block, Transcript};
use wire_client::{Inbound, WireClient};

/// Where the conversation stands; drives the status bar and what `Enter` does.
#[derive(Clone, PartialEq)]
pub enum Phase {
    Initializing,
    Replaying,
    Ready,
    Running,
    /// The connection ended and there is a way back to the agent. Not an
    /// ending: the agent keeps its turn, its context and its pid, and `Ctrl+R`
    /// rejoins it.
    Detached(String),
    Failed(String),
}

/// How a detached session gets back to its agent: an `attach` through the
/// bridge daemon it came in by, which finds the live agent or starts one on
/// the same session files.
#[derive(Clone)]
struct WayBack {
    endpoint: String,
    args: Vec<String>,
}

/// A live approval waiting for an answer: `(rpc_id, request_id)`.
pub type PendingApproval = (String, String);

/// One agent session: subprocess, inbound queue, transcript, phase.
pub struct AgentSession {
    client: WireClient,
    inbound: Receiver<Inbound>,
    pub transcript: Transcript,
    pub phase: Phase,
    pub server_name: String,
    init_id: Option<String>,
    replay_id: Option<String>,
    prompt_id: Option<String>,
    /// Approvals awaiting a decision, oldest first.
    pub approvals: Vec<PendingApproval>,
    /// Which session on the agent's machine this is, once the agent or the
    /// daemon has said. Without it a reconnect has nothing to ask for.
    session_id: Option<String>,
    /// How to get back after a detach, or `None` for a locally spawned agent
    /// whose life is this process's pipe.
    way_back: Option<WayBack>,
    /// The repaint hook, kept because a reconnect builds a whole new client
    /// and every client needs one.
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl AgentSession {
    /// Start a session per the launch configuration: spawn a local agent,
    /// or attach through a bridge daemon when `--remote` named one.
    ///
    /// `remote` is what `launch.remote` resolved to — a configured entry in
    /// `~/.kimi/bridge.toml` or a bare `host:port` — so the endpoint dialled
    /// here is already the real address.
    ///
    /// The remote path *attaches*: quitting leaves the agent running, and the
    /// same command run again rejoins it. A daemon too old for the op falls
    /// back to a spawn, and then quitting means what it used to.
    pub fn launch(
        launch: &AgentLaunch,
        remote: Option<&Remote>,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self, String> {
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(wake);
        let session = session_arg(&launch.agent_args);
        let (client, inbound, session_id, way_back) = match remote {
            Some(remote) => {
                let nudge = Arc::clone(&wake);
                let attached = WireClient::attach_tcp(
                    &remote.endpoint,
                    session.as_deref(),
                    &launch.agent_args,
                    move || nudge(),
                )
                .map_err(|err| {
                    let name = &remote.name;
                    format!("remote bridge `{name}`: {err}")
                })?;
                let way_back = attached.supervised.then(|| WayBack {
                    endpoint: remote.endpoint.clone(),
                    args: launch.agent_args.clone(),
                });
                (
                    attached.client,
                    attached.inbound,
                    attached.session,
                    way_back,
                )
            }
            None => {
                // Plain `spawn`: a terminal frontend shares its console with
                // the agent, and there is no window to suppress.
                let nudge = Arc::clone(&wake);
                let (client, inbound) =
                    WireClient::spawn(&launch.agent_bin, &launch.agent_args, move || nudge())
                        .map_err(|err| {
                            format!("failed to spawn agent `{}`: {err}", launch.agent_bin)
                        })?;
                (client, inbound, session, None)
            }
        };
        let init_id = client.send_request(
            "initialize",
            json!({
                "protocol_version": WIRE_PROTOCOL_VERSION,
                "client": {
                    "name": "dvadva-tui",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            }),
        );
        Ok(Self {
            client,
            inbound,
            transcript: Transcript::default(),
            phase: Phase::Initializing,
            server_name: String::new(),
            init_id: Some(init_id),
            replay_id: None,
            prompt_id: None,
            approvals: Vec::new(),
            session_id,
            way_back,
            wake,
        })
    }

    /// Whether quitting leaves the agent running.
    pub fn outlives_this_process(&self) -> bool {
        self.way_back.is_some()
    }

    /// Which session this is, once the agent or the daemon has said.
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub fn can_reconnect(&self) -> bool {
        self.way_back.is_some() && matches!(self.phase, Phase::Detached(_))
    }

    /// Rejoin the agent after a detach (`Ctrl+R`).
    ///
    /// A fresh connection replays the whole session, so the transcript is
    /// dropped first — otherwise every block would appear twice.
    pub fn reconnect(&mut self) {
        let Some(way_back) = self.way_back.clone() else {
            return;
        };
        let nudge = Arc::clone(&self.wake);
        let attached = match WireClient::attach_tcp(
            &way_back.endpoint,
            self.session_id.as_deref(),
            &way_back.args,
            move || nudge(),
        ) {
            Ok(attached) => attached,
            Err(err) => {
                self.phase = Phase::Detached(format!("{err}"));
                return;
            }
        };
        if let Some(session) = attached.session {
            self.session_id = Some(session);
        }
        self.client = attached.client;
        self.inbound = attached.inbound;
        self.transcript = Transcript::default();
        self.init_id = Some(self.client.send_request(
            "initialize",
            json!({
                "protocol_version": WIRE_PROTOCOL_VERSION,
                "client": {
                    "name": "dvadva-tui",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            }),
        ));
        self.phase = Phase::Initializing;
    }

    /// Ask the agent itself to stop, rather than only leaving it. The only
    /// way to end an agent on another machine, which cannot be signalled
    /// from here.
    pub fn stop_agent(&mut self) {
        if self.way_back.is_some() {
            self.client.send_request("shutdown", json!({}));
        }
        self.way_back = None;
        self.client.shutdown();
    }

    pub fn has_pending_approvals(&self) -> bool {
        !self.approvals.is_empty()
    }

    /// Info block of the oldest pending approval, for the overlay.
    pub fn first_approval_info(&self) -> Option<&ApprovalInfo> {
        let request_id = &self.approvals.first()?.1;
        self.transcript.blocks.iter().find_map(|block| match block {
            Block::Approval { info, response } if info.request_id == *request_id => Some(info),
            _ => None,
        })
    }

    pub fn shutdown(&mut self) {
        self.client.shutdown();
    }

    /// Consume everything that arrived since the last call.
    pub fn drain_inbound(&mut self) {
        while let Ok(msg) = self.inbound.try_recv() {
            match msg {
                Inbound::Event(event) => {
                    // Any client attached to this session can answer an
                    // approval, and the agent broadcasts the resolution to
                    // all of them. Take our own prompt down for a request
                    // somebody else just answered.
                    if let WireMessage::ApprovalResponse(resp) = &event {
                        self.approvals
                            .retain(|(_, request_id)| *request_id != resp.request_id);
                    }
                    // A turn this client did not start still ends for it: a
                    // reattached client watching a turn from before it
                    // arrived has no `prompt` answer coming, so the event is
                    // the only thing that can bring it back to ready.
                    if matches!(event, WireMessage::TurnEnd(_))
                        && self.phase == Phase::Running
                        && self.prompt_id.is_none()
                    {
                        self.phase = Phase::Ready;
                    }
                    self.transcript.apply_event(event);
                }
                Inbound::Request { id, message } => self.handle_request(id, message),
                Inbound::Response { id, result, error } => {
                    self.handle_response(id, result, error);
                }
                Inbound::AgentExited(reason) => {
                    if matches!(self.phase, Phase::Failed(_) | Phase::Detached(_)) {
                        continue;
                    }
                    // A modal whose answer cannot reach anybody is worse than
                    // no modal.
                    self.approvals.clear();
                    self.init_id = None;
                    self.replay_id = None;
                    self.prompt_id = None;
                    self.phase = match self.way_back {
                        Some(_) => Phase::Detached(reason),
                        None => Phase::Failed(format!("agent exited: {reason}")),
                    };
                }
                Inbound::ProtocolError(err) => {
                    self.transcript
                        .blocks
                        .push(Block::Info(format!("wire error: {err}")));
                }
            }
        }
    }

    fn handle_request(&mut self, rpc_id: String, message: WireMessage) {
        match message {
            WireMessage::ApprovalRequest(req) => {
                self.transcript.push_approval(ApprovalInfo {
                    request_id: req.id.clone(),
                    sender: req.sender.clone(),
                    action: req.action.clone(),
                    description: req.description.clone(),
                    display: req.display.clone(),
                });
                if self.phase == Phase::Replaying {
                    // Historical: already answered in a previous run; render only.
                    return;
                }
                self.approvals.push((rpc_id, req.id));
            }
            WireMessage::ToolCallRequest(req) => {
                if self.phase == Phase::Replaying {
                    self.transcript.blocks.push(Block::Info(format!(
                        "external tool call (replayed): {}",
                        req.name
                    )));
                    return;
                }
                self.client.respond_error(
                    &rpc_id,
                    -32000,
                    "External tools are not supported by this client",
                );
            }
            other => {
                self.transcript.blocks.push(Block::Info(format!(
                    "unexpected request: {}",
                    other.type_name()
                )));
            }
        }
    }

    fn handle_response(&mut self, id: String, result: Option<Value>, error: Option<Value>) {
        if Some(&id) == self.init_id.as_ref() {
            self.init_id = None;
            match (result, error) {
                (Some(result), None) => {
                    // Refuse a protocol we cannot speak before folding any of
                    // the result in: everything below assumes the shapes this
                    // build knows.
                    if let Err(err) = wire_client::check_server_protocol(&result) {
                        self.phase = Phase::Failed(err);
                        return;
                    }
                    // The agent's own answer to "which session is this" — the
                    // one thing a reconnect cannot do without, and the only
                    // way a new session's id is ever learned.
                    if let Some(session) = result.get("session").and_then(|v| v.as_str()) {
                        self.session_id = Some(session.to_string());
                    }
                    self.server_name = result
                        .pointer("/server/name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Kimi")
                        .to_string();
                    if let Some(model) = result.pointer("/server/model").and_then(|v| v.as_str()) {
                        self.transcript
                            .status
                            .model
                            .get_or_insert(model.to_string());
                    }
                    self.replay_id = Some(self.client.send_request("replay", json!({})));
                    self.phase = Phase::Replaying;
                }
                (_, error) => {
                    self.phase = Phase::Failed(format!(
                        "initialize failed: {}",
                        error.map(|e| e.to_string()).unwrap_or_default()
                    ));
                }
            }
        } else if Some(&id) == self.replay_id.as_ref() {
            self.replay_id = None;
            // Nothing in the replayed history says whether a turn is running
            // *now* — a `TurnBegin` with no end reads the same whether the
            // turn is live or was interrupted — so the agent says it
            // outright. An older agent omits the field, and its silence
            // reads as ready, exactly as before.
            let running = result
                .as_ref()
                .and_then(|result| result.get("turn_running"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            self.phase = if running {
                Phase::Running
            } else {
                Phase::Ready
            };
        } else if Some(&id) == self.prompt_id.as_ref() {
            self.prompt_id = None;
            self.phase = Phase::Ready;
            match (result, error) {
                (Some(result), None) => {
                    let status = result.get("status").and_then(|s| s.as_str()).unwrap_or("");
                    if status == "max_steps_reached" {
                        self.transcript
                            .blocks
                            .push(Block::Info("max steps reached".into()));
                    }
                }
                (_, Some(error)) => {
                    let message = error
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("unknown error");
                    self.transcript
                        .blocks
                        .push(Block::Info(format!("turn failed: {message}")));
                }
                _ => {}
            }
        }
        // steer/cancel responses need no handling.
    }

    /// Submit one user line: prompt when ready, steer mid-turn, drop otherwise.
    pub fn submit(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        match self.phase {
            Phase::Ready => {
                self.prompt_id = Some(
                    self.client
                        .send_request("prompt", json!({ "user_input": text })),
                );
                self.phase = Phase::Running;
            }
            Phase::Running => {
                // TurnBegin/SteerInput events echo the input back for display.
                self.client
                    .send_request("steer", json!({ "user_input": text }));
            }
            _ => {}
        }
    }

    /// Cancel the running turn (`Esc`).
    pub fn cancel(&mut self) {
        if self.phase == Phase::Running {
            self.client.send_request("cancel", json!({}));
        }
    }

    /// Answer the oldest pending approval.
    pub fn resolve_approval(&mut self, kind: ApprovalResponseKind) {
        let Some((rpc_id, request_id)) = self.approvals.first().cloned() else {
            return;
        };
        self.approvals.remove(0);
        let response = ApprovalResponse {
            request_id: request_id.clone(),
            response: kind.clone(),
        };
        self.client.respond_result(
            &rpc_id,
            serde_json::to_value(&response).unwrap_or(Value::Null),
        );
        // Mirror the decision onto its transcript block, as inkvizitor does.
        if let Some(block) = self
            .transcript
            .blocks
            .iter_mut()
            .find_map(|block| match block {
                Block::Approval { info, response } if info.request_id == request_id => {
                    Some(response)
                }
                _ => None,
            })
        {
            *block = Some(kind);
        }
    }
}
