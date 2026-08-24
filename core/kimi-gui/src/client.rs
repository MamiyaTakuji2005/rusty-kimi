//! Wire protocol client: spawns `kimi-agent`, speaks newline-delimited
//! JSON-RPC 2.0 over its stdio, and forwards everything to the UI thread.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};

use serde_json::{Value, json};

use kimi_agent::wire::{WireMessage, deserialize_wire_message};

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
    pub fn spawn(
        agent_bin: &str,
        agent_args: &[String],
        egui_ctx: eframe::egui::Context,
    ) -> std::io::Result<(Self, Receiver<Inbound>)> {
        let mut child = Command::new(agent_bin)
            .args(agent_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;

        let stdin = child.stdin.take().expect("child stdin is piped");
        let stdout = child.stdout.take().expect("child stdout is piped");

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
        std::thread::Builder::new()
            .name("wire-reader".into())
            .spawn(move || {
                let mut reader = BufReader::with_capacity(1024 * 1024, stdout);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) => {
                            let _ = inbound_tx
                                .send(Inbound::AgentExited("agent stdout closed".into()));
                            break;
                        }
                        Ok(_) => {
                            let trimmed = line.trim();
                            if trimmed.is_empty() {
                                continue;
                            }
                            let inbound = classify_line(trimmed);
                            let closed = inbound_tx.send(inbound).is_err();
                            egui_ctx.request_repaint();
                            if closed {
                                break;
                            }
                        }
                        Err(err) => {
                            let _ = inbound_tx
                                .send(Inbound::AgentExited(format!("read error: {err}")));
                            break;
                        }
                    }
                }
                egui_ctx.request_repaint();
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
        let id = format!("gui-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
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
    /// killing it. Called from `App::on_exit`.
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
