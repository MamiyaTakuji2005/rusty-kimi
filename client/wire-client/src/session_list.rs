//! Background listing of past sessions, used by the resume menu (the book
//! button in the tab strip).
//!
//! The agent process owns session persistence, so this reuses the agent's own
//! `Session::list` (reading `~/.kimi/kimi.json` + the session directories) on
//! a background thread with a private tokio runtime, and hands the flattened
//! result back to the UI thread through a channel.

use std::io::{BufReader, Write};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Local};
use dvadva_agent::live::Registry;
use dvadva_agent::metadata::load_metadata;
use dvadva_agent::session::Session as AgentSession;
use kaos::KaosPath;
use serde::{Deserialize, Serialize};

use crate::bridge;

/// One resumable past session, flattened across all known work directories.
/// Also the wire shape of a bridge daemon's `list_sessions` reply.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ResumeEntry {
    pub id: String,
    /// First user input of the session, as computed by the agent.
    pub title: String,
    /// Working directory the session belongs to.
    pub work_dir: PathBuf,
    /// Last modification time of `context.jsonl` (unix seconds).
    pub updated_at: f64,
    /// Whether an agent is hosting this session right now
    /// (`dvadva_agent::live`) — resuming it would join that process rather
    /// than start one. Defaulted, so a listing from a bridge daemon too old
    /// to know reads as "all cold", which is what such a daemon means.
    #[serde(default)]
    pub live: bool,
}

impl ResumeEntry {
    /// Shortened title for a tab: first user input without the trailing
    /// session-id suffix, capped to a sane width.
    pub fn tab_title(&self) -> String {
        let suffix = format!(" ({})", self.id);
        let base = self.title.strip_suffix(&suffix).unwrap_or(&self.title);
        let base = base.trim();
        if base.is_empty() {
            return format!("resume {}", self.short_id());
        }
        let mut out: String = base.chars().take(24).collect();
        if base.chars().count() > 24 {
            out.push('…');
        }
        out
    }

    /// First 8 characters of the session id (enough to disambiguate in the UI).
    fn short_id(&self) -> String {
        self.id.chars().take(8).collect()
    }

    /// One-line metadata string for the resume menu rows.
    pub fn meta_line(&self) -> String {
        let dir = self
            .work_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.work_dir.to_string_lossy().into_owned());
        format!(
            "{} · {} · {}",
            format_relative_time(self.updated_at),
            dir,
            self.short_id(),
        )
    }
}

/// Spawn a background thread that lists every session of every work directory
/// known to `~/.kimi/kimi.json`, newest first. The result (or an error
/// message) arrives on the returned receiver; `wake` is called when it lands
/// so the caller's UI picks it up without polling delays (egui:
/// `Context::request_repaint`).
pub fn spawn_session_listing<W>(wake: W) -> Receiver<Result<Vec<ResumeEntry>, String>>
where
    W: Fn() + Send + 'static,
{
    let (tx, rx) = channel();
    std::thread::Builder::new()
        .name("session-listing".into())
        .spawn(move || {
            // `Session::list` panics on unexpected filesystem errors; never
            // take the whole app down just because the menu was opened.
            let result = match std::panic::catch_unwind(list_all_sessions) {
                Ok(Ok(sessions)) => Ok(sessions),
                Ok(Err(err)) => Err(err),
                Err(panic) => Err(format!("session listing panicked: {panic:?}")),
            };
            let _ = tx.send(result);
            wake();
        })
        .expect("spawn session-listing thread");
    rx
}

/// Remote twin of [`spawn_session_listing`]: ask the bridge daemon at
/// `endpoint` for the sessions living on *its* machine (the daemon answers
/// from its own `~/.kimi` — see `remote/dvadva-bridge`). Same receiver and
/// wake contract as the local variant.
pub fn spawn_remote_session_listing<W>(
    endpoint: &str,
    wake: W,
) -> Receiver<Result<Vec<ResumeEntry>, String>>
where
    W: Fn() + Send + 'static,
{
    let (tx, rx) = channel();
    let endpoint = endpoint.to_string();
    std::thread::Builder::new()
        .name("remote-session-listing".into())
        .spawn(move || {
            let _ = tx.send(list_remote_sessions(&endpoint));
            wake();
        })
        .expect("spawn remote-session-listing thread");
    rx
}

fn list_remote_sessions(endpoint: &str) -> Result<Vec<ResumeEntry>, String> {
    let mut stream = bridge::connect(endpoint, bridge::CONNECT_TIMEOUT)?;
    // Bounded like the spawn handshake: a menu that never answers is worse
    // than one that says why, and this thread would otherwise hang forever.
    stream
        .set_read_timeout(Some(bridge::HANDSHAKE_TIMEOUT))
        .map_err(|err| format!("bridge `{endpoint}`: {err}"))?;
    let header = bridge::list_sessions_header();
    stream
        .write_all(header.as_bytes())
        .and_then(|_| stream.write_all(b"\n"))
        .and_then(|_| stream.flush())
        .map_err(|err| format!("bridge `{endpoint}` write failed: {err}"))?;

    let line = bridge::read_frame_line(&mut BufReader::new(&mut stream))
        .map_err(|err| format!("bridge `{endpoint}`: {err}"))?;
    let reply = bridge::decode_reply(&line)?;
    if !reply.ok {
        return Err(reply
            .error
            .unwrap_or_else(|| "bridge listing failed".into()));
    }
    let mut entries = reply.sessions.unwrap_or_default();
    entries.sort_by(|a, b| b.updated_at.total_cmp(&a.updated_at));
    Ok(entries)
}

/// The agent hosting `session` on this machine right now, if one is.
///
/// The other half of the `live` flag: a listing can say a session is live,
/// and this is what turns that into something to connect to. Reading the
/// registry also prunes entries that no longer answer, so a `None` here means
/// "not reachable", not merely "no file".
///
/// Blocking, and briefly — the registry dials each candidate with a short
/// timeout. Frontends call it on the thread that opens a session, which is
/// already the thread that dials a bridge.
pub fn find_live_session(session: &str) -> Option<dvadva_agent::live::LiveSession> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    rt.block_on(Registry::shared().find(session))
}

fn list_all_sessions() -> Result<Vec<ResumeEntry>, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("failed to create runtime: {err}"))?;
    rt.block_on(async {
        // Which of them an agent is already holding. The same registry the
        // bridge daemon reads on the remote side, for the same reason: a
        // session with a live agent is resumed by attaching to it, not by
        // starting a second process on the same files.
        let live: std::collections::HashSet<String> = Registry::shared()
            .list()
            .await
            .into_iter()
            .map(|entry| entry.session)
            .collect();

        let metadata = load_metadata().await;
        let mut entries: Vec<ResumeEntry> = Vec::new();
        for work_dir in &metadata.work_dirs {
            for session in AgentSession::list(KaosPath::new(&work_dir.path)).await {
                entries.push(ResumeEntry {
                    live: live.contains(&session.id),
                    id: session.id,
                    title: session.title,
                    work_dir: session.work_dir.as_path().to_path_buf(),
                    updated_at: session.updated_at,
                });
            }
        }
        entries.sort_by(|a, b| b.updated_at.total_cmp(&a.updated_at));
        Ok(entries)
    })
}

/// Human-friendly relative timestamp ("3m ago"), falling back to a local date
/// for anything older than a week.
pub fn format_relative_time(updated_at: f64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    let delta = (now - updated_at).max(0.0);
    if delta < 60.0 {
        "just now".to_string()
    } else if delta < 3600.0 {
        format!("{}m ago", (delta / 60.0) as u64)
    } else if delta < 86400.0 {
        format!("{}h ago", (delta / 3600.0) as u64)
    } else if delta < 7.0 * 86400.0 {
        format!("{}d ago", (delta / 86400.0) as u64)
    } else {
        DateTime::from_timestamp(updated_at as i64, 0)
            .map(|t| t.with_timezone(&Local).format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "unknown".to_string())
    }
}
