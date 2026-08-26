//! The eframe application: a hub of sessions, each backed by its own
//! kimi-agent subprocess and shown as a top-level tab. A second tab layer
//! inside each session (main + subagent transcripts) lives in `session.rs`.
//!
//! Session creation lives in the tab strip: the `+` button opens the native
//! OS folder picker and instantly starts a session in the chosen directory,
//! while the two buttons pinned to the right edge open the resume menu with
//! every past session found under `~/.kimi` and cycle the theme.
//!
//! Everything here is also reachable from the keyboard alone — see
//! [`KimiGuiApp::handle_shortcuts`].

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError, channel};

use eframe::egui::{self, Align, Align2, Key, Modifiers, RichText};

use kimi_agent::share::get_share_dir as share_dir;

use crate::palette::{Command, Palette};
use crate::session::Session;
use crate::theme::Theme;
use wire_client::session_list::{ResumeEntry, spawn_remote_session_listing, spawn_session_listing};

/// Which overlay is on top and therefore owns the keyboard. Ordered most
/// modal first; `focus_owner` is the single place that decides.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FocusOwner {
    Error,
    CloseConfirm,
    ResumeMenu,
    Palette,
    Session,
}

pub struct KimiGuiApp {
    agent_bin: String,
    /// Remote bridge endpoint (`--remote` / `KIMI_REMOTE`), when set: every
    /// session and the resume listing go through the daemon instead.
    remote: Option<String>,
    sessions: Vec<Session>,
    active: usize,
    next_session_id: usize,
    /// In-flight native folder picker started by the `+` button.
    folder_pick: Option<Receiver<Option<PathBuf>>>,
    /// Sessions shown by the resume menu, newest first.
    resume_sessions: Vec<ResumeEntry>,
    /// In-flight resume listing (a result is pending).
    resume_listing: Option<Receiver<Result<Vec<ResumeEntry>, String>>>,
    /// The resume list is showing (`Ctrl+O` or the book button).
    resume_open: bool,
    /// Row highlighted in the resume list, for arrow-key browsing.
    resume_cursor: usize,
    /// Bring the highlighted resume row into view on the next draw. Only set
    /// when the keyboard moved it, so it never fights the mouse wheel.
    resume_scroll: bool,
    /// Session id waiting on a close confirmation (`Ctrl+T` or a tab's ×).
    close_confirm: Option<usize>,
    /// The command palette (`Ctrl+P`).
    palette: Palette,
    /// Something the user needs to be told about, shown in a small modal:
    /// a session that would not spawn, a file that would not open.
    error: Option<String>,
    /// Directory of the session most recently opened this run (any session
    /// carries its own `work_dir`; this covers the no-active-session case).
    last_workdir: Option<PathBuf>,
    /// Light / dark / Kimi, cycled by the toolbar button and `Ctrl+D`.
    theme: Theme,
}

/// The shortcut keys taken out of one frame's event queue.
struct Keys {
    next_session: bool,
    prev_session: bool,
    next_subtab: bool,
    prev_subtab: bool,
    new_session: bool,
    resume: bool,
    close: bool,
    palette: bool,
    theme: bool,
}

impl Keys {
    /// `Some(true)` to step forward through the session tabs, `Some(false)`
    /// backward, `None` to stay put.
    fn session_step(&self) -> Option<bool> {
        (self.next_session || self.prev_session).then_some(self.next_session)
    }

    /// The same for the fork/subagent row.
    fn subtab_step(&self) -> Option<bool> {
        (self.next_subtab || self.prev_subtab).then_some(self.next_subtab)
    }
}

impl KimiGuiApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        agent_bin: &str,
        remote: Option<String>,
        agent_args: &[String],
    ) -> Result<Self, String> {
        install_fallback_fonts(&cc.egui_ctx);
        let theme = Theme::load();
        theme.apply(&cc.egui_ctx);
        let mut app = Self {
            agent_bin: agent_bin.to_string(),
            remote,
            sessions: Vec::new(),
            active: 0,
            next_session_id: 1,
            folder_pick: None,
            resume_sessions: Vec::new(),
            resume_listing: None,
            resume_open: false,
            resume_cursor: 0,
            resume_scroll: false,
            close_confirm: None,
            palette: Palette::default(),
            error: None,
            last_workdir: None,
            theme,
        };
        app.open_session(agent_args.to_vec(), &cc.egui_ctx, None)?;
        // List past sessions right away: it pre-warms the resume menu and
        // supplies the most recent session directory as the folder-picker
        // default after a GUI restart. Through a remote bridge the sessions
        // live on the daemon's machine, so ask there instead.
        let ctx = cc.egui_ctx.clone();
        let endpoint = app.remote.clone();
        app.resume_listing = Some(match endpoint {
            Some(endpoint) => {
                spawn_remote_session_listing(&endpoint, move || ctx.request_repaint())
            }
            None => spawn_session_listing(move || ctx.request_repaint()),
        });
        Ok(app)
    }

    fn open_session(
        &mut self,
        args: Vec<String>,
        ctx: &egui::Context,
        title: Option<String>,
    ) -> Result<(), String> {
        // Only an explicit `-w` counts as a chosen directory; a session
        // launched without one runs in the GUI's incidental cwd, which must
        // not shadow the newest-on-disk default in the folder picker.
        let work_dir = args_workdir(&args).map(PathBuf::from);
        let id = self.next_session_id;
        self.next_session_id += 1;
        let title = title.unwrap_or_else(|| session_title(&args, id));
        let mut session = match &self.remote {
            // Paths in `args` (e.g. `-w`) resolve on the remote machine.
            Some(endpoint) => Session::connect(id, title, endpoint, &args, ctx.clone())?,
            None => Session::spawn(id, title, &self.agent_bin, &args, ctx.clone())?,
        };
        session.work_dir = work_dir.clone();
        self.last_workdir = work_dir.or(self.last_workdir.take());
        self.sessions.push(session);
        self.active = self.sessions.len() - 1;
        Ok(())
    }

    fn close_session(&mut self, index: usize) {
        let mut session = self.sessions.remove(index);
        session.shutdown();
        if self.active > index {
            self.active -= 1;
        }
        if self.active >= self.sessions.len() {
            self.active = self.sessions.len().saturating_sub(1);
        }
    }

    /// Who owns the keyboard this frame. Overlays stack, and only the topmost
    /// one gets keys — including `Enter` and `Escape`, which otherwise mean
    /// "send" and "cancel the turn" to the session underneath.
    fn focus_owner(&self) -> FocusOwner {
        if self.error.is_some() {
            FocusOwner::Error
        } else if self.close_confirm.is_some() {
            FocusOwner::CloseConfirm
        } else if self.resume_open {
            FocusOwner::ResumeMenu
        } else if self.palette.open {
            FocusOwner::Palette
        } else {
            FocusOwner::Session
        }
    }

    /// App-wide keyboard shortcuts, so the whole hub works without a mouse:
    ///
    /// * `Tab` / `Ctrl+Tab` — next / previous session tab (row one), wrapping
    /// * `Shift+Tab` / `Ctrl+Shift+Tab` — next / previous fork tab (row two)
    /// * `Ctrl+N` — new session (opens the folder picker)
    /// * `Ctrl+O` — resume menu, browsed with `↑`/`↓`, `Enter` to open
    /// * `Ctrl+T` — close the active session, after a confirmation
    /// * `Ctrl+P` — command palette, for everything without a key of its own
    /// * `Ctrl+D` — cycle the theme: light → dark → Kimi
    ///
    /// This runs before any widget is drawn and *consumes* the keys: the chat
    /// box holds focus permanently and would otherwise swallow `Tab` as an
    /// indent, `Enter` as a newline and the arrows as cursor movement.
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        match self.focus_owner() {
            // Answered by clicking; it takes no keys, and swallows the rest.
            FocusOwner::Error => return,
            FocusOwner::CloseConfirm => return self.close_confirm_keys(ctx),
            FocusOwner::ResumeMenu => return self.resume_menu_keys(ctx),
            FocusOwner::Palette => return self.palette_keys(ctx),
            FocusOwner::Session => {}
        }

        let keys = ctx.input_mut(|i| {
            // Most specific first: `consume_key` ignores an *extra* shift, so
            // a Ctrl+Tab pattern matches Ctrl+Shift+Tab too and would eat it.
            // (A bare pattern does reject ctrl, so those two can't collide.)
            let prev_subtab = i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::Tab);
            let prev_session = i.consume_key(Modifiers::COMMAND, Key::Tab);
            let next_subtab = i.consume_key(Modifiers::SHIFT, Key::Tab);
            let next_session = i.consume_key(Modifiers::NONE, Key::Tab);
            Keys {
                prev_subtab,
                prev_session,
                next_subtab,
                next_session,
                new_session: i.consume_key(Modifiers::COMMAND, Key::N),
                resume: i.consume_key(Modifiers::COMMAND, Key::O),
                close: i.consume_key(Modifiers::COMMAND, Key::T),
                // One pattern covers Ctrl+Shift+P as well, which is the same
                // command everywhere else and so does the same thing here.
                palette: i.consume_key(Modifiers::COMMAND, Key::P),
                theme: i.consume_key(Modifiers::COMMAND, Key::D),
            }
        });

        if let Some(forward) = keys.session_step()
            && !self.sessions.is_empty()
        {
            let len = self.sessions.len();
            let step = if forward { 1 } else { len - 1 };
            self.active = (self.active + step) % len;
        }
        if let Some(forward) = keys.subtab_step()
            && let Some(session) = self.sessions.get_mut(self.active)
        {
            session.cycle_subtab(forward);
        }
        if keys.new_session {
            self.start_folder_pick(ctx);
        }
        if keys.resume {
            self.open_resume_menu(ctx);
        }
        if keys.close {
            self.request_close(self.active);
        }
        if keys.palette {
            self.palette.open();
        }
        if keys.theme {
            self.cycle_theme(ctx);
        }
    }

    /// Step to the next theme and remember it for the next launch.
    fn cycle_theme(&mut self, ctx: &egui::Context) {
        self.theme = self.theme.next();
        self.theme.apply(ctx);
        self.theme.save();
    }

    /// Keys while the palette is up. The query box owns the printable
    /// characters; everything that steers the list is taken here first.
    fn palette_keys(&mut self, ctx: &egui::Context) {
        let (down, up, accept, cancel, toggle) = ctx.input_mut(|i| {
            // Tab steps the list, as in the resume menu: it has to be consumed
            // regardless, or it reaches a text box as an indent.
            let up = i.consume_key(Modifiers::SHIFT, Key::Tab)
                | i.consume_key(Modifiers::NONE, Key::ArrowUp);
            let down = i.consume_key(Modifiers::NONE, Key::Tab)
                | i.consume_key(Modifiers::NONE, Key::ArrowDown);
            let accept = i.consume_key(Modifiers::NONE, Key::Enter);
            let cancel = i.consume_key(Modifiers::NONE, Key::Escape);
            let toggle = i.consume_key(Modifiers::COMMAND, Key::P);
            (down, up, accept, cancel, toggle)
        });
        if cancel || toggle {
            self.palette.close();
            return;
        }
        let matches = self.palette.matches(!self.sessions.is_empty());
        if down || up {
            self.palette.step(down, matches.len());
        }
        // Clamp the way the list does, so Enter always takes the row that is
        // actually highlighted rather than silently doing nothing.
        self.palette.cursor = self.palette.cursor.min(matches.len().saturating_sub(1));
        if accept && let Some(entry) = matches.get(self.palette.cursor) {
            let command = entry.command;
            self.palette.close();
            self.run_command(command, ctx);
        }
    }

    /// Carry out one palette command. Adding a feature to the palette is a row
    /// in `palette::COMMANDS` plus an arm here.
    fn run_command(&mut self, command: Command, ctx: &egui::Context) {
        match command {
            Command::NewSession => self.start_folder_pick(ctx),
            Command::ResumeSession => self.open_resume_menu(ctx),
            Command::CloseSession => self.request_close(self.active),
            Command::CycleTheme => self.cycle_theme(ctx),
            Command::OpenConfig => self.open_path(kimi_agent::config::get_config_file()),
            Command::OpenMcpConfig => {
                self.open_path(kimi_agent::mcp::get_global_mcp_config_file());
            }
            Command::OpenLogFolder => self.open_path(share_dir().join("logs")),
            Command::OpenShareFolder => self.open_path(share_dir()),
            Command::OpenWorkDir => {
                // The tab may be running in the GUI's own cwd, with no `-w`.
                let dir = self
                    .sessions
                    .get(self.active)
                    .and_then(|session| session.work_dir.clone())
                    .or_else(|| std::env::current_dir().ok());
                match dir {
                    Some(dir) => self.open_path(dir),
                    None => self.error = Some("this session has no working directory".into()),
                }
            }
        }
    }

    /// Hand a path to the desktop, surfacing a failure where it can be seen.
    fn open_path(&mut self, path: PathBuf) {
        if let Err(err) = crate::os::open_in_default_app(&path) {
            self.error = Some(err);
        }
    }

    /// Enter/Escape for the close-confirmation modal. Answered here rather
    /// than in the window so the chat box never sees these keys.
    fn close_confirm_keys(&mut self, ctx: &egui::Context) {
        let (confirm, cancel) = ctx.input_mut(|i| {
            let confirm = i.consume_key(Modifiers::NONE, Key::Enter);
            let cancel = i.consume_key(Modifiers::NONE, Key::Escape);
            (confirm, cancel)
        });
        if confirm {
            self.confirm_close();
        } else if cancel {
            self.close_confirm = None;
        }
    }

    /// Ask before closing a tab. Every path here goes through the modal: a
    /// session is a live agent process, and a mis-aimed click or a `Ctrl+T`
    /// meant for a browser should not end one.
    fn request_close(&mut self, index: usize) {
        if let Some(session) = self.sessions.get(index) {
            self.close_confirm = Some(session.id);
        }
    }

    /// Act on the answered confirmation. The session is found by id, not by
    /// the index it had when the modal opened.
    fn confirm_close(&mut self) {
        let Some(id) = self.close_confirm.take() else {
            return;
        };
        if let Some(index) = self.sessions.iter().position(|session| session.id == id) {
            self.close_session(index);
        }
    }

    /// Arrow-key browsing while the resume list is open.
    ///
    /// Tab moves within the list instead of switching sessions: it has to be
    /// swallowed here either way, or it reaches the still-focused chat box as
    /// an indent behind the window.
    fn resume_menu_keys(&mut self, ctx: &egui::Context) {
        let (down, up, accept, cancel) = ctx.input_mut(|i| {
            // Shift+Tab before Tab; `consume_key` ignores an extra shift.
            let up = i.consume_key(Modifiers::SHIFT, Key::Tab)
                | i.consume_key(Modifiers::NONE, Key::ArrowUp);
            let down = i.consume_key(Modifiers::NONE, Key::Tab)
                | i.consume_key(Modifiers::NONE, Key::ArrowDown);
            let accept = i.consume_key(Modifiers::NONE, Key::Enter);
            let cancel = i.consume_key(Modifiers::NONE, Key::Escape);
            (down, up, accept, cancel)
        });
        if cancel {
            self.resume_open = false;
            return;
        }
        let Some(last) = self.resume_sessions.len().checked_sub(1) else {
            return;
        };
        if down {
            self.resume_cursor = (self.resume_cursor + 1).min(last);
            self.resume_scroll = true;
        }
        if up {
            self.resume_cursor = self.resume_cursor.saturating_sub(1);
            self.resume_scroll = true;
        }
        if accept {
            let entry = self.resume_sessions[self.resume_cursor.min(last)].clone();
            self.resume_open = false;
            self.resume_session(&entry, ctx);
        }
    }

    /// Open the folder picker for a new session, unless one is already up.
    fn start_folder_pick(&mut self, ctx: &egui::Context) {
        if self.folder_pick.is_some() {
            return;
        }
        // Open the picker where a parallel session is one Enter away:
        // the active tab's directory, else the most recently opened one
        // this run, else the newest past session on disk, else cwd.
        let start = self
            .sessions
            .get(self.active)
            .and_then(|session| session.work_dir.clone())
            .or_else(|| self.last_workdir.clone())
            .or_else(|| {
                self.resume_sessions
                    .first()
                    .map(|entry| entry.work_dir.clone())
            })
            .or_else(|| std::env::current_dir().ok());
        self.folder_pick = Some(pick_folder_async(ctx, start.as_deref()));
    }

    /// Show the resume list and re-list what is on disk behind it.
    fn open_resume_menu(&mut self, ctx: &egui::Context) {
        self.resume_open = true;
        self.resume_cursor = 0;
        self.resume_scroll = true;
        if self.resume_listing.is_none() {
            let ctx = ctx.clone();
            self.resume_listing = Some(match self.remote.clone() {
                // Remote sessions live on the bridge's machine.
                Some(endpoint) => {
                    spawn_remote_session_listing(&endpoint, move || ctx.request_repaint())
                }
                None => spawn_session_listing(move || ctx.request_repaint()),
            });
        }
    }

    /// Open a past session in a new tab (`kimi-agent -w <dir> --session <id>`).
    fn resume_session(&mut self, entry: &ResumeEntry, ctx: &egui::Context) {
        let args = vec![
            "-w".to_string(),
            entry.work_dir.to_string_lossy().into_owned(),
            "--session".to_string(),
            entry.id.clone(),
        ];
        if let Err(error) = self.open_session(args, ctx, Some(entry.tab_title())) {
            self.error = Some(error);
        }
    }

    /// Pick up the result of the `+` button's folder picker, if it finished.
    fn poll_folder_pick(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.folder_pick.take() else {
            return;
        };
        match rx.try_recv() {
            Ok(Some(dir)) => {
                let args = vec!["-w".to_string(), dir.to_string_lossy().into_owned()];
                if let Err(error) = self.open_session(args, ctx, None) {
                    self.error = Some(error);
                }
            }
            // Cancelled or the picker thread died: nothing to do.
            Ok(None) | Err(TryRecvError::Disconnected) => {}
            Err(TryRecvError::Empty) => self.folder_pick = Some(rx),
        }
    }

    /// Pick up the resume listing for the book menu, if it finished.
    fn poll_resume_listing(&mut self) {
        let Some(rx) = self.resume_listing.take() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(sessions)) => self.resume_sessions = sessions,
            // Keep the previous list on failure; the error is dropped for
            // simplicity (stale data is still shown in the menu).
            Ok(Err(_)) | Err(TryRecvError::Disconnected) => {}
            Err(TryRecvError::Empty) => self.resume_listing = Some(rx),
        }
    }

    fn tab_strip(&mut self, ctx: &egui::Context) {
        let mut close: Option<usize> = None;
        let mut pick_folder = false;
        let mut refresh_resume = false;
        let mut cycle_theme = false;
        let theme = self.theme;
        let bar = crate::theme::SESSION_BAR;
        egui::TopBottomPanel::top("session_tabs").show(ctx, |ui| {
            // One set of metrics for the whole strip: the tabs, their ×, the
            // `+`, and the two buttons in the panel below all inherit it.
            bar.apply(ui);
            // Resume and theme, pinned to the right edge of the strip. The
            // frame is replaced with a bare horizontal margin — a nested panel
            // brings its own vertical padding, which stacks on the strip's own
            // and puts the buttons in a band half again their height.
            let (book, paint) = egui::SidePanel::right("tabs_right")
                .resizable(false)
                .exact_width(66.0)
                .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(6, 0)))
                .show_inside(ui, |ui| {
                    ui.horizontal(|ui| {
                        let book = bar
                            .square(ui, "📖")
                            .on_hover_text("resume a session (Ctrl+O)");
                        let paint = bar.square(ui, theme.glyph()).on_hover_text(theme.hover());
                        (book, paint)
                    })
                    .inner
                })
                .inner;
            if book.clicked() {
                refresh_resume = true;
            }
            if paint.clicked() {
                cycle_theme = true;
            }

            // Session tabs plus the `+` button on the left.
            let colors = theme.colors();
            ui.horizontal_wrapped(|ui| {
                for (index, session) in self.sessions.iter().enumerate() {
                    let mut text = RichText::new(&session.title);
                    if session.is_failed() {
                        text = text.color(colors.error);
                    } else if session.has_pending_approvals() {
                        text = RichText::new(format!("⚠ {}", session.title)).color(colors.warning);
                    } else if session.is_running() {
                        text = RichText::new(format!("▶ {}", session.title));
                    }
                    if ui.selectable_label(index == self.active, text).clicked() {
                        self.active = index;
                    }
                    if bar
                        .square(ui, RichText::new("×").weak())
                        .on_hover_text("close session (Ctrl+T)")
                        .clicked()
                    {
                        close = Some(index);
                    }
                }
                if bar
                    .square(ui, "+")
                    .on_hover_text("new session, pick a folder (Ctrl+N)")
                    .clicked()
                {
                    pick_folder = true;
                }
            });
        });
        if let Some(index) = close {
            self.request_close(index);
        }
        if pick_folder {
            self.start_folder_pick(ctx);
        }
        if refresh_resume {
            if self.resume_open {
                self.resume_open = false;
            } else {
                self.open_resume_menu(ctx);
            }
        }
        if cycle_theme {
            self.cycle_theme(ctx);
        }
    }

    /// The resume list, opened by `Ctrl+O` or the book button.  Clicking a row
    /// — or moving to it with the arrow keys and pressing Enter — resumes that
    /// session in a new tab.
    ///
    /// A centered window rather than a menu hanging off the book button: that
    /// button sits in a 40px strip at the window's edge, and an anchored popup
    /// is fitted to the room left beside it, which is almost none. Sized off
    /// the window so it stays generous as the window grows.
    fn resume_window(&mut self, ctx: &egui::Context) {
        if !self.resume_open {
            return;
        }
        let mut resume: Option<ResumeEntry> = None;
        let cursor = self.resume_cursor;
        let want_scroll = std::mem::take(&mut self.resume_scroll);
        let sessions = &self.resume_sessions;
        let loading = self.resume_listing.is_some();
        let screen = ctx.screen_rect();
        let width = (screen.width() * 0.7).clamp(360.0, 1100.0);
        let height = (screen.height() * 0.6).clamp(240.0, 800.0);
        egui::Window::new("Resume session")
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_TOP, [0.0, 60.0])
            .default_width(width)
            .show(ctx, |ui| {
                ui.set_width(width);
                if loading {
                    ui.horizontal(|ui| {
                        crate::theme::spinner(ui);
                        ui.label("loading sessions...");
                    });
                    return;
                }
                if sessions.is_empty() {
                    ui.label(RichText::new("no past sessions found").weak());
                    return;
                }
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .max_height(height)
                    .show(ui, |ui| {
                        for (index, entry) in sessions.iter().enumerate() {
                            let selected = index == cursor;
                            let row = ui
                                .selectable_label(selected, RichText::new(&entry.title).strong())
                                .on_hover_text(format!("resume {}", entry.id));
                            if row.clicked() {
                                resume = Some(entry.clone());
                            }
                            if selected && want_scroll {
                                row.scroll_to_me(Some(Align::Center));
                            }
                            ui.label(RichText::new(entry.meta_line()).weak().small());
                            ui.add_space(2.0);
                        }
                    });
            });
        if let Some(entry) = resume {
            self.resume_open = false;
            self.resume_session(&entry, ctx);
        }
    }

    /// "Close this session?" — shown for `Ctrl+T` and for the tab's ×.
    fn close_confirm_window(&mut self, ctx: &egui::Context) {
        let Some(id) = self.close_confirm else {
            return;
        };
        // The session can go away underneath the modal (its agent died and
        // the tab was closed elsewhere); drop the question with it.
        let Some(session) = self.sessions.iter().find(|session| session.id == id) else {
            self.close_confirm = None;
            return;
        };
        let title = session.title.clone();
        let running = session.is_running();
        let (mut confirm, mut cancel) = (false, false);
        egui::Window::new("Close session")
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(RichText::new(format!("Close “{title}”?")).strong());
                if running {
                    ui.label(
                        RichText::new("A turn is still running and will be cancelled.")
                            .color(crate::theme::colors(ui.ctx()).warning),
                    );
                }
                ui.label(
                    RichText::new("The transcript is on disk — Ctrl+O reopens it.")
                        .weak()
                        .small(),
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Close (Enter)").clicked() {
                        confirm = true;
                    }
                    if ui.button("Cancel (Esc)").clicked() {
                        cancel = true;
                    }
                });
            });
        if confirm {
            self.confirm_close();
        } else if cancel {
            self.close_confirm = None;
        }
    }

    /// The command palette: a query box over a filtered command list.
    fn palette_window(&mut self, ctx: &egui::Context) {
        if !self.palette.open {
            return;
        }
        let has_session = !self.sessions.is_empty();
        let matches = self.palette.matches(has_session);
        // The cursor is an index into a list that the query just reshuffled.
        let cursor = self.palette.cursor.min(matches.len().saturating_sub(1));
        let want_scroll = std::mem::take(&mut self.palette.scroll);
        let mut chosen: Option<Command> = None;
        let query_id = egui::Id::new("palette_query");
        egui::Window::new("Command palette")
            // The query box and the list say what this is; a title bar over
            // them is just chrome.
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_TOP, [0.0, 80.0])
            .default_width(520.0)
            .show(ctx, |ui| {
                let query = ui.add(
                    egui::TextEdit::singleline(&mut self.palette.query)
                        .id(query_id)
                        .desired_width(f32::INFINITY)
                        .hint_text("type a command..."),
                );
                // The palette owns the keyboard while it is up, and the chat
                // box below has stopped grabbing focus back.
                if !query.has_focus() {
                    query.request_focus();
                }
                // Another keystroke means another list; the highlight belongs
                // back at the best match, not wherever it was in the old one.
                if query.changed() {
                    self.palette.cursor = 0;
                    self.palette.scroll = true;
                }
                ui.add_space(4.0);
                if matches.is_empty() {
                    ui.label(RichText::new("no matching command").weak());
                    return;
                }
                egui::ScrollArea::vertical()
                    .auto_shrink([false, true])
                    .max_height(320.0)
                    .show(ui, |ui| {
                        for (index, entry) in matches.iter().enumerate() {
                            let selected = index == cursor;
                            let row =
                                ui.selectable_label(selected, RichText::new(entry.title).strong());
                            if row.clicked() {
                                chosen = Some(entry.command);
                            }
                            if selected && want_scroll {
                                row.scroll_to_me(Some(Align::Center));
                            }
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(entry.detail).weak().small());
                                if let Some(binding) = entry.binding {
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            ui.label(
                                                RichText::new(binding).weak().small().monospace(),
                                            );
                                        },
                                    );
                                }
                            });
                            ui.add_space(2.0);
                        }
                    });
            });
        if let Some(command) = chosen {
            self.palette.close();
            self.run_command(command, ctx);
        }
    }

    fn error_window(&mut self, ctx: &egui::Context) {
        if self.error.is_none() {
            return;
        }
        let mut close = false;
        egui::Window::new("Kimi")
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(self.error.as_deref().unwrap_or_default())
                        .color(crate::theme::colors(ui.ctx()).error),
                );
                ui.add_space(6.0);
                if ui.button("OK").clicked() {
                    close = true;
                }
            });
        if close {
            self.error = None;
        }
    }
}

impl eframe::App for KimiGuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Background sessions keep progressing even while not visible.
        for session in &mut self.sessions {
            session.drain_inbound();
        }
        if self.sessions.iter().any(Session::is_running) {
            // Keep spinners animated.
            ctx.request_repaint_after(std::time::Duration::from_millis(120));
        }

        self.poll_folder_pick(ctx);
        self.poll_resume_listing();
        // Before any widget: the shortcuts take the keys they need out of the
        // event queue, ahead of the always-focused chat box.
        self.handle_shortcuts(ctx);
        self.tab_strip(ctx);
        self.resume_window(ctx);
        self.palette_window(ctx);
        self.close_confirm_window(ctx);
        self.error_window(ctx);

        if self.sessions.is_empty() {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.label(
                        RichText::new(
                            "no sessions — Ctrl+N to pick a folder, Ctrl+O to resume one",
                        )
                        .weak(),
                    );
                });
            });
            return;
        }

        // The session's own keys (Esc cancels, Enter sends) and its habit of
        // grabbing focus stay out of the way while anything is over it.
        let overlaid = self.focus_owner() != FocusOwner::Session;
        self.sessions[self.active].ui(ctx, overlaid);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        for session in &mut self.sessions {
            session.shutdown();
        }
    }
}

/// Open the native OS folder picker on a background thread (it blocks while
/// the dialog is shown) and report the picked directory, if any. The dialog
/// starts in `start_dir` when that directory still exists on disk.
fn pick_folder_async(
    ctx: &egui::Context,
    start_dir: Option<&std::path::Path>,
) -> Receiver<Option<PathBuf>> {
    let start_dir = start_dir.filter(|dir| dir.is_dir()).map(ToOwned::to_owned);
    let (tx, rx) = channel();
    let ctx = ctx.clone();
    std::thread::Builder::new()
        .name("folder-picker".into())
        .spawn(move || {
            let mut dialog = rfd::FileDialog::new();
            if let Some(dir) = start_dir {
                dialog = dialog.set_directory(dir);
            }
            let picked = dialog.pick_folder();
            let _ = tx.send(picked);
            ctx.request_repaint();
        })
        .expect("spawn folder-picker thread");
    rx
}

/// The working directory passed to the agent via `-w`/`--workdir`, if any.
fn args_workdir(args: &[String]) -> Option<&str> {
    let pos = args.iter().position(|a| a == "-w" || a == "--workdir")?;
    args.get(pos + 1).map(String::as_str)
}

/// Tab title: the workdir's basename when `-w <dir>` is present, else a number.
fn session_title(args: &[String], id: usize) -> String {
    if let Some(dir) = args_workdir(args)
        && let Some(name) = std::path::Path::new(dir).file_name()
    {
        return name.to_string_lossy().to_string();
    }
    format!("session {id}")
}

/// egui's bundled fonts have no CJK coverage and no emoji-presentation
/// symbols; pull in system fonts so session content renders and the resume
/// (book) button has a glyph. Probes well-known font locations across
/// platforms — Windows registry-less font dirs, Linux fontconfig staples,
/// macOS system fonts.
///
/// Returns how many fallback fonts were actually installed. Zero means the
/// system has none of the probed files (a fontless container, e.g. CI):
/// callers can't do better there, and glyph-coverage tests skip rather
/// than fail on what no installed font could cover anyway.
fn install_fallback_fonts(ctx: &egui::Context) -> usize {
    let mut fonts = egui::FontDefinitions::default();

    // First font that exists wins, per role.
    let cjk_candidates = [
        r"C:\Windows\Fonts\YuGothM.ttc",
        r"C:\Windows\Fonts\meiryo.ttc",
        r"C:\Windows\Fonts\msgothic.ttc",
        r"C:\Windows\Fonts\msyh.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
    ];
    // Monochrome symbol fallback for the 📖 glyph and the Kimi theme's moon
    // phases; egui's rasterizer cannot use color emoji fonts like Segoe UI
    // Emoji, and its bundled NotoEmoji subset lacks several of these.
    let symbol_candidates = [
        r"C:\Windows\Fonts\seguisym.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf",
        "/System/Library/Fonts/Apple Symbols.ttf",
        "/Library/Fonts/Arial Unicode.ttf",
    ];

    let mut installed = 0;
    for (name, candidates) in [
        ("cjk-fallback", &cjk_candidates[..]),
        ("symbol-fallback", &symbol_candidates[..]),
    ] {
        let Some(bytes) = candidates.iter().find_map(|p| std::fs::read(p).ok()) else {
            continue;
        };
        fonts.font_data.insert(
            name.to_owned(),
            std::sync::Arc::new(egui::FontData::from_owned(bytes)),
        );
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts
                .families
                .entry(family)
                .or_default()
                .push(name.to_owned());
        }
        installed += 1;
    }

    ctx.set_fonts(fonts);
    installed
}

#[cfg(test)]
mod tests {
    use super::install_fallback_fonts;
    use crate::theme::{MOON_PHASES, Theme};

    /// The glyphs the toolbar and the Kimi spinner are made of. Without a font
    /// behind them they do not fail loudly — they render as tofu boxes, which
    /// only a person looking at the window would notice.
    #[test]
    fn test_toolbar_and_spinner_glyphs_have_a_font() {
        let ctx = eframe::egui::Context::default();
        let installed = install_fallback_fonts(&ctx);
        // Fonts are built lazily, on the first pass.
        let _ = ctx.run(Default::default(), |_| {});

        let font = eframe::egui::FontId::proportional(14.0);
        let mut glyphs: Vec<&str> = MOON_PHASES.to_vec();
        glyphs.push("📖");
        for theme in [Theme::Light, Theme::Dark, Theme::Kimi] {
            glyphs.push(theme.glyph());
        }
        if installed == 0 {
            // A fontless system (headless CI container): no font could cover
            // these glyphs, which is an environment fact, not a regression.
            // The assertion still runs wherever fallback fonts exist.
            return;
        }
        for glyph in glyphs {
            assert!(
                ctx.fonts(|fonts| fonts.has_glyphs(&font, glyph)),
                "no installed font covers {glyph}"
            );
        }
    }
}
