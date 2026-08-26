//! The agent-side state machine: one kimi-agent conversation over a
//! [`WireClient`], mirroring kimi-gui's `Session` minus every UI concern.
//!
//! Lifecycle: `initialize` → `replay` (history events fold into the
//! transcript) → ready. A user line sends `prompt` (or `steer` mid-turn);
//! approval reverse-requests are collected for the UI to answer; `Esc`
//! cancels.

use std::sync::mpsc::Receiver;

use serde_json::{Value, json};

use kimi_agent::wire::protocol::WIRE_PROTOCOL_VERSION;
use kimi_agent::wire::{ApprovalResponse, ApprovalResponseKind, WireMessage};

use wire_client::launch::AgentLaunch;
use wire_client::transcript::{ApprovalInfo, Block, Transcript};
use wire_client::{Inbound, WireClient};

/// Where the conversation stands; drives the status bar and what `Enter` does.
#[derive(Clone, PartialEq)]
pub enum Phase {
    Initializing,
    Replaying,
    Ready,
    Running,
    Failed(String),
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
}

impl AgentSession {
    /// Start a session per the launch configuration: spawn a local agent,
    /// or connect through a remote bridge daemon when `--remote` is set.
    pub fn launch(
        launch: &AgentLaunch,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self, String> {
        let (client, inbound) = match &launch.remote {
            Some(endpoint) => WireClient::connect_tcp(endpoint, &launch.agent_args, wake)
                .map_err(|err| format!("remote bridge `{endpoint}`: {err}"))?,
            None => {
                // Plain `spawn`: a terminal frontend shares its console with
                // the agent, and there is no window to suppress.
                WireClient::spawn(&launch.agent_bin, &launch.agent_args, wake)
                    .map_err(|err| format!("failed to spawn agent `{}`: {err}", launch.agent_bin))?
            }
        };
        let init_id = client.send_request(
            "initialize",
            json!({
                "protocol_version": WIRE_PROTOCOL_VERSION,
                "client": {
                    "name": "kimi-tui",
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
        })
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
                    self.transcript.apply_event(event);
                }
                Inbound::Request { id, message } => self.handle_request(id, message),
                Inbound::Response { id, result, error } => {
                    self.handle_response(id, result, error);
                }
                Inbound::AgentExited(reason) => {
                    if !matches!(self.phase, Phase::Failed(_)) {
                        self.phase = Phase::Failed(format!("agent exited: {reason}"));
                    }
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
            self.phase = Phase::Ready;
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
        // Mirror the decision onto its transcript block, as kimi-gui does.
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
