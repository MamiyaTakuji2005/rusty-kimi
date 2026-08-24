//! The eframe application: a hub of sessions, each backed by its own
//! kimi-agent subprocess and shown as a top-level tab. A second tab layer
//! inside each session (main + subagent transcripts) lives in `session.rs`.
//!
//! Session creation lives in the tab strip: the `+` button opens the native
//! OS folder picker and instantly starts a session in the chosen directory,
//! while the book button pinned to the right edge opens the resume menu with
//! every past session found under `~/.kimi`.
//!
//! Everything here is also reachable from the keyboard alone — see
//! [`KimiGuiApp::handle_shortcuts`].

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError, channel};

use eframe::egui::{self, Align, Align2, Color32, Key, Modifiers, Popup, RichText};

use crate::session::Session;
use crate::session_list::{ResumeEntry, spawn_session_listing};

pub struct KimiGuiApp {
    agent_bin: String,
    sessions: Vec<Session>,
    active: usize,
    next_session_id: usize,
    /// In-flight native folder picker started by the `+` button.
    folder_pick: Option<Receiver<Option<PathBuf>>>,
    /// Sessions shown by the resume menu, newest first.
    resume_sessions: Vec<ResumeEntry>,
    /// In-flight resume listing (a result is pending).
    resume_listing: Option<Receiver<Result<Vec<ResumeEntry>, String>>>,
    /// Row highlighted in the resume menu, for arrow-key browsing.
    resume_cursor: usize,
    /// Bring the highlighted resume row into view on the next draw. Only set
    /// when the keyboard moved it, so it never fights the mouse wheel.
    resume_scroll: bool,
    /// Session id waiting on a close confirmation (`Ctrl+T` or a tab's ×).
    close_confirm: Option<usize>,
    /// Error message from a failed `Session::spawn`, shown in a small modal.
    spawn_error: Option<String>,
    /// Directory of the session most recently opened this run (any session
    /// carries its own `work_dir`; this covers the no-active-session case).
    last_workdir: Option<PathBuf>,
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
        agent_args: &[String],
    ) -> Result<Self, String> {
        install_fallback_fonts(&cc.egui_ctx);
        let mut app = Self {
            agent_bin: agent_bin.to_string(),
            sessions: Vec::new(),
            active: 0,
            next_session_id: 1,
            folder_pick: None,
            resume_sessions: Vec::new(),
            resume_listing: None,
            resume_cursor: 0,
            resume_scroll: false,
            close_confirm: None,
            spawn_error: None,
            last_workdir: None,
        };
        app.open_session(agent_args.to_vec(), &cc.egui_ctx, None)?;
        // List past sessions right away: it pre-warms the resume menu and
        // supplies the most recent session directory as the folder-picker
        // default after a GUI restart.
        app.resume_listing = Some(spawn_session_listing(&cc.egui_ctx));
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
        let mut session = Session::spawn(id, title, &self.agent_bin, &args, ctx.clone())?;
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

    /// App-wide keyboard shortcuts, so the whole hub works without a mouse:
    ///
    /// * `Tab` / `Ctrl+Tab` — next / previous session tab (row one), wrapping
    /// * `Shift+Tab` / `Ctrl+Shift+Tab` — next / previous fork tab (row two)
    /// * `Ctrl+N` — new session (opens the folder picker)
    /// * `Ctrl+O` — resume menu, browsed with `↑`/`↓`, `Enter` to open
    /// * `Ctrl+T` — close the active session, after a confirmation
    ///
    /// This runs before any widget is drawn and *consumes* the keys: the chat
    /// box holds focus permanently and would otherwise swallow `Tab` as an
    /// indent, `Enter` as a newline and the arrows as cursor movement.
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        // The spawn-error modal is answered before anything else.
        if self.spawn_error.is_some() {
            return;
        }
        if self.close_confirm.is_some() {
            self.close_confirm_keys(ctx);
            return;
        }
        if Popup::is_id_open(ctx, resume_popup_id()) {
            self.resume_menu_keys(ctx);
            return;
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

    /// Arrow-key browsing while the resume menu is open. `Escape` closes it —
    /// egui's popup handles that itself.
    ///
    /// Tab moves within the menu instead of switching sessions: it has to be
    /// swallowed here either way, or it reaches the still-focused chat box as
    /// an indent behind the menu.
    fn resume_menu_keys(&mut self, ctx: &egui::Context) {
        let (down, up, accept) = ctx.input_mut(|i| {
            // Shift+Tab before Tab; `consume_key` ignores an extra shift.
            let up = i.consume_key(Modifiers::SHIFT, Key::Tab)
                | i.consume_key(Modifiers::NONE, Key::ArrowUp);
            let down = i.consume_key(Modifiers::NONE, Key::Tab)
                | i.consume_key(Modifiers::NONE, Key::ArrowDown);
            let accept = i.consume_key(Modifiers::NONE, Key::Enter);
            (down, up, accept)
        });
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
            Popup::close_id(ctx, resume_popup_id());
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

    /// Show the resume menu and re-list what is on disk behind it.
    fn open_resume_menu(&mut self, ctx: &egui::Context) {
        Popup::open_id(ctx, resume_popup_id());
        self.resume_cursor = 0;
        self.resume_scroll = true;
        if self.resume_listing.is_none() {
            self.resume_listing = Some(spawn_session_listing(ctx));
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
            self.spawn_error = Some(error);
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
                    self.spawn_error = Some(error);
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
        egui::TopBottomPanel::top("session_tabs").show(ctx, |ui| {
            // Book button: resume menu, pinned to the right edge of the strip.
            let book = egui::SidePanel::right("tabs_right")
                .resizable(false)
                .exact_width(40.0)
                .show_inside(ui, |ui| {
                    ui.centered_and_justified(|ui| {
                        ui.button("📖").on_hover_text("resume a session (Ctrl+O)")
                    })
                    .inner
                })
                .inner;
            if book.clicked() {
                refresh_resume = true;
            }

            // Session tabs plus the `+` button on the left.
            ui.horizontal_wrapped(|ui| {
                for (index, session) in self.sessions.iter().enumerate() {
                    let mut text = RichText::new(&session.title);
                    if session.is_failed() {
                        text = text.color(Color32::from_rgb(200, 80, 80));
                    } else if session.has_pending_approvals() {
                        text = RichText::new(format!("⚠ {}", session.title))
                            .color(Color32::from_rgb(220, 160, 60));
                    } else if session.is_running() {
                        text = RichText::new(format!("▶ {}", session.title));
                    }
                    if ui.selectable_label(index == self.active, text).clicked() {
                        self.active = index;
                    }
                    if ui
                        .small_button(RichText::new("×").weak())
                        .on_hover_text("close session (Ctrl+T)")
                        .clicked()
                    {
                        close = Some(index);
                    }
                    ui.add_space(6.0);
                }
                if ui
                    .button("+")
                    .on_hover_text("new session, pick a folder (Ctrl+N)")
                    .clicked()
                {
                    pick_folder = true;
                }
            });

            self.resume_popup(ui, &book);
        });
        if let Some(index) = close {
            self.request_close(index);
        }
        if pick_folder {
            self.start_folder_pick(ctx);
        }
        if refresh_resume {
            // The click already toggled the popup open; this only refreshes
            // the listing and resets the keyboard cursor behind it.
            self.resume_cursor = 0;
            self.resume_scroll = true;
            if self.resume_listing.is_none() {
                self.resume_listing = Some(spawn_session_listing(ctx));
            }
        }
    }

    /// The resume menu below the book button, also opened by `Ctrl+O`.
    /// Clicking a row — or moving to it with the arrow keys and pressing
    /// Enter — resumes that session in a new tab.
    fn resume_popup(&mut self, ui: &mut egui::Ui, anchor: &egui::Response) {
        let mut resume: Option<ResumeEntry> = None;
        let cursor = self.resume_cursor;
        let want_scroll = self.resume_scroll;
        let mut scrolled = false;
        let sessions = &self.resume_sessions;
        let loading = self.resume_listing.is_some();
        // A fixed id, so `Ctrl+O` can open this without the button being hit.
        egui::Popup::menu(anchor)
            .id(resume_popup_id())
            .width(1380.0)
            .show(|ui| {
                if loading {
                    ui.horizontal(|ui| {
                        ui.spinner();
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
                    .max_height(1080.0)
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
                                scrolled = true;
                            }
                            ui.label(RichText::new(entry.meta_line()).weak().small());
                            ui.add_space(2.0);
                        }
                    });
            });
        if scrolled {
            self.resume_scroll = false;
        }
        if let Some(entry) = resume {
            self.resume_session(&entry, ui.ctx());
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
                            .color(Color32::from_rgb(220, 160, 60)),
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

    fn spawn_error_window(&mut self, ctx: &egui::Context) {
        if self.spawn_error.is_none() {
            return;
        }
        let mut close = false;
        egui::Window::new("Could not start session")
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(self.spawn_error.as_deref().unwrap_or_default())
                        .color(Color32::from_rgb(200, 80, 80)),
                );
                ui.add_space(6.0);
                if ui.button("OK").clicked() {
                    close = true;
                }
            });
        if close {
            self.spawn_error = None;
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
        self.close_confirm_window(ctx);
        self.spawn_error_window(ctx);

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

        // The session's own keys (Esc cancels, Enter sends) stay out of the
        // way while a modal or the resume menu is taking input.
        let popup_open = self.spawn_error.is_some()
            || self.close_confirm.is_some()
            || Popup::is_id_open(ctx, resume_popup_id());
        self.sessions[self.active].ui(ctx, popup_open);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        for session in &mut self.sessions {
            session.shutdown();
        }
    }
}

/// Id of the resume menu. Fixed rather than derived from the book button's
/// response, so `Ctrl+O` can open the same popup the button toggles.
fn resume_popup_id() -> egui::Id {
    egui::Id::new("resume_popup")
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
/// (book) button has a glyph.
fn install_fallback_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    // CJK fallback: first font that exists wins.
    let cjk_candidates = [
        r"C:\Windows\Fonts\YuGothM.ttc",
        r"C:\Windows\Fonts\meiryo.ttc",
        r"C:\Windows\Fonts\msgothic.ttc",
        r"C:\Windows\Fonts\msyh.ttc",
    ];
    for path in cjk_candidates {
        if let Ok(bytes) = std::fs::read(path) {
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
            break;
        }
    }

    // Monochrome symbol fallback (Segoe UI Symbol) for the 📖 glyph; egui's
    // rasterizer cannot use color emoji fonts like Segoe UI Emoji.
    if let Ok(bytes) = std::fs::read(r"C:\Windows\Fonts\seguisym.ttf") {
        fonts.font_data.insert(
            "symbol-fallback".to_owned(),
            std::sync::Arc::new(egui::FontData::from_owned(bytes)),
        );
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts
                .families
                .entry(family)
                .or_default()
                .push("symbol-fallback".to_owned());
        }
    }

    ctx.set_fonts(fonts);
}
