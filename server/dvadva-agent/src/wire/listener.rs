//! The listening transport: clients that can leave without taking the agent.
//!
//! Over stdio, "the client" and "the process" are the same thing. The pipe is
//! the session, EOF is the end, and detaching means killing. That is right
//! for the one-shot path and wrong for everything else, so this is the other
//! binding: a loopback socket that frontends attach to and detach from while
//! the agent keeps its turn, its context and its pid.
//!
//! The wire protocol is unchanged here. What changes is *lifetime*, which was
//! never part of the protocol to begin with — it was a property of the only
//! transport there was. [`serve_detachable`] runs the same
//! [`WireServer::serve_connection`] that stdio runs, and simply does not
//! treat the end of a connection as the end of the session.
//!
//! Two rules make that safe to expose, and both belong to the transport
//! rather than to the protocol:
//!
//! - **Loopback only.** An agent that accepts a `prompt` runs shell commands
//!   for whoever can reach it. Binding a non-loopback address is refused here
//!   rather than left to a config mistake; reaching an agent from another
//!   machine is `ssh -L`'s job, which is the rule `remote/PLAN.md` already
//!   sets for the bridge.
//! - **A shared secret.** Loopback still means every other account on the
//!   box. A connection must present the token from the session's token file
//!   before its bytes reach the wire server at all — before `initialize`,
//!   which is not itself a gate, since nothing stops a client from sending
//!   `prompt` first.
//!
//! Two channels say where the agent went, because they answer for two
//! different askers: the announce line on stderr, for the process that
//! spawned it and is holding its pipes, and the live-session registry
//! (`crate::live`), for everyone who did not — a second frontend, a
//! supervisor that has since restarted, a person with a terminal.
//!
//! A client that inherited this process's stdio does not do the handshake:
//! holding the pipes is a stronger claim than knowing the token, and making
//! the one-shot path carry a secret would have made every existing caller a
//! new caller.
//!
//! **How an agent that outlives its clients ever ends.** Three ways, all
//! cancelling the one token `WireServer::stop_token` hands out: a signal
//! (whoever owns the process), the wire's `shutdown` method (any attached
//! client, including one two daemons away that has no way to signal
//! anything), and the idle watch below (nobody at all). The last is what
//! keeps a machine from collecting one process per session ever opened, and
//! it is why the plainly-idle case is not the only one that counts as idle —
//! see [`WireServer::is_idle`].

use std::io::IsTerminal;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use rand::Rng;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::live::{self, LiveSession, Registration, Registry};
use crate::wire::protocol::WIRE_PROTOCOL_VERSION;
use crate::wire::server::WireServer;

/// Where `--listen` binds when it is given no address: loopback, kernel-
/// chosen port. The port is announced on stderr, which is how a supervisor
/// that spawned the agent learns where to attach.
pub const DEFAULT_LISTEN_ADDR: &str = "127.0.0.1:0";

/// The file name the token lives under, inside the session directory.
pub const TOKEN_FILE_NAME: &str = "attach.token";

/// How long a fresh connection has to present its token before it is dropped.
/// Short enough that a port scanner does not get to hold a slot, long enough
/// that a real client across a slow `ssh -L` does not lose the race.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The handshake is one small line. Anything longer is not one, and reading
/// it to find out is how a stranger gets to allocate memory here.
const HANDSHAKE_LINE_LIMIT: usize = 8 * 1024;

/// How often the idle watch asks whether anything is happening. An upper
/// bound: a timeout shorter than this is checked at its own length instead,
/// so a test can ask for two seconds and get two seconds.
const IDLE_CHECK_INTERVAL: Duration = Duration::from_secs(30);

/// What the listening transport needs and the wire protocol does not.
pub struct ListenOptions {
    pub addr: SocketAddr,
    /// The shared secret's home. Created, with a fresh token, on first use.
    pub token_file: PathBuf,
    /// Whether to serve whoever handed this process its pipes as a client
    /// too. True for the agent binary, where stdin is either a parent's pipe
    /// or nothing; false in tests, where it belongs to the harness.
    pub inherit_stdio: bool,
    /// Where to announce this agent to anybody who did not spawn it
    /// (`crate::live`). `None` is the shared registry every process on the
    /// machine reads; tests name their own so a test run does not advertise
    /// itself to the user's frontends.
    pub registry_dir: Option<PathBuf>,
    /// Stop after this long with nobody attached and nothing to do. `None`
    /// means never, which is right for an agent a person started by hand and
    /// wrong for one a supervisor started on their behalf — so the flag is
    /// off by default and the bridge daemon passes one.
    pub idle_timeout: Option<Duration>,
}

/// A bound listener with its secret already resolved.
///
/// Binding is split from serving because everything that can fail does so
/// here — the address is taken, the address is not ours, the token file is
/// unwritable — while [`Listening::serve`] runs until the process is asked to
/// stop. It also lets a caller find out which port the kernel picked.
pub struct Listening {
    listener: TcpListener,
    token: Arc<str>,
    token_file: PathBuf,
    inherit_stdio: bool,
    registry: Registry,
    idle_timeout: Option<Duration>,
}

/// Take the address and the token, or fail before anything is running.
pub async fn bind(options: ListenOptions) -> Result<Listening> {
    // Before the token file is touched, let alone bound: a listen that is
    // going to be refused should leave nothing behind that suggests it
    // nearly happened.
    refuse_non_loopback(&options)?;

    let token: Arc<str> = resolve_token(&options.token_file).await?.into();
    let listener = TcpListener::bind(options.addr)
        .await
        .with_context(|| format!("failed to listen on {}", options.addr))?;

    let registry = match options.registry_dir {
        Some(dir) => Registry::at(dir),
        None => Registry::shared(),
    };

    Ok(Listening {
        listener,
        token,
        token_file: options.token_file,
        inherit_stdio: options.inherit_stdio,
        registry,
        idle_timeout: options.idle_timeout,
    })
}

/// A connection that opened and closed without saying anything.
///
/// Its own type because it is the shape of a *probe*, not of an intrusion:
/// the live-session registry decides whether an agent is still there by
/// connecting to it (`crate::live`), and every listing would otherwise leave
/// a warning behind in this agent's log.
#[derive(Debug, thiserror::Error)]
#[error("the connection closed before its handshake")]
struct SilentClose;

/// What an attaching client sends first. `client` is for the log only: the
/// token is the whole of the decision.
#[derive(Debug, Deserialize)]
struct AttachRequest {
    #[serde(default)]
    auth: String,
    #[serde(default)]
    client: Option<String>,
}

/// Run the agent as a service that outlives the clients attached to it.
///
/// Returns when the process is asked to stop (an interrupt, or `SIGTERM` on
/// unix). A client detaching is not such an ask, which is the entire point.
pub async fn serve_detachable(server: Arc<WireServer>, options: ListenOptions) -> Result<()> {
    bind(options).await?.serve(server).await
}

impl Listening {
    /// Where clients should attach. Worth asking for when the bind address
    /// named port 0 and the kernel chose.
    pub fn addr(&self) -> SocketAddr {
        self.listener
            .local_addr()
            .unwrap_or_else(|_| SocketAddr::from(([127, 0, 0, 1], 0)))
    }

    /// The secret an attaching client must present.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Serve attached clients until the process is asked to stop.
    pub async fn serve(self, server: Arc<WireServer>) -> Result<()> {
        let addr = self.addr();
        let session_id = server.session_id();

        announce(addr, &session_id, &self.token_file);
        info!(
            "listening on {addr} for session {session_id}; token in {}",
            self.token_file.display()
        );
        // Held for exactly as long as this call runs: the entry appears when
        // the agent starts answering and is withdrawn when it stops.
        let _registration = self.publish(&server, addr, &session_id).await;

        let background = server.spawn_background();
        if self.inherit_stdio {
            serve_inherited_stdio(Arc::clone(&server));
        }

        // One token for every way this process can be asked to end: a
        // signal, an idle timeout, or a `shutdown` from an attached client.
        // The wire server owns it because the third of those arrives as a
        // wire message and has to reach the accept loop somehow.
        let shutdown = server.stop_token();
        let signals = spawn_signal_watch(shutdown.clone());
        let idle = self.spawn_idle_watch(Arc::clone(&server), shutdown.clone());
        let result = self
            .accept_loop(Arc::clone(&server), &session_id, shutdown)
            .await;

        info!("stopping the listening agent");
        server.shutdown().await;
        signals.abort();
        if let Some(idle) = idle {
            idle.abort();
        }
        background.abort();
        result
    }

    /// Stop an agent that nobody came back to.
    ///
    /// A detached agent has no natural end: its clients are gone, and the
    /// thing that gave it a lifetime — a pipe, a socket — is the thing it now
    /// outlives. Without this, every session ever attached to accumulates a
    /// process. `WireServer::is_idle` decides what counts; this only decides
    /// how long it has to keep being true.
    ///
    /// Polled rather than event-driven on purpose: idleness is a conjunction
    /// of three facts that change independently, and one clock asking is far
    /// simpler to reason about than three notifications racing.
    fn spawn_idle_watch(
        &self,
        server: Arc<WireServer>,
        shutdown: CancellationToken,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let timeout = self.idle_timeout?;
        info!("will stop after {}s idle", timeout.as_secs());
        Some(tokio::spawn(async move {
            // Check often enough that the timeout is roughly honoured and
            // rarely enough to be free; a whole minute of slack on a timeout
            // measured in minutes or hours is nobody's problem.
            let tick = timeout.min(IDLE_CHECK_INTERVAL).max(Duration::from_secs(1));
            let mut idle_for = Duration::ZERO;
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    _ = tokio::time::sleep(tick) => {}
                }
                if !server.is_idle().await {
                    idle_for = Duration::ZERO;
                    continue;
                }
                idle_for += tick;
                if idle_for >= timeout {
                    info!(
                        "nothing attached and nothing to do for {}s; stopping",
                        timeout.as_secs()
                    );
                    shutdown.cancel();
                    return;
                }
            }
        }))
    }

    /// Put this agent in the live-session registry, so that something which
    /// did not spawn it — another frontend, a restarted supervisor, a person
    /// — can still find it.
    ///
    /// Best effort on purpose. The agent is already bound and serving by
    /// now, and whoever started it has the announce line; a registry that
    /// cannot be written is a discoverability problem, not a reason to
    /// refuse to run.
    async fn publish(
        &self,
        server: &Arc<WireServer>,
        addr: SocketAddr,
        session_id: &str,
    ) -> Option<Registration> {
        let entry = LiveSession {
            session: session_id.to_string(),
            pid: std::process::id(),
            addr: addr.to_string(),
            token_file: self.token_file.clone(),
            work_dir: server.session_work_dir(),
            protocol_version: WIRE_PROTOCOL_VERSION.to_string(),
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
            started_at: live::now_seconds(),
        };
        match self.registry.register(&entry).await {
            Ok(registration) => Some(registration),
            Err(err) => {
                warn!("not listed in {}: {err:#}", self.registry.dir().display());
                None
            }
        }
    }

    async fn accept_loop(
        &self,
        server: Arc<WireServer>,
        session_id: &str,
        shutdown: CancellationToken,
    ) -> Result<()> {
        loop {
            let accepted = tokio::select! {
                _ = shutdown.cancelled() => break,
                accepted = self.listener.accept() => accepted,
            };
            let (socket, peer) = match accepted {
                Ok(pair) => pair,
                Err(err) => {
                    warn!("accept failed: {err}");
                    continue;
                }
            };
            // Belt to the bind's braces: a loopback socket should only ever
            // see loopback peers, and if that ever stops being true this is
            // where it shows up rather than inside a turn.
            if !peer.ip().is_loopback() {
                warn!("refusing {peer}: not a loopback peer");
                continue;
            }
            let server = Arc::clone(&server);
            let token = Arc::clone(&self.token);
            let session_id = session_id.to_string();
            tokio::spawn(async move {
                if let Err(err) = attach(server, socket, &token, &session_id).await {
                    // A connection that closed without saying anything is
                    // the registry's reachability probe (`crate::live`), and
                    // a listing must not fill this log.
                    if err.downcast_ref::<SilentClose>().is_some() {
                        debug!("{peer} looked and left without attaching");
                    } else {
                        warn!("{peer} did not attach: {err}");
                    }
                }
            });
        }
        Ok(())
    }
}

/// An agent takes prompts, so it runs shell commands for whoever can reach
/// it. That makes the bind address a security decision rather than a
/// preference, and this is the one place it gets made.
fn refuse_non_loopback(options: &ListenOptions) -> Result<()> {
    if options.addr.ip().is_loopback() {
        return Ok(());
    }
    bail!(
        "refusing to listen on {}: an agent takes prompts, so it only ever binds loopback. \
         Reach it from another machine with `ssh -L <port>:127.0.0.1:<port>`.",
        options.addr
    )
}

/// Tell whoever spawned us where we ended up. On stderr, not stdout: stdout
/// may be a client's wire, and this is not a wire message. Logs go to a file,
/// so this line is the one thing a parent process reliably sees.
fn announce(addr: SocketAddr, session_id: &str, token_file: &Path) {
    let payload = json!({
        "addr": addr.to_string(),
        "session": session_id,
        "pid": std::process::id(),
        "protocol_version": WIRE_PROTOCOL_VERSION,
        "token_file": token_file.display().to_string(),
    });
    eprintln!(
        "dvadva-agent: listening {}",
        serde_json::to_string(&payload).unwrap_or_default()
    );
}

/// Serve whoever handed us our pipes, if anybody did.
///
/// A parent process that spawned us with pipes is a client and gets served
/// without a token. A human who typed the command is not, and would get wire
/// JSON sprayed across their terminal, so a tty is left alone.
fn serve_inherited_stdio(server: Arc<WireServer>) {
    if std::io::stdin().is_terminal() {
        info!("stdin is a terminal, so nobody is attached over it");
        return;
    }
    tokio::spawn(async move {
        let result = server
            .serve_connection(tokio::io::stdin(), tokio::io::stdout())
            .await;
        if let Err(err) = result {
            debug!("the stdio client ended with: {err}");
        }
        // Deliberately no shutdown. On this transport the end of a
        // connection is a detach, and the session belongs to the process.
        info!("the stdio client detached; the agent stays up");
    });
}

/// Take one connection through the handshake and hand it to the wire server.
async fn attach(
    server: Arc<WireServer>,
    socket: TcpStream,
    token: &str,
    session_id: &str,
) -> Result<()> {
    // Wire traffic is many small lines that a client is waiting on.
    let _ = socket.set_nodelay(true);
    let (reader, mut writer) = tokio::io::split(socket);
    let mut reader = BufReader::new(reader);

    match handshake(&mut reader, token).await {
        Ok(client) => {
            let ok = json!({
                "auth": "ok",
                // Informational: the compatibility gate stays in
                // `initialize`, so that there is one of it. This is here so a
                // supervisor can log what it reached without a full session.
                "protocol_version": WIRE_PROTOCOL_VERSION,
                "session": session_id,
            });
            write_line(&mut writer, &ok).await?;
            info!(
                "attached {}",
                client.as_deref().unwrap_or("an unnamed client")
            );
        }
        Err(err) => {
            // Answer before closing: a client with the wrong token should be
            // told so, not left to guess at a silent disconnect. The message
            // says which check failed, never what would have passed it.
            let denied = json!({"auth": "denied", "error": err.to_string()});
            let _ = write_line(&mut writer, &denied).await;
            let _ = writer.shutdown().await;
            return Err(err);
        }
    }

    server.serve_connection(reader, writer).await
}

/// Read the first line and decide whether it may talk to the agent.
async fn handshake<R>(reader: &mut R, token: &str) -> Result<Option<String>>
where
    R: AsyncBufRead + Unpin,
{
    let line = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_handshake_line(reader))
        .await
        .map_err(|_| anyhow!("no handshake within {}s", HANDSHAKE_TIMEOUT.as_secs()))??;

    let request: AttachRequest =
        serde_json::from_str(&line).map_err(|_| anyhow!("malformed handshake"))?;
    if !tokens_match(&request.auth, token) {
        bail!("invalid token");
    }
    Ok(request.client)
}

/// Read one line, capped, leaving everything after it buffered for the wire
/// server: a client is free to pipeline its `initialize` behind the
/// handshake, and those bytes must not be eaten here.
async fn read_handshake_line<R>(reader: &mut R) -> Result<String>
where
    R: AsyncBufRead + Unpin,
{
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Err(SilentClose.into());
        }
        match available.iter().position(|byte| *byte == b'\n') {
            Some(end) => {
                line.extend_from_slice(&available[..end]);
                reader.consume(end + 1);
                break;
            }
            None => {
                let taken = available.len();
                line.extend_from_slice(available);
                reader.consume(taken);
            }
        }
        if line.len() > HANDSHAKE_LINE_LIMIT {
            bail!("the handshake line is too long to be one");
        }
    }
    Ok(String::from_utf8_lossy(&line).trim().to_string())
}

/// Compare in time that does not depend on how much of the token is right.
///
/// The lengths are compared openly — a token's length is not the secret — but
/// every byte is looked at either way, so a near miss and a wild guess take
/// the same time to refuse.
fn tokens_match(presented: &str, expected: &str) -> bool {
    let presented = presented.as_bytes();
    let expected = expected.as_bytes();
    if presented.len() != expected.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in presented.iter().zip(expected) {
        difference |= a ^ b;
    }
    difference == 0
}

/// The session's attach token, minted on first use.
///
/// Kept in a file rather than passed on the command line, where every other
/// user on the machine can read it out of the process table.
pub async fn resolve_token(path: &Path) -> Result<String> {
    if let Ok(existing) = tokio::fs::read_to_string(path).await {
        let existing = existing.trim().to_string();
        if !existing.is_empty() {
            debug!("using the attach token already in {}", path.display());
            return Ok(existing);
        }
    }
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let token = mint_token();
    tokio::fs::write(path, format!("{token}\n"))
        .await
        .with_context(|| format!("failed to write the attach token to {}", path.display()))?;
    restrict_to_owner(path).await;
    info!("minted a new attach token in {}", path.display());
    Ok(token)
}

fn mint_token() -> String {
    let bytes: [u8; 32] = rand::rng().random();
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Keep the token to its owner where the filesystem can express that. On
/// Windows it inherits the ACL of the profile directory it lives in, which is
/// the same protection the session's own transcripts get.
async fn restrict_to_owner(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = std::fs::Permissions::from_mode(0o600);
        if let Err(err) = tokio::fs::set_permissions(path, permissions).await {
            warn!("could not restrict {} to its owner: {err}", path.display());
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

async fn write_line<W>(writer: &mut W, value: &Value) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

/// Stop on the signals a supervisor uses to end a process, so that a detached
/// agent can be asked to go rather than only killed. One of three ways in;
/// the others are the wire's `shutdown` method and the idle watch, and all
/// three cancel the same token.
fn spawn_signal_watch(shutdown: CancellationToken) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            match signal(SignalKind::terminate()) {
                Ok(mut terminate) => {
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => info!("interrupted"),
                        _ = terminate.recv() => info!("terminated"),
                    }
                }
                Err(err) => {
                    warn!("cannot watch for SIGTERM: {err}");
                    let _ = tokio::signal::ctrl_c().await;
                    info!("interrupted");
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
            info!("interrupted");
        }
        shutdown.cancel();
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_only_matches_itself() {
        assert!(tokens_match("abc", "abc"));
        assert!(!tokens_match("abc", "abd"));
        assert!(!tokens_match("abc", "abcd"));
        assert!(!tokens_match("", "abc"));
        assert!(!tokens_match("abc", ""));
    }

    #[test]
    fn a_minted_token_is_not_guessable_by_being_short() {
        let token = mint_token();
        assert_eq!(token.len(), 64, "32 bytes, hex");
        assert_ne!(token, mint_token());
    }

    #[tokio::test]
    async fn a_token_file_is_minted_once_and_then_reused() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("nested").join(TOKEN_FILE_NAME);

        let first = resolve_token(&path).await.expect("mint");
        let second = resolve_token(&path).await.expect("reuse");

        assert_eq!(
            first, second,
            "a restart must not lock out attached clients"
        );
        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap().trim(),
            first
        );
    }

    #[tokio::test]
    async fn a_handshake_leaves_the_bytes_behind_it_alone() {
        let stream = b"{\"auth\":\"secret\"}\n{\"jsonrpc\":\"2.0\"}\n";
        let mut reader = BufReader::new(&stream[..]);

        handshake(&mut reader, "secret").await.expect("accepted");

        let mut rest = String::new();
        reader.read_line(&mut rest).await.expect("read the rest");
        assert_eq!(rest.trim(), "{\"jsonrpc\":\"2.0\"}");
    }

    #[tokio::test]
    async fn a_wrong_token_is_refused() {
        let stream = b"{\"auth\":\"wrong\"}\n";
        let mut reader = BufReader::new(&stream[..]);

        let err = handshake(&mut reader, "secret").await.expect_err("refused");

        assert_eq!(err.to_string(), "invalid token");
    }

    #[tokio::test]
    async fn a_handshake_that_is_not_a_handshake_is_refused() {
        let stream = b"hello?\n";
        let mut reader = BufReader::new(&stream[..]);

        let err = handshake(&mut reader, "secret").await.expect_err("refused");

        assert_eq!(err.to_string(), "malformed handshake");
    }

    #[tokio::test]
    async fn an_endless_first_line_is_dropped_rather_than_buffered() {
        let flood = vec![b'x'; HANDSHAKE_LINE_LIMIT * 2];
        let mut reader = BufReader::new(&flood[..]);

        let err = handshake(&mut reader, "secret").await.expect_err("refused");

        assert_eq!(err.to_string(), "the handshake line is too long to be one");
    }

    #[test]
    fn a_non_loopback_address_is_refused_before_anything_is_bound() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let options = ListenOptions {
            addr: "0.0.0.0:0".parse().expect("addr"),
            token_file: dir.path().join(TOKEN_FILE_NAME),
            inherit_stdio: false,
            registry_dir: Some(dir.path().join("live")),
            idle_timeout: None,
        };

        // Deliberately not built with a soul: the refusal has to come before
        // anything is bound, so nothing else is needed to reach it.
        let err = refuse_non_loopback(&options).expect_err("refused");

        assert!(err.to_string().contains("only ever binds loopback"));
        assert!(
            !dir.path().join(TOKEN_FILE_NAME).exists(),
            "a refused listen must not leave a token behind"
        );
    }
}
