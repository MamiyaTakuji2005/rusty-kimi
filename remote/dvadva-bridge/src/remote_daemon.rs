//! The remote daemon: runs on the machine that hosts `dvadva-agent`.
//!
//! Per connection: read the bridge header, then do one of three things.
//!
//! - `spawn` — the one-shot path. Start an agent on this connection's pipes
//!   and relay; the connection's lifetime *is* the agent's, and the agent
//!   is killed on the way out.
//! - `attach` — the supervised path. Find the agent hosting a session, or
//!   start one that is not this connection's to kill, and relay to it over
//!   its own loopback socket. The connection closing is a detach.
//! - `list_sessions` — answer from the local `~/.kimi`, marking the ones an
//!   agent is holding right now.
//!
//! The two spawn paths differ in exactly one thing, and it is not the
//! transport: **who owns the agent's lifetime**. A `spawn` agent belongs to
//! its connection. An `attach` agent belongs to the machine — it goes into
//! its own process group, its stderr goes to a file rather than to a pipe
//! this daemon holds, and it stays up when the daemon that started it does
//! not.

use std::collections::{HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kaos::KaosPath;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, ChildStderr, Command};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use dvadva_agent::live::{self, LiveSession, Registry};
use dvadva_agent::metadata::load_metadata;
use dvadva_agent::session::Session as AgentSession;

use crate::proto::{self, Reply, Request, SessionEntry};

/// How long a connection may take to send its header line.
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);

/// Grace period for the agent to exit on its own (its stdin just closed)
/// before the daemon kills it.
const AGENT_EXIT_GRACE: Duration = Duration::from_secs(5);

/// How long to wait for the stderr forwarder to drain after the agent is
/// gone, so a startup failure's last words make it into the exit trailer.
const STDERR_DRAIN: Duration = Duration::from_secs(1);

/// How many stderr lines to keep for the exit trailer. Matches the tail
/// `wire_client` keeps for a locally spawned agent, so a remote failure
/// reads the same as a local one.
const STDERR_TAIL_LINES: usize = 20;

/// Consecutive `accept()` failures tolerated before the daemon gives up. A
/// transient error (fd exhaustion, a peer that vanished mid-handshake) must
/// never take a long-running daemon down; a listener that is broken for
/// good must not spin forever.
const MAX_CONSECUTIVE_ACCEPT_ERRORS: usize = 64;

/// Pause after a failed `accept()`, so a persistent error cannot spin the
/// loop hot while it burns through its budget.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);

/// How long a freshly started agent has to bind its socket and list itself.
/// Generous on purpose: an agent loads its config, its skills and whatever
/// MCP servers it was told about *before* it listens, and none of that is
/// this daemon's business to hurry.
const AGENT_START_PATIENCE: Duration = Duration::from_secs(45);

/// How long the agent's own attach handshake may take. Loopback, and the
/// agent answers it before doing any work, so this only ever expires on
/// something that is not the agent.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(10);

/// How many lines of a supervised agent's log to quote when it fails.
/// Matches [`STDERR_TAIL_LINES`], so both paths fail the same way.
const LOG_TAIL_LINES: usize = STDERR_TAIL_LINES;

/// How much of the end of a log file to read to find those lines.
const LOG_TAIL_BYTES: u64 = 8 * 1024;

/// Monotonic connection id, only used in daemon log lines.
static CONN_ID: AtomicU64 = AtomicU64::new(1);

/// What the remote daemon needs to serve connections.
#[derive(Clone, Debug)]
pub struct Config {
    /// The `dvadva-agent` binary to spawn.
    pub agent_bin: String,
    /// Work directory for agents whose spawn args name none — normally the
    /// daemon user's home directory, so a frontend on another OS never has
    /// to guess a path that exists over here. `None` leaves the agent's own
    /// default (the daemon's working directory) in place.
    pub default_work_dir: Option<String>,
    /// The `~/.kimi` this daemon supervises: where the live-session registry
    /// lives, where supervised agents' logs go, and what those agents are
    /// told to use. `None` means this user's own, which is what every
    /// deployment wants and no test does.
    pub share_dir: Option<PathBuf>,
    /// How long a supervised agent may idle before stopping itself, in
    /// seconds; `0` means never. Only ever passed to agents *this* daemon
    /// starts — an agent somebody else started keeps whatever policy it was
    /// given, which is the same rule as every other argument here.
    pub agent_idle_timeout: u64,
}

impl Config {
    /// A config that only names the agent binary; the agent then picks its
    /// own work directory. Mostly for tests.
    pub fn new(agent_bin: impl Into<String>) -> Self {
        Self {
            agent_bin: agent_bin.into(),
            default_work_dir: None,
            share_dir: None,
            agent_idle_timeout: crate::config::DEFAULT_AGENT_IDLE_TIMEOUT,
        }
    }

    /// Same, with a different idle policy for the agents it starts.
    pub fn with_agent_idle_timeout(mut self, seconds: u64) -> Self {
        self.agent_idle_timeout = seconds;
        self
    }

    /// Same, with a default work directory for args that name none.
    pub fn with_default_work_dir(mut self, dir: Option<String>) -> Self {
        self.default_work_dir = dir;
        self
    }

    /// Same, with a `~/.kimi` of its own — a test's, or a second daemon's on
    /// one box. Supervised agents inherit it through `KIMI_SHARE_DIR`, so
    /// the daemon and the agents it starts always agree about which registry
    /// they are talking about.
    pub fn with_share_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.share_dir = dir;
        self
    }

    /// Where live agents announce themselves and leave their logs.
    pub fn live_dir(&self) -> PathBuf {
        let share = match &self.share_dir {
            Some(dir) => dir.clone(),
            None => dvadva_agent::share::get_share_dir(),
        };
        share.join(live::DIR_NAME)
    }

    /// The registry this daemon reads and its agents write.
    fn registry(&self) -> Registry {
        Registry::at(self.live_dir())
    }

    /// Where a supervised agent's stderr goes: beside the registry, because
    /// everything about a live agent belongs in one place, and named for the
    /// daemon and connection that started it, so a log line here leads to a
    /// file over there. A listing ignores it — it is not a `.json`.
    fn agent_log_path(&self, conn: u64) -> PathBuf {
        self.live_dir()
            .join(format!("agent-{}-{conn}.log", std::process::id()))
    }
}

/// Serve bridge connections forever. Each connection is handled on its own
/// task; a failure ends only that connection, never the daemon.
pub async fn serve(listener: TcpListener, config: Config) -> io::Result<()> {
    let mut consecutive_errors = 0usize;
    loop {
        let (socket, peer) = match listener.accept().await {
            Ok(accepted) => {
                consecutive_errors = 0;
                accepted
            }
            Err(err) => {
                consecutive_errors += 1;
                eprintln!("dvadva-bridge: accept failed ({consecutive_errors}): {err}");
                if consecutive_errors >= MAX_CONSECUTIVE_ACCEPT_ERRORS {
                    return Err(io::Error::new(
                        err.kind(),
                        format!(
                            "giving up after {consecutive_errors} consecutive accept failures: {err}"
                        ),
                    ));
                }
                tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                continue;
            }
        };
        // Wire traffic is many small writes (streamed tokens, one JSON line
        // per event); Nagle would batch them into visible stutter once a
        // real network is in the path.
        let _ = socket.set_nodelay(true);
        let config = config.clone();
        tokio::spawn(async move {
            if let Err(err) = handle(socket, &config).await {
                eprintln!("dvadva-bridge: {peer}: connection error: {err}");
            }
        });
    }
}

async fn handle(socket: TcpStream, config: &Config) -> io::Result<()> {
    let conn = CONN_ID.fetch_add(1, Ordering::Relaxed);
    let mut socket = BufReader::new(socket);

    let line = match timeout(HEADER_TIMEOUT, proto::read_line(&mut socket)).await {
        Ok(result) => result?,
        Err(_) => {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "no bridge frame within 10s",
            ));
        }
    };
    let request = match proto::decode::<Request>(&line) {
        Ok(request) => request,
        Err(err) => {
            write_reply(&mut socket, Reply::error(err)).await?;
            return socket.get_mut().shutdown().await;
        }
    };

    match request {
        Request::Spawn { args } => spawn_relay(conn, socket, config, args).await,
        Request::Attach { session, args } => {
            attach_relay(conn, socket, config, session, args).await
        }
        Request::ListSessions => {
            let reply = match list_sessions(&config.registry()).await {
                Ok(entries) => Reply::sessions(entries),
                Err(err) => Reply::error(err),
            };
            write_reply(&mut socket, reply).await?;
            socket.get_mut().shutdown().await
        }
        Request::Version => {
            write_reply(&mut socket, Reply::version(env!("CARGO_PKG_VERSION"))).await?;
            socket.get_mut().shutdown().await
        }
    }
}

/// Spawn the agent, acknowledge, then relay bytes until the agent's output
/// ends. Close propagation is the whole contract:
///
/// - client half-closes / drops → agent stdin gets EOF → agent exits by
///   itself (this is also how a frontend asks the agent to exit),
/// - agent exits → one exit trailer frame, then the socket write half shuts
///   down → the client's reader sees the end of the stream.
async fn spawn_relay(
    conn: u64,
    mut socket: BufReader<TcpStream>,
    config: &Config,
    args: Vec<String>,
) -> io::Result<()> {
    let agent_bin = &config.agent_bin;
    let args = with_default_work_dir(args, config.default_work_dir.as_deref());
    let mut child = match Command::new(agent_bin)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // This agent belongs to this connection, and the kill is the
        // contract. `attach` is where an agent belongs to the machine.
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            // Surface spawn failures (missing binary, exec format) as a
            // bridge error instead of a silent stream close.
            let reply = Reply::error(format!("failed to spawn agent `{agent_bin}`: {err}"));
            write_reply(&mut socket, reply).await?;
            return socket.get_mut().shutdown().await;
        }
    };

    // Acknowledge before any agent output flows, so the client can tell a
    // daemon failure from a slow agent.
    write_reply(&mut socket, Reply::spawn_ok()).await?;

    let (stderr_task, stderr_tail) = forward_stderr(conn, child.stderr.take());

    let mut stdin = child.stdin.take().expect("stdin is piped");
    let mut stdout = child.stdout.take().expect("stdout is piped");

    // Split the socket so both copy tasks can own a half. `io::split`
    // (unlike `into_split`) keeps any bytes the header read buffered
    // inside the `BufReader` intact and drained first.
    let (mut rd, mut wr) = tokio::io::split(socket);

    let to_agent = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut rd, &mut stdin).await;
        // Client → agent direction ended: EOF (client closed) or write
        // error (agent died). Either way, close the agent's stdin.
        let _ = stdin.shutdown().await;
        drop(stdin);
    });
    let to_client = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut stdout, &mut wr).await;
        // Hand the write half back instead of shutting down here: the exit
        // trailer still has to go out on it.
        wr
    });

    // The agent's output ended, so the agent is exiting or already gone.
    let write_half = to_client.await.ok();

    // The agent should be exiting (stdin closed); give it a grace period
    // before killing — kill_on_drop is the belt to this suspenders.
    let status = match timeout(AGENT_EXIT_GRACE, child.wait()).await {
        Ok(Ok(status)) => {
            eprintln!("dvadva-bridge: conn {conn}: agent exited: {status}");
            format!("{status}")
        }
        Ok(Err(err)) => {
            eprintln!("dvadva-bridge: conn {conn}: agent wait failed: {err}");
            format!("wait failed: {err}")
        }
        Err(_) => {
            eprintln!("dvadva-bridge: conn {conn}: agent did not exit in time, killing");
            let _ = child.kill().await;
            "did not exit in time, killed".to_string()
        }
    };
    // An agent's last words are written on its way out; let the forwarder
    // drain the now-closed pipe before the tail is read.
    let _ = timeout(STDERR_DRAIN, stderr_task).await;

    if let Some(mut wr) = write_half {
        // One trailer frame, then the half-close. Without it a remote
        // failure (bad work dir, missing API key) reaches the frontend as a
        // bare "connection closed": the local transport shows the agent's
        // stderr tail, and this is how the remote one keeps parity.
        let reply = Reply::error(exit_reason(&status, &stderr_tail));
        let _ = write_reply(&mut wr, reply).await;
        let _ = wr.shutdown().await;
    }
    // Nothing can consume client bytes any more, so do not wait on a client
    // that may never close its write half.
    to_agent.abort();
    Ok(())
}

/// Join the agent hosting this session, starting one if none is, and relay
/// until the client leaves.
///
/// The supervised path, and the whole difference from [`spawn_relay`] is
/// what the end of this connection means: there, the agent dies with it;
/// here, closing the daemon's socket to the agent is a *detach*, and the
/// next `attach` for the same session lands on the same process, mid-turn
/// context and all.
async fn attach_relay(
    conn: u64,
    mut socket: BufReader<TcpStream>,
    config: &Config,
    session: Option<String>,
    args: Vec<String>,
) -> io::Result<()> {
    let registry = config.registry();

    // Find. An entry that is listed but will not take us is not an error to
    // report — it is a dead agent's leftovers, and the request can still be
    // served by starting a live one.
    let mut joined: Option<(LiveSession, BufReader<TcpStream>, Option<PathBuf>)> = None;
    if let Some(id) = session.as_deref()
        && let Some(entry) = registry.find(id).await
    {
        match connect_to_agent(&entry).await {
            Ok(stream) => {
                eprintln!(
                    "dvadva-bridge: conn {conn}: attached to the live agent for session {id} \
                     (pid {}, {})",
                    entry.pid, entry.addr
                );
                joined = Some((entry, stream, None));
            }
            Err(err) => eprintln!(
                "dvadva-bridge: conn {conn}: the listed agent for session {id} did not take \
                 us ({err}); starting a new one"
            ),
        }
    }

    // Or start.
    let (entry, agent, log) = match joined {
        Some(joined) => joined,
        None => match start_agent(conn, config, &registry, args).await {
            Ok((entry, stream, log)) => (entry, stream, Some(log)),
            Err(err) => {
                write_reply(&mut socket, Reply::error(err)).await?;
                return socket.get_mut().shutdown().await;
            }
        },
    };

    // Acknowledged only once there is an agent on the other end, so that a
    // failure to reach one is an error frame and never a silent close. The
    // session is named because a caller who asked for a new one has no other
    // way to learn which it got.
    write_reply(&mut socket, Reply::attach_ok(entry.session.as_str())).await?;
    relay_attached(conn, socket, agent, entry, log).await
}

/// Start an agent that this daemon does not own, and wait for it to say
/// where it is.
///
/// Three things make it not ours: `--listen` (so clients reach it over a
/// socket instead of our pipes), its own process group (so a Ctrl-C in this
/// daemon's terminal does not take it), and a log file instead of a piped
/// stderr — a pipe dies with the daemon, and the agent writes to stderr from
/// places that panic if that write fails.
///
/// We learn where it landed from the registry rather than from its announce
/// line, and we wait by *pid*: the interesting case is a brand-new session,
/// whose id nobody knows until the agent mints it.
async fn start_agent(
    conn: u64,
    config: &Config,
    registry: &Registry,
    args: Vec<String>,
) -> Result<(LiveSession, BufReader<TcpStream>, PathBuf), String> {
    let args = with_default_work_dir(args, config.default_work_dir.as_deref());
    let args = with_idle_timeout(args, config.agent_idle_timeout);
    let log = config.agent_log_path(conn);
    if let Some(parent) = log.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let stderr = std::fs::File::create(&log)
        .map_err(|err| format!("failed to open the agent log {}: {err}", log.display()))?;

    let agent_bin = &config.agent_bin;
    let mut command = Command::new(agent_bin);
    command
        .args(&args)
        // Additive: the agent keeps everything the caller asked for and
        // gains a socket. The port is the kernel's to choose and reaches us
        // through the registry, not through this argv.
        .arg("--listen")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .kill_on_drop(false);
    if let Some(share) = &config.share_dir {
        command.env("KIMI_SHARE_DIR", share);
    }
    detach(&mut command);

    let mut child = command
        .spawn()
        .map_err(|err| format!("failed to spawn agent `{agent_bin}`: {err}"))?;
    let pid = child.id().unwrap_or_default();
    eprintln!(
        "dvadva-bridge: conn {conn}: started agent pid {pid}; log {}",
        log.display()
    );

    let listed = tokio::select! {
        listed = registry.wait_for_pid(pid, AGENT_START_PATIENCE) => listed,
        // It died on the way up: a bad work directory, a missing key. This
        // is the diagnosis the exit trailer exists for, arriving before the
        // relay instead of after it.
        status = child.wait() => {
            let status = match status {
                Ok(status) => format!("{status}"),
                Err(err) => format!("wait failed: {err}"),
            };
            return Err(with_log_tail(
                format!("the agent exited before it started listening ({status})"),
                &log,
            )
            .await);
        }
    };

    let Some(entry) = listed else {
        // Alive and unreachable is worse than gone: nothing would ever
        // attach to it, and it would hold the session against the next try.
        let _ = child.kill().await;
        return Err(with_log_tail(
            format!(
                "the agent did not start listening within {}s",
                AGENT_START_PATIENCE.as_secs()
            ),
            &log,
        )
        .await);
    };

    let stream = match connect_to_agent(&entry).await {
        Ok(stream) => stream,
        Err(err) => {
            let _ = child.kill().await;
            return Err(format!(
                "the agent listed itself at {} and then would not take us: {err}",
                entry.addr
            ));
        }
    };

    reap_when_it_exits(conn, child, entry.session.clone(), log.clone());
    Ok((entry, stream, log))
}

/// Put a supervised agent out of this daemon's reach.
///
/// Without this, a Ctrl-C in the daemon's terminal — or the terminal closing
/// — goes to every agent it ever started, which is the exact opposite of
/// what a supervisor is for. On Windows this is also the answer to there
/// being no `setsid`: somebody has to pass the creation flags, and the
/// daemon is the natural somebody.
fn detach(command: &mut Command) {
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    {
        /// No console of its own, and not in this daemon's process group.
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
}

/// Wait on a supervised agent, so that a detached child does not become a
/// zombie, and say how it went.
///
/// A clean exit takes its log with it; a failure keeps it, because the only
/// reason to have written the file is to read it afterwards.
fn reap_when_it_exits(conn: u64, mut child: Child, session: String, log: PathBuf) {
    tokio::spawn(async move {
        match child.wait().await {
            Ok(status) => {
                eprintln!(
                    "dvadva-bridge: conn {conn}: agent for session {session} exited: {status}"
                );
                if status.success() {
                    let _ = tokio::fs::remove_file(&log).await;
                }
            }
            Err(err) => eprintln!(
                "dvadva-bridge: conn {conn}: agent for session {session}: wait failed: {err}"
            ),
        }
    });
}

/// What the agent answers its attach handshake with
/// (`server/dvadva-agent/src/wire/listener.rs`).
#[derive(Debug, Deserialize)]
struct AgentHandshake {
    #[serde(default)]
    auth: String,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    session: Option<String>,
}

/// Dial a listed agent and get through its token handshake.
///
/// The one place the daemon speaks anything but its own framing — and it is
/// still not the wire protocol. The agent's handshake is a *transport*
/// check, settled before the first wire byte, so relaying past it leaves the
/// no-parsing rule where it was: everything after this line flows untouched.
///
/// The reply names the session, and it is checked. A registry entry is a
/// hint, not a promise: a stale one can point at a port something else has
/// since taken, and the caller asked for a session rather than for an
/// address.
async fn connect_to_agent(entry: &LiveSession) -> Result<BufReader<TcpStream>, String> {
    let addr = entry
        .socket_addr()
        .ok_or_else(|| format!("`{}` is not an address", entry.addr))?;
    let token = tokio::fs::read_to_string(&entry.token_file)
        .await
        .map_err(|err| {
            format!(
                "cannot read the attach token {}: {err}",
                entry.token_file.display()
            )
        })?;

    let stream = timeout(ATTACH_TIMEOUT, TcpStream::connect(addr))
        .await
        .map_err(|_| format!("{addr} did not accept within {}s", ATTACH_TIMEOUT.as_secs()))?
        .map_err(|err| format!("failed to connect to {addr}: {err}"))?;
    let _ = stream.set_nodelay(true);
    let mut stream = BufReader::new(stream);

    let handshake = serde_json::json!({ "auth": token.trim(), "client": format!("dvadva-bridge {}", env!("CARGO_PKG_VERSION")) });
    let line = format!("{handshake}\n");
    stream
        .get_mut()
        .write_all(line.as_bytes())
        .await
        .map_err(|err| format!("failed to greet {addr}: {err}"))?;
    stream
        .get_mut()
        .flush()
        .await
        .map_err(|err| format!("failed to greet {addr}: {err}"))?;

    let reply = timeout(ATTACH_TIMEOUT, proto::read_line(&mut stream))
        .await
        .map_err(|_| {
            format!(
                "{addr} did not answer the attach handshake within {}s",
                ATTACH_TIMEOUT.as_secs()
            )
        })?
        .map_err(|err| format!("{addr} did not answer the attach handshake: {err}"))?;
    let reply: AgentHandshake = serde_json::from_str(&reply)
        .map_err(|err| format!("unreadable attach handshake from {addr}: {err}"))?;

    if reply.auth != "ok" {
        return Err(format!(
            "{addr} refused the attach token: {}",
            reply.error.as_deref().unwrap_or("no reason given")
        ));
    }
    if reply.session.as_deref() != Some(entry.session.as_str()) {
        return Err(format!(
            "{addr} hosts session {}, not {}",
            reply.session.as_deref().unwrap_or("(unnamed)"),
            entry.session
        ));
    }
    Ok(stream)
}

/// Relay between the client and an agent neither of us owns.
///
/// Close propagation is where this differs from [`spawn_relay`], and it is
/// the point of the whole phase:
///
/// - client EOF or drop → close the daemon's socket to the agent, which the
///   agent reads as one client detaching. Its turn, its context and its pid
///   are untouched. A hard drop counts: a killed frontend resets the
///   connection rather than closing it politely, and that is still a client
///   leaving.
/// - agent's stream ends → it died or was stopped; one exit trailer, then
///   half-close, so the client gets a diagnosis instead of a bare close.
///
/// Awaiting the agent-to-client direction covers both, because closing the
/// agent's socket makes the agent close its own side of it.
async fn relay_attached(
    conn: u64,
    socket: BufReader<TcpStream>,
    agent: BufReader<TcpStream>,
    entry: LiveSession,
    log: Option<PathBuf>,
) -> io::Result<()> {
    // `io::split` (unlike `into_split`) keeps the bytes each header read
    // left buffered, on both sides.
    let (mut client_rd, mut client_wr) = tokio::io::split(socket);
    let (mut agent_rd, mut agent_wr) = tokio::io::split(agent);

    let mut to_agent = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut client_rd, &mut agent_wr).await;
        // Closing this socket is the whole of detaching.
        let _ = agent_wr.shutdown().await;
    });
    let mut to_client = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut agent_rd, &mut client_wr).await;
        // Handed back rather than shut down here: the trailer may still
        // have to go out on it.
        client_wr
    });

    // *Which direction ended first* is what says who left, and it is the
    // only thing that does. Whether the client's copy ended in an EOF or in
    // an error says nothing: a frontend that is killed with bytes still in
    // its receive buffer resets the connection, and a reset is an error on
    // a read that is nonetheless a client leaving.
    //
    // `biased` resolves the one tie that happens: a client leaving makes the
    // agent close its side a moment later, so by the time this is polled
    // both are often ready — and the client is the causally earlier
    // explanation. The agent dying leaves the client-to-agent copy pending
    // on a client that is still there, so it does not tie.
    let mut write_half = None;
    let detached = tokio::select! {
        biased;
        _ = &mut to_agent => true,
        half = &mut to_client => {
            write_half = half.ok();
            false
        }
    };
    to_agent.abort();

    if detached {
        eprintln!(
            "dvadva-bridge: conn {conn}: detached from session {}; the agent (pid {}) stays up",
            entry.session, entry.pid
        );
        // The agent answers our half-close by closing its side, which ends
        // the other copy and hands the client's write half back. Bounded,
        // because an agent that will not let go must not wedge the daemon.
        write_half = match timeout(AGENT_EXIT_GRACE, &mut to_client).await {
            Ok(Ok(half)) => Some(half),
            _ => {
                to_client.abort();
                None
            }
        };
    }

    if let Some(mut wr) = write_half {
        if !detached {
            let reason = agent_gone(&entry, log.as_deref()).await;
            eprintln!("dvadva-bridge: conn {conn}: {reason}");
            let _ = write_reply(&mut wr, Reply::error(reason)).await;
        }
        // Both endings half-close explicitly rather than letting the socket
        // fall out of scope: a detach has to *read* as the end of a stream
        // at the other end, not as a connection that broke.
        let _ = wr.shutdown().await;
    }
    Ok(())
}

/// The last words a client gets when the agent's end closed on its own.
///
/// A locally spawned agent leaves its stderr tail behind (`wire_client`),
/// and this is how the supervised path keeps that parity — but only for an
/// agent *this connection* started, because that is the only one whose log
/// this daemon knows the name of. Joining somebody else's agent buys you the
/// fact and not the diagnosis.
async fn agent_gone(entry: &LiveSession, log: Option<&Path>) -> String {
    let reason = format!(
        "the agent for session {} (pid {}) closed the connection",
        entry.session, entry.pid
    );
    match log {
        Some(log) => with_log_tail(reason, log).await,
        None => reason,
    }
}

/// A reason, plus whatever the agent last wrote to its log.
async fn with_log_tail(reason: String, log: &Path) -> String {
    let tail = log_tail(log).await;
    if tail.is_empty() {
        reason
    } else {
        format!("{reason}\n{tail}")
    }
}

/// The last few lines of a log file, for a message that has to explain a
/// failure to somebody who cannot see this machine. Reads only the end of
/// it: a long-running agent's log is not a thing to load into memory to
/// quote twenty lines of.
async fn log_tail(path: &Path) -> String {
    let Ok(mut file) = tokio::fs::File::open(path).await else {
        return String::new();
    };
    let len = file.metadata().await.map(|meta| meta.len()).unwrap_or(0);
    let seeked = len > LOG_TAIL_BYTES;
    if seeked
        && file
            .seek(io::SeekFrom::End(-(LOG_TAIL_BYTES as i64)))
            .await
            .is_err()
    {
        return String::new();
    }
    let mut body = Vec::new();
    if file.read_to_end(&mut body).await.is_err() {
        return String::new();
    }

    let text = String::from_utf8_lossy(&body);
    let mut lines: Vec<&str> = text.lines().collect();
    if seeked && !lines.is_empty() {
        // The seek landed mid-line; a fragment quoted as a log line reads
        // like a corrupted message rather than a truncated one.
        lines.remove(0);
    }
    lines.retain(|line| !line.trim().is_empty());
    lines[lines.len().saturating_sub(LOG_TAIL_LINES)..].join("\n")
}

/// Prepend the daemon's default `-w` unless the caller named a work
/// directory itself. Prepending (not appending) means a caller's own `-w`
/// still wins under clap's last-one-wins even if a spelling slips past
/// [`args_name_work_dir`].
fn with_default_work_dir(args: Vec<String>, default: Option<&str>) -> Vec<String> {
    let Some(dir) = default else {
        return args;
    };
    if args_name_work_dir(&args) {
        return args;
    }
    let mut with_default = Vec::with_capacity(args.len() + 2);
    with_default.push("-w".to_string());
    with_default.push(dir.to_string());
    with_default.extend(args);
    with_default
}

/// Prepend the daemon's idle policy unless the caller stated one.
///
/// Prepended for the same reason as `-w`: a caller who names its own wins
/// under clap's last-one-wins. Unlike `-w` this is the daemon's decision by
/// default rather than only in the absence of one — a frontend has no idea
/// how many agents this machine is already holding, and the daemon is the
/// only party that can see the accumulation it causes.
fn with_idle_timeout(args: Vec<String>, seconds: u64) -> Vec<String> {
    if args_name_idle_timeout(&args) {
        return args;
    }
    let mut with_timeout = Vec::with_capacity(args.len() + 2);
    with_timeout.push("--idle-timeout".to_string());
    with_timeout.push(seconds.to_string());
    with_timeout.extend(args);
    with_timeout
}

fn args_name_idle_timeout(args: &[String]) -> bool {
    args.iter()
        .any(|arg| arg == "--idle-timeout" || arg.starts_with("--idle-timeout="))
}

/// Every spelling of the agent's work-dir flag: `-w DIR`, `-wDIR`,
/// `--work-dir DIR`, `--work-dir=DIR`.
fn args_name_work_dir(args: &[String]) -> bool {
    args.iter().any(|arg| {
        arg == "-w"
            || arg == "--work-dir"
            || arg.starts_with("--work-dir=")
            || (arg.starts_with("-w") && arg.len() > 2)
    })
}

/// The exit reason handed to the client: the agent's exit status plus
/// whatever it last wrote to stderr.
fn exit_reason(status: &str, tail: &Mutex<VecDeque<String>>) -> String {
    let reason = format!("agent exited: {status}");
    let Ok(tail) = tail.lock() else {
        return reason;
    };
    if tail.is_empty() {
        return reason;
    }
    let lines: Vec<&str> = tail.iter().map(String::as_str).collect();
    format!("{reason}\n{}", lines.join("\n"))
}

/// Forward the agent's stderr into the daemon's own stderr (tagged), where
/// the machine's operator can see panics and startup failures, and keep the
/// last few lines for the exit trailer the client gets.
fn forward_stderr(
    conn: u64,
    stderr: Option<ChildStderr>,
) -> (JoinHandle<()>, Arc<Mutex<VecDeque<String>>>) {
    let tail = Arc::new(Mutex::new(VecDeque::new()));
    let Some(stderr) = stderr else {
        return (tokio::spawn(async {}), tail);
    };
    let collector = Arc::clone(&tail);
    let task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            eprintln!("dvadva-bridge: conn {conn}: agent stderr: {line}");
            let Ok(mut tail) = collector.lock() else {
                break;
            };
            if tail.len() == STDERR_TAIL_LINES {
                tail.pop_front();
            }
            tail.push_back(line);
        }
    });
    (task, tail)
}

/// List every session of every work directory known to this machine's
/// `~/.kimi/kimi.json` — the remote twin of
/// `wire_client::session_list::list_all_sessions`, same reads, same order
/// of operations, so both sides list identically — and say which of them an
/// agent is holding right now.
///
/// `Session::list` panics on unexpected filesystem errors; the task spawn
/// turns a panic into an `Err` instead of taking the daemon down.
async fn list_sessions(registry: &Registry) -> Result<Vec<SessionEntry>, String> {
    let live = registry.list().await;

    let listing = tokio::task::spawn(async {
        let metadata = load_metadata().await;
        let mut entries = Vec::new();
        for work_dir in &metadata.work_dirs {
            for session in AgentSession::list(KaosPath::new(&work_dir.path)).await {
                entries.push(SessionEntry {
                    id: session.id,
                    title: session.title,
                    work_dir: session.work_dir.as_path().to_string_lossy().into_owned(),
                    updated_at: session.updated_at,
                    live: false,
                });
            }
        }
        entries
    });
    let mut entries = match listing.await {
        Ok(entries) => entries,
        Err(err) => return Err(format!("session listing panicked: {err}")),
    };

    let running: HashSet<&str> = live.iter().map(|entry| entry.session.as_str()).collect();
    for entry in &mut entries {
        entry.live = running.contains(entry.id.as_str());
    }

    // A session that exists only because an agent is holding it: brand new,
    // with nothing written to it yet. `Session::list` skips those — there is
    // no context file to read — and a live session nobody can see is a live
    // session nobody can attach to.
    let known: HashSet<String> = entries.iter().map(|entry| entry.id.clone()).collect();
    entries.extend(
        live.iter()
            .filter(|entry| !known.contains(&entry.session))
            .map(|entry| SessionEntry {
                id: entry.session.clone(),
                // The agent's own name for a session with nothing in it.
                title: format!("Untitled ({})", entry.session),
                work_dir: entry.work_dir.clone(),
                updated_at: entry.started_at,
                live: true,
            }),
    );
    entries.sort_by(|a, b| {
        b.updated_at
            .partial_cmp(&a.updated_at)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(entries)
}

/// Write one reply frame and flush it.
async fn write_reply<W>(writer: &mut W, reply: Reply) -> io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let frame = proto::encode(&reply);
    writer.write_all(frame.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn default_work_dir_fills_in_only_when_unset() {
        assert_eq!(
            with_default_work_dir(args(&["--session", "abc"]), Some("/home/kimi")),
            args(&["-w", "/home/kimi", "--session", "abc"])
        );
        assert_eq!(
            with_default_work_dir(args(&[]), Some("/home/kimi")),
            args(&["-w", "/home/kimi"])
        );
        // No default configured: the agent keeps its own default.
        assert_eq!(with_default_work_dir(args(&["-y"]), None), args(&["-y"]));
    }

    #[test]
    fn every_work_dir_spelling_is_recognized() {
        for named in [
            args(&["-w", "/srv"]),
            args(&["-w/srv"]),
            args(&["--work-dir", "/srv"]),
            args(&["--work-dir=/srv"]),
        ] {
            assert!(args_name_work_dir(&named), "not recognized: {named:?}");
            assert_eq!(
                with_default_work_dir(named.clone(), Some("/home/kimi")),
                named,
                "the caller's own work dir must survive"
            );
        }
        assert!(!args_name_work_dir(&args(&["--wire", "-y"])));
    }

    #[test]
    fn exit_reason_appends_the_stderr_tail() {
        let tail = Mutex::new(VecDeque::from(vec!["boom".to_string()]));
        assert_eq!(
            exit_reason("exit status: 1", &tail),
            "agent exited: exit status: 1\nboom"
        );
        let empty = Mutex::new(VecDeque::new());
        assert_eq!(
            exit_reason("exit status: 0", &empty),
            "agent exited: exit status: 0"
        );
    }
}
