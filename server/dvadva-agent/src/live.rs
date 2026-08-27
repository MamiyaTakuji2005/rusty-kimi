//! The live-session registry: which agents are up, and where to attach.
//!
//! A detached agent is only useful if somebody can find it again. The
//! announce line on stderr (`wire/listener.rs`) tells whoever *spawned* the
//! agent where it landed, which is everything a supervisor holding the pipes
//! needs and nothing at all for anybody else — a second frontend, a restarted
//! daemon, an operator asking what is running on this box. This directory is
//! the answer to those: one small JSON file per listening agent, under
//! `~/.kimi/live/`, written once the socket is bound and removed when the
//! process stops.
//!
//! **Liveness is decided by the endpoint, not by the pid.** The plan called
//! for reaping entries whose pid is gone. What a reader actually wants to
//! know is whether it can still attach, and a live pid with a dead listener
//! answers the wrong question — while asking the pid portably costs either a
//! new dependency or `unsafe`, and this workspace denies the second. So a
//! stale entry is one whose address refuses a connection, and the pid is kept
//! for the humans: log lines, `kill`, a task manager.
//!
//! **One writer per file.** A process hosts exactly one session (`app.rs`
//! chdirs the whole process into its work directory), so no two processes
//! ever write the same entry. Readers are many, so an entry is written to a
//! temporary file and renamed into place: a reader sees the old file or the
//! new one, never half of either.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tracing::{debug, warn};

/// The registry's directory inside the share dir (`~/.kimi/live`).
pub const DIR_NAME: &str = "live";

/// Extension of a registry entry. Anything else in the directory is somebody
/// else's business and is left alone.
const ENTRY_EXTENSION: &str = "json";

/// How long an entry's address gets to accept a connection before the entry
/// is called stale. Loopback either answers at once or is not there; this is
/// slack for a loaded box, not a network round trip.
pub const REACHABILITY_TIMEOUT: Duration = Duration::from_millis(500);

/// One agent, listening, right now.
///
/// Everything a client needs in order to attach (`addr` plus the secret in
/// `token_file`), plus enough to describe the agent to a person who is
/// deciding whether to.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LiveSession {
    /// The session this agent hosts. One per process, and the entry's name.
    pub session: String,
    /// The agent's process id — for operators, not for liveness.
    pub pid: u32,
    /// Where to attach: the loopback address the agent bound.
    pub addr: String,
    /// The file holding the secret an attaching client must present.
    pub token_file: PathBuf,
    /// The agent's working directory, so a listing can say which project
    /// this is without opening the session.
    pub work_dir: String,
    /// The wire protocol this build speaks. A supervisor can refuse an
    /// attach it could only fail at `initialize`.
    pub protocol_version: String,
    /// The agent binary's own version. Says which build, never whether it
    /// is compatible — that is `protocol_version`'s job.
    pub agent_version: String,
    /// When the agent started listening (unix seconds), matching the
    /// `updated_at` convention of the session listings.
    pub started_at: f64,
}

impl LiveSession {
    /// Parse [`Self::addr`], which is a string in the file because a registry
    /// entry is a document before it is a socket address.
    pub fn socket_addr(&self) -> Option<SocketAddr> {
        self.addr.parse().ok()
    }
}

/// A directory of [`LiveSession`] entries.
///
/// Takes its directory explicitly so that a test — or a second daemon on one
/// box — can have its own; [`Registry::shared`] is the one every real process
/// uses.
#[derive(Clone, Debug)]
pub struct Registry {
    dir: PathBuf,
}

impl Registry {
    /// The registry every agent and every supervisor on this machine shares:
    /// `~/.kimi/live`.
    pub fn shared() -> Self {
        Self::at(crate::share::get_share_dir().join(DIR_NAME))
    }

    /// A registry in a directory of your choosing.
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Where the entries live.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Announce this agent, and keep announcing it until the returned
    /// [`Registration`] is dropped.
    ///
    /// Failing to register is not failing to run: the agent is already bound
    /// and serving by the time this is called, and whoever spawned it has the
    /// announce line. The error is for the log.
    pub async fn register(&self, entry: &LiveSession) -> Result<Registration> {
        tokio::fs::create_dir_all(&self.dir)
            .await
            .with_context(|| format!("failed to create {}", self.dir.display()))?;

        let path = self.entry_path(&entry.session);
        let body = serde_json::to_vec_pretty(entry).context("failed to encode a registry entry")?;

        // Rename into place, so a reader mid-listing never sees a half
        // written entry. The temporary name carries the pid: two processes
        // cannot own the same session, but a crashed one can leave litter,
        // and litter that names its owner is diagnosable.
        let staging = self
            .dir
            .join(format!(".{}.{}.tmp", entry.session, entry.pid));
        tokio::fs::write(&staging, &body)
            .await
            .with_context(|| format!("failed to write {}", staging.display()))?;
        if let Err(err) = tokio::fs::rename(&staging, &path).await {
            let _ = tokio::fs::remove_file(&staging).await;
            return Err(err).with_context(|| format!("failed to publish {}", path.display()));
        }
        debug!(
            "registered live session {} at {}",
            entry.session, entry.addr
        );
        Ok(Registration { path })
    }

    /// The entry for one session, if it is live.
    ///
    /// Verified the same way [`Self::list`] verifies: an entry whose address
    /// no longer answers is removed and reported as absent, because the
    /// caller's next move would have been to connect to it.
    pub async fn find(&self, session: &str) -> Option<LiveSession> {
        let path = self.entry_path(session);
        let entry = read_entry(&path).await?;
        if is_reachable(&entry).await {
            return Some(entry);
        }
        reap(&path).await;
        None
    }

    /// Every agent that is still answering, newest first.
    ///
    /// Entries whose address refuses a connection are deleted on the way
    /// past: a registry nobody prunes is a list of ghosts within a day, and
    /// the reader who noticed is the one holding the evidence.
    pub async fn list(&self) -> Vec<LiveSession> {
        let mut dir = match tokio::fs::read_dir(&self.dir).await {
            Ok(dir) => dir,
            // No directory means no live agents, which is the normal state of
            // a machine that has never run one.
            Err(_) => return Vec::new(),
        };

        let mut live = Vec::new();
        while let Ok(Some(item)) = dir.next_entry().await {
            let path = item.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some(ENTRY_EXTENSION) {
                continue;
            }
            let Some(entry) = read_entry(&path).await else {
                continue;
            };
            if is_reachable(&entry).await {
                live.push(entry);
            } else {
                reap(&path).await;
            }
        }
        live.sort_by(|a, b| {
            b.started_at
                .partial_cmp(&a.started_at)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        live
    }

    /// Wait for an agent with this pid to announce itself, or give up.
    ///
    /// How a supervisor learns where the agent it just started ended up. It
    /// waits on the *pid* rather than on a session id because the interesting
    /// case is a brand-new session, whose id nobody knows until the agent
    /// mints it.
    pub async fn wait_for_pid(&self, pid: u32, patience: Duration) -> Option<LiveSession> {
        let poll = Duration::from_millis(50);
        let deadline = tokio::time::Instant::now() + patience;
        loop {
            for entry in self.list().await {
                if entry.pid == pid {
                    return Some(entry);
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(poll).await;
        }
    }

    /// Where one session's entry lives.
    pub fn entry_path(&self, session: &str) -> PathBuf {
        self.dir.join(format!("{session}.{ENTRY_EXTENSION}"))
    }
}

/// This process's place in the registry, held for as long as it is listening.
///
/// Dropping it withdraws the entry. That covers a clean stop; a kill leaves
/// the file behind, which is exactly what the reachability check on the
/// reading side is for.
pub struct Registration {
    path: PathBuf,
}

impl Registration {
    /// The entry file this registration owns.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        // Blocking, in a Drop, on purpose: it is one unlink, and the async
        // alternative is a task that may never be polled because the runtime
        // is already shutting down when this runs.
        if let Err(err) = std::fs::remove_file(&self.path)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            warn!("failed to withdraw {}: {err}", self.path.display());
        }
    }
}

/// Unix seconds, for [`LiveSession::started_at`].
pub fn now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs_f64())
        .unwrap_or(0.0)
}

/// Read one entry, treating anything unreadable as absent: a truncated or
/// hand-edited file is not a reason to fail a listing.
async fn read_entry(path: &Path) -> Option<LiveSession> {
    let body = tokio::fs::read(path).await.ok()?;
    match serde_json::from_slice::<LiveSession>(&body) {
        Ok(entry) => Some(entry),
        Err(err) => {
            debug!(
                "ignoring unreadable registry entry {}: {err}",
                path.display()
            );
            None
        }
    }
}

/// Can anything still be reached at this entry's address?
///
/// A bare connect, deliberately: the handshake needs the token file and would
/// turn a listing into an authentication. Whether the thing that answered is
/// the agent this entry describes is settled later, by the attach itself —
/// the handshake reply names the session.
async fn is_reachable(entry: &LiveSession) -> bool {
    let Some(addr) = entry.socket_addr() else {
        return false;
    };
    matches!(
        tokio::time::timeout(REACHABILITY_TIMEOUT, TcpStream::connect(addr)).await,
        Ok(Ok(_))
    )
}

/// Remove an entry that no longer answers.
async fn reap(path: &Path) {
    debug!("reaping stale registry entry {}", path.display());
    let _ = tokio::fs::remove_file(path).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::net::TcpListener;

    fn entry(session: &str, addr: &str) -> LiveSession {
        LiveSession {
            session: session.to_string(),
            pid: std::process::id(),
            addr: addr.to_string(),
            token_file: PathBuf::from("attach.token"),
            work_dir: "/srv/proj".to_string(),
            protocol_version: "1.3".to_string(),
            agent_version: "1.8.0".to_string(),
            started_at: now_seconds(),
        }
    }

    /// A listener nobody serves: enough to be reachable, which is all the
    /// registry ever asks.
    async fn somewhere_reachable() -> (TcpListener, String) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr").to_string();
        (listener, addr)
    }

    #[tokio::test]
    async fn a_registered_agent_can_be_found_again() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let registry = Registry::at(tmp.path());
        let (_listener, addr) = somewhere_reachable().await;

        let written = entry("abc", &addr);
        let _registration = registry.register(&written).await.expect("register");

        assert_eq!(registry.find("abc").await.as_ref(), Some(&written));
        assert_eq!(registry.list().await, vec![written]);
    }

    #[tokio::test]
    async fn withdrawing_is_dropping() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let registry = Registry::at(tmp.path());
        let (_listener, addr) = somewhere_reachable().await;

        let registration = registry
            .register(&entry("abc", &addr))
            .await
            .expect("register");
        let path = registration.path().to_path_buf();
        drop(registration);

        assert!(!path.exists(), "the entry outlived its registration");
        assert!(registry.find("abc").await.is_none());
    }

    #[tokio::test]
    async fn an_entry_that_no_longer_answers_is_reaped_by_whoever_reads_it() {
        // The killed-agent case: the process never got to withdraw, so the
        // file is still there and the port is not.
        let tmp = tempfile::tempdir().expect("temp dir");
        let registry = Registry::at(tmp.path());
        let (listener, addr) = somewhere_reachable().await;
        let dead = entry("gone", &addr);
        let registration = registry.register(&dead).await.expect("register");
        std::mem::forget(registration); // as a kill would leave it
        drop(listener);

        assert!(registry.find("gone").await.is_none());
        assert!(
            !registry.entry_path("gone").exists(),
            "a stale entry must not survive the reader that noticed it"
        );
    }

    #[tokio::test]
    async fn a_listing_keeps_the_live_and_drops_the_dead() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let registry = Registry::at(tmp.path());
        let (_alive, alive_addr) = somewhere_reachable().await;
        let (dead, dead_addr) = somewhere_reachable().await;

        let _kept = registry
            .register(&entry("alive", &alive_addr))
            .await
            .expect("register");
        std::mem::forget(
            registry
                .register(&entry("dead", &dead_addr))
                .await
                .expect("register"),
        );
        drop(dead);

        let listed = registry.list().await;
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0].session, "alive");
        assert!(!registry.entry_path("dead").exists());
    }

    #[tokio::test]
    async fn a_supervisor_waits_for_the_agent_it_started_by_pid() {
        // The new-session case: nobody knows the session id yet, so the pid
        // is the only handle the spawner has.
        let tmp = tempfile::tempdir().expect("temp dir");
        let registry = Registry::at(tmp.path());
        let (_listener, addr) = somewhere_reachable().await;

        let writer = registry.clone();
        let addr_for_writer = addr.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            let mut late = entry("late", &addr_for_writer);
            late.pid = 4242;
            std::mem::forget(writer.register(&late).await.expect("register"));
        });

        let found = registry
            .wait_for_pid(4242, Duration::from_secs(5))
            .await
            .expect("the entry arrives while we wait");
        assert_eq!(found.session, "late");
    }

    #[tokio::test]
    async fn waiting_for_a_pid_that_never_arrives_gives_up() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let registry = Registry::at(tmp.path());
        assert!(
            registry
                .wait_for_pid(1, Duration::from_millis(120))
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn junk_in_the_directory_is_not_a_failed_listing() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let registry = Registry::at(tmp.path());
        let (_listener, addr) = somewhere_reachable().await;
        let _kept = registry
            .register(&entry("good", &addr))
            .await
            .expect("register");

        std::fs::write(tmp.path().join("notes.txt"), "not an entry").expect("write");
        std::fs::write(tmp.path().join("broken.json"), "{ half").expect("write");

        let listed = registry.list().await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].session, "good");
    }
}
