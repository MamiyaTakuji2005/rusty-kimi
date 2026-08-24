//! The eframe application: drives the wire client lifecycle
//! (initialize -> replay -> ready -> turn running) and renders the session.

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

pub struct KimiGuiApp {
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
    init_id: Option<String>,
    replay_id: Option<String>,
    prompt_id: Option<String>,
}

impl KimiGuiApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        agent_bin: &str,
        agent_args: &[String],
    ) -> Result<Self, String> {
        install_cjk_fallback_fonts(&cc.egui_ctx);
        let (client, inbound) = WireClient::spawn(agent_bin, agent_args, cc.egui_ctx.clone())
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
            init_id: Some(init_id),
            replay_id: None,
            prompt_id: None,
        })
    }

    fn drain_inbound(&mut self) {
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
                    self.transcript.blocks.push(crate::transcript::Block::Info(
                        format!("external tool call (replayed): {}", req.name),
                    ));
                    return;
                }
                self.client.respond_error(
                    &rpc_id,
                    -32000,
                    "External tools are not supported by this client",
                );
            }
            other => {
                self.transcript.blocks.push(crate::transcript::Block::Info(
                    format!("unexpected request: {}", other.type_name()),
                ));
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
                        self.transcript.status.model.get_or_insert(model.to_string());
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
                        .push(crate::transcript::Block::Info(format!("turn failed: {message}")));
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
                self.prompt_id =
                    Some(self.client.send_request("prompt", json!({"user_input": text})));
                self.phase = Phase::Running;
            }
            Phase::Running => {
                // TurnBegin/SteerInput events echo the input back for display.
                self.client.send_request("steer", json!({"user_input": text}));
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
        egui::Window::new("Approval required")
            .collapsible(false)
            .resizable(true)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .max_width(640.0)
            .show(ctx, |ui| {
                ui.label(RichText::new(format!("{} · {}", info.sender, info.action)).weak());
                ui.label(RichText::new(&info.description).strong());
                if !info.display.is_empty() {
                    ui.separator();
                    egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                        for block in &info.display {
                            display_block_ui(ui, block);
                        }
                    });
                }
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Approve").clicked() {
                        choice = Some(ApprovalResponseKind::Approve);
                    }
                    if ui.button("Approve for session").clicked() {
                        choice = Some(ApprovalResponseKind::ApproveForSession);
                    }
                    if ui
                        .button(RichText::new("Reject").color(Color32::from_rgb(200, 80, 80)))
                        .clicked()
                    {
                        choice = Some(ApprovalResponseKind::Reject);
                    }
                });
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
                if let (Some(tokens), Some(max)) = (status.context_tokens, status.max_context_tokens)
                {
                    let pct = status
                        .context_usage
                        .map(|u| format!("{:.0}%", u * 100.0))
                        .unwrap_or_default();
                    ui.label(RichText::new(format!("{tokens}/{max} {pct}")).weak().monospace());
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

    fn input_area(&mut self, ui: &mut egui::Ui) {
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
                egui::Frame::group(ui.style()).inner_margin(4.0).show(ui, |ui| {
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
        if self.input_had_focus {
            submit = ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter));
        }
        let response = ui.add(
            egui::TextEdit::multiline(&mut self.input)
                .desired_width(f32::INFINITY)
                .desired_rows(3)
                .hint_text("Message the agent... (Enter to send, Shift+Enter for newline)"),
        );
        self.input_had_focus = response.has_focus();
        if submit {
            self.submit_input();
            response.request_focus();
        }
    }
}

impl eframe::App for KimiGuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_inbound();

        if self.phase == Phase::Running {
            // Keep the spinner animated.
            ctx.request_repaint_after(std::time::Duration::from_millis(120));
            if ctx.input(|i| i.key_pressed(Key::Escape)) {
                self.client.send_request("cancel", json!({}));
            }
        }

        egui::TopBottomPanel::bottom("input_panel")
            .resizable(false)
            .show(ctx, |ui| {
                ui.add_space(4.0);
                self.input_area(ui);
                self.status_bar(ui);
                ui.add_space(2.0);
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    let running = self.phase == Phase::Running;
                    let last = self.transcript.blocks.len().saturating_sub(1);
                    for (index, block) in self.transcript.blocks.iter().enumerate() {
                        block_ui(
                            ui,
                            index,
                            block,
                            &mut self.md_cache,
                            index == last,
                            running,
                        );
                        ui.add_space(6.0);
                    }
                });
        });

        self.approval_modal(ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.client.shutdown();
    }
}

/// egui's bundled fonts have no CJK coverage; pull in a system font so
/// Japanese/Chinese session content doesn't render as tofu.
fn install_cjk_fallback_fonts(ctx: &egui::Context) {
    let candidates = [
        r"C:\Windows\Fonts\YuGothM.ttc",
        r"C:\Windows\Fonts\meiryo.ttc",
        r"C:\Windows\Fonts\msgothic.ttc",
        r"C:\Windows\Fonts\msyh.ttc",
    ];
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert(
                "cjk-fallback".to_owned(),
                std::sync::Arc::new(egui::FontData::from_owned(bytes)),
            );
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                fonts
                    .families
                    .entry(family)
                    .or_default()
                    .push("cjk-fallback".to_owned());
            }
            ctx.set_fonts(fonts);
            return;
        }
    }
}
