//! The eframe application: a hub of sessions, each backed by its own
//! kimi-agent subprocess and shown as a top-level tab. A second tab layer
//! inside each session (main + subagent transcripts) lives in `session.rs`.

use eframe::egui::{self, Align2, Color32, Key, RichText};

use crate::session::Session;

/// Draft state for the "new session" popup.
#[derive(Default)]
struct NewSessionDraft {
    workdir: String,
    extra_args: String,
    error: Option<String>,
}

pub struct KimiGuiApp {
    agent_bin: String,
    sessions: Vec<Session>,
    active: usize,
    next_session_id: usize,
    new_session: Option<NewSessionDraft>,
}

impl KimiGuiApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        agent_bin: &str,
        agent_args: &[String],
    ) -> Result<Self, String> {
        install_cjk_fallback_fonts(&cc.egui_ctx);
        let mut app = Self {
            agent_bin: agent_bin.to_string(),
            sessions: Vec::new(),
            active: 0,
            next_session_id: 1,
            new_session: None,
        };
        app.open_session(agent_args.to_vec(), &cc.egui_ctx)?;
        Ok(app)
    }

    fn open_session(&mut self, args: Vec<String>, ctx: &egui::Context) -> Result<(), String> {
        let id = self.next_session_id;
        self.next_session_id += 1;
        let title = session_title(&args, id);
        let session = Session::spawn(id, title, &self.agent_bin, &args, ctx.clone())?;
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

    fn tab_strip(&mut self, ctx: &egui::Context) {
        let mut close: Option<usize> = None;
        egui::TopBottomPanel::top("session_tabs").show(ctx, |ui| {
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
                if ui.button("+").on_hover_text("new session").clicked()
                    && self.new_session.is_none()
                {
                    self.new_session = Some(NewSessionDraft::default());
                }
            });
        });
        if let Some(index) = close {
            self.close_session(index);
        }
    }

    fn new_session_popup(&mut self, ctx: &egui::Context) {
        if self.new_session.is_none() {
            return;
        }
        let mut create = false;
        let mut cancel = ctx.input(|i| i.key_pressed(Key::Escape));
        if let Some(draft) = &mut self.new_session {
            egui::Window::new("New session")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label("Working directory");
                    ui.add(
                        egui::TextEdit::singleline(&mut draft.workdir)
                            .desired_width(420.0)
                            .hint_text(r"e.g. C:\code\my-project (empty = current dir)"),
                    );
                    ui.add_space(4.0);
                    ui.label("Extra agent args");
                    ui.add(
                        egui::TextEdit::singleline(&mut draft.extra_args)
                            .desired_width(420.0)
                            .hint_text("e.g. --continue, --session <id>, -y"),
                    );
                    if let Some(error) = &draft.error {
                        ui.add_space(4.0);
                        ui.label(RichText::new(error).color(Color32::from_rgb(200, 80, 80)));
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("Create").clicked() {
                            create = true;
                        }
                        if ui.button("Cancel").clicked() {
                            cancel = true;
                        }
                    });
                });
        }
        if create {
            let draft = self.new_session.take().expect("checked above");
            let mut args = Vec::new();
            let workdir = draft.workdir.trim();
            if !workdir.is_empty() {
                args.push("-w".to_string());
                args.push(workdir.to_string());
            }
            args.extend(draft.extra_args.split_whitespace().map(String::from));
            if let Err(error) = self.open_session(args, ctx) {
                // Reopen the popup with the draft and the spawn error.
                self.new_session = Some(NewSessionDraft {
                    workdir: draft.workdir,
                    extra_args: draft.extra_args,
                    error: Some(error),
                });
            }
        } else if cancel {
            self.new_session = None;
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

        self.tab_strip(ctx);
        self.new_session_popup(ctx);

        if self.sessions.is_empty() {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.label(RichText::new("no sessions — press + to start one").weak());
                });
            });
            return;
        }

        let popup_open = self.new_session.is_some();
        self.sessions[self.active].ui(ctx, popup_open);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        for session in &mut self.sessions {
            session.shutdown();
        }
    }
}

/// Tab title: the workdir's basename when `-w <dir>` is present, else a number.
fn session_title(args: &[String], id: usize) -> String {
    if let Some(pos) = args.iter().position(|a| a == "-w" || a == "--workdir")
        && let Some(dir) = args.get(pos + 1)
        && let Some(name) = std::path::Path::new(dir).file_name()
    {
        return name.to_string_lossy().to_string();
    }
    format!("session {id}")
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
