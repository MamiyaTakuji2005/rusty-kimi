//! Wire protocol client: spawns `dvadva-agent`, speaks newline-delimited
//! JSON-RPC 2.0 over its stdio, and hands everything to the caller through a
//! channel.
//!
//! This is the shared client for every frontend of the agent — `inkvizitor`
//! today, a terminal frontend later. It knows nothing about any UI toolkit:
//! instead it takes a `wake` hook, invoked whenever a message arrives, and
//! each frontend uses it to nudge its own event loop (egui:
//! `Context::request_repaint`; a terminal UI: whatever unblocks its poll).
//!
//! The transport is not fixed to a child process: [`WireClient::connect_tcp`]
//! reaches the same wire protocol through a `dvadva-bridge` daemon
//! ([`bridge`]) — the agent then runs on a remote machine and everything
//! except process management behaves identically.
//!
//! The crate also holds the frontend-agnostic state every client needs:
//! [`transcript`] folds the wire event stream into renderable blocks,
//! [`session_list`] lists the sessions stored under `~/.kimi` for resume
//! (locally, or through a bridge daemon), [`remotes`] reads which remotes
//! this machine knows about, and [`tunnel`] runs the ssh process one is
//! reached through.

pub mod bridge;
pub mod launch;
pub mod remotes;
pub mod session_list;
pub mod transcript;
pub mod tunnel;

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use dvadva_agent::wire::protocol::{ProtocolVersion, check_peer};
use dvadva_agent::wire::{WireMessage, deserialize_wire_message};

/// How much of the agent's stderr to keep for the "it exited" message.
const STDERR_TAIL_LINES: usize = 20;

/// Everything the agent can send us, normalized for the UI thread.
pub enum Inbound {
    /// `method: "event"` notification.
    Event(WireMessage),
    /// `method: "request"` reverse-RPC that expects a JSON-RPC response.
    Request { id: String, message: WireMessage },
    /// Response to one of our own requests.
    Response {
        id: String,
        result: Option<Value>,
        error: Option<Value>,
    },
    /// The agent process exited (or its stdout closed).
    AgentExited(String),
    /// A line we could not make sense of.
    ProtocolError(String),
}

/// Check the protocol version an agent declared in its `initialize` result.
///
/// The agent runs the same check on our declared version from its side. Both
/// ends do it because the two failures read very differently to whoever has
/// to fix them: "the agent refused us" and "the agent answers in a protocol
/// we do not know" point at the same mismatched pair of binaries, but only if
/// each end says which version it saw.
///
/// The returned version is the peer's, for gating individual features on its
/// minor; a compatible peer may still be older than some message you wanted
/// to send.
pub fn check_server_protocol(result: &Value) -> Result<ProtocolVersion, String> {
    let Some(declared) = result.get("protocol_version").and_then(|v| v.as_str()) else {
        return Err(
            "the agent's initialize result names no protocol version: it predates              version negotiation, or it is not a dvadva-agent"
                .to_string(),
        );
    };
    check_peer(declared).map_err(|err| format!("{err} (the two binaries need to match)"))
}

pub struct WireClient {
    transport: Transport,
    writer_tx: Option<Sender<String>>,
    next_id: AtomicU64,
}

/// The result of [`WireClient::attach_tcp`]: a connected client, plus the two
/// things only the daemon's ack could tell us.
pub struct Attached {
    pub client: WireClient,
    pub inbound: Receiver<Inbound>,
    /// Which session this connection landed on, when the daemon named one.
    /// For a caller that asked for a *new* session this is the only place
    /// the id appears before the wire starts; the agent repeats it in the
    /// `initialize` result, which is where a local session learns its own.
    pub session: Option<String>,
    /// Whether the daemon actually attached, rather than falling back to a
    /// `spawn`. False means this connection is the agent's whole life again,
    /// so a frontend must not tell anybody they can close the window and
    /// come back.
    pub supervised: bool,
}

/// A bridge connection whose one header frame has been acknowledged.
struct Dialled {
    stream: TcpStream,
    reader: BufReader<TcpStream>,
    reply: bridge::BridgeReply,
}

/// Whether a refusal is a daemon that does not know the op, rather than one
/// that refused it on the merits.
///
/// Matching the daemon's own words, which is normally a bad idea and is safe
/// exactly here: the only builds that produce it are ones already released,
/// whose text can no longer change. `proto::decode` fails before the daemon
/// has looked at the request at all, so `bad bridge frame` is the frame layer
/// speaking — a genuine refusal of an `attach` (no such directory, no API
/// key) comes from the op and reads nothing like it.
fn is_unknown_op(err: &std::io::Error) -> bool {
    err.to_string().contains("bad bridge frame")
}

/// What a client is connected to — how it shuts down depends on this.
enum Transport {
    /// A locally spawned agent process.
    Child(Child),
    /// A byte stream to a bridge daemon: the boxed closure half-closes the
    /// write side, which the daemon turns into the remote agent's stdin
    /// EOF (the graceful "please exit").
    Stream(Option<Box<dyn Fn() + Send + Sync>>),
    /// Already shut down.
    Closed,
}

impl WireClient {
    /// Spawn the agent with the parent's console, if it has one.
    ///
    /// For terminal frontends: the frontend and the agent share a terminal,
    /// and there is nowhere for a stray console window to come from.
    pub fn spawn<W>(
        agent_bin: &str,
        agent_args: &[String],
        wake: W,
    ) -> std::io::Result<(Self, Receiver<Inbound>)>
    where
        W: Fn() + Send + 'static,
    {
        Self::spawn_inner(agent_bin, agent_args, false, wake)
    }

    /// Spawn the agent with no console of its own.
    ///
    /// For windowed frontends: a console binary spawned by a windowed parent
    /// gets a console window of its own on Windows — one terminal popping up
    /// per session. Nothing is lost by suppressing it: the agent logs to
    /// `~/.kimi/logs`, and its stderr tail is still captured here for crash
    /// reporting.
    pub fn spawn_without_console<W>(
        agent_bin: &str,
        agent_args: &[String],
        wake: W,
    ) -> std::io::Result<(Self, Receiver<Inbound>)>
    where
        W: Fn() + Send + 'static,
    {
        Self::spawn_inner(agent_bin, agent_args, true, wake)
    }

    /// Connect to a `dvadva-bridge` daemon at `endpoint` (`host:port`) and
    /// have it spawn an agent with `agent_args`, then speak the wire
    /// protocol over the resulting byte stream.
    ///
    /// The bridge handshake happens here, synchronously: a refused spawn
    /// (unreachable daemon, missing agent binary) surfaces as a connect
    /// error instead of a confusing protocol error once the session is
    /// already running. Every step of it is bounded — frontends call this on
    /// the thread that draws their UI, and a daemon that accepts but never
    /// answers must not freeze them.
    pub fn connect_tcp<W>(
        endpoint: &str,
        agent_args: &[String],
        wake: W,
    ) -> std::io::Result<(Self, Receiver<Inbound>)>
    where
        W: Fn() + Send + 'static,
    {
        let dialled = Self::dial(endpoint, &bridge::spawn_header(agent_args), "spawn")?;
        Self::over_bridge(dialled.stream, dialled.reader, wake)
    }

    /// Attach to the agent hosting `session` on the daemon at `endpoint`,
    /// having it start one if none is live.
    ///
    /// The difference from [`Self::connect_tcp`] is lifetime, not transport.
    /// A `spawn` connection *is* the agent's life: dropping it kills the
    /// agent, which is right for a one-shot run and wrong for a window a
    /// person closes. An attached connection dropping is a detach — the turn
    /// keeps running, and coming back reaches the same process.
    ///
    /// Pass `None` for a session that does not exist yet; the daemon's ack
    /// names the one the agent minted, which is the only way to learn it
    /// before the wire starts and the only thing a later reconnect can use.
    ///
    /// Falls back to `spawn` against a daemon too old to know the op, so
    /// upgrading a frontend does not mean upgrading every remote first.
    /// [`Attached::supervised`] says which happened, because a connection
    /// that cannot be detached from must not be offered as one.
    pub fn attach_tcp<W>(
        endpoint: &str,
        session: Option<&str>,
        agent_args: &[String],
        wake: W,
    ) -> std::io::Result<Attached>
    where
        W: Fn() + Send + 'static,
    {
        let header = bridge::attach_header(session, agent_args);
        let (dialled, supervised) = match Self::dial(endpoint, &header, "attach") {
            Ok(dialled) => (dialled, true),
            // A daemon too old for the op cannot decode the frame at all and
            // says so before doing anything, so there is nothing to undo:
            // dial again and ask for the one op it does have.
            Err(err) if is_unknown_op(&err) => (
                Self::dial(endpoint, &bridge::spawn_header(agent_args), "spawn")?,
                false,
            ),
            Err(err) => return Err(err),
        };
        let session = dialled
            .reply
            .session
            .clone()
            .or_else(|| session.map(String::from));
        let (client, inbound) = Self::over_bridge(dialled.stream, dialled.reader, wake)?;
        Ok(Attached {
            client,
            inbound,
            session,
            supervised,
        })
    }

    /// Attach to a listening agent on *this* machine, from its live-session
    /// registry entry (`dvadva_agent::live`).
    ///
    /// No daemon in the path: the entry names an address and a token file,
    /// which is everything the agent's own handshake asks for. This is what
    /// makes a local session's `live` flag actionable — without it a live
    /// local session could be seen and not joined.
    pub fn attach_live<W>(
        entry: &dvadva_agent::live::LiveSession,
        wake: W,
    ) -> std::io::Result<(Self, Receiver<Inbound>)>
    where
        W: Fn() + Send + 'static,
    {
        let addr = entry
            .socket_addr()
            .ok_or_else(|| std::io::Error::other(format!("`{}` is not an address", entry.addr)))?;
        let token = std::fs::read_to_string(&entry.token_file).map_err(|err| {
            std::io::Error::other(format!(
                "cannot read the attach token in {}: {err}",
                entry.token_file.display()
            ))
        })?;

        let mut stream = TcpStream::connect_timeout(&addr, bridge::CONNECT_TIMEOUT)?;
        let _ = stream.set_nodelay(true);
        let handshake = json!({"auth": token.trim(), "client": "wire-client"}).to_string();
        stream.write_all(handshake.as_bytes())?;
        stream.write_all(b"\n")?;
        stream.flush()?;

        // One bounded line, then the wire — the same shape as the bridge
        // handshake, for the same reason: an agent that accepts and never
        // answers must not freeze the thread that draws a UI.
        let ack_handle = stream.try_clone()?;
        ack_handle.set_read_timeout(Some(bridge::HANDSHAKE_TIMEOUT))?;
        let mut reader = BufReader::new(ack_handle);
        let ack = bridge::read_frame_line(&mut reader)
            .map_err(|err| std::io::Error::other(format!("agent handshake failed: {err}")))?;
        reader.get_ref().set_read_timeout(None)?;
        let ack: Value = serde_json::from_str(&ack)
            .map_err(|err| std::io::Error::other(format!("bad agent handshake: {err}")))?;
        if ack.get("auth").and_then(Value::as_str) != Some("ok") {
            let reason = ack
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("the agent refused the attach token");
            return Err(std::io::Error::other(format!(
                "the agent for session {} refused us: {reason}",
                entry.session
            )));
        }

        Self::over_bridge(stream, reader, wake)
    }

    /// Dial a bridge daemon, send one header frame, and read its ack.
    fn dial(endpoint: &str, header: &str, op: &str) -> std::io::Result<Dialled> {
        let mut stream =
            bridge::connect(endpoint, bridge::CONNECT_TIMEOUT).map_err(std::io::Error::other)?;

        stream.write_all(header.as_bytes())?;
        stream.write_all(b"\n")?;
        stream.flush()?;

        // Exactly one reply frame before the relay starts. The timeout lives
        // on the handle the reader thread will keep using, so it has to come
        // back off again once the handshake is done — the wire stream itself
        // is idle for as long as the user is thinking.
        let ack_handle = stream.try_clone()?;
        ack_handle.set_read_timeout(Some(bridge::HANDSHAKE_TIMEOUT))?;
        let mut reader = BufReader::new(ack_handle);
        let ack = bridge::read_frame_line(&mut reader).map_err(|err| {
            std::io::Error::other(format!("bridge `{endpoint}` handshake failed: {err}"))
        })?;
        reader.get_ref().set_read_timeout(None)?;
        let reply = bridge::decode_reply(&ack)
            .map_err(|err| std::io::Error::other(format!("bad bridge handshake: {err}")))?;
        if !reply.ok {
            let reason = reply
                .error
                .unwrap_or_else(|| format!("bridge refused {op}"));
            return Err(std::io::Error::other(format!(
                "bridge `{endpoint}` refused {op}: {reason}"
            )));
        }
        Ok(Dialled {
            stream,
            reader,
            reply,
        })
    }

    /// Wire up a stream whose handshake is already done, whichever kind it
    /// was. The reader is the one the ack was read from: it may have
    /// buffered agent output pipelined behind that line, and dropping it
    /// would drop those bytes.
    fn over_bridge<W>(
        stream: TcpStream,
        reader: BufReader<TcpStream>,
        wake: W,
    ) -> std::io::Result<(Self, Receiver<Inbound>)>
    where
        W: Fn() + Send + 'static,
    {
        let reader: Box<dyn BufRead + Send> = Box::new(reader);
        let writer: Box<dyn Write + Send> = Box::new(stream.try_clone()?);
        // Half-close on shutdown: on a `spawn` connection the daemon turns it
        // into the agent's stdin EOF (the graceful "please exit"); on an
        // attached one it is simply this client leaving.
        let shutdown_handle = stream;
        let half_close = Box::new(move || {
            let _ = shutdown_handle.shutdown(Shutdown::Write);
        });

        let (writer_tx, inbound_rx) =
            Self::start_io(reader, writer, None, "remote connection closed", wake);
        Ok((
            Self {
                transport: Transport::Stream(Some(half_close)),
                writer_tx: Some(writer_tx),
                next_id: AtomicU64::new(1),
            },
            inbound_rx,
        ))
    }

    fn spawn_inner<W>(
        agent_bin: &str,
        agent_args: &[String],
        hide_console: bool,
        wake: W,
    ) -> std::io::Result<(Self, Receiver<Inbound>)>
    where
        W: Fn() + Send + 'static,
    {
        let mut command = Command::new(agent_bin);
        command
            .args(agent_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Piped, not inherited: the tail is worth keeping to explain a
            // crash even when the parent could have inherited it.
            .stderr(Stdio::piped());
        // `cfg!` keeps the operand referenced on every platform, so there is
        // no unused-parameter warning on non-Windows builds.
        if cfg!(windows) && hide_console {
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                command.creation_flags(CREATE_NO_WINDOW);
            }
        }
        let mut child = command.spawn()?;

        let stdin = child.stdin.take().expect("child stdin is piped");
        let stdout = child.stdout.take().expect("child stdout is piped");
        let stderr = child.stderr.take().expect("child stderr is piped");

        // A startup failure (bad config, missing credentials) deserves an
        // explanation even when nobody is watching a console.
        let stderr_tail = Arc::new(Mutex::new(VecDeque::new()));
        let collector = Arc::clone(&stderr_tail);
        std::thread::Builder::new()
            .name("wire-stderr".into())
            .spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    let Ok(mut tail) = collector.lock() else {
                        break;
                    };
                    if tail.len() == STDERR_TAIL_LINES {
                        tail.pop_front();
                    }
                    tail.push_back(line);
                }
            })
            .expect("spawn wire-stderr thread");

        let reader: Box<dyn BufRead + Send> =
            Box::new(BufReader::with_capacity(1024 * 1024, stdout));
        let writer: Box<dyn Write + Send> = Box::new(stdin);
        let (writer_tx, inbound_rx) = Self::start_io(
            reader,
            writer,
            Some(stderr_tail),
            "agent stdout closed",
            wake,
        );

        Ok((
            Self {
                transport: Transport::Child(child),
                writer_tx: Some(writer_tx),
                next_id: AtomicU64::new(1),
            },
            inbound_rx,
        ))
    }

    /// Wire up the writer/reader threads around any duplex byte stream.
    /// Both ends must be line-oriented: the wire protocol is
    /// newline-delimited JSON regardless of the transport.
    fn start_io<W>(
        reader: Box<dyn BufRead + Send>,
        writer: Box<dyn Write + Send>,
        stderr_tail: Option<Arc<Mutex<VecDeque<String>>>>,
        eof_reason: &'static str,
        wake: W,
    ) -> (Sender<String>, Receiver<Inbound>)
    where
        W: Fn() + Send + 'static,
    {
        // An empty tail for stream transports (no child stderr to keep).
        let exit_tail = stderr_tail.unwrap_or_else(|| Arc::new(Mutex::new(VecDeque::new())));

        let (writer_tx, writer_rx) = channel::<String>();
        std::thread::Builder::new()
            .name("wire-writer".into())
            .spawn(move || {
                let mut writer = writer;
                while let Ok(line) = writer_rx.recv() {
                    if writer.write_all(line.as_bytes()).is_err() {
                        break;
                    }
                    if writer.write_all(b"\n").is_err() {
                        break;
                    }
                    let _ = writer.flush();
                }
                // Dropping the writer closes the peer's stdin => graceful
                // exit (for a socket, dropping only this handle does *not*
                // close the connection — `shutdown()` on the client is what
                // half-closes it).
            })
            .expect("spawn wire-writer thread");

        let (inbound_tx, inbound_rx) = channel::<Inbound>();
        std::thread::Builder::new()
            .name("wire-reader".into())
            .spawn(move || {
                let mut reader = reader;
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) => {
                            let _ = inbound_tx.send(Inbound::AgentExited(with_stderr_tail(
                                eof_reason, &exit_tail,
                            )));
                            break;
                        }
                        Ok(_) => {
                            let trimmed = line.trim();
                            if trimmed.is_empty() {
                                continue;
                            }
                            // A bridge daemon appends one trailer frame after
                            // the remote agent's output ends, carrying the
                            // exit status and its stderr tail. It is the last
                            // line of the stream, and the only non-wire line
                            // either transport ever produces.
                            if let Some(reason) = bridge::exit_trailer(trimmed) {
                                let _ = inbound_tx.send(Inbound::AgentExited(reason));
                                break;
                            }
                            let inbound = classify_line(trimmed);
                            let closed = inbound_tx.send(inbound).is_err();
                            wake();
                            if closed {
                                break;
                            }
                        }
                        Err(err) => {
                            let _ = inbound_tx.send(Inbound::AgentExited(with_stderr_tail(
                                &format!("read error: {err}"),
                                &exit_tail,
                            )));
                            break;
                        }
                    }
                }
                wake();
            })
            .expect("spawn wire-reader thread");

        (writer_tx, inbound_rx)
    }

    /// Send a JSON-RPC request; returns the generated id.
    pub fn send_request(&self, method: &str, params: Value) -> String {
        let id = format!("client-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        self.send_raw(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        id
    }

    /// Answer a reverse-RPC request from the agent with a success result.
    pub fn respond_result(&self, id: &str, result: Value) {
        self.send_raw(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }));
    }

    /// Answer a reverse-RPC request from the agent with an error.
    pub fn respond_error(&self, id: &str, code: i64, message: &str) {
        self.send_raw(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message},
        }));
    }

    fn send_raw(&self, value: Value) {
        if let Some(tx) = &self.writer_tx {
            let _ = tx.send(value.to_string());
        }
    }

    /// Ask the peer to exit and wait briefly before killing it. Called from
    /// the frontend's exit path.
    ///
    /// - child: close stdin, wait up to 2 s, kill,
    /// - stream: half-close the write side (the remote agent sees stdin EOF
    ///   and exits on its own).
    pub fn shutdown(&mut self) {
        self.writer_tx = None; // drops the sender => writer thread exits => stdin closes
        match std::mem::replace(&mut self.transport, Transport::Closed) {
            Transport::Child(mut child) => {
                for _ in 0..20 {
                    match child.try_wait() {
                        Ok(Some(_)) => return,
                        Ok(None) => std::thread::sleep(std::time::Duration::from_millis(100)),
                        Err(_) => break,
                    }
                }
                let _ = child.kill();
            }
            Transport::Stream(half_close) => {
                if let Some(half_close) = half_close {
                    half_close();
                }
            }
            Transport::Closed => {}
        }
    }
}

impl Drop for WireClient {
    fn drop(&mut self) {
        match std::mem::replace(&mut self.transport, Transport::Closed) {
            Transport::Child(mut child) => {
                self.writer_tx = None;
                let _ = child.kill();
            }
            // Half-close (not abort): the remote agent may still be
            // streaming its final output into our reader.
            Transport::Stream(half_close) => {
                if let Some(half_close) = half_close {
                    half_close();
                }
            }
            Transport::Closed => {}
        }
    }
}

/// Attach the agent's last stderr lines to an exit reason. Its own logs go to
/// a file, so anything here is a panic or a failure to get that far.
fn with_stderr_tail(reason: &str, tail: &Mutex<VecDeque<String>>) -> String {
    let Ok(tail) = tail.lock() else {
        return reason.to_string();
    };
    if tail.is_empty() {
        return reason.to_string();
    }
    let lines: Vec<&str> = tail.iter().map(String::as_str).collect();
    format!("{reason}\n{}", lines.join("\n"))
}

fn classify_line(line: &str) -> Inbound {
    let value: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(err) => return Inbound::ProtocolError(format!("invalid JSON: {err}")),
    };
    let method = value.get("method").and_then(|m| m.as_str());
    let id = value.get("id").and_then(|i| i.as_str()).map(str::to_string);
    match (method, id) {
        (Some("event"), _) => match value.get("params") {
            Some(params) => match deserialize_wire_message(params.clone()) {
                Ok(msg) => Inbound::Event(msg),
                Err(err) => Inbound::ProtocolError(format!("bad event payload: {err}")),
            },
            None => Inbound::ProtocolError("event without params".into()),
        },
        (Some("request"), Some(id)) => match value.get("params") {
            Some(params) => match deserialize_wire_message(params.clone()) {
                Ok(msg) => Inbound::Request { id, message: msg },
                Err(err) => Inbound::ProtocolError(format!("bad request payload: {err}")),
            },
            None => Inbound::ProtocolError("request without params".into()),
        },
        (Some(other), _) => Inbound::ProtocolError(format!("unexpected method: {other}")),
        (None, Some(id)) => Inbound::Response {
            id,
            result: value.get("result").cloned(),
            error: value.get("error").cloned(),
        },
        (None, None) => Inbound::ProtocolError(format!("unclassifiable line: {line}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One known-good envelope payload; the shape is owned by `dvadva-agent`'s
    /// own wire tests, this only needs *a* valid message.
    const TURN_BEGIN: &str = r#"{"type":"TurnBegin","payload":{"user_input":"hi"}}"#;

    #[test]
    fn classifies_events_and_requests() {
        let event = format!(r#"{{"jsonrpc":"2.0","method":"event","params":{TURN_BEGIN}}}"#);
        assert!(matches!(classify_line(&event), Inbound::Event(_)));

        let request = format!(
            r#"{{"jsonrpc":"2.0","method":"request","id":"agent-1","params":{TURN_BEGIN}}}"#
        );
        let Inbound::Request { id, .. } = classify_line(&request) else {
            panic!("expected a request");
        };
        assert_eq!(id, "agent-1");
    }

    #[test]
    fn the_server_protocol_gate_reads_the_initialize_result() {
        // What both frontends call before trusting anything else in the
        // result. The three outcomes have to stay distinguishable.
        let ok = json!({"protocol_version": "1.2", "server": {"name": "Kimi"}});
        assert!(check_server_protocol(&ok).is_ok());

        let newer = json!({"protocol_version": "1.7"});
        assert_eq!(check_server_protocol(&newer).unwrap().minor, 7);

        let foreign = json!({"protocol_version": "2.0"});
        let err = check_server_protocol(&foreign).unwrap_err();
        assert!(err.contains("2.0"), "{err}");
        assert!(err.contains("binaries"), "{err}");

        // An agent too old to declare one at all, or something that is not an
        // agent: must not be reported as an incompatible protocol.
        let silent = json!({"server": {"name": "Kimi"}});
        let err = check_server_protocol(&silent).unwrap_err();
        assert!(err.contains("names no protocol version"), "{err}");
    }

    #[test]
    fn classifies_responses() {
        let ok = r#"{"jsonrpc":"2.0","id":"client-1","result":{"ok":true}}"#;
        let Inbound::Response { id, result, error } = classify_line(ok) else {
            panic!("expected a response");
        };
        assert_eq!(id, "client-1");
        assert!(error.is_none());
        assert_eq!(
            result.and_then(|r| r.get("ok").and_then(Value::as_bool)),
            Some(true)
        );
    }

    #[test]
    fn classifies_protocol_garbage() {
        assert!(matches!(
            classify_line("not json"),
            Inbound::ProtocolError(_)
        ));
        assert!(matches!(
            classify_line(r#"{"jsonrpc":"2.0","method":"event"}"#),
            Inbound::ProtocolError(_)
        ));
        assert!(matches!(
            classify_line(r#"{"jsonrpc":"2.0","method":"event","params":{"type":"Nope"}}"#),
            Inbound::ProtocolError(_)
        ));
        assert!(matches!(
            classify_line(r#"{"jsonrpc":"2.0","method":"surprise"}"#),
            Inbound::ProtocolError(_)
        ));
        assert!(matches!(
            classify_line(r#"{"jsonrpc":"2.0"}"#),
            Inbound::ProtocolError(_)
        ));
    }

    // --- connect_tcp (fake bridge daemon on loopback) ----------------------

    /// A minimal in-test bridge daemon: reads the header line, answers
    /// `handler(header)`, then closes the socket. Returns the endpoint.
    fn fake_daemon<F>(handler: F) -> String
    where
        F: FnOnce(&str) -> String + Send + 'static,
    {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::Builder::new()
            .name("fake-bridge".into())
            .spawn(move || {
                let (sock, _) = listener.accept().unwrap();
                let mut writer = sock.try_clone().unwrap();
                let mut reader = BufReader::new(sock);
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                writeln!(writer, "{}", handler(header.trim_end())).unwrap();
                // Dropping both halves closes the connection.
            })
            .unwrap();
        addr
    }

    /// Same, but answering every connection rather than one — for the cases
    /// where the client dials twice (an `attach` refused by a daemon that has
    /// no such op, then the `spawn` it falls back to).
    fn fake_daemon_serving<F>(handler: F) -> String
    where
        F: Fn(&str) -> String + Send + Sync + 'static,
    {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::Builder::new()
            .name("fake-bridge".into())
            .spawn(move || {
                for sock in listener.incoming() {
                    let Ok(sock) = sock else { break };
                    let mut writer = sock.try_clone().unwrap();
                    let mut reader = BufReader::new(sock);
                    let mut header = String::new();
                    if reader.read_line(&mut header).is_err() {
                        continue;
                    }
                    let _ = writeln!(writer, "{}", handler(header.trim_end()));
                }
            })
            .unwrap();
        addr
    }

    /// A fake daemon that runs an arbitrary script against the connection's
    /// reader and writer, for the cases a one-shot reply cannot express
    /// (a trailer after the ack, a peer that never answers).
    fn fake_daemon_script<F>(script: F) -> String
    where
        F: FnOnce(&mut BufReader<std::net::TcpStream>, &mut std::net::TcpStream) + Send + 'static,
    {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::Builder::new()
            .name("fake-bridge".into())
            .spawn(move || {
                let (sock, _) = listener.accept().unwrap();
                let mut writer = sock.try_clone().unwrap();
                let mut reader = BufReader::new(sock);
                script(&mut reader, &mut writer);
            })
            .unwrap();
        addr
    }

    #[test]
    fn connect_tcp_handshake_round_trip_and_stream_eof() {
        // A scripted peer: ack the spawn, answer the client's first request,
        // then close — the client must see the response, then a clean
        // stream EOF (the peer drains our request first; closing with
        // unread data would RST on Windows instead of FIN).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = listener.local_addr().unwrap().to_string();
        std::thread::Builder::new()
            .name("fake-bridge".into())
            .spawn(move || {
                let (sock, _) = listener.accept().unwrap();
                let mut writer = sock.try_clone().unwrap();
                let mut reader = BufReader::new(sock);
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                let header = header.trim_end();
                assert!(header.starts_with("BRIDGE1 "), "header: {header}");
                assert!(header.contains(r#""op":"spawn""#), "header: {header}");
                assert!(
                    header.contains(r#""args":["-w","/remote"]"#),
                    "header: {header}"
                );
                writeln!(writer, r#"BRIDGE1 {{"ok":true}}"#).unwrap();

                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                assert!(request.contains(r#""method":"initialize""#), "{request}");
                writeln!(
                    writer,
                    r#"{{"jsonrpc":"2.0","id":"client-1","result":{{"server":"fake"}}}}"#
                )
                .unwrap();
                writer.flush().unwrap();
                // Both halves drop here: the client's reader sees EOF.
            })
            .unwrap();

        let wake_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = std::sync::Arc::clone(&wake_count);
        let (mut client, inbound) =
            WireClient::connect_tcp(&endpoint, &["-w".into(), "/remote".into()], move || {
                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            })
            .expect("connect");

        let id = client.send_request("initialize", json!({}));
        assert_eq!(id, "client-1");

        let next = || {
            inbound
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("inbound message")
        };
        match next() {
            Inbound::Response { id, result, .. } => {
                assert_eq!(id, "client-1");
                let server = result
                    .as_ref()
                    .and_then(|r| r.get("server"))
                    .and_then(Value::as_str);
                assert_eq!(server, Some("fake"));
            }
            _ => panic!("expected the JSON-RPC response first"),
        }
        match next() {
            Inbound::AgentExited(reason) => {
                assert!(reason.contains("remote connection closed"), "{reason}")
            }
            _ => panic!("expected AgentExited after the daemon closed the stream"),
        }

        // Shutdown half-closes; nothing may panic on a closed stream.
        client.shutdown();
        assert!(wake_count.load(std::sync::atomic::Ordering::Relaxed) > 0);
    }

    #[test]
    fn attach_tcp_names_the_session_the_daemon_gave_it() {
        // The case a reconnect depends on: the caller asked for a *new*
        // session, and the ack is the only place its id appears before the
        // wire starts.
        let endpoint = fake_daemon(|header| {
            assert!(header.contains(r#""op":"attach""#), "{header}");
            r#"BRIDGE1 {"ok":true,"session":"minted-by-the-agent"}"#.to_string()
        });
        let attached = WireClient::attach_tcp(&endpoint, None, &[], || {}).expect("attach");
        assert_eq!(attached.session.as_deref(), Some("minted-by-the-agent"));
        assert!(attached.supervised);
    }

    #[test]
    fn attach_tcp_keeps_the_session_it_asked_for_when_the_ack_is_silent() {
        // A daemon that acknowledges without naming one: the caller already
        // knew, and losing the id would cost the reconnect its target.
        let endpoint = fake_daemon(|_| r#"BRIDGE1 {"ok":true}"#.to_string());
        let attached =
            WireClient::attach_tcp(&endpoint, Some("known-already"), &[], || {}).expect("attach");
        assert_eq!(attached.session.as_deref(), Some("known-already"));
    }

    #[test]
    fn attach_tcp_falls_back_to_spawn_against_a_daemon_that_has_no_attach() {
        // A frame-protocol 1.0 daemon cannot decode the op at all. Upgrading
        // a frontend must not require upgrading every remote first — but the
        // session that results is not detachable, and says so.
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let endpoint = fake_daemon_serving(move |header| {
            recorder.lock().unwrap().push(header.to_string());
            if header.contains(r#""op":"attach""#) {
                r#"BRIDGE1 {"ok":false,"error":"bad bridge frame: unknown variant `attach`"}"#
                    .to_string()
            } else {
                r#"BRIDGE1 {"ok":true}"#.to_string()
            }
        });

        let attached = WireClient::attach_tcp(&endpoint, Some("s"), &[], || {}).expect("attach");

        assert!(
            !attached.supervised,
            "a spawn fallback must not be sold as an attach"
        );
        let headers = seen.lock().unwrap().clone();
        assert_eq!(headers.len(), 2, "attach then spawn: {headers:?}");
        assert!(headers[1].contains(r#""op":"spawn""#), "{headers:?}");
    }

    #[test]
    fn a_refusal_on_the_merits_is_not_mistaken_for_an_old_daemon() {
        // The fallback must not swallow a real diagnosis. A daemon that
        // understood the op and refused it says why, and that reason is what
        // the user needs to read.
        let endpoint = fake_daemon(|_| {
            r#"BRIDGE1 {"ok":false,"error":"the agent exited before it started listening (exit status: 2)"}"#
                .to_string()
        });
        let err = match WireClient::attach_tcp(&endpoint, None, &[], || {}) {
            Ok(_) => panic!("attach must be refused"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("exit status: 2"), "{err}");
    }

    #[test]
    fn connect_tcp_surfaces_refused_spawn() {
        let endpoint = fake_daemon(|_| {
            r#"BRIDGE1 {"ok":false,"error":"failed to spawn agent `x`: not found"}"#.to_string()
        });
        let err = match WireClient::connect_tcp(&endpoint, &[], || {}) {
            Ok(_) => panic!("spawn must be refused"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("not found"), "{err}");
        assert!(err.to_string().contains("refused spawn"), "{err}");
    }

    #[test]
    fn connect_tcp_surfaces_the_exit_trailer() {
        // The daemon's last word after the remote agent dies: the client
        // must report *that* reason, not a bare "connection closed".
        let endpoint = fake_daemon_script(|reader, writer| {
            let mut header = String::new();
            reader.read_line(&mut header).unwrap();
            writeln!(writer, r#"BRIDGE1 {{"ok":true}}"#).unwrap();
            writeln!(
                writer,
                r#"BRIDGE1 {{"ok":false,"error":"agent exited: exit status: 2\nwork dir does not exist"}}"#
            )
            .unwrap();
            writer.flush().unwrap();
        });
        let (_client, inbound) = WireClient::connect_tcp(&endpoint, &[], || {}).expect("connect");
        match inbound
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("inbound message")
        {
            Inbound::AgentExited(reason) => {
                assert!(reason.contains("exit status: 2"), "{reason}");
                assert!(reason.contains("work dir does not exist"), "{reason}");
            }
            _ => panic!("expected AgentExited carrying the trailer's reason"),
        }
    }

    #[test]
    fn connect_tcp_times_out_a_silent_daemon() {
        // A daemon that accepts and then says nothing: the frontend calls
        // this on its UI thread, so it must come back with an error rather
        // than hang. The wait is shortened here to keep the test quick.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = listener.local_addr().unwrap().to_string();
        let (done_tx, done_rx) = channel::<()>();
        std::thread::Builder::new()
            .name("silent-bridge".into())
            .spawn(move || {
                let (_sock, _) = listener.accept().unwrap();
                // Hold the connection open until the client has given up.
                let _ = done_rx.recv_timeout(std::time::Duration::from_secs(30));
            })
            .unwrap();

        let mut stream = bridge::connect(&endpoint, bridge::CONNECT_TIMEOUT).expect("connect");
        stream
            .set_read_timeout(Some(std::time::Duration::from_millis(250)))
            .unwrap();
        let err = bridge::read_frame_line(&mut BufReader::new(&mut stream)).unwrap_err();
        assert!(err.contains("did not answer"), "{err}");
        let _ = done_tx.send(());
    }

    #[test]
    fn connect_tcp_reports_a_daemon_that_says_nothing_at_all() {
        // Accept, then close without a frame: the message must name that,
        // not "missing BRIDGE1 prefix" against an empty line.
        let endpoint = fake_daemon_script(|reader, _writer| {
            let mut header = String::new();
            reader.read_line(&mut header).unwrap();
        });
        let err = match WireClient::connect_tcp(&endpoint, &[], || {}) {
            Ok(_) => panic!("handshake must fail"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("without replying"), "{err}");
    }

    #[test]
    fn read_frame_line_caps_an_endless_peer() {
        // Pointed at something that is not a bridge daemon (an HTTP server,
        // a log stream), the client must stop rather than buffer forever.
        let endpoint = fake_daemon_script(|reader, writer| {
            let mut header = String::new();
            reader.read_line(&mut header).unwrap();
            // One endless line: no newline in sight, ever.
            let chunk = "x".repeat(4096);
            for _ in 0..32 {
                if write!(writer, "{chunk}").is_err() {
                    return;
                }
            }
            let _ = writer.flush();
        });
        let mut stream = bridge::connect(&endpoint, bridge::CONNECT_TIMEOUT).expect("connect");
        writeln!(stream, "BRIDGE1 {{\"op\":\"list_sessions\"}}").unwrap();
        let err = bridge::read_frame_line(&mut BufReader::new(&mut stream)).unwrap_err();
        assert!(err.contains("size limit"), "{err}");
    }

    #[test]
    fn connect_tcp_reports_unreachable_daemon() {
        // Bind and immediately drop: a port with no listener.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = listener.local_addr().unwrap().to_string();
        drop(listener);
        let err = match WireClient::connect_tcp(&endpoint, &[], || {}) {
            Ok(_) => panic!("connect must fail"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("failed to connect"), "{err}");
    }
}
