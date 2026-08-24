//! The eframe application: a hub of sessions, each backed by its own
//! kimi-agent subprocess and shown as a top-level tab. A second tab layer
//! inside each session (main + subagent transcripts) lives in `session.rs`.
//!
//! Session creation lives in the tab strip: the `+` button opens the native
//! OS folder picker and instantly starts a session in the chosen directory,
//! while the book button pinned to the right edge opens the resume menu with
//! every past session found under `~/.kimi`.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError, channel};

use eframe::egui::{self, Align2, Color32, RichText};

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
    /// Error message from a failed `Session::spawn`, shown in a small modal.
    spawn_error: Option<String>,
    /// Directory of the session most recently opened this run (any session
    /// carries its own `work_dir`; this covers the no-active-session case).
    last_workdir: Option<PathBuf>,
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
                        ui.button("📖").on_hover_text("resume a session")
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
                        .on_hover_text("close session")
                        .clicked()
                    {
                        close = Some(index);
                    }
                    ui.add_space(6.0);
                }
                if ui
                    .button("+")
                    .on_hover_text("new session (pick a folder)")
                    .clicked()
                {
                    pick_folder = true;
                }
            });

            self.resume_popup(ui, &book);
        });
        if let Some(index) = close {
            self.close_session(index);
        }
        if pick_folder && self.folder_pick.is_none() {
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
        if refresh_resume && self.resume_listing.is_none() {
            self.resume_listing = Some(spawn_session_listing(ctx));
        }
    }

    /// The resume menu below the book button. Clicking a row resumes that
    /// session as a new tab (`kimi-agent -w <dir> --session <id>`).
    fn resume_popup(&mut self, ui: &mut egui::Ui, anchor: &egui::Response) {
        let mut resume: Option<&ResumeEntry> = None;
        egui::Popup::menu(anchor).width(1380.0).show(|ui| {
            if self.resume_listing.is_some() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("loading sessions...");
                });
                return;
            }
            if self.resume_sessions.is_empty() {
                ui.label(RichText::new("no past sessions found").weak());
                return;
            }
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .max_height(1080.0)
                .show(ui, |ui| {
                    for entry in &self.resume_sessions {
                        if ui
                            .selectable_label(false, RichText::new(&entry.title).strong())
                            .on_hover_text(format!("resume {}", entry.id))
                            .clicked()
                        {
                            resume = Some(entry);
                        }
                        ui.label(RichText::new(entry.meta_line()).weak().small());
                        ui.add_space(2.0);
                    }
                });
        });
        let Some(entry) = resume else {
            return;
        };
        let args = vec![
            "-w".to_string(),
            entry.work_dir.to_string_lossy().into_owned(),
            "--session".to_string(),
            entry.id.clone(),
        ];
        let title = entry.tab_title();
        if let Err(error) = self.open_session(args, ui.ctx(), Some(title)) {
            self.spawn_error = Some(error);
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
        self.tab_strip(ctx);
        self.spawn_error_window(ctx);

        if self.sessions.is_empty() {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.label(
                        RichText::new(
                            "no sessions — press + to pick a folder, or 📖 to resume one",
                        )
                        .weak(),
                    );
                });
            });
            return;
        }

        let popup_open = self.spawn_error.is_some();
        self.sessions[self.active].ui(ctx, popup_open);
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
