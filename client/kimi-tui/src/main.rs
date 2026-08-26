//! kimi-tui: a terminal frontend for the kimi-agent wire protocol.
//!
//! Usage:
//!   kimi-tui [--agent-bin <path>] [agent args...]
//!
//! One conversation per invocation — a TUI owns the whole terminal. The agent
//! binary is resolved by [`wire_client::launch`] exactly as in kimi-gui; the
//! remaining arguments go to the agent verbatim (`-w <dir>`, `--session <id>`,
//! `--continue`, ...).
//!
//! Keys:
//!   Enter   send the message (or steer mid-turn)
//!   Esc     cancel the running turn / close an overlay
//!   1/2/3   answer the approval overlay
//!   Tab     cycle subagent sub-transcripts
//!   PgUp/PgDn, mouse wheel   scroll
//!   Ctrl+O  resume menu · Ctrl+C / q(when idle) quit

mod agent;
mod input;
mod render;
mod theme;

use std::sync::mpsc;
use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap};
use ratatui::{Terminal, backend::CrosstermBackend};

use crossterm::event::{Event as CtEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use wire_client::session_list::{ResumeEntry, spawn_session_listing};

use crate::agent::{AgentSession, Phase};
use crate::input::Editor;
use crate::render::{RenderedTranscript, Row, push_display_block_lines};
use wire_client::transcript::Block as TranscriptBlock;

/// What wakes the loop. Wire traffic and key presses land on one channel so a
/// single `recv_timeout` serves both.
enum Msg {
    Key(KeyEvent),
    Mouse(Box<crossterm::event::MouseEvent>),
    Wake,
    Resize,
}

/// Which overlay owns the keyboard.
#[derive(PartialEq)]
enum Overlay {
    None,
    Approval,
    Resume,
}

fn main() -> std::io::Result<()> {
    let launch = wire_client::launch::AgentLaunch::from_env()
        .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?;

    // Terminal setup before anything that can fail visibly.
    let mut terminal = init_terminal()?;
    let result = run_app(&mut terminal, &launch.agent_bin, &launch.agent_args);
    restore_terminal(&mut terminal)?;
    result
}

fn init_terminal() -> std::io::Result<Terminal<CrosstermBackend<std::io::Stdout>>> {
    let mut stdout = std::io::stdout();
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture
    )?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend)
}

fn restore_terminal(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
) -> std::io::Result<()> {
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::event::DisableMouseCapture
    )?;
    crossterm::terminal::disable_raw_mode()?;
    Ok(())
}

struct App {
    session: AgentSession,
    editor: Editor,
    /// Cached wrapped rows for the current transcript state.
    rendered: RenderedTranscript,
    /// Width the rows were wrapped at (0 = nothing rendered yet).
    rendered_width: u16,
    /// Transcript version the rows were built from (see `Transcript::version`).
    rendered_version: u64,
    /// Which transcript (None = main, Some = subagent index) the rows were
    /// built from.
    rendered_source: Option<usize>,
    /// Set when the visible source switched; forces the next rebuild.
    rendered_dirty: bool,
    /// Index just past the last visible row (`rendered.len()` = pinned to live).
    scroll_bottom: usize,
    overlay: Overlay,
    resume_entries: Vec<ResumeEntry>,
    resume_listing: Option<mpsc::Receiver<Result<Vec<ResumeEntry>, String>>>,
    resume_cursor: usize,
    list_state: ListState,
    /// Set when the user picked a session to resume: run_app exits with the
    /// agent args to relaunch with, printed after terminal restore.
    resume_command: Option<String>,
    /// Visible transcript: None = main, Some(task_tool_call_id) = subagent.
    active_subtab: Option<String>,
}

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    agent_bin: &str,
    agent_args: &[String],
) -> std::io::Result<()> {
    let (tx, rx) = mpsc::channel::<Msg>();
    // The wake hook lands on the same channel: wire messages trigger an
    // immediate redraw instead of waiting for the spinner tick.
    let wake_tx = tx.clone();

    // Input thread: terminal events → channel.
    std::thread::Builder::new()
        .name("ct-events".into())
        .spawn(move || -> std::io::Result<()> {
            loop {
                match crossterm::event::read()? {
                    CtEvent::Key(key) => {
                        if tx.send(Msg::Key(key)).is_err() {
                            return Ok(());
                        }
                    }
                    CtEvent::Mouse(mouse) => {
                        if tx.send(Msg::Mouse(Box::new(mouse))).is_err() {
                            return Ok(());
                        }
                    }
                    CtEvent::Resize(..) => {
                        if tx.send(Msg::Resize).is_err() {
                            return Ok(());
                        }
                    }
                    _ => {}
                }
            }
        })
        .expect("spawn ct-events thread");

    let mut app = App::new(agent_bin, agent_args, move || {
        let _ = wake_tx.send(Msg::Wake);
    })
    .map_err(std::io::Error::other)?;

    // Drain whatever arrived while we were starting up.
    app.session.drain_inbound();
    app.rebuild_if_needed(terminal.size()?.width);

    loop {
        // Spinner animation and startup grace: poll while busy, otherwise
        // block until something actually happens.
        let timeout = if app.session.phase == Phase::Running
            || app.session.phase == Phase::Initializing
            || app.session.phase == Phase::Replaying
        {
            Duration::from_millis(120)
        } else {
            Duration::from_secs(3600)
        };
        match rx.recv_timeout(timeout) {
            Ok(Msg::Wake) => {} // fall through to drain below
            Ok(Msg::Key(key)) => {
                if !app.handle_key(key) {
                    break;
                }
            }
            Ok(Msg::Mouse(mouse)) => app.handle_mouse(&mouse),
            Ok(Msg::Resize) => { /* next draw re-lays out */ }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        // Coalesce the inbound backlog into one repaint per burst: streaming
        // deltas arrive far faster than the terminal can usefully repaint,
        // so drain the queue dry and draw the end state once.
        let mut quit = false;
        loop {
            match rx.try_recv() {
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    quit = true;
                    break;
                }
                Ok(msg) => {
                    let keep_going = match msg {
                        Msg::Key(key) => app.handle_key(key),
                        Msg::Mouse(mouse) => {
                            app.handle_mouse(&mouse);
                            true
                        }
                        Msg::Resize | Msg::Wake => true, /* next draw re-lays out */
                    };
                    if !keep_going {
                        quit = true;
                        break;
                    }
                }
            }
            app.session.drain_inbound();
            app.poll_resume_listing();
        }
        if quit {
            break;
        }

        app.session.drain_inbound();
        app.poll_resume_listing();

        app.rebuild_if_needed(terminal.size()?.width);
        terminal.draw(|frame| app.draw(frame))?;
    }

    let resume_command = app.resume_command.take();
    app.session.shutdown();
    if let Some(args) = resume_command {
        // After restore_terminal this lands in the normal scrollback.
        println!(
            "To resume the picked session, run:\n  kimi-tui {args}\n(or add -w <dir> for its work directory)"
        );
    }
    Ok(())
}

impl App {
    fn new(
        agent_bin: &str,
        agent_args: &[String],
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self, String> {
        let session = AgentSession::spawn(agent_bin, agent_args, wake)?;
        Ok(Self {
            session,
            editor: Editor::default(),
            rendered: RenderedTranscript::new(),
            rendered_width: 0,
            rendered_version: 0,
            rendered_source: None,
            // Force the first rebuild.
            rendered_dirty: true,
            scroll_bottom: 0, // set on first rebuild
            overlay: Overlay::None,
            resume_entries: Vec::new(),
            resume_listing: None,
            resume_cursor: 0,
            list_state: ListState::default(),
            resume_command: None,
            active_subtab: None,
        })
    }

    /// Rebuild the wrapped rows when content, width, or visible source
    /// changed. The transcript exposes a monotonically bumped `version` —
    /// streaming deltas mutate blocks in place, so block *count* would miss
    /// them; version catches every change.
    ///
    /// Scroll policy: `scroll_bottom == rendered.len()` means "pinned to the
    /// live tail" and follows growth; once the user scrolls up, their
    /// position is preserved (offset from the tail, since rows shift).
    fn rebuild_if_needed(&mut self, width: u16) {
        // Which block list is visible: main, or the active subagent's.
        let mut fallback_to_main = false;
        let source = match &self.active_subtab {
            None => None,
            Some(task_id) => {
                match self
                    .session
                    .transcript
                    .subagents
                    .iter()
                    .position(|s| &s.task_tool_call_id == task_id)
                {
                    Some(index) => Some(index),
                    None => {
                        fallback_to_main = true;
                        None
                    }
                }
            }
        };
        if fallback_to_main {
            self.active_subtab = None;
        }

        let running = match source {
            None => self.session.phase == Phase::Running,
            Some(index) => !self.session.transcript.subagents[index].done,
        };
        let version = match source {
            None => self.session.transcript.version,
            Some(index) => self.session.transcript.subagents[index].transcript.version,
        };

        let force = self.rendered_dirty;
        if force
            || self.rendered_width != width
            || self.rendered_source != source
            || self.rendered_version != version
        {
            let was_pinned = self.scroll_bottom >= self.rendered.len();
            let from_tail = self.rendered.len().saturating_sub(self.scroll_bottom);
            let blocks: &[TranscriptBlock] = match source {
                None => &self.session.transcript.blocks,
                Some(index) => &self.session.transcript.subagents[index].transcript.blocks,
            };
            self.rendered = RenderedTranscript::rebuild(blocks, width, running);
            self.rendered_width = width;
            self.rendered_version = version;
            self.rendered_source = source;
            self.rendered_dirty = false;
            self.scroll_bottom = if was_pinned {
                self.rendered.len()
            } else {
                // Keep the same distance above the live tail.
                self.rendered.len() - from_tail.min(self.rendered.len())
            };
        }
    }

    /// Returns false when the app should exit.
    fn handle_key(&mut self, key: KeyEvent) -> bool {
        // Only honor press events (Windows sends releases too under some
        // terminals).
        if key.kind == KeyEventKind::Release {
            return true;
        }
        match self.overlay {
            Overlay::Approval => return self.handle_approval_key(key),
            Overlay::Resume => return self.handle_resume_key(key),
            Overlay::None => {}
        }

        match (key.modifiers, key.code) {
            (m, KeyCode::Char('c')) if m.contains(KeyModifiers::CONTROL) => false,
            (KeyModifiers::CONTROL, KeyCode::Char('o')) => {
                self.open_resume_menu();
                true
            }
            (_, KeyCode::Esc) => {
                self.session.cancel();
                true
            }
            (_, KeyCode::Tab) => {
                self.cycle_subtab();
                true
            }
            (m, KeyCode::Char('q')) if m.is_empty() && self.editor.text().is_empty() => false,
            (_, KeyCode::Enter) => {
                let text = self.editor.text().to_string();
                self.editor.clear();
                self.session.submit(&text);
                true
            }
            (m, KeyCode::Backspace) if m.is_empty() => {
                self.editor.backspace();
                true
            }
            (m, KeyCode::Delete) if m.is_empty() => {
                self.editor.delete();
                true
            }
            (m, KeyCode::Left) if m.is_empty() => {
                self.editor.left();
                true
            }
            (m, KeyCode::Right) if m.is_empty() => {
                self.editor.right();
                true
            }
            (m, KeyCode::Home) if m.is_empty() => {
                self.editor.home();
                true
            }
            (m, KeyCode::End) if m.is_empty() => {
                self.editor.end();
                true
            }
            (_, KeyCode::PageUp) => {
                self.scroll_by(-(20i32));
                true
            }
            (_, KeyCode::PageDown) => {
                self.scroll_by(20);
                true
            }
            (_, KeyCode::Char(ch)) => {
                self.editor.insert(&ch.to_string());
                true
            }
            _ => true,
        }
    }

    fn handle_approval_key(&mut self, key: KeyEvent) -> bool {
        use kimi_agent::wire::ApprovalResponseKind as K;
        match key.code {
            KeyCode::Char('1') => {
                self.session.resolve_approval(K::Approve);
                self.overlay = Overlay::None;
            }
            KeyCode::Char('2') => {
                self.session.resolve_approval(K::ApproveForSession);
                self.overlay = Overlay::None;
            }
            KeyCode::Char('3') => {
                self.session.resolve_approval(K::Reject);
                self.overlay = Overlay::None;
            }
            KeyCode::Esc => {
                // Dismiss visually; the approval stays pending.
                self.overlay = Overlay::None;
            }
            _ => {}
        }
        true
    }

    fn handle_resume_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc => self.overlay = Overlay::None,
            KeyCode::Down => {
                if !self.resume_entries.is_empty() {
                    self.resume_cursor =
                        (self.resume_cursor + 1).min(self.resume_entries.len() - 1);
                }
            }
            KeyCode::Up => {
                self.resume_cursor = self.resume_cursor.saturating_sub(1);
            }
            KeyCode::Enter => {
                if let Some(entry) = self.resume_entries.get(self.resume_cursor).cloned() {
                    // Resuming means starting a different agent session; the
                    // cleanest flow is to exit and let a wrapper re-launch.
                    // We print the exact command before restoring the
                    // terminal so it survives into the normal buffer.
                    self.resume_command = Some(format!("--session {}", entry.id));
                    return false;
                }
            }
            _ => {}
        }
        true
    }

    fn open_resume_menu(&mut self) {
        self.overlay = Overlay::Resume;
        self.resume_cursor = 0;
        // A fresh listing each time the menu opens; it finishes quickly, and
        // draw() shows "loading" until poll_resume_listing lands the result.
        self.resume_listing = Some(spawn_session_listing(move || {}));
    }

    fn poll_resume_listing(&mut self) {
        if let Some(rx) = &self.resume_listing {
            match rx.try_recv() {
                Ok(Ok(entries)) => {
                    self.resume_entries = entries;
                    self.resume_listing = None;
                }
                Ok(Err(_)) | Err(mpsc::TryRecvError::Disconnected) => {
                    self.resume_listing = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
    }

    /// Cycle main → each subagent in spawn order → main, mirroring
    /// kimi-gui's fork row. No-op without subagents.
    fn cycle_subtab(&mut self) {
        let subagents = &self.session.transcript.subagents;
        if subagents.is_empty() {
            self.active_subtab = None;
            return;
        }
        let current = self
            .active_subtab
            .as_deref()
            .and_then(|id| subagents.iter().position(|s| s.task_tool_call_id == id));
        // slots = subagents + main; walk with wrap, 0 meaning main.
        let slots = subagents.len() + 1;
        let next = match current {
            None => 1,
            Some(index) => (index + 2) % slots,
        };
        self.active_subtab = if next == 0 {
            None
        } else {
            Some(subagents[next - 1].task_tool_call_id.clone())
        };
        // The visible block set changed; force a rebuild.
        self.rendered_dirty = true;
    }

    fn scroll_by(&mut self, delta: i32) {
        let len = self.rendered.len();
        let next = (self.scroll_bottom as i64 + delta as i64).clamp(1, len.max(1) as i64) as usize;
        self.scroll_bottom = next;
    }

    /// Wheel scrolling: up unpins from the live tail, down past the end
    /// re-pins. Only wheel events reach here; other mouse activity is noise.
    fn handle_mouse(&mut self, event: &crossterm::event::MouseEvent) {
        let delta = match event.kind {
            crossterm::event::MouseEventKind::ScrollUp => -3,
            crossterm::event::MouseEventKind::ScrollDown => 3,
            _ => return,
        };
        self.scroll_by(delta);
    }
}

impl App {
    fn draw(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        // Layout, Python-shell style — no boxes anywhere:
        //   ✨ main · ▶ coder · researcher     (tab strip, only when forks exist)
        //   transcript rows …
        //   ── input · hint ──────────────────
        //   ✨ <editor>
        //   ─────────────────────────────────
        //   status line (dim)
        let has_subtabs = !self.session.transcript.subagents.is_empty();
        let tab_height = u16::from(has_subtabs);
        let separator_height = 1u16;
        let editor_height = 1u16;
        let status_height = 1u16;

        let [
            tab_area,
            transcript_area,
            input_sep,
            editor_area,
            status_sep,
            status_area,
        ] = Layout::vertical([
            Constraint::Length(tab_height),
            Constraint::Min(1),
            Constraint::Length(separator_height),
            Constraint::Length(editor_height),
            Constraint::Length(separator_height),
            Constraint::Length(status_height),
        ])
        .areas(area);

        // --- tab strip ------------------------------------------------------
        if has_subtabs {
            frame.render_widget(self.tab_strip(area.width), tab_area);
        }

        // --- transcript ---------------------------------------------------
        let viewport_height = transcript_area.height as usize;
        let bottom = self.scroll_bottom.min(self.rendered.len());
        let rows = self.rendered.viewport(bottom, viewport_height);
        let mut lines: Vec<Line> = rows.iter().map(|row: &Row| row.to_span_line()).collect();
        if lines.is_empty() {
            lines.push(Line::from(Span::styled("starting…", theme::dim())));
        }
        frame.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }),
            transcript_area,
        );

        // --- input rule ----------------------------------------------------
        // Doubles as the subtab indicator: "input" or "input · <fork>".
        let hint = match self.session.phase {
            Phase::Ready => "",
            Phase::Running => "working · Esc cancels · Enter steers",
            Phase::Initializing => "initializing",
            Phase::Replaying => "loading history",
            Phase::Failed(_) => "failed",
        };
        let mut label = String::from("input");
        if let Some(sub) = self.subtab_suffix() {
            label.push_str(&format!(" · {sub}"));
        }
        if !hint.is_empty() {
            label.push_str(&format!(" · {hint}"));
        }
        let sep_style = match self.session.phase {
            Phase::Running => Style::default().fg(theme::ACCENT),
            Phase::Failed(_) => Style::default().fg(theme::ERROR),
            _ => theme::dim(),
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                theme::separator_line(&label, area.width),
                sep_style,
            ))),
            input_sep,
        );

        // --- editor row ----------------------------------------------------
        let text = format!("✨ {}", self.editor.text());
        let cursor_col = self.editor.cursor_chars() as u16 + 3; // after "✨ "
        frame.render_widget(Paragraph::new(text), editor_area);
        if self.overlay == Overlay::None && cursor_col < area.width {
            frame.set_cursor_position(Position::new(cursor_col, editor_area.y));
        }

        // --- status rule + status bar --------------------------------------
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "─".repeat(area.width as usize),
                theme::dim(),
            ))),
            status_sep,
        );
        let status = self.status_line(area.width);
        frame.render_widget(status, status_area);

        // --- overlays -----------------------------------------------------
        match self.overlay {
            Overlay::Approval => self.draw_approval(frame, area),
            Overlay::Resume => self.draw_resume(frame, area),
            Overlay::None => {
                // An approval may have arrived without a key press yet.
                if self.session.has_pending_approvals() {
                    self.overlay = Overlay::Approval;
                }
            }
        }
    }

    /// Label for the active fork view, if one is selected.
    fn subtab_suffix(&self) -> Option<String> {
        let task_id = self.active_subtab.as_deref()?;
        self.session
            .transcript
            .subagents
            .iter()
            .find(|s| s.task_tool_call_id == task_id)
            .map(|sub| sub.title.clone())
    }

    /// The top tab strip: `✨ main · ▶ coder · researcher` — the active tab
    /// bold in the accent color, still-running forks marked `▶` in yellow,
    /// finished ones dim. Mirrors kimi-gui's fork row.
    fn tab_strip(&self, width: u16) -> Paragraph<'static> {
        let mut spans = vec![Span::styled("✨ ", theme::accent())];
        // Main first, then subagents in spawn order — same walk as Tab.
        let active_index = self.active_subtab.as_deref().and_then(|id| {
            self.session
                .transcript
                .subagents
                .iter()
                .position(|s| s.task_tool_call_id == id)
        });
        let push_tab =
            |spans: &mut Vec<Span<'static>>, label: String, active: bool, running: bool| {
                if spans.len() > 1 {
                    spans.push(Span::styled(" · ", theme::dim()));
                }
                let style = if active {
                    theme::title()
                } else if running {
                    Style::default().fg(theme::WARNING)
                } else {
                    theme::dim()
                };
                if running && !active {
                    spans.push(Span::styled("▶ ", style));
                }
                spans.push(Span::styled(label, style));
            };
        push_tab(
            &mut spans,
            "main".to_string(),
            active_index.is_none(),
            self.session.phase == Phase::Running,
        );
        for (index, sub) in self.session.transcript.subagents.iter().enumerate() {
            let mut label = sub.title.clone();
            truncate_chars(&mut label, width.saturating_sub(12).max(16) as usize);
            push_tab(&mut spans, label, Some(index) == active_index, !sub.done);
        }
        Paragraph::new(Line::from(spans))
    }

    fn status_line(&self, width: u16) -> Paragraph<'static> {
        let t = &self.session.transcript.status;
        let mut left = match &self.session.phase {
            Phase::Initializing => "· initializing".to_string(),
            Phase::Replaying => "· loading history".to_string(),
            Phase::Running => "▶ working".to_string(),
            Phase::Ready => "ready".to_string(),
            Phase::Failed(err) => format!("✗ {}", err.lines().next().unwrap_or("failed")),
        };
        if self.session.has_pending_approvals() {
            let _ = std::fmt::Write::write_fmt(&mut left, format_args!(" · approval pending"));
        }
        let mut right = String::new();
        if let Some(model) = &t.model {
            right.push_str(model);
        }
        if t.yolo_enabled == Some(true) {
            right.push_str(" · yolo");
        }
        if let Some(label) = t.context_label() {
            right.push_str(" · ");
            right.push_str(&label);
        }
        // Trim from the middle so both ends stay visible.
        let budget = width.saturating_sub(4) as usize;
        if left.chars().count() + right.chars().count() + 3 > budget {
            let keep_left = budget.saturating_sub(right.chars().count() + 6);
            let cut: String = left.chars().take(keep_left).collect();
            left = format!("{cut}…");
        }
        let gap = (budget.saturating_sub(left.chars().count() + right.chars().count())).max(1);
        let line = format!("{}{}{}", left, " ".repeat(gap), right.trim_start());
        Paragraph::new(Line::from(vec![Span::styled(
            line,
            if matches!(self.session.phase, Phase::Failed(_)) {
                theme::error()
            } else {
                theme::dim()
            },
        )]))
    }

    fn draw_approval(&mut self, frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
        let Some(info) = self.session.first_approval_info() else {
            self.overlay = Overlay::None;
            return;
        };

        // Build the inner content once to measure it; the box then hugs the
        // text instead of eating the screen.
        let content_width = area.width.saturating_sub(4).max(20).min(100);
        let mut lines = vec![
            Line::from(vec![
                Span::styled("✨ ", theme::accent()),
                Span::styled("approval required", theme::title()),
                Span::styled(
                    format!(" · {} · {}", info.sender, info.action),
                    theme::dim(),
                ),
            ]),
            Line::from(""),
            Line::from(info.description.clone()),
            Line::from(""),
        ];
        // Display blocks (command previews etc.), dim like tool output.
        for block in &info.display {
            push_display_block_lines(&mut lines, block, content_width);
        }
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("[1] approve   ", theme::success()),
            Span::styled("[2] approve for session   ", theme::success()),
            Span::styled("[3] reject   ", theme::error()),
            Span::styled("[Esc] later", theme::dim()),
        ]));

        // Height = wrapped line count (approximate long lines at the content
        // width) + border + one padding row top and bottom.
        let wrapped: usize = lines
            .iter()
            .map(|line| {
                let text: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
                (text / content_width.max(1) as usize).max(1)
            })
            .sum();
        let height = (wrapped as u16 + 4)
            .min(area.height.saturating_sub(2))
            .max(6);
        let width = (content_width + 2).min(area.width.saturating_sub(2));
        let popup = centered_rect(area, width, height);

        let block = Block::bordered()
            .border_style(Style::default().fg(theme::ACCENT))
            .title(Span::styled(
                " approval ",
                Style::default()
                    .fg(theme::ACCENT_BRIGHT)
                    .add_modifier(Modifier::BOLD),
            ))
            .padding(Padding::horizontal(1));
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(block),
            popup,
        );
    }

    fn draw_resume(&mut self, frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
        let popup = centered_rect(area, area.width * 7 / 10, area.height * 6 / 10);
        frame.render_widget(Clear, popup);

        let mut items: Vec<ListItem> = vec![ListItem::new(Line::from(vec![
            Span::styled("✨ ", theme::accent()),
            Span::styled("resume", theme::title()),
            Span::styled("  · ↑↓ move · Enter opens · Esc closes", theme::dim()),
        ]))];
        if self.resume_listing.is_some() {
            items.push(ListItem::new(Line::from(Span::styled(
                "loading sessions…",
                theme::dim(),
            ))));
        } else if self.resume_entries.is_empty() {
            items.push(ListItem::new(Line::from(Span::styled(
                "no past sessions found",
                theme::dim(),
            ))));
        } else {
            for entry in &self.resume_entries {
                items.push(ListItem::new(Line::from(vec![
                    Span::styled(
                        entry.tab_title(),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("  {}", entry.meta_line()), theme::dim()),
                ])));
            }
        }
        self.list_state.select(Some(self.resume_cursor));
        let list = List::new(items)
            .highlight_style(
                Style::default()
                    .fg(theme::ACCENT_BRIGHT)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("› ");
        frame.render_stateful_widget(list, popup, &mut self.list_state);
    }
}

/// A rect of `w`×`h` centered inside `outer`, clamped to fit.
fn centered_rect(outer: ratatui::layout::Rect, w: u16, h: u16) -> ratatui::layout::Rect {
    let w = w.min(outer.width);
    let h = h.min(outer.height);
    ratatui::layout::Rect {
        x: outer.x + (outer.width - w) / 2,
        y: outer.y + (outer.height - h) / 2,
        width: w,
        height: h,
    }
}

/// Clip `text` to `max` chars, appending `…` when something was cut.
fn truncate_chars(text: &mut String, max: usize) {
    if text.chars().count() > max.saturating_sub(1) {
        let cut: String = text.chars().take(max.saturating_sub(1)).collect();
        *text = format!("{cut}…");
    }
}
