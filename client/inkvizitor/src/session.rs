//! One agent session: a dvadva-agent subprocess, its wire lifecycle
//! (initialize -> replay -> ready -> turn running), its transcript, and the
//! per-session UI (input, status bar, approvals, subagent sub-tabs).

use std::sync::mpsc::Receiver;

use eframe::egui::{self, Align, Align2, Key, Modifiers, RichText};
use egui_commonmark::CommonMarkCache;
use serde_json::{Value, json};

use dvadva_agent::wire::protocol::WIRE_PROTOCOL_VERSION;
use dvadva_agent::wire::{ApprovalResponse, ApprovalResponseKind, WireMessage};

use crate::render::{block_ui, display_block_ui};
use crate::theme;
use wire_client::transcript::{ApprovalInfo, Transcript};
use wire_client::{Inbound, WireClient};

#[derive(PartialEq)]
enum Phase {
    Initializing,
    Replaying,
    Ready,
    Running,
    Failed(String),
}

/// Inner margin of the transcript's central panel, per side.
const PANEL_MARGIN: i8 = 8;

/// Which pane of a split window a session is being drawn into.
///
/// A session is not owned by a pane: the same one can be open in two panes at
/// once, and each needs its own scroll position, chat box and modal. That is
/// what `index` is for — it salts every id below the session, so egui sees
/// two independent copies rather than one widget drawn twice.
#[derive(Clone, Copy)]
pub struct PaneSlot {
    /// Position of this pane in the window, and the id salt.
    pub index: usize,
    /// How many panes share the window's *width*. The transcript's wrap floor
    /// is measured against the whole monitor, which overflows a pane that
    /// only owns a fraction of it; this is what divides it down.
    pub columns: usize,
    /// Whether this is the pane the keyboard belongs to.
    ///
    /// Distinct from `suppress_keys`, which is also set by an overlay: this
    /// one says *which copy is real*. A session drawn in two panes has one
    /// set of "did the chat box have focus last frame" flags, and only the
    /// pane the typing happens in may write them — otherwise the idle copy,
    /// drawn afterwards, reports its own unfocused box and the typing pane
    /// wakes up next frame believing Enter is not a send.
    pub focused: bool,
}

impl Default for PaneSlot {
    /// The unsplit window: one pane, owning the full width.
    fn default() -> Self {
        Self {
            index: 0,
            columns: 1,
            focused: true,
        }
    }
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
    /// The remote this session runs on (`name`, `endpoint`), or `None` for a
    /// local agent. Sessions are per-tab, so a window can hold both.
    pub remote: Option<(String, String)>,
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
    /// Index into the active view's blocks the keyboard has climbed to;
    /// `None` while the chat box owns the arrows as normal.
    selected_block: Option<usize>,
    /// Scroll the selection into view on the next draw; set only by the
    /// keyboard so it never fights the mouse wheel.
    nav_scroll: bool,
    /// Whether the chat box's caret sat at the very start of the text (or
    /// the box was empty) as of last frame. Cached so this frame can decide,
    /// before the box redraws, whether an incoming Up arrow means "climb
    /// into the transcript" rather than "move the cursor".
    input_at_start: bool,
}

impl Session {
    /// Spawn a local agent for this tab.
    pub fn spawn(
        id: usize,
        title: String,
        agent_bin: &str,
        agent_args: &[String],
        egui_ctx: egui::Context,
    ) -> Result<Self, String> {
        let ctx = egui_ctx;
        let (client, inbound) =
            WireClient::spawn_without_console(agent_bin, agent_args, move || ctx.request_repaint())
                .map_err(|err| format!("failed to spawn agent `{agent_bin}`: {err}"))?;
        Self::from_client(id, title, None, client, inbound)
    }

    /// Connect this tab through a remote `dvadva-bridge` daemon instead of
    /// spawning a local agent; the agent (and its `~/.kimi`) lives on the
    /// daemon's machine.
    pub fn connect(
        id: usize,
        title: String,
        name: &str,
        endpoint: &str,
        agent_args: &[String],
        egui_ctx: egui::Context,
    ) -> Result<Self, String> {
        let ctx = egui_ctx;
        let (client, inbound) =
            WireClient::connect_tcp(endpoint, agent_args, move || ctx.request_repaint())
                .map_err(|err| format!("remote bridge `{name}`: {err}"))?;
        let remote = Some((name.to_string(), endpoint.to_string()));
        Self::from_client(id, title, remote, client, inbound)
    }

    fn from_client(
        id: usize,
        title: String,
        remote: Option<(String, String)>,
        client: WireClient,
        inbound: Receiver<Inbound>,
    ) -> Result<Self, String> {
        let init_id = client.send_request(
            "initialize",
            json!({
                "protocol_version": WIRE_PROTOCOL_VERSION,
                "client": {
                    "name": "inkvizitor",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            }),
        );
        Ok(Self {
            id,
            title,
            remote,
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
            selected_block: None,
            nav_scroll: false,
            input_at_start: true,
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
        self.selected_block = None;
    }

    /// The blocks currently on screen: the main transcript, or the active
    /// fork/subagent tab's own. Used for the keyboard-climb bounds — kept
    /// separate from [`Self::transcript_view`]'s copy of this match because
    /// that one also needs a `&mut self.md_cache` alongside it, which a
    /// `&self` helper returning a borrow would conflict with.
    fn active_blocks(&self) -> &[wire_client::transcript::Block] {
        match &self.active_subtab {
            None => &self.transcript.blocks,
            Some(task_id) => self
                .transcript
                .subagents
                .iter()
                .find(|s| &s.task_tool_call_id == task_id)
                .map(|s| s.transcript.blocks.as_slice())
                .unwrap_or(&self.transcript.blocks),
        }
    }

    pub fn drain_inbound(&mut self) {
        while let Ok(msg) = self.inbound.try_recv() {
            match msg {
                Inbound::Event(event) => {
                    // Any client attached to this session can answer an
                    // approval, and the agent broadcasts the resolution to
                    // all of them. Take our own dialog down for a request
                    // somebody else just answered, rather than leaving a
                    // modal up that no longer decides anything.
                    if let WireMessage::ApprovalResponse(resp) = &event {
                        self.approvals
                            .retain(|pending| pending.request_id != resp.request_id);
                    }
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
                        .push(wire_client::transcript::Block::Info(format!(
                            "wire error: {err}"
                        )));
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
                        .push(wire_client::transcript::Block::Info(format!(
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
                    .push(wire_client::transcript::Block::Info(format!(
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
                            .push(wire_client::transcript::Block::Info(
                                "max steps reached".into(),
                            ));
                    }
                }
                (_, Some(error)) => {
                    let message = error
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("unknown error");
                    self.transcript
                        .blocks
                        .push(wire_client::transcript::Block::Info(format!(
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
        if let Some(wire_client::transcript::Block::Approval { response, .. }) =
            self.transcript.blocks.get_mut(pending.block)
        {
            *response = Some(kind);
        }
    }

    /// Draw this session into one pane of the window. `suppress_keys`
    /// disables the session's keyboard shortcuts — an app-level popup is
    /// open, or this is not the focused pane.
    ///
    /// The panels here are `show_inside` rather than window panels: a pane is
    /// a region, and two of them draw side by side. Everything id-shaped is
    /// salted with `slot.index` so the same session may be open in both.
    pub fn ui(&mut self, ui: &mut egui::Ui, slot: PaneSlot, suppress_keys: bool) {
        // The panels below borrow `ui`, so the context cannot be borrowed
        // from it at the same time. It is an `Arc` handle; a clone is a
        // pointer copy.
        let ctx = ui.ctx().clone();
        // While the keyboard has climbed into the transcript it owns Escape
        // too — closing the climb takes priority over cancelling a turn.
        let mut toggle_fold = false;
        if !suppress_keys {
            if self.selected_block.is_some() {
                toggle_fold = self.transcript_nav_keys(&ctx, slot);
            } else if self.phase == Phase::Running && ctx.input(|i| i.key_pressed(Key::Escape)) {
                self.client.send_request("cancel", json!({}));
            }
        }

        // When an approval arrives, drop the chat box's focus once so the
        // 1/2/3 shortcuts answer the modal. Clicking back into the box
        // restores normal typing (digits included) for this approval.
        if slot.focused {
            if let Some(pending) = self.approvals.first() {
                if self.focus_released_for.as_deref() != Some(pending.request_id.as_str()) {
                    self.focus_released_for = Some(pending.request_id.clone());
                    ctx.memory_mut(|mem| mem.surrender_focus(self.input_id(slot)));
                }
            } else {
                self.focus_released_for = None;
            }
        }

        egui::TopBottomPanel::bottom(egui::Id::new(("input_panel", slot.index)))
            .resizable(false)
            .show_inside(ui, |ui| {
                ui.push_id((slot.index, self.id), |ui| {
                    ui.add_space(4.0);
                    self.input_area(ui, slot, suppress_keys);
                    self.status_bar(ui);
                    ui.add_space(2.0);
                });
            });

        // The fork strip is the first thing in this panel, so the panel's
        // usual top margin lands above a row of tabs rather than above text.
        // Trim it there and keep it everywhere else.
        let frame = egui::Frame::central_panel(&ctx.style()).inner_margin(egui::Margin {
            top: 2,
            ..egui::Margin::same(PANEL_MARGIN)
        });
        egui::CentralPanel::default()
            .frame(frame)
            .show_inside(ui, |ui| {
                ui.push_id((slot.index, self.id), |ui| {
                    self.subtab_strip(ui);
                    self.transcript_view(ui, slot, toggle_fold);
                });
            });

        if !suppress_keys {
            self.approval_modal(&ctx, slot);
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
        let warning = theme::colors(ui.ctx()).warning;
        // Scoped: the bar's metrics must not reach the transcript below it.
        ui.scope(|ui| {
            theme::FORK_BAR.apply(ui);
            ui.horizontal_wrapped(|ui| {
                if ui
                    .selectable_label(self.active_subtab.is_none(), "main")
                    .clicked()
                {
                    self.active_subtab = None;
                    self.selected_block = None;
                }
                for sub in &self.transcript.subagents {
                    let selected =
                        self.active_subtab.as_deref() == Some(sub.task_tool_call_id.as_str());
                    let label = if sub.done {
                        RichText::new(&sub.title)
                    } else {
                        RichText::new(format!("▶ {}", sub.title)).color(warning)
                    };
                    if ui.selectable_label(selected, label).clicked() {
                        self.active_subtab = Some(sub.task_tool_call_id.clone());
                        self.selected_block = None;
                    }
                }
            });
            // A hairline, not egui's default separator: that one reserves six
            // points of its own plus a gap either side, which is most of why
            // this row sat in a band half again its own height.
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.add(egui::Separator::default().spacing(1.0));
        });
    }

    /// Keys while the keyboard has climbed into the transcript: arrows move
    /// the highlighted block, Space/Enter folds it (the block itself decides
    /// whether that means anything), Ctrl+C copies it, Escape — or stepping
    /// past the newest block — drops back to the chat box. Returns whether a
    /// fold was requested, for [`Self::transcript_view`] to apply to the one
    /// block it lands on.
    fn transcript_nav_keys(&mut self, ctx: &egui::Context, slot: PaneSlot) -> bool {
        let (up, down, toggle, copy, cancel) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::ArrowUp),
                i.consume_key(Modifiers::NONE, Key::ArrowDown),
                i.consume_key(Modifiers::NONE, Key::Space)
                    || i.consume_key(Modifiers::NONE, Key::Enter),
                i.consume_key(Modifiers::COMMAND, Key::C),
                i.consume_key(Modifiers::NONE, Key::Escape),
            )
        });
        if cancel {
            self.exit_nav(ctx, slot);
            return false;
        }
        let Some(index) = self.selected_block else {
            return false;
        };
        if copy && let Some(block) = self.active_blocks().get(index) {
            ctx.copy_text(crate::render::block_copy_text(block));
        }
        if up {
            self.selected_block = Some(index.saturating_sub(1));
            self.nav_scroll = true;
        } else if down {
            if index + 1 < self.active_blocks().len() {
                self.selected_block = Some(index + 1);
                self.nav_scroll = true;
            } else {
                self.exit_nav(ctx, slot);
            }
        }
        toggle
    }

    /// Drop the climb and hand the keyboard back to the chat box.
    fn exit_nav(&mut self, ctx: &egui::Context, slot: PaneSlot) {
        self.selected_block = None;
        ctx.memory_mut(|mem| mem.request_focus(self.input_id(slot)));
    }

    fn transcript_view(&mut self, ui: &mut egui::Ui, slot: PaneSlot, toggle_fold: bool) {
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
        // Defensive: a stale index (e.g. the tab it pointed into just
        // changed underneath it) should fall back to "not climbing" rather
        // than panic or highlight the wrong row.
        if self
            .selected_block
            .is_some_and(|index| index >= blocks.len())
        {
            self.selected_block = None;
        }
        let selected = self.selected_block;
        let want_scroll = std::mem::take(&mut self.nav_scroll);
        let md_cache = &mut self.md_cache;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                // Everything in the transcript wraps at `available_width`;
                // hold that at the wrap floor so a window narrower than a
                // third of the screen clips text instead of folding it
                // tighter. The scroll area's clip rect does the clipping.
                let monitor = ui.input(|i| i.viewport().monitor_size).map(|size| size.x);
                ui.set_max_width(wrap_width(ui.available_width(), monitor, slot.columns));
                let last = blocks.len().saturating_sub(1);
                for (index, block) in blocks.iter().enumerate() {
                    let is_selected = selected == Some(index);
                    let response = block_ui(
                        ui,
                        index,
                        block,
                        md_cache,
                        index == last,
                        running,
                        is_selected,
                        is_selected && toggle_fold,
                    );
                    if is_selected && want_scroll {
                        // Instant, not animated: climbing is a discrete jump,
                        // and retargeting a smooth scroll mid-flight (which
                        // happens whenever the next key lands before the
                        // previous animation finishes) reuses the old
                        // animation's timing for the new distance, so it can
                        // "arrive" well short of the target. Top-aligned, not
                        // centered — a block taller than the viewport has no
                        // well-defined center, and top-aligning keeps its
                        // header (and the selection outline's top edge) on
                        // screen instead of stranding both mid-block.
                        response.scroll_to_me_animation(
                            Some(Align::TOP),
                            egui::style::ScrollAnimation::none(),
                        );
                    }
                    ui.add_space(6.0);
                }
            });
    }

    fn approval_modal(&mut self, ctx: &egui::Context, slot: PaneSlot) {
        let Some(pending) = self.approvals.first() else {
            return;
        };
        let Some(wire_client::transcript::Block::Approval { info, .. }) =
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
            .id(egui::Id::new(("approval_modal", slot.index, self.id)))
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
                        .button(RichText::new("Reject (3)").color(theme::colors(ui.ctx()).error))
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
        let colors = theme::colors(ui.ctx());
        ui.horizontal(|ui| {
            match &self.phase {
                Phase::Initializing => {
                    theme::spinner(ui);
                    ui.label("initializing...");
                }
                Phase::Replaying => {
                    theme::spinner(ui);
                    ui.label("loading history...");
                }
                Phase::Running => {
                    theme::spinner(ui);
                    ui.label("working... (Esc to cancel, Enter to steer)");
                }
                Phase::Ready => {
                    ui.label(RichText::new("ready").weak());
                }
                Phase::Failed(err) => {
                    ui.label(RichText::new(err).color(colors.error));
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let status = &self.transcript.status;
                if let Some(label) = status.context_label() {
                    let text = RichText::new(label).monospace();
                    // Warn before compaction rather than after it surprises you.
                    let text = match status.context_ratio().unwrap_or(0.0) {
                        r if r >= 0.95 => text.color(colors.error),
                        r if r >= 0.80 => text.color(colors.warning),
                        _ => text.weak(),
                    };
                    ui.label(text);
                }
                if status.yolo_enabled == Some(true) {
                    ui.label(RichText::new("yolo").color(colors.warning));
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
    /// Per pane: two panes showing this session are two boxes, and one of
    /// them holds the keyboard.
    fn input_id(&self, slot: PaneSlot) -> egui::Id {
        egui::Id::new(("session_input", slot.index, self.id))
    }

    fn input_area(&mut self, ui: &mut egui::Ui, slot: PaneSlot, suppress_keys: bool) {
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
                                // Ellipsized: a label in a horizontal row
                                // extends past the window instead of wrapping.
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(&cmd.description).weak().small(),
                                    )
                                    .truncate(),
                                );
                            });
                        }
                    });
            }
        }

        // Up from the very start of the box — Home, or the box is empty —
        // climbs into the transcript instead of moving a cursor that was
        // already at the top. `input_at_start` is last frame's cursor
        // position: it has to be, since this has to run (and consume the
        // key) before the box redraws and could claim it as its own.
        let climb = self.input_had_focus
            && !suppress_keys
            && self.selected_block.is_none()
            && self.input_at_start
            && !self.active_blocks().is_empty()
            && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowUp));
        if climb {
            self.selected_block = self.active_blocks().len().checked_sub(1);
            self.nav_scroll = true;
            ui.ctx()
                .memory_mut(|mem| mem.surrender_focus(self.input_id(slot)));
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
        let input_id = self.input_id(slot);
        let output = egui::TextEdit::multiline(&mut self.input)
            // Stable id: the slash-command hint above toggles in and out,
            // which would otherwise change this widget's auto-generated id
            // and drop keyboard focus.
            .id(input_id)
            .desired_width(f32::INFINITY)
            .desired_rows(3)
            .hint_text(hint)
            .show(ui);
        let response = output.response;
        // Last frame's state, for the keys consumed above before this widget
        // redraws — and only ever written by the pane those keys go to.
        if slot.focused {
            self.input_had_focus = response.has_focus();
            self.input_at_start = output.cursor_range.is_none_or(|r| r.primary.index == 0);
        }
        // Regaining focus — a click, most likely — is how climbing ends
        // early: the box owns the arrows again the moment it is the thing
        // being typed into.
        if response.has_focus() {
            self.selected_block = None;
        }
        if submit {
            self.submit_input();
            response.request_focus();
        }

        // Keep the chat box focused so the keyboard works without a click,
        // unless a modal/popup is open, an approval is waiting for a
        // decision, or the keyboard has climbed into the transcript.
        if !suppress_keys
            && !self.has_pending_approvals()
            && self.selected_block.is_none()
            && !response.has_focus()
            && ui.input(|i| i.viewport().focused != Some(false))
            && !egui::Popup::is_any_open(ui.ctx())
        {
            response.request_focus();
        }
    }
}

/// The width the transcript lays its text out at: the real width while the
/// pane is wide enough, floored at a third of the monitor — divided by the
/// panes sharing that monitor — below that.
///
/// This is what "word wrap" means here once the window gets small: text
/// follows the pane down to a third of the screen, and past that the layout
/// freezes and the pane edge clips it — a squeezed window stays a readable
/// column instead of folding prose one word per line. Widths are in logical
/// points and the monitor is whichever one the window is on, so the third
/// holds at any DPI scale; some backends cannot report a monitor, so assume
/// WQHD. The margin term makes a pane of exactly its share wrap seamlessly
/// at its real width.
fn wrap_width(available: f32, monitor_width: Option<f32>, columns: usize) -> f32 {
    let monitor = monitor_width.filter(|width| *width > 0.0).unwrap_or(2560.0);
    // A split window is still one window on one monitor: the floor is a
    // third of what this pane could ever be given, not a third of the
    // screen, or half a screen of transcript would clip at every width.
    let floor = monitor / (3 * columns.max(1)) as f32 - f32::from(PANEL_MARGIN) * 2.0;
    available.max(floor)
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
    use super::{PANEL_MARGIN, step_subtab, wrap_width};

    /// A wide-enough window wraps at its real width — the floor is invisible.
    #[test]
    fn test_wrap_width_uses_the_real_width_when_wide() {
        assert_eq!(wrap_width(1200.0, Some(2560.0), 1), 1200.0);
    }

    #[test]
    fn test_wrap_width_floors_at_a_third_of_the_monitor() {
        let floor = 2560.0 / 3.0 - f32::from(PANEL_MARGIN) * 2.0;
        assert_eq!(wrap_width(400.0, Some(2560.0), 1), floor);
        // At exactly a third of the screen the two widths agree, so shrinking
        // through the boundary never jumps.
        assert_eq!(wrap_width(floor, Some(2560.0), 1), floor);
    }

    #[test]
    fn test_wrap_width_follows_the_monitor_the_window_is_on() {
        assert!(wrap_width(100.0, Some(1920.0), 1) < wrap_width(100.0, Some(2560.0), 1));
    }

    /// No monitor info (or a nonsense zero) falls back to assuming WQHD.
    #[test]
    fn test_wrap_width_assumes_wqhd_without_monitor_info() {
        assert_eq!(
            wrap_width(400.0, None, 1),
            wrap_width(400.0, Some(2560.0), 1)
        );
        assert_eq!(wrap_width(400.0, Some(0.0), 1), wrap_width(400.0, None, 1));
    }

    /// Side by side, each pane's floor is its own share of the screen —
    /// otherwise both halves of a split clip at every width.
    #[test]
    fn test_wrap_width_divides_the_floor_between_columns() {
        let single = wrap_width(100.0, Some(2560.0), 1);
        let halved = wrap_width(100.0, Some(2560.0), 2);
        assert!(halved < single);
        assert_eq!(halved, 2560.0 / 6.0 - f32::from(PANEL_MARGIN) * 2.0);
        // A vertical split shares no width, so it passes one column and gets
        // the unsplit floor back.
        assert_eq!(wrap_width(100.0, Some(2560.0), 0), single);
    }

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
