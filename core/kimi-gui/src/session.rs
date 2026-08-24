//! One agent session: a kimi-agent subprocess, its wire lifecycle
//! (initialize -> replay -> ready -> turn running), its transcript, and the
//! per-session UI (input, status bar, approvals, subagent sub-tabs).

use std::sync::mpsc::Receiver;

use eframe::egui::{self, Align2, Color32, Key, Modifiers, RichText};
use egui_commonmark::CommonMarkCache;
use serde_json::{Value, json};

use kimi_agent::wire::protocol::WIRE_PROTOCOL_VERSION;
use kimi_agent::wire::{ApprovalResponse, ApprovalResponseKind, WireMessage};

use crate::client::{Inbound, WireClient};
use crate::render::{block_ui, display_block_ui};
use crate::transcript::{ApprovalInfo, Transcript};

#[derive(PartialEq)]
enum Phase {
    Initializing,
    Replaying,
    Ready,
    Running,
    Failed(String),
}

struct SlashCommand {
    name: String,
    description: String,
}

/// A live approval waiting for the user; `block` points at its transcript entry.
struct PendingApproval {
    rpc_id: String,
    request_id: String,
    block: usize,
}

pub struct Session {
    /// Stable unique id, used to scope egui widget state per session.
    pub id: usize,
    pub title: String,
    /// Explicitly chosen working directory (`-w`), if any; the `+` folder
    /// picker opens here so a parallel session of the active tab is one
    /// Enter away.
    pub work_dir: Option<std::path::PathBuf>,
    client: WireClient,
    inbound: Receiver<Inbound>,
    transcript: Transcript,
    md_cache: CommonMarkCache,
    phase: Phase,
    server_name: String,
    slash_commands: Vec<SlashCommand>,
    approvals: Vec<PendingApproval>,
    input: String,
    input_had_focus: bool,
    /// Approval request the chat box already surrendered focus for, so the
    /// 1/2/3 shortcuts reach the modal without hijacking deliberate typing.
    focus_released_for: Option<String>,
    init_id: Option<String>,
    replay_id: Option<String>,
    prompt_id: Option<String>,
    /// Visible tab within this session: None = main transcript,
    /// Some(task_tool_call_id) = that subagent's transcript.
    active_subtab: Option<String>,
}

impl Session {
    pub fn spawn(
        id: usize,
        title: String,
        agent_bin: &str,
        agent_args: &[String],
        egui_ctx: egui::Context,
    ) -> Result<Self, String> {
        let (client, inbound) = WireClient::spawn(agent_bin, agent_args, egui_ctx)
            .map_err(|err| format!("failed to spawn agent `{agent_bin}`: {err}"))?;
        let init_id = client.send_request(
            "initialize",
            json!({
                "protocol_version": WIRE_PROTOCOL_VERSION,
                "client": {
                    "name": "kimi-gui",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            }),
        );
        Ok(Self {
            id,
            title,
            work_dir: None,
            client,
            inbound,
            transcript: Transcript::default(),
            md_cache: CommonMarkCache::default(),
            phase: Phase::Initializing,
            server_name: String::new(),
            slash_commands: Vec::new(),
            approvals: Vec::new(),
            input: String::new(),
            input_had_focus: false,
            focus_released_for: None,
            init_id: Some(init_id),
            replay_id: None,
            prompt_id: None,
            active_subtab: None,
        })
    }

    pub fn is_running(&self) -> bool {
        self.phase == Phase::Running
    }

    pub fn is_failed(&self) -> bool {
        matches!(self.phase, Phase::Failed(_))
    }

    pub fn has_pending_approvals(&self) -> bool {
        !self.approvals.is_empty()
    }

    pub fn shutdown(&mut self) {
        self.client.shutdown();
    }

    /// Step through the second-row tabs, wrapping: main → each fork/subagent
    /// in spawn order → back to main. A session with no forks stays on main.
    pub fn cycle_subtab(&mut self, forward: bool) {
        let subagents = &self.transcript.subagents;
        let current = self
            .active_subtab
            .as_deref()
            .and_then(|id| subagents.iter().position(|s| s.task_tool_call_id == id));
        self.active_subtab = step_subtab(current, subagents.len(), forward)
            .map(|index| subagents[index].task_tool_call_id.clone());
    }

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
                        .push(crate::transcript::Block::Info(format!("wire error: {err}")));
                }
            }
        }
    }

    fn handle_request(&mut self, rpc_id: String, message: WireMessage) {
        match message {
            WireMessage::ApprovalRequest(req) => {
                let block = self.transcript.push_approval(ApprovalInfo {
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
                self.approvals.push(PendingApproval {
                    rpc_id,
                    request_id: req.id,
                    block,
                });
            }
            WireMessage::ToolCallRequest(req) => {
                if self.phase == Phase::Replaying {
                    self.transcript
                        .blocks
                        .push(crate::transcript::Block::Info(format!(
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
                self.transcript
                    .blocks
                    .push(crate::transcript::Block::Info(format!(
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
                    if let Some(cmds) = result.get("slash_commands").and_then(|v| v.as_array()) {
                        self.slash_commands = cmds
                            .iter()
                            .filter_map(|cmd| {
                                Some(SlashCommand {
                                    name: cmd.get("name")?.as_str()?.to_string(),
                                    description: cmd
                                        .get("description")
                                        .and_then(|d| d.as_str())
                                        .unwrap_or_default()
                                        .to_string(),
                                })
                            })
                            .collect();
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
                            .push(crate::transcript::Block::Info("max steps reached".into()));
                    }
                }
                (_, Some(error)) => {
                    let message = error
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("unknown error");
                    self.transcript
                        .blocks
                        .push(crate::transcript::Block::Info(format!(
                            "turn failed: {message}"
                        )));
                }
                _ => {}
            }
        }
        // steer/cancel responses need no handling.
    }

    fn submit_input(&mut self) {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.input.clear();
        match self.phase {
            Phase::Ready => {
                self.prompt_id = Some(
                    self.client
                        .send_request("prompt", json!({"user_input": text})),
                );
                self.phase = Phase::Running;
            }
            Phase::Running => {
                // TurnBegin/SteerInput events echo the input back for display.
                self.client
                    .send_request("steer", json!({"user_input": text}));
            }
            _ => {}
        }
    }

    fn resolve_approval(&mut self, index: usize, kind: ApprovalResponseKind) {
        let pending = self.approvals.remove(index);
        let response = ApprovalResponse {
            request_id: pending.request_id.clone(),
            response: kind.clone(),
        };
        self.client.respond_result(
            &pending.rpc_id,
            serde_json::to_value(&response).unwrap_or(Value::Null),
        );
        if let Some(crate::transcript::Block::Approval { response, .. }) =
            self.transcript.blocks.get_mut(pending.block)
        {
            *response = Some(kind);
        }
    }

    /// Draw this session's panels. `suppress_keys` disables the session's
    /// keyboard shortcuts (e.g. while an app-level popup is open).
    pub fn ui(&mut self, ctx: &egui::Context, suppress_keys: bool) {
        if !suppress_keys
            && self.phase == Phase::Running
            && ctx.input(|i| i.key_pressed(Key::Escape))
        {
            self.client.send_request("cancel", json!({}));
        }

        // When an approval arrives, drop the chat box's focus once so the
        // 1/2/3 shortcuts answer the modal. Clicking back into the box
        // restores normal typing (digits included) for this approval.
        if let Some(pending) = self.approvals.first() {
            if self.focus_released_for.as_deref() != Some(pending.request_id.as_str()) {
                self.focus_released_for = Some(pending.request_id.clone());
                ctx.memory_mut(|mem| mem.surrender_focus(self.input_id()));
            }
        } else {
            self.focus_released_for = None;
        }

        egui::TopBottomPanel::bottom("input_panel")
            .resizable(false)
            .show(ctx, |ui| {
                ui.push_id(self.id, |ui| {
                    ui.add_space(4.0);
                    self.input_area(ui, suppress_keys);
                    self.status_bar(ui);
                    ui.add_space(2.0);
                });
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.push_id(self.id, |ui| {
                self.subtab_strip(ui);
                self.transcript_view(ui);
            });
        });

        if !suppress_keys {
            self.approval_modal(ctx);
        }
    }

    /// Second-layer tab row, always visible: the session's main transcript on
    /// the left, then one read-only tab per fork/subagent in spawn order.
    fn subtab_strip(&mut self, ui: &mut egui::Ui) {
        // Drop the selection if it no longer resolves (defensive).
        if let Some(active) = &self.active_subtab
            && !self
                .transcript
                .subagents
                .iter()
                .any(|s| &s.task_tool_call_id == active)
        {
            self.active_subtab = None;
        }
        ui.horizontal_wrapped(|ui| {
            if ui
                .selectable_label(self.active_subtab.is_none(), "main")
                .clicked()
            {
                self.active_subtab = None;
            }
            for sub in &self.transcript.subagents {
                let selected =
                    self.active_subtab.as_deref() == Some(sub.task_tool_call_id.as_str());
                let label = if sub.done {
                    RichText::new(&sub.title)
                } else {
                    RichText::new(format!("▶ {}", sub.title)).color(Color32::from_rgb(220, 160, 60))
                };
                if ui.selectable_label(selected, label).clicked() {
                    self.active_subtab = Some(sub.task_tool_call_id.clone());
                }
            }
        });
        ui.separator();
    }

    fn transcript_view(&mut self, ui: &mut egui::Ui) {
        // A fork/subagent tab runs on its own clock: the child streams while
        // the parent is Ready, so "running" comes from its done flag there.
        let (blocks, running) = match &self.active_subtab {
            None => (&self.transcript.blocks, self.phase == Phase::Running),
            Some(task_id) => {
                match self
                    .transcript
                    .subagents
                    .iter()
                    .find(|s| &s.task_tool_call_id == task_id)
                {
                    Some(sub) => (&sub.transcript.blocks, !sub.done),
                    None => (&self.transcript.blocks, self.phase == Phase::Running),
                }
            }
        };
        let md_cache = &mut self.md_cache;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                let last = blocks.len().saturating_sub(1);
                for (index, block) in blocks.iter().enumerate() {
                    block_ui(ui, index, block, md_cache, index == last, running);
                    ui.add_space(6.0);
                }
            });
    }

    fn approval_modal(&mut self, ctx: &egui::Context) {
        let Some(pending) = self.approvals.first() else {
            return;
        };
        let Some(crate::transcript::Block::Approval { info, .. }) =
            self.transcript.blocks.get(pending.block)
        else {
            return;
        };
        let mut choice: Option<ApprovalResponseKind> = None;
        // Number-key shortcuts; inert while the chat box holds focus so
        // deliberate typing (digits included) is never hijacked.
        if !self.input_had_focus {
            ctx.input_mut(|i| {
                if i.consume_key(Modifiers::NONE, Key::Num1) {
                    choice = Some(ApprovalResponseKind::Approve);
                } else if i.consume_key(Modifiers::NONE, Key::Num2) {
                    choice = Some(ApprovalResponseKind::ApproveForSession);
                } else if i.consume_key(Modifiers::NONE, Key::Num3) {
                    choice = Some(ApprovalResponseKind::Reject);
                }
            });
        }
        egui::Window::new("Approval required")
            .id(egui::Id::new(("approval_modal", self.id)))
            .collapsible(false)
            .resizable(true)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .max_width(640.0)
            .show(ctx, |ui| {
                ui.label(RichText::new(format!("{} · {}", info.sender, info.action)).weak());
                ui.label(RichText::new(&info.description).strong());
                if !info.display.is_empty() {
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .max_height(320.0)
                        .show(ui, |ui| {
                            for block in &info.display {
                                display_block_ui(ui, block);
                            }
                        });
                }
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Approve (1)").clicked() {
                        choice = Some(ApprovalResponseKind::Approve);
                    }
                    if ui.button("Approve for session (2)").clicked() {
                        choice = Some(ApprovalResponseKind::ApproveForSession);
                    }
                    if ui
                        .button(RichText::new("Reject (3)").color(Color32::from_rgb(200, 80, 80)))
                        .clicked()
                    {
                        choice = Some(ApprovalResponseKind::Reject);
                    }
                });
                if self.input_had_focus {
                    ui.label(
                        RichText::new("number keys are off while the message box has focus")
                            .weak()
                            .small(),
                    );
                }
            });
        if let Some(kind) = choice {
            self.resolve_approval(0, kind);
        }
    }

    fn status_bar(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            match &self.phase {
                Phase::Initializing => {
                    ui.spinner();
                    ui.label("initializing...");
                }
                Phase::Replaying => {
                    ui.spinner();
                    ui.label("loading history...");
                }
                Phase::Running => {
                    ui.spinner();
                    ui.label("working... (Esc to cancel, Enter to steer)");
                }
                Phase::Ready => {
                    ui.label(RichText::new("ready").weak());
                }
                Phase::Failed(err) => {
                    ui.label(RichText::new(err).color(Color32::from_rgb(200, 80, 80)));
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let status = &self.transcript.status;
                if let Some(label) = status.context_label() {
                    let text = RichText::new(label).monospace();
                    // Warn before compaction rather than after it surprises you.
                    let text = match status.context_ratio().unwrap_or(0.0) {
                        r if r >= 0.95 => text.color(Color32::from_rgb(200, 80, 80)),
                        r if r >= 0.80 => text.color(Color32::from_rgb(220, 160, 60)),
                        _ => text.weak(),
                    };
                    ui.label(text);
                }
                if status.yolo_enabled == Some(true) {
                    ui.label(RichText::new("yolo").color(Color32::from_rgb(220, 160, 60)));
                }
                if status.thinking == Some(true) {
                    ui.label(RichText::new("thinking").weak());
                }
                if let Some(model) = &status.model {
                    ui.label(RichText::new(model).weak().monospace());
                }
            });
        });
    }

    /// Stable id of the chat input box (also used to surrender its focus).
    fn input_id(&self) -> egui::Id {
        egui::Id::new(("session_input", self.id))
    }

    fn input_area(&mut self, ui: &mut egui::Ui, suppress_keys: bool) {
        // Slash-command hints while typing a command.
        if self.input.starts_with('/') && !self.slash_commands.is_empty() {
            let needle = self.input.trim_start_matches('/').to_ascii_lowercase();
            let matches: Vec<_> = self
                .slash_commands
                .iter()
                .filter(|cmd| cmd.name.to_ascii_lowercase().starts_with(&needle))
                .take(6)
                .collect();
            if !matches.is_empty() {
                egui::Frame::group(ui.style())
                    .inner_margin(4.0)
                    .show(ui, |ui| {
                        for cmd in matches {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(format!("/{}", cmd.name)).monospace());
                                ui.label(RichText::new(&cmd.description).weak().small());
                            });
                        }
                    });
            }
        }

        // Enter submits (consumed before the widget sees it); Shift+Enter = newline.
        let mut submit = false;
        if self.input_had_focus && !suppress_keys {
            submit = ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter));
        }
        let hint = if self.active_subtab.is_some() {
            "Viewing a fork (read-only) — input goes to the main agent"
        } else {
            "Message the agent... (Enter to send, Shift+Enter for newline)"
        };
        let input_id = self.input_id();
        let response = ui.add(
            egui::TextEdit::multiline(&mut self.input)
                // Stable id: the slash-command hint above toggles in and out,
                // which would otherwise change this widget's auto-generated id
                // and drop keyboard focus.
                .id(input_id)
                .desired_width(f32::INFINITY)
                .desired_rows(3)
                .hint_text(hint),
        );
        self.input_had_focus = response.has_focus();
        if submit {
            self.submit_input();
            response.request_focus();
        }

        // Keep the chat box focused so the keyboard works without a click,
        // unless a modal/popup is open or an approval is waiting for a decision.
        if !suppress_keys
            && !self.has_pending_approvals()
            && !response.has_focus()
            && ui.input(|i| i.viewport().focused != Some(false))
            && !egui::Popup::is_any_open(ui.ctx())
        {
            response.request_focus();
        }
    }
}

/// One step along the fork row, which is `main` followed by `subagents`
/// entries. `None` is the main transcript, `Some(i)` the i-th fork; the walk
/// wraps in both directions and stays on main when there are no forks.
fn step_subtab(current: Option<usize>, subagents: usize, forward: bool) -> Option<usize> {
    if subagents == 0 {
        return None;
    }
    let slots = subagents + 1;
    let current = current.map_or(0, |index| index + 1);
    let next = if forward {
        (current + 1) % slots
    } else {
        (current + slots - 1) % slots
    };
    next.checked_sub(1)
}

#[cfg(test)]
mod tests {
    use super::step_subtab;

    #[test]
    fn test_no_forks_stays_on_main() {
        assert_eq!(step_subtab(None, 0, true), None);
        assert_eq!(step_subtab(None, 0, false), None);
    }

    #[test]
    fn test_forward_wraps_through_main() {
        // main -> fork 0 -> fork 1 -> main
        assert_eq!(step_subtab(None, 2, true), Some(0));
        assert_eq!(step_subtab(Some(0), 2, true), Some(1));
        assert_eq!(step_subtab(Some(1), 2, true), None);
    }

    #[test]
    fn test_backward_is_the_mirror() {
        // main -> fork 1 -> fork 0 -> main
        assert_eq!(step_subtab(None, 2, false), Some(1));
        assert_eq!(step_subtab(Some(1), 2, false), Some(0));
        assert_eq!(step_subtab(Some(0), 2, false), None);
    }

    #[test]
    fn test_single_fork_toggles() {
        assert_eq!(step_subtab(None, 1, true), Some(0));
        assert_eq!(step_subtab(Some(0), 1, true), None);
        assert_eq!(step_subtab(None, 1, false), Some(0));
        assert_eq!(step_subtab(Some(0), 1, false), None);
    }
}
