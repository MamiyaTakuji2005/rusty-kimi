//! Wire protocol client: spawns `kimi-agent`, speaks newline-delimited
//! JSON-RPC 2.0 over its stdio, and hands everything to the caller through a
//! channel.
//!
//! This is the shared client for every frontend of the agent — `kimi-gui`
//! today, a terminal frontend later. It knows nothing about any UI toolkit:
//! instead it takes a `wake` hook, invoked whenever a message arrives, and
//! each frontend uses it to nudge its own event loop (egui:
//! `Context::request_repaint`; a terminal UI: whatever unblocks its poll).

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use kimi_agent::wire::{WireMessage, deserialize_wire_message};

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

pub struct WireClient {
    child: Child,
    writer_tx: Option<Sender<String>>,
    next_id: AtomicU64,
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

        let (writer_tx, writer_rx) = channel::<String>();
        std::thread::Builder::new()
            .name("wire-writer".into())
            .spawn(move || {
                let mut stdin = stdin;
                while let Ok(line) = writer_rx.recv() {
                    if stdin.write_all(line.as_bytes()).is_err() {
                        break;
                    }
                    if stdin.write_all(b"\n").is_err() {
                        break;
                    }
                    let _ = stdin.flush();
                }
                // Dropping stdin closes the agent's stdin => graceful exit.
            })
            .expect("spawn wire-writer thread");

        let (inbound_tx, inbound_rx) = channel::<Inbound>();
        let exit_tail = Arc::clone(&stderr_tail);
        std::thread::Builder::new()
            .name("wire-reader".into())
            .spawn(move || {
                let mut reader = BufReader::with_capacity(1024 * 1024, stdout);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) => {
                            let _ = inbound_tx.send(Inbound::AgentExited(with_stderr_tail(
                                "agent stdout closed",
                                &exit_tail,
                            )));
                            break;
                        }
                        Ok(_) => {
                            let trimmed = line.trim();
                            if trimmed.is_empty() {
                                continue;
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

        Ok((
            Self {
                child,
                writer_tx: Some(writer_tx),
                next_id: AtomicU64::new(1),
            },
            inbound_rx,
        ))
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

    /// Close the agent's stdin (asking it to exit) and wait briefly before
    /// killing it. Called from the frontend's exit path.
    pub fn shutdown(&mut self) {
        self.writer_tx = None; // drops the sender => writer thread exits => stdin closes
        for _ in 0..20 {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(100)),
                Err(_) => break,
            }
        }
        let _ = self.child.kill();
    }
}

impl Drop for WireClient {
    fn drop(&mut self) {
        self.writer_tx = None;
        let _ = self.child.kill();
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

    /// One known-good envelope payload; the shape is owned by `kimi-agent`'s
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
}
