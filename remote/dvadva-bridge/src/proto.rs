//! The bridge's own control protocol — the only bytes either daemon ever
//! parses. Everything after the header line is an opaque byte stream (the
//! dvadva-agent wire protocol), relayed without inspection.
//!
//! Frame shape: one `\n`-terminated UTF-8 line per frame,
//!
//! ```text
//! BRIDGE1 {"op":"spawn","args":["-w","/srv/proj"]}     // client → daemon
//! BRIDGE1 {"op":"list_sessions"}                        // client → daemon
//! BRIDGE1 {"op":"version"}                              // client → daemon
//! BRIDGE1 {"ok":true}                                   // daemon → client
//! BRIDGE1 {"ok":false,"error":"…"}                      // daemon → client
//! BRIDGE1 {"ok":true,"sessions":[…]}                    // daemon → client
//! BRIDGE1 {"ok":true,"version":"1.8.0"}                 // daemon → client
//! ```
//!
//! The `BRIDGE1 ` prefix keeps daemon frames trivially distinguishable from
//! any pipelined wire-protocol JSON: the daemons only parse a line that
//! starts with the magic, and an agent's first message never does. The
//! digit is the protocol version — bump it (and update both halves) if the
//! frame set ever changes.
//!
//! The client-side twin of this framing lives in
//! `client/wire-client/src/bridge.rs`; a dev-dependency test there asserts
//! the two stay byte-compatible.

use std::io;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::AsyncBufReadExt;

/// Magic prefix of every bridge frame.
pub const MAGIC: &str = "BRIDGE1";

/// Upper bound on a frame line. Real frames are a few hundred bytes; the
/// cap keeps a hostile or confused client from buffering unbounded memory.
pub const MAX_LINE_BYTES: usize = 64 * 1024;

/// A parsed bridge request — the first line of a connection.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Spawn an agent with these CLI arguments (verbatim, e.g. `-w`,
    /// `--session`), then relay bytes both ways until either side closes.
    Spawn { args: Vec<String> },
    /// List the sessions visible on the machine the daemon runs on.
    ListSessions,
    /// Liveness probe: answer with the daemon's version and close. Spawns
    /// nothing and touches no disk, so a UI can poll it on a timer to tell
    /// "the tunnel is up but nothing is listening" from "the daemon is
    /// there" — there is no long-lived connection to observe otherwise,
    /// since every session dials its own.
    Version,
}

/// One entry of a `list_sessions` reply — the same shape as
/// `wire_client::session_list::ResumeEntry` (kept in sync by the e2e
/// tests).
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionEntry {
    pub id: String,
    pub title: String,
    pub work_dir: String,
    pub updated_at: f64,
}

/// Every daemon reply: exactly one line, then the connection either closes
/// (`list_sessions`, errors) or switches to opaque relay (`spawn` ack).
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sessions: Option<Vec<SessionEntry>>,
    /// The daemon's own version, on a `version` reply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

impl Reply {
    /// The `spawn` acknowledgement: the agent exists, relay starts now.
    pub fn spawn_ok() -> Self {
        Self {
            ok: true,
            error: None,
            sessions: None,
            version: None,
        }
    }

    /// A `list_sessions` result.
    pub fn sessions(entries: Vec<SessionEntry>) -> Self {
        Self {
            ok: true,
            error: None,
            sessions: Some(entries),
            version: None,
        }
    }

    /// A `version` result: this daemon is alive and speaks BRIDGE1.
    pub fn version(version: impl Into<String>) -> Self {
        Self {
            ok: true,
            error: None,
            sessions: None,
            version: Some(version.into()),
        }
    }

    /// A failure the client should see (bad frame, spawn failure, …).
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(message.into()),
            sessions: None,
            version: None,
        }
    }
}

/// Encode a frame line (without the trailing newline).
pub fn encode<T: Serialize>(value: &T) -> String {
    let json = serde_json::to_string(value).expect("bridge frames always serialize");
    format!("{MAGIC} {json}")
}

/// Parse a frame line produced by [`encode`].
pub fn decode<T: DeserializeOwned>(line: &str) -> Result<T, String> {
    let json = line
        .strip_prefix(MAGIC)
        .map(str::trim_start)
        .ok_or_else(|| "not a bridge frame (missing BRIDGE1 prefix)".to_string())?;
    serde_json::from_str(json).map_err(|err| format!("bad bridge frame: {err}"))
}

/// Read one `\n`-terminated frame line, bounded by [`MAX_LINE_BYTES`].
///
/// Hand-rolled over `fill_buf`/`consume` rather than `read_until` so that
/// (a) the size cap actually applies, and (b) bytes a client pipelined
/// *after* the newline stay buffered in the reader for whoever owns the
/// stream next. Errors on close-before-delimiter: the caller treats that
/// as "no usable frame".
pub async fn read_line<R>(reader: &mut R) -> io::Result<String>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed before a bridge frame",
            ));
        }
        match available.iter().position(|&b| b == b'\n') {
            Some(pos) => {
                buf.extend_from_slice(&available[..pos]);
                reader.consume(pos + 1);
                break;
            }
            None => {
                let len = available.len();
                buf.extend_from_slice(available);
                reader.consume(len);
                if buf.len() > MAX_LINE_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "bridge frame exceeds size limit",
                    ));
                }
            }
        }
    }
    let line = String::from_utf8_lossy(&buf);
    Ok(line.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let spawn = Request::Spawn {
            args: vec!["-w".into(), "/srv".into()],
        };
        let encoded = encode(&spawn);
        assert!(encoded.starts_with("BRIDGE1 "), "magic prefix: {encoded}");
        assert_eq!(decode::<Request>(&encoded).unwrap(), spawn);

        let list = encode(&Request::ListSessions);
        assert_eq!(decode::<Request>(&list).unwrap(), Request::ListSessions);

        let version = encode(&Request::Version);
        assert_eq!(version, r#"BRIDGE1 {"op":"version"}"#);
        assert_eq!(decode::<Request>(&version).unwrap(), Request::Version);
    }

    #[test]
    fn replies_round_trip() {
        let ok = encode(&Reply::spawn_ok());
        assert_eq!(decode::<Reply>(&ok).unwrap(), Reply::spawn_ok());

        let sessions = Reply::sessions(vec![SessionEntry {
            id: "abc".into(),
            title: "t (abc)".into(),
            work_dir: "/w".into(),
            updated_at: 1.5,
        }]);
        let encoded = encode(&sessions);
        assert_eq!(decode::<Reply>(&encoded).unwrap(), sessions);

        let error = Reply::error("boom");
        assert_eq!(decode::<Reply>(&encode(&error)).unwrap(), error);

        let version = Reply::version("1.8.0");
        assert_eq!(encode(&version), r#"BRIDGE1 {"ok":true,"version":"1.8.0"}"#);
        assert_eq!(decode::<Reply>(&encode(&version)).unwrap(), version);
    }

    #[test]
    fn unknown_op_and_missing_magic_are_errors() {
        assert!(decode::<Request>("BRIDGE1 {\"op\":\"dance\"}").is_err());
        assert!(decode::<Request>(r#"{"op":"spawn","args":[]}"#).is_err());
        assert!(decode::<Request>("BRIDGE1 not json").is_err());
        // Wire-protocol JSON (what an agent would send) must never parse as
        // a bridge frame.
        assert!(decode::<Request>(r#"{"jsonrpc":"2.0","method":"initialize"}"#).is_err());
    }

    #[tokio::test]
    async fn read_line_caps_oversized_frames() {
        let (mut client, server) = tokio::io::duplex(64);
        let mut server = tokio::io::BufReader::new(server);
        let oversized = format!("BRIDGE1 {{\"pad\":\"{}\"}}", "x".repeat(MAX_LINE_BYTES));
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let _ = client.write_all(oversized.as_bytes()).await;
        });
        let err = read_line(&mut server).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn read_line_reports_premature_close() {
        let (mut client, server) = tokio::io::duplex(64);
        let mut server = tokio::io::BufReader::new(server);
        use tokio::io::AsyncWriteExt;
        let _ = client.write_all(b"BRIDGE1 {").await;
        drop(client); // close mid-frame
        let err = read_line(&mut server).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn read_line_keeps_pipelined_bytes_for_the_next_reader() {
        // A client may send its first wire-protocol line right after the
        // header; those bytes must survive in the buffer.
        let (mut client, server) = tokio::io::duplex(64);
        let mut server = tokio::io::BufReader::new(server);
        use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
        tokio::spawn(async move {
            let _ = client
                .write_all(b"BRIDGE1 {\"op\":\"list_sessions\"}\n{\"jsonrpc\":\"2.0\"}\n")
                .await;
        });
        let line = read_line(&mut server).await.unwrap();
        assert!(line.contains("list_sessions"));
        let mut next = String::new();
        server.read_line(&mut next).await.unwrap();
        assert_eq!(next.trim_end(), r#"{"jsonrpc":"2.0"}"#);
    }
}
