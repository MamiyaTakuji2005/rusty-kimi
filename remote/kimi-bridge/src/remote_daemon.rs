//! The remote daemon: runs on the machine that hosts `kimi-agent`.
//!
//! Per connection: read the bridge header, then either spawn an agent and
//! relay bytes (the connection's lifetime is the agent's lifetime) or
//! answer a `list_sessions` query from the local `~/.kimi`.

use std::collections::VecDeque;
use std::io;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kaos::KaosPath;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{ChildStderr, Command};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use kimi_agent::metadata::load_metadata;
use kimi_agent::session::Session as AgentSession;

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

/// Monotonic connection id, only used in daemon log lines.
static CONN_ID: AtomicU64 = AtomicU64::new(1);

/// What the remote daemon needs to serve connections.
#[derive(Clone, Debug)]
pub struct Config {
    /// The `kimi-agent` binary to spawn.
    pub agent_bin: String,
    /// Work directory for agents whose spawn args name none — normally the
    /// daemon user's home directory, so a frontend on another OS never has
    /// to guess a path that exists over here. `None` leaves the agent's own
    /// default (the daemon's working directory) in place.
    pub default_work_dir: Option<String>,
}

impl Config {
    /// A config that only names the agent binary; the agent then picks its
    /// own work directory. Mostly for tests.
    pub fn new(agent_bin: impl Into<String>) -> Self {
        Self {
            agent_bin: agent_bin.into(),
            default_work_dir: None,
        }
    }

    /// Same, with a default work directory for args that name none.
    pub fn with_default_work_dir(mut self, dir: Option<String>) -> Self {
        self.default_work_dir = dir;
        self
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
                eprintln!("kimi-bridge: accept failed ({consecutive_errors}): {err}");
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
                eprintln!("kimi-bridge: {peer}: connection error: {err}");
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
        Request::ListSessions => {
            let reply = match list_sessions().await {
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
            eprintln!("kimi-bridge: conn {conn}: agent exited: {status}");
            format!("{status}")
        }
        Ok(Err(err)) => {
            eprintln!("kimi-bridge: conn {conn}: agent wait failed: {err}");
            format!("wait failed: {err}")
        }
        Err(_) => {
            eprintln!("kimi-bridge: conn {conn}: agent did not exit in time, killing");
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
            eprintln!("kimi-bridge: conn {conn}: agent stderr: {line}");
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
/// of operations, so both sides list identically.
///
/// `Session::list` panics on unexpected filesystem errors; the task spawn
/// turns a panic into an `Err` instead of taking the daemon down.
async fn list_sessions() -> Result<Vec<SessionEntry>, String> {
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
                });
            }
        }
        entries
    });
    match listing.await {
        Ok(entries) => Ok(entries),
        Err(err) => Err(format!("session listing panicked: {err}")),
    }
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
