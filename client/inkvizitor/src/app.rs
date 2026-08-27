//! The eframe application: a hub of sessions, each backed by its own
//! dvadva-agent subprocess and shown as a top-level tab. A second tab layer
//! inside each session (main + subagent transcripts) lives in `session.rs`.
//!
//! Session creation lives in the tab strip: the `+` button opens the native
//! OS folder picker and instantly starts a session in the chosen directory,
//! while the buttons pinned to the right edge open the resume menu with
//! every past session found under `~/.kimi`, manage the connections to the
//! configured remotes — one chain button per `[[remotes]]` entry, yellow
//! until its daemon answers, and a single placeholder chain when none is
//! configured — and cycle the theme.
//!
//! Sessions are **per-tab local or remote**: a tab either owns a
//! `dvadva-agent` child process here, or speaks to one through a `dvadva-bridge`
//! daemon on another machine ([`SessionTarget`]). `+` follows the active
//! tab's machine — a parallel session of what you are looking at — and each
//! connect button opens one on its own remote.
//!
//! The window can be **split** into panes ("Split right" / "Split down" in
//! the palette). A pane is a whole duplicate of the scene — its own tab
//! strip listing *every* session, its own active tab, its own scroll and chat
//! box — so a split is two views of one set of tabs, not two workspaces.
//! Only the overlays stay single: the palette, the resume list and the
//! confirmations are centered windows that act on the focused pane, since two
//! copies would fight over the same middle of the screen. `Alt+←/→` (or
//! `↑/↓`, along whichever axis the split runs) moves the focus, and so does
//! clicking into a pane.
//!
//! Everything here is also reachable from the keyboard alone — see
//! [`InkvizitorApp::handle_shortcuts`].

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError, channel};

use eframe::egui::{self, Align, Align2, Color32, Key, Modifiers, RichText};

use dvadva_agent::share::get_share_dir as share_dir;

use crate::palette::{Command, Palette};
use crate::remote_link::{LinkLight, RemoteLink};
use crate::session::{PaneSlot, Session};
use crate::theme::Theme;
use wire_client::launch::session_arg;
use wire_client::remotes;
use wire_client::session_list::{
    ResumeEntry, find_live_session, spawn_remote_session_listing, spawn_session_listing,
};

/// Which machine a session runs on. Sessions are per-tab, so both kinds live
/// in one window and every path in `args` means whatever it means *there*.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum SessionTarget {
    /// A `dvadva-agent` child process on this machine.
    #[default]
    Local,
    /// An agent behind a `dvadva-bridge` daemon: `(name, endpoint)`.
    Remote(String, String),
}

impl SessionTarget {
    /// The endpoint to talk to, for the listing and connect paths.
    fn endpoint(&self) -> Option<&str> {
        match self {
            Self::Local => None,
            Self::Remote(_, endpoint) => Some(endpoint),
        }
    }
}

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

/// One view of the hub: a tab strip listing every session, over the one it
/// currently has open. Panes are duplicates — two of them may well be
/// looking at the same tab — so only the *choice* of tab lives here.
struct Pane {
    active: usize,
}

/// Which way the panes divide the window. One axis for the whole window
/// rather than a tree of splits: two columns or two rows, three of either,
/// and nothing that needs a layout model to explain.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Split {
    Columns,
    Rows,
}

pub struct InkvizitorApp {
    agent_bin: String,
    /// One connection state per configured remote — a chain button each —
    /// plus an ad-hoc entry when `--remote host:port` named one outside the
    /// file. Empty means no remote is configured (or the config is broken —
    /// see `config_error`); the strip still paints one yellow chain to say
    /// so. Sessions are per-tab: these are what the remote commands open
    /// new tabs against, not a window-wide mode.
    links: Vec<RemoteLink>,
    /// Why `bridge.toml` could not be read, when it exists but is broken —
    /// the connect button is where the user learns of it.
    config_error: Option<String>,
    sessions: Vec<Session>,
    /// The panes, left to right or top to bottom. **Never empty** — closing
    /// the last one is refused, so `focused` always indexes something.
    panes: Vec<Pane>,
    /// Which way `panes` divides the window; meaningless while there is one.
    split: Split,
    /// The pane the keyboard and every overlay act on.
    focused: usize,
    next_session_id: usize,
    /// In-flight native folder picker started by the `+` button.
    folder_pick: Option<Receiver<Option<PathBuf>>>,
    /// Sessions shown by the resume menu, newest first.
    resume_sessions: Vec<ResumeEntry>,
    /// Which machine the current resume list came from, so resuming a row
    /// opens it where it actually lives.
    resume_source: SessionTarget,
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
    next_pane: bool,
    prev_pane: bool,
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

    /// The same for the panes of a split.
    fn pane_step(&self) -> Option<bool> {
        (self.next_pane || self.prev_pane).then_some(self.next_pane)
    }
}

impl InkvizitorApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        agent_bin: &str,
        remote: Option<String>,
        agent_args: &[String],
    ) -> Result<Self, String> {
        install_fallback_fonts(&cc.egui_ctx);
        let theme = Theme::load();
        theme.apply(&cc.egui_ctx);

        // `--remote` names the remote the first tab opens on; without it
        // every configured remote still gets a button, just no session yet.
        // A broken bridge.toml must not take the GUI down: the connect
        // button is always there, and it is where the error surfaces.
        let config_error;
        let configured = match remotes::load() {
            Ok(remotes) => {
                config_error = None;
                remotes
            }
            Err(error) if remote.is_none() => {
                config_error = Some(error);
                Vec::new()
            }
            // A remote was explicitly asked for: the broken file is the
            // reason it cannot be had, so it is the error to fail with.
            Err(error) => return Err(error),
        };
        // Every configured remote gets a link (and its chain button); the
        // one `--remote` named — possibly an ad-hoc `host:port` outside the
        // file — is where the first tab opens, so that connection is wanted:
        // its light starts yellow and goes green on its own.
        let mut links: Vec<RemoteLink> = configured.iter().cloned().map(RemoteLink::new).collect();
        let first_target = match &remote {
            Some(spec) => {
                let chosen = remotes::resolve(spec, &configured)?;
                let index = links
                    .iter()
                    .position(|link| link.remote().name == chosen.name)
                    .unwrap_or_else(|| {
                        links.push(RemoteLink::new(chosen.clone()));
                        links.len() - 1
                    });
                links[index].connect();
                SessionTarget::Remote(chosen.name.clone(), chosen.endpoint.clone())
            }
            None => SessionTarget::Local,
        };

        let mut app = Self {
            agent_bin: agent_bin.to_string(),
            links,
            config_error,
            sessions: Vec::new(),
            panes: vec![Pane { active: 0 }],
            split: Split::Columns,
            focused: 0,
            next_session_id: 1,
            folder_pick: None,
            resume_sessions: Vec::new(),
            resume_source: SessionTarget::Local,
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
        app.open_session(agent_args.to_vec(), &cc.egui_ctx, None, &first_target)?;
        // List past sessions right away: it pre-warms the resume menu and
        // supplies the most recent session directory as the folder-picker
        // default after a GUI restart. Sessions live on the machine that ran
        // them, so the listing follows the first tab.
        app.start_resume_listing(&cc.egui_ctx, first_target);
        Ok(app)
    }

    /// Ask one machine for its past sessions, remembering which machine
    /// answered so a resumed row opens where it actually lives.
    fn start_resume_listing(&mut self, ctx: &egui::Context, target: SessionTarget) {
        let egui_ctx = ctx.clone();
        self.resume_listing = Some(match target.endpoint() {
            Some(endpoint) => {
                spawn_remote_session_listing(endpoint, move || egui_ctx.request_repaint())
            }
            None => spawn_session_listing(move || egui_ctx.request_repaint()),
        });
        self.resume_source = target;
    }

    /// The focused pane's active tab — what "the session" means to every
    /// command that does not name one.
    fn active(&self) -> usize {
        self.panes[self.focused].active
    }

    /// Panes sharing the window's *width*: all of them when the split runs in
    /// columns, one otherwise. The transcript's wrap floor needs it — see
    /// `session::wrap_width`.
    fn columns(&self) -> usize {
        match self.split {
            Split::Columns => self.panes.len(),
            Split::Rows => 1,
        }
    }

    /// Add a pane beside (or below) the focused one, showing what it shows,
    /// and move the focus into it. The new pane is a second view, not a
    /// second workspace: same tabs, same one selected, its own scroll
    /// position and chat box.
    fn split_pane(&mut self, axis: Split) {
        let active = self.active();
        self.split = axis;
        self.panes.insert(self.focused + 1, Pane { active });
        self.focused += 1;
    }

    /// Drop the focused pane. The last one is the window itself and stays.
    fn close_split(&mut self) {
        if self.panes.len() == 1 {
            self.error = Some("this is the only pane — nothing to close".into());
            return;
        }
        self.panes.remove(self.focused);
        self.focused = self.focused.min(self.panes.len() - 1);
    }

    /// The machine the active tab runs on. `+` and the resume menu follow it,
    /// so both mean "more of what I am looking at".
    fn active_target(&self) -> SessionTarget {
        match self
            .sessions
            .get(self.active())
            .and_then(|s| s.remote.clone())
        {
            Some((name, endpoint)) => SessionTarget::Remote(name, endpoint),
            None => SessionTarget::Local,
        }
    }

    fn open_session(
        &mut self,
        args: Vec<String>,
        ctx: &egui::Context,
        title: Option<String>,
        target: &SessionTarget,
    ) -> Result<(), String> {
        self.open_session_for(args, ctx, title, target, None)
    }

    /// As [`Self::open_session`], naming the session to rejoin.
    ///
    /// `session` is the *attach key*, and separate from any `--session` in
    /// `args` on purpose: one says which live agent to look for, the other is
    /// what a cold start should be told to resume. A resumed row passes both,
    /// which is the same thing the daemon's own contract asks for.
    fn open_session_for(
        &mut self,
        args: Vec<String>,
        ctx: &egui::Context,
        title: Option<String>,
        target: &SessionTarget,
        session: Option<&str>,
    ) -> Result<(), String> {
        // Only an explicit `-w` counts as a chosen directory; a session
        // launched without one runs in the GUI's incidental cwd, which must
        // not shadow the newest-on-disk default in the folder picker.
        let work_dir = args_workdir(&args).map(PathBuf::from);
        // A `--session` in the args is also an attach key, even when the
        // caller did not think of it as one — the command line this window
        // was started with, for instance. Without this, `--remote vps
        // --session abc` would start a *second* agent on a session that
        // already has one.
        let from_args = session_arg(&args);
        let session = session.or(from_args.as_deref());
        let id = self.next_session_id;
        self.next_session_id += 1;
        let title = title.unwrap_or_else(|| session_title(&args, id, target));
        let mut session = match target {
            // Paths in `args` (e.g. `-w`) resolve on the remote machine, and
            // a remote session naming none gets the daemon's default.
            SessionTarget::Remote(name, endpoint) => {
                Session::connect(id, title, name, endpoint, session, &args, ctx.clone())?
            }
            // A live local session is joined, never re-spawned: two agents
            // on one session's files would be two writers of one transcript.
            SessionTarget::Local => match session.and_then(find_live_session) {
                Some(entry) => Session::join_local(id, title, &entry, ctx.clone())?,
                None => Session::spawn(id, title, &self.agent_bin, &args, ctx.clone())?,
            },
        };
        session.work_dir = work_dir.clone();
        // Only a local directory belongs in the folder picker's memory: a
        // remote path does not exist on this machine.
        if matches!(target, SessionTarget::Local) {
            self.last_workdir = work_dir.or(self.last_workdir.take());
        }
        self.sessions.push(session);
        // A new tab opens in the pane that asked for it, and nowhere else.
        self.panes[self.focused].active = self.sessions.len() - 1;
        Ok(())
    }

    /// The link a remote command should act on: the named one, else the
    /// default. `Err` is the user-facing explanation — an unknown name
    /// lists what *is* configured, the way the TUI's `--remote` does.
    fn target_link(&self, name: Option<&str>) -> Result<usize, String> {
        pick_link(&self.links, name).map_err(|unknown| match unknown {
            Some(name) => {
                let known: Vec<&str> = self
                    .links
                    .iter()
                    .map(|link| link.remote().name.as_str())
                    .collect();
                format!(
                    "`{name}` is not one of the configured remotes ({})",
                    known.join(", ")
                )
            }
            None => self.unconfigured_message(),
        })
    }

    /// The target for a link whose daemon has answered, or `None` with the
    /// not-yet explanation posted — shared by everything that needs a
    /// green light to mean anything.
    fn connected_target(&mut self, index: usize) -> Option<SessionTarget> {
        let link = &self.links[index];
        let remote = link.remote();
        if link.light() != LinkLight::Connected {
            self.error = Some(format!(
                "{} ({}) has not answered yet.\n\n\
                 Connect first (chain button or \"Connect to remote\"), then \
                 retry once the light turns green.",
                remote.name, remote.endpoint
            ));
            return None;
        }
        Some(SessionTarget::Remote(
            remote.name.clone(),
            remote.endpoint.clone(),
        ))
    }

    /// Open a session on a remote — what its connect button does once it
    /// is green. The palette's "New remote session" shares it, so both
    /// answer for the states where there is nothing to open.
    fn open_remote_session(&mut self, ctx: &egui::Context, name: Option<&str>) {
        match self.target_link(name) {
            Ok(index) => self.open_remote_session_at(index, ctx),
            Err(message) => self.error = Some(message),
        }
    }

    fn open_remote_session_at(&mut self, index: usize, ctx: &egui::Context) {
        let Some(target) = self.connected_target(index) else {
            return;
        };
        // No args: the daemon supplies the work directory, because this
        // machine cannot name a path that exists on that one.
        if let Err(error) = self.open_session(Vec::new(), ctx, None, &target) {
            self.error = Some(error);
        }
    }

    /// What a click on the connect button means with no remote behind it:
    /// not a dead end, but a pointer to the file that names one.
    fn unconfigured_message(&self) -> String {
        match &self.config_error {
            Some(err) => format!("bridge config is broken:\n\n{err}"),
            None => format!(
                "no remote is configured.\n\nAdd one to {}:\n\n{}",
                remotes::path().display(),
                remotes_skeleton()
            ),
        }
    }

    /// What the chain buttons and the palette both mean by "connect": green
    /// opens a remote session tab, anything else starts the connection (or
    /// retries it now), and nothing configured explains what the file wants.
    fn connect_remote(&mut self, ctx: &egui::Context, name: Option<&str>) {
        match self.target_link(name) {
            Ok(index) => self.connect_remote_at(index, ctx),
            Err(message) => self.error = Some(message),
        }
    }

    fn connect_remote_at(&mut self, index: usize, ctx: &egui::Context) {
        if self.links[index].light() == LinkLight::Connected {
            self.open_remote_session_at(index, ctx);
        } else {
            self.links[index].connect();
        }
    }

    /// Open `bridge.toml` in the default editor. Creating it is the user's
    /// call, so a missing file gets the skeleton and the path it belongs at
    /// rather than an empty editor window that silently does nothing.
    fn open_bridge_config(&mut self) {
        let path = remotes::path();
        if path.exists() {
            self.open_path(path);
        } else {
            self.error = Some(format!(
                "{} does not exist yet.\n\nCreate it with a remote:\n\n{}",
                path.display(),
                remotes_skeleton()
            ));
        }
    }

    fn close_session(&mut self, index: usize) {
        let mut session = self.sessions.remove(index);
        session.shutdown();
        // Every pane indexes the same list, so they all shift — not just the
        // one the close came from.
        let len = self.sessions.len();
        for pane in &mut self.panes {
            pane.active = shift_active(pane.active, index, len);
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
    /// * `Alt+←`/`Alt+→` (or `↑`/`↓`) — focus the previous / next pane of a
    ///   split, along whichever axis it runs
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

        // Read before the closure: which arrows move the focus depends on
        // which way the window is divided.
        let (back, forward) = match self.split {
            Split::Columns => (Key::ArrowLeft, Key::ArrowRight),
            Split::Rows => (Key::ArrowUp, Key::ArrowDown),
        };
        let split = self.panes.len() > 1;
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
                // Alt, not Shift: these keys are consumed before any widget
                // sees them, and Shift+arrow is how text is selected in the
                // chat box that holds focus permanently. Short-circuited on
                // `split`, so an undivided window consumes nothing new and
                // behaves exactly as it did.
                prev_pane: split && i.consume_key(Modifiers::ALT, back),
                next_pane: split && i.consume_key(Modifiers::ALT, forward),
            }
        });

        if let Some(forward) = keys.session_step()
            && !self.sessions.is_empty()
        {
            let len = self.sessions.len();
            let step = if forward { 1 } else { len - 1 };
            let active = &mut self.panes[self.focused].active;
            *active = (*active + step) % len;
        }
        if let Some(forward) = keys.pane_step() {
            let len = self.panes.len();
            let step = if forward { 1 } else { len - 1 };
            self.focused = (self.focused + step) % len;
        }
        if let Some(forward) = keys.subtab_step() {
            // Resolved after the step above, which may just have moved it.
            let active = self.active();
            if let Some(session) = self.sessions.get_mut(active) {
                session.cycle_subtab(forward);
            }
        }
        if keys.new_session {
            self.start_folder_pick(ctx);
        }
        if keys.resume {
            self.open_resume_menu(ctx);
        }
        if keys.close {
            self.request_close(self.active());
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
        if accept && let Some(m) = matches.get(self.palette.cursor) {
            let command = m.entry.command;
            let arg = m.arg.clone();
            self.palette.close();
            self.run_command(command, arg.as_deref(), ctx);
        }
    }

    /// Carry out one palette command. Adding a feature to the palette is a row
    /// in `palette::COMMANDS` plus an arm here. `arg` is the trailing remote
    /// name the palette peeled off, for the commands that take one; a bare
    /// command means the default remote.
    fn run_command(&mut self, command: Command, arg: Option<&str>, ctx: &egui::Context) {
        match command {
            Command::NewSession => self.start_folder_pick(ctx),
            Command::ResumeSession => self.open_resume_menu(ctx),
            Command::CloseSession => self.request_close(self.active()),
            Command::StopAgent => {
                if let Some(id) = self.sessions.get(self.active()).map(|session| session.id) {
                    self.stop_agent(id);
                }
            }
            Command::SplitRight => self.split_pane(Split::Columns),
            Command::SplitDown => self.split_pane(Split::Rows),
            Command::CloseSplit => self.close_split(),
            Command::ConnectRemote => self.connect_remote(ctx, arg),
            Command::NewRemoteSession => self.open_remote_session(ctx, arg),
            Command::OpenRemoteSession => self.open_remote_resume_menu(ctx, arg),
            Command::CycleTheme => self.cycle_theme(ctx),
            Command::OpenConfig => self.open_path(dvadva_agent::config::get_config_file()),
            Command::OpenMcpConfig => {
                self.open_path(dvadva_agent::mcp::get_global_mcp_config_file());
            }
            Command::OpenBridgeConfig => self.open_bridge_config(),
            Command::OpenLogFolder => self.open_path(share_dir().join("logs")),
            Command::OpenShareFolder => self.open_path(share_dir()),
            Command::OpenWorkDir => {
                // The tab may be running in the GUI's own cwd, with no `-w`.
                let dir = self
                    .sessions
                    .get(self.active())
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

    /// Start a session on the active tab's machine — a parallel session of
    /// what you are looking at.
    ///
    /// Locally that means the folder picker. On a remote there is nothing to
    /// pick: this machine's directories do not exist over there, and a
    /// Windows path sent to a Linux box only produces a session that dies on
    /// startup. The daemon supplies its own default instead, so a new remote
    /// tab simply asks for one.
    fn start_folder_pick(&mut self, ctx: &egui::Context) {
        let target = self.active_target();
        if target != SessionTarget::Local {
            if let Err(error) = self.open_session(Vec::new(), ctx, None, &target) {
                self.error = Some(error);
            }
            return;
        }
        if self.folder_pick.is_some() {
            return;
        }
        // Open the picker where a parallel session is one Enter away:
        // the active tab's directory, else the most recently opened one
        // this run, else the newest past session on disk, else cwd.
        let start = self
            .sessions
            .get(self.active())
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
        let target = self.active_target();
        if target != self.resume_source {
            self.resume_sessions.clear();
            self.resume_listing = None;
        }
        if self.resume_listing.is_none() {
            self.start_resume_listing(ctx, target);
        }
        // The list itself is shown regardless of the listing's state; the
        // window renders "loading" while a result is in flight.
        self.show_resume_list();
    }

    /// Show the resume list pointed at a remote's machine — the palette's
    /// "Open remote session". Unlike `Ctrl+O` this does not follow the
    /// active tab: the machine is the point, so it is named in the error
    /// when there is nothing to point at yet.
    fn open_remote_resume_menu(&mut self, ctx: &egui::Context, name: Option<&str>) {
        let index = match self.target_link(name) {
            Ok(index) => index,
            Err(message) => {
                self.error = Some(message);
                return;
            }
        };
        let Some(target) = self.connected_target(index) else {
            return;
        };
        if target != self.resume_source {
            self.resume_sessions.clear();
            self.resume_listing = None;
        }
        if self.resume_listing.is_none() {
            self.start_resume_listing(ctx, target);
        }
        self.show_resume_list();
    }

    /// Open the resume overlay. Callers arrange the listing first.
    fn show_resume_list(&mut self) {
        self.resume_open = true;
        self.resume_cursor = 0;
        self.resume_scroll = true;
    }

    /// Open a past session in a new tab (`dvadva-agent -w <dir> --session <id>`)
    /// on whichever machine the listing came from — its `work_dir` only means
    /// anything there.
    fn resume_session(&mut self, entry: &ResumeEntry, ctx: &egui::Context) {
        let args = vec![
            "-w".to_string(),
            entry.work_dir.to_string_lossy().into_owned(),
            "--session".to_string(),
            entry.id.clone(),
        ];
        let target = self.resume_source.clone();
        if let Err(error) =
            self.open_session_for(args, ctx, Some(entry.tab_title()), &target, Some(&entry.id))
        {
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
                if let Err(error) = self.open_session(args, ctx, None, &SessionTarget::Local) {
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

    /// The strip at the top of one pane. Every pane lists every session —
    /// the tabs are the window's, only the selection is the pane's — and
    /// carries its own copy of the buttons, so no pane is the one that owns
    /// the toolbar.
    fn tab_strip(&mut self, pane: usize, ui: &mut egui::Ui) {
        // The panel below borrows `ui`; the context is an `Arc` handle, so
        // the clone that outlives the borrow is a pointer copy.
        let ctx = ui.ctx().clone();
        let mut close: Option<usize> = None;
        let mut pick_folder = false;
        let mut refresh_resume = false;
        let mut cycle_theme = false;
        let mut link_click = None;
        let theme = self.theme;
        let colors = theme.colors();
        let bar = crate::theme::SESSION_BAR;
        egui::TopBottomPanel::top(egui::Id::new(("session_tabs", pane))).show_inside(ui, |ui| {
            // One set of metrics for the whole strip: the tabs, their ×, the
            // `+`, and the two buttons in the panel below all inherit it.
            bar.apply(ui);
            // Resume and theme, pinned to the right edge of the strip. The
            // frame is replaced with a bare horizontal margin — a nested panel
            // brings its own vertical padding, which stacks on the strip's own
            // and puts the buttons in a band half again their height.
            // One chain button per configured remote — and a single
            // placeholder chain when none is configured (or the config is
            // broken), so the strip always says how this machine reaches
            // elsewhere and where to fix it when it cannot.
            let lights: Vec<(LinkLight, String)> = if self.links.is_empty() {
                let hover = match &self.config_error {
                    Some(err) => format!("bridge config broken:\n{err}\nclick for details"),
                    None => format!(
                        "no remote configured\nclick for how to add one to {}",
                        remotes::path().display()
                    ),
                };
                vec![(LinkLight::Trying, hover)]
            } else {
                self.links
                    .iter()
                    .map(|link| (link.light(), link.hover_text()))
                    .collect()
            };
            // Sized to its buttons — book + chains + theme — their gaps and
            // margins, plus the slack the old fixed 99 carried for three.
            let buttons = 2 + lights.len();
            let width = 15.0 + buttons as f32 * bar.height + (buttons - 1) as f32 * bar.spacing;
            let (book, link_hits, paint) =
                egui::SidePanel::right(egui::Id::new(("tabs_right", pane)))
                    .resizable(false)
                    .exact_width(width)
                    .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(6, 0)))
                    .show_inside(ui, |ui| {
                        ui.horizontal(|ui| {
                            let book = bar
                                .square(ui, "📖")
                                .on_hover_text("resume a session (Ctrl+O)");
                            let mut hits = None;
                            for (index, (light, hover)) in lights.iter().enumerate() {
                                let link =
                                    crate::remote_link::link_button(bar, ui, *light, &colors)
                                        .on_hover_text(hover);
                                if link.clicked() {
                                    hits = Some((index, true));
                                } else if link.secondary_clicked() {
                                    hits = Some((index, false));
                                }
                            }
                            let paint = bar.square(ui, theme.glyph()).on_hover_text(theme.hover());
                            (book, hits, paint)
                        })
                        .inner
                    })
                    .inner;
            if book.clicked() {
                refresh_resume = true;
            }
            link_click = link_hits;
            if paint.clicked() {
                cycle_theme = true;
            }

            // Session tabs plus the `+` button on the left.
            ui.horizontal_wrapped(|ui| {
                for (index, session) in self.sessions.iter().enumerate() {
                    let mut text = RichText::new(&session.title);
                    if session.is_failed() {
                        text = text.color(colors.error);
                    } else if session.is_detached() {
                        // Not red: the agent is probably still there, and
                        // this tab is on its way back to it.
                        text = RichText::new(format!("⇄ {}", session.title)).color(colors.warning);
                    } else if session.has_pending_approvals() {
                        text = RichText::new(format!("⚠ {}", session.title)).color(colors.warning);
                    } else if session.is_running() {
                        text = RichText::new(format!("▶ {}", session.title));
                    }
                    let (tab, close_button) =
                        bar.tab_with_close(ui, index == self.panes[pane].active, text, &colors);
                    if tab.clicked() {
                        self.panes[pane].active = index;
                        self.focused = pane;
                    }
                    if close_button
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
        // A click on this strip is a click in this pane: the actions below
        // run on it, not on whichever pane the keyboard happened to have.
        if close.is_some() || pick_folder || refresh_resume || link_click.is_some() {
            self.focused = pane;
        }
        if let Some(index) = close {
            self.request_close(index);
        }
        if pick_folder {
            self.start_folder_pick(&ctx);
        }
        if refresh_resume {
            if self.resume_open {
                self.resume_open = false;
            } else {
                self.open_resume_menu(&ctx);
            }
        }
        if cycle_theme {
            self.cycle_theme(&ctx);
        }
        match link_click {
            // Green means there is something to open; anything else means
            // the user wants the connection to start (or to retry now). The
            // placeholder chain has no link behind it — its click explains
            // how to configure one.
            Some((_, true)) if self.links.is_empty() => {
                self.error = Some(self.unconfigured_message());
            }
            Some((index, true)) => self.connect_remote_at(index, &ctx),
            Some((index, false)) => {
                if let Some(link) = self.links.get_mut(index) {
                    link.disconnect();
                }
            }
            None => {}
        }
    }

    /// Lay the panes out and draw each one.
    ///
    /// One pane is the whole window and takes the central panel it always
    /// took, so an unsplit window is pixel-for-pixel what it was. More than
    /// one and every pane but the last claims a resizable panel first —
    /// which is where the draggable divider between them comes from.
    fn panes_ui(&mut self, ctx: &egui::Context) {
        let axis = self.split;
        let count = self.panes.len();
        let screen = ctx.screen_rect();
        // The panes carry no frame of their own: the tab strip and the
        // session's panels bring the margins, and a second set here would
        // inset every pane away from its own divider.
        let frame = egui::Frame::new().fill(ctx.style().visuals.panel_fill);
        for index in 0..count - 1 {
            let id = pane_id(axis, count, index);
            match axis {
                Split::Columns => {
                    egui::SidePanel::left(id)
                        .resizable(true)
                        .frame(frame)
                        .default_width(screen.width() / count as f32)
                        .show(ctx, |ui| self.pane_ui(index, ui));
                }
                Split::Rows => {
                    egui::TopBottomPanel::top(id)
                        .resizable(true)
                        .frame(frame)
                        .default_height(screen.height() / count as f32)
                        .show(ctx, |ui| self.pane_ui(index, ui));
                }
            }
        }
        egui::CentralPanel::default()
            .frame(frame)
            .show(ctx, |ui| self.pane_ui(count - 1, ui));
    }

    /// One pane: its own tab strip, then the tab it has open.
    fn pane_ui(&mut self, pane: usize, ui: &mut egui::Ui) {
        // Before the strip claims its share, so the ring below frames the
        // whole pane rather than what is left of it.
        let rect = ui.max_rect();
        self.tab_strip(pane, ui);
        if self.sessions.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label(
                    RichText::new("no sessions — Ctrl+N to pick a folder, Ctrl+O to resume one")
                        .weak(),
                );
            });
        } else {
            // The session's own keys (Esc cancels, Enter sends) and its habit
            // of grabbing focus belong to one pane at a time — and stay out
            // of the way entirely while anything is over it.
            let focused = pane == self.focused;
            let suppress = !focused || self.focus_owner() != FocusOwner::Session;
            let slot = PaneSlot {
                index: pane,
                columns: self.columns(),
                focused,
            };
            let active = self.panes[pane].active;
            self.sessions[active].ui(ui, slot, suppress);
        }
        // Which pane the keyboard, the palette and the next new tab act on
        // has to be visible, so the focused one wears a thin accent ring; a
        // click is what moves it, the way it moves between an editor's panes.
        // Neither exists in an unsplit window — there is nothing to say.
        if self.panes.len() > 1 {
            if pane == self.focused {
                ui.painter().rect_stroke(
                    rect,
                    0.0,
                    egui::Stroke::new(1.0, self.theme.colors().accent),
                    egui::StrokeKind::Inside,
                );
            } else if ui.rect_contains_pointer(rect) && ui.ctx().input(|i| i.pointer.any_pressed())
            {
                self.focused = pane;
            }
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
                        let colors = crate::theme::colors(ui.ctx());
                        for (index, entry) in sessions.iter().enumerate() {
                            let selected = index == cursor;
                            // A live row is joined, not resumed, and the two
                            // are worth telling apart before clicking: one
                            // reaches a process that has been thinking since
                            // you left, the other reads a file.
                            let label = if entry.live {
                                RichText::new(format!("● {}", entry.title))
                                    .strong()
                                    .color(colors.accent)
                            } else {
                                RichText::new(&entry.title).strong()
                            };
                            let hint = if entry.live {
                                format!("join the running agent for {}", entry.id)
                            } else {
                                format!("resume {}", entry.id)
                            };
                            let row = ui.selectable_label(selected, label).on_hover_text(hint);
                            if row.clicked() {
                                resume = Some(entry.clone());
                            }
                            if selected && want_scroll {
                                row.scroll_to_me(Some(Align::Center));
                            }
                            let meta = match entry.live {
                                true => format!("{} · running", entry.meta_line()),
                                false => entry.meta_line(),
                            };
                            ui.label(RichText::new(meta).weak().small());
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
        let detaches = session.outlives_its_tab();
        let (mut confirm, mut cancel, mut stop) = (false, false, false);
        egui::Window::new("Close session")
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(RichText::new(format!("Close “{title}”?")).strong());
                // Closing means two different things now, and the difference
                // is the whole of this project: an attached tab is a window
                // onto an agent it does not own, so closing it is leaving.
                if detaches {
                    ui.label("The agent keeps running; reopening this session rejoins it.");
                    if running {
                        ui.label(
                            RichText::new("The turn in progress carries on without you.")
                                .color(crate::theme::colors(ui.ctx()).warning),
                        );
                    }
                } else {
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
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let close_label = if detaches {
                        "Detach (Enter)"
                    } else {
                        "Close (Enter)"
                    };
                    if ui.button(close_label).clicked() {
                        confirm = true;
                    }
                    if detaches
                        && ui
                            .button("Stop the agent")
                            .on_hover_text("end the agent process too, not just this window")
                            .clicked()
                    {
                        stop = true;
                    }
                    if ui.button("Cancel (Esc)").clicked() {
                        cancel = true;
                    }
                });
            });
        if stop {
            self.stop_agent(id);
        } else if confirm {
            self.confirm_close();
        } else if cancel {
            self.close_confirm = None;
        }
    }

    /// End the agent behind a tab, then close the tab.
    ///
    /// The one thing a detached session cannot be talked out of by dropping a
    /// socket. Only meaningful for an attached tab; a local child is stopped
    /// by closing it, which is what `close_session` already does.
    fn stop_agent(&mut self, id: usize) {
        if let Some(session) = self.sessions.iter_mut().find(|session| session.id == id) {
            session.stop_agent();
        }
        self.close_confirm = None;
        if let Some(index) = self.sessions.iter().position(|session| session.id == id) {
            self.close_session(index);
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
        let accent = self.theme.colors().accent;
        let mut chosen: Option<(Command, Option<String>)> = None;
        let mut hovered: Option<usize> = None;
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
                        for (index, m) in matches.iter().enumerate() {
                            let selected = index == cursor;
                            let mut title =
                                highlighted_title(ui, m.entry.title, &m.positions, accent);
                            // The peeled-off remote name rides along in the
                            // row, so what Enter would act on is visible
                            // before it is pressed.
                            if let Some(arg) = &m.arg {
                                title.append(
                                    &format!(" {arg}"),
                                    0.0,
                                    egui::TextFormat {
                                        font_id: egui::TextStyle::Button.resolve(ui.style()),
                                        color: accent,
                                        ..Default::default()
                                    },
                                );
                            }
                            let row = ui.selectable_label(selected, title);
                            if row.clicked() {
                                chosen = Some((m.entry.command, m.arg.clone()));
                            }
                            // The keyboard cursor follows the mouse, the way
                            // it does in Sublime's palette: whichever row you
                            // are pointing at is the one Enter would take.
                            if row.hovered() {
                                hovered = Some(index);
                            }
                            if selected && want_scroll {
                                row.scroll_to_me(Some(Align::Center));
                            }
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(m.entry.detail).weak().small());
                                if let Some(binding) = m.entry.binding {
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
        if let Some(index) = hovered {
            self.palette.cursor = index;
        }
        if let Some((command, arg)) = chosen {
            self.palette.close();
            self.run_command(command, arg.as_deref(), ctx);
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

impl eframe::App for InkvizitorApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Background sessions keep progressing even while not visible — and
        // a background session whose connection dropped keeps trying to get
        // back, so it is there when the tab is looked at.
        for session in &mut self.sessions {
            session.poll(ctx);
        }
        if self.sessions.iter().any(Session::is_running) {
            // Keep spinners animated.
            ctx.request_repaint_after(std::time::Duration::from_millis(120));
        }

        self.poll_folder_pick(ctx);
        self.poll_resume_listing();
        for link in &mut self.links {
            let repaint_ctx = ctx.clone();
            link.poll(move || repaint_ctx.request_repaint());
            if let Some(delay) = link.repaint_delay() {
                // Come back when the next probe is actually due, so the light
                // still updates on its own without the window busy-redrawing
                // between probes. A probe already out wakes us itself.
                ctx.request_repaint_after(delay);
            }
        }
        // Before any widget: the shortcuts take the keys they need out of the
        // event queue, ahead of the always-focused chat box.
        self.handle_shortcuts(ctx);
        // The overlays are windows and float above the panes regardless of
        // order; declaring them first keeps `focus_owner` answering the same
        // question for every pane the panes below then ask it.
        self.resume_window(ctx);
        self.palette_window(ctx);
        self.close_confirm_window(ctx);
        self.error_window(ctx);
        self.panes_ui(ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        for session in &mut self.sessions {
            session.shutdown();
        }
    }
}

/// The id egui files a pane's divider position under.
///
/// **The axis has to be part of it.** egui keeps one `PanelState` per id and
/// reads a *width* out of it for a side panel but a *height* for a top one,
/// so an id shared between the two hands a row split the full-window height
/// its column incarnation stored: the top pane claims everything, the pane
/// below it gets nothing, and the split appears to do nothing at all — then
/// does the same in mirror image the next time the window is split the other
/// way.
///
/// The count is in there for a different reason: each layout then remembers
/// its own dividers, so a third pane divides the window evenly instead of
/// squeezing itself into whatever the first two left over — and closing it
/// again brings the old divider back.
fn pane_id(axis: Split, count: usize, index: usize) -> egui::Id {
    let axis = match axis {
        Split::Columns => "col",
        Split::Rows => "row",
    };
    egui::Id::new(("pane", axis, count, index))
}

/// Where a pane's active tab lands once the tab at `removed` is closed: the
/// tabs after it shift down one, and a pane left pointing past the end falls
/// back to the last tab there is.
fn shift_active(active: usize, removed: usize, len: usize) -> usize {
    let active = if active > removed { active - 1 } else { active };
    active.min(len.saturating_sub(1))
}

/// Which of `links` a remote command means: the named one, else the default
/// — the entry marked `default` in `bridge.toml`, else the first, the same
/// rule as `remotes::default_remote`. `Err(Some(name))` is an unknown name,
/// `Err(None)` no remotes at all; the caller owns the wording of both.
fn pick_link(links: &[RemoteLink], name: Option<&str>) -> Result<usize, Option<String>> {
    if links.is_empty() {
        return Err(None);
    }
    match name {
        Some(name) => links
            .iter()
            .position(|link| link.remote().name == name)
            .ok_or_else(|| Some(name.to_string())),
        None => Ok(links
            .iter()
            .position(|link| link.remote().default)
            .unwrap_or(0)),
    }
}

/// The `[[remotes]]` skeleton shown wherever the user is pointed at the file
/// that names a remote — the connect button's dead end, and the palette's
/// open-bridge.toml when the file does not exist yet. One remote, an
/// endpoint, and the tunnel that reaches it.
fn remotes_skeleton() -> String {
    "[[remotes]]\n\
     name = \"vps\"\n\
     endpoint = \"127.0.0.1:9000\"\n\
     tunnel = \"ssh -N -L 9000:127.0.0.1:9000 user@vps\"\n\
     default = true"
        .into()
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
/// A palette row's title with the letters the query actually matched picked
/// out in the theme's accent color — the same cue Sublime's palette gives,
/// so the ranking is not the only evidence a row is the one you meant.
fn highlighted_title(
    ui: &egui::Ui,
    title: &str,
    positions: &[usize],
    accent: Color32,
) -> egui::text::LayoutJob {
    let font_id = egui::TextStyle::Button.resolve(ui.style());
    let base = ui.visuals().strong_text_color();
    let mut job = egui::text::LayoutJob::default();
    let chars: Vec<char> = title.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        let matched = positions.contains(&index);
        let start = index;
        while index < chars.len() && positions.contains(&index) == matched {
            index += 1;
        }
        let run: String = chars[start..index].iter().collect();
        job.append(
            &run,
            0.0,
            egui::TextFormat {
                font_id: font_id.clone(),
                color: if matched { accent } else { base },
                ..Default::default()
            },
        );
    }
    job
}

fn args_workdir(args: &[String]) -> Option<&str> {
    let pos = args.iter().position(|a| a == "-w" || a == "--workdir")?;
    args.get(pos + 1).map(String::as_str)
}

/// Tab title: the workdir's basename when `-w <dir>` is present, else a
/// number — prefixed with the remote's name when the session is not on this
/// machine, since that is the one thing two identical-looking tabs differ by.
fn session_title(args: &[String], id: usize, target: &SessionTarget) -> String {
    let base = match args_workdir(args).map(|dir| std::path::Path::new(dir).file_name()) {
        Some(Some(name)) => name.to_string_lossy().to_string(),
        _ => format!("session {id}"),
    };
    match target {
        SessionTarget::Local => base,
        SessionTarget::Remote(name, _) => format!("{name}:{base}"),
    }
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
        // Leaked on purpose. These files are large — a CJK .ttc is over 10 MB
        // — and egui keeps them for the process lifetime either way. The
        // difference is that `FontData::from_owned` hands egui a `Cow::Owned`,
        // which its font cache *clones in full* per font, and clones again
        // every time `pixels_per_point` changes (dragging the window to a
        // monitor with different DPI). `from_static` takes the borrowed path
        // instead and parses the bytes in place, so they are stored once.
        fonts.font_data.insert(
            name.to_owned(),
            std::sync::Arc::new(egui::FontData::from_static(bytes.leak())),
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
    use super::{Split, install_fallback_fonts, pane_id, pick_link, shift_active};
    use crate::remote_link::RemoteLink;
    use crate::theme::{MOON_PHASES, Theme};
    use wire_client::remotes::Remote;

    fn link(name: &str, default: bool) -> RemoteLink {
        RemoteLink::new(Remote {
            name: name.into(),
            endpoint: "127.0.0.1:1".into(),
            tunnel: None,
            default,
        })
    }

    /// The palette's contract: a bare remote command means the default —
    /// the entry marked in bridge.toml, else the first — and a name picks
    /// its remote wherever it sits in the file.
    /// The regression that made "split down" look like a no-op: egui stores
    /// one panel state per id, reading a width from it for a column and a
    /// height for a row, so the two axes must never share one.
    #[test]
    fn test_pane_ids_differ_between_the_axes() {
        assert_ne!(pane_id(Split::Columns, 2, 0), pane_id(Split::Rows, 2, 0));
    }

    /// Each layout keeps its own dividers — and the same layout keeps the
    /// same ones, or every repaint would reset the drag.
    #[test]
    fn test_pane_ids_are_per_layout_and_stable() {
        assert_ne!(pane_id(Split::Columns, 2, 0), pane_id(Split::Columns, 3, 0));
        assert_ne!(pane_id(Split::Columns, 3, 0), pane_id(Split::Columns, 3, 1));
        assert_eq!(pane_id(Split::Columns, 2, 0), pane_id(Split::Columns, 2, 0));
    }

    /// Closing a tab renumbers every pane's selection, not just the one the
    /// close came from — they all index the same list.
    #[test]
    fn test_closing_an_earlier_tab_shifts_the_selection_down() {
        // Four tabs, tab 1 closed: a pane on 2 follows it to 1.
        assert_eq!(shift_active(2, 1, 3), 1);
        assert_eq!(shift_active(3, 1, 3), 2);
    }

    #[test]
    fn test_closing_a_later_tab_leaves_the_selection_alone() {
        assert_eq!(shift_active(0, 2, 3), 0);
        assert_eq!(shift_active(1, 2, 3), 1);
    }

    /// The pane that was showing the closed tab keeps its index, which is now
    /// its neighbour — unless it was the last, and there is no neighbour.
    #[test]
    fn test_closing_the_shown_tab_lands_on_a_real_one() {
        assert_eq!(shift_active(1, 1, 3), 1);
        assert_eq!(shift_active(3, 3, 3), 2);
        assert_eq!(shift_active(0, 0, 0), 0);
    }

    #[test]
    fn test_pick_link_bare_means_the_default() {
        let links = [link("vps", false), link("buildbox", true)];
        assert_eq!(pick_link(&links, None), Ok(1));

        let unmarked = [link("vps", false), link("buildbox", false)];
        assert_eq!(pick_link(&unmarked, None), Ok(0));
    }

    #[test]
    fn test_pick_link_takes_a_name() {
        let links = [link("vps", false), link("buildbox", true)];
        assert_eq!(pick_link(&links, Some("vps")), Ok(0));
        // An unknown name is handed back for the caller's error message.
        assert_eq!(pick_link(&links, Some("vpz")), Err(Some("vpz".to_string())));
    }

    #[test]
    fn test_pick_link_with_nothing_configured() {
        assert_eq!(pick_link(&[], None), Err(None));
        assert_eq!(pick_link(&[], Some("vps")), Err(None));
    }

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
