//! The remote daemon: runs on the machine that hosts `kimi-agent`.
//!
//! Per connection: read the bridge header, then either spawn an agent and
//! relay bytes (the connection's lifetime is the agent's lifetime) or
//! answer a `list_sessions` query from the local `~/.kimi`.

use std::io;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use kaos::KaosPath;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{ChildStderr, Command};
use tokio::time::timeout;

use kimi_agent::metadata::load_metadata;
use kimi_agent::session::Session as AgentSession;

use crate::proto::{self, Reply, Request, SessionEntry};

/// How long a connection may take to send its header line.
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);

/// Grace period for the agent to exit on its own (its stdin just closed)
/// before the daemon kills it.
const AGENT_EXIT_GRACE: Duration = Duration::from_secs(5);

/// Monotonic connection id, only used in daemon log lines.
static CONN_ID: AtomicU64 = AtomicU64::new(1);

/// Serve bridge connections forever. Each connection is handled on its own
/// task; a failure ends only that connection, never the daemon.
pub async fn serve(listener: TcpListener, agent_bin: String) -> io::Result<()> {
    loop {
        let (socket, peer) = listener.accept().await?;
        let agent_bin = agent_bin.clone();
        tokio::spawn(async move {
            if let Err(err) = handle(socket, &agent_bin).await {
                eprintln!("kimi-bridge: {peer}: connection error: {err}");
            }
        });
    }
}

async fn handle(socket: TcpStream, agent_bin: &str) -> io::Result<()> {
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
        Request::Spawn { args } => spawn_relay(conn, socket, agent_bin, args).await,
        Request::ListSessions => {
            let reply = match list_sessions().await {
                Ok(entries) => Reply::sessions(entries),
                Err(err) => Reply::error(err),
            };
            write_reply(&mut socket, reply).await?;
            socket.get_mut().shutdown().await
        }
    }
}

/// Spawn the agent, acknowledge, then relay bytes until both directions
/// have closed. Close propagation is the whole contract:
///
/// - client half-closes / drops → agent stdin gets EOF → agent exits by
///   itself (this is also how a frontend asks the agent to exit),
/// - agent exits → socket write half shuts down → the client's reader sees
///   the end of the stream.
async fn spawn_relay(
    conn: u64,
    mut socket: BufReader<TcpStream>,
    agent_bin: &str,
    args: Vec<String>,
) -> io::Result<()> {
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

    forward_stderr(conn, child.stderr.take());

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
        // Agent → client direction ended: the agent exited. Half-close so
        // the client's reader observes it.
        let _ = wr.shutdown().await;
    });
    let (_, _) = tokio::join!(to_agent, to_client);

    // The agent should be exiting (stdin closed); give it a grace period
    // before killing — kill_on_drop is the belt to this suspenders.
    match timeout(AGENT_EXIT_GRACE, child.wait()).await {
        Ok(Ok(status)) => {
            eprintln!("kimi-bridge: conn {conn}: agent exited: {status}");
        }
        Ok(Err(err)) => {
            eprintln!("kimi-bridge: conn {conn}: agent wait failed: {err}");
        }
        Err(_) => {
            eprintln!("kimi-bridge: conn {conn}: agent did not exit in time, killing");
            let _ = child.kill().await;
        }
    }
    Ok(())
}

/// Forward the agent's stderr into the daemon's own stderr (tagged), where
/// the machine's operator can see panics and startup failures.
fn forward_stderr(conn: u64, stderr: Option<ChildStderr>) {
    let Some(stderr) = stderr else {
        return;
    };
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            eprintln!("kimi-bridge: conn {conn}: agent stderr: {line}");
        }
    });
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
