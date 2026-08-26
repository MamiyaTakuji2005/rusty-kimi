//! The client side of the kimi-bridge control framing.
//!
//! A remote connection begins with one `BRIDGE1` frame (see
//! `remote/kimi-bridge/src/proto.rs` for the daemon-side definition and the
//! rationale): the client states what it wants — spawn an agent with these
//! args, or list the sessions on the far side — the daemon answers with one
//! reply frame, and everything after that is the opaque kimi-agent wire
//! stream.
//!
//! The two definitions are deliberately separate crates (the daemon must
//! not depend on the frontend kit, and vice versa); a dev-dependency test
//! in this crate pins them byte-for-byte against each other.

use std::io::{BufRead, Read};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use serde::Deserialize;

use crate::session_list::ResumeEntry;

/// Magic prefix of every bridge frame — kept in sync with the daemon side.
pub const MAGIC: &str = "BRIDGE1";

/// Upper bound on a frame line, matching `kimi_bridge::proto::MAX_LINE_BYTES`.
/// A frontend pointed at the wrong port (an HTTP server, a log stream) must
/// fail rather than buffer whatever the peer feels like sending.
pub const MAX_LINE_BYTES: usize = 64 * 1024;

/// How long to wait for the TCP connection itself. `ssh -L` accepts on
/// loopback immediately and only then discovers the far end, so this mostly
/// bounds a direct (non-tunnelled) dial at a host that is not answering.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to wait for the daemon's single reply frame. The daemon answers
/// before any agent output flows, so this is generous; without it a wedged
/// daemon freezes the frontend before it has a window to say so in.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// Encode a request frame line (without the trailing newline).
fn frame(json: &str) -> String {
    format!("{MAGIC} {json}")
}

/// The header asking a bridge daemon to spawn an agent with `agent_args`
/// (verbatim agent CLI arguments: `-w`, `--session`, …) and relay.
pub fn spawn_header(agent_args: &[String]) -> String {
    frame(&serde_json::json!({ "op": "spawn", "args": agent_args }).to_string())
}

/// The header asking a bridge daemon for the sessions on its machine.
pub fn list_sessions_header() -> String {
    frame(&serde_json::json!({ "op": "list_sessions" }).to_string())
}

/// The header asking a bridge daemon whether it is there at all.
pub fn version_header() -> String {
    frame(&serde_json::json!({ "op": "version" }).to_string())
}

/// Ask the daemon at `endpoint` for its version — the liveness probe behind
/// a UI's connection indicator.
///
/// Deliberately its own short-lived connection: there is no persistent one
/// to observe (each session dials its own), and a probe that spawns nothing
/// is safe to run on a timer. `timeout` bounds the whole exchange, so a
/// caller can poll with something much tighter than the handshake budget.
pub fn probe(endpoint: &str, timeout: Duration) -> Result<String, String> {
    use std::io::Write;

    let mut stream = connect(endpoint, timeout)?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|err| format!("bridge `{endpoint}`: {err}"))?;
    stream
        .write_all(version_header().as_bytes())
        .and_then(|_| stream.write_all(b"\n"))
        .and_then(|_| stream.flush())
        .map_err(|err| format!("bridge `{endpoint}` write failed: {err}"))?;

    let line = read_frame_line(&mut std::io::BufReader::new(&mut stream))
        .map_err(|err| format!("bridge `{endpoint}`: {err}"))?;
    let reply = decode_reply(&line)?;
    if !reply.ok {
        return Err(reply
            .error
            .unwrap_or_else(|| "bridge refused the probe".into()));
    }
    // An older daemon that does not know the op answers `{"ok":false}` and
    // never reaches here; one that does always names its version.
    Ok(reply.version.unwrap_or_else(|| "unknown".to_string()))
}

/// The daemon's single reply frame to any request.
#[derive(Debug, Deserialize)]
pub struct BridgeReply {
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub sessions: Option<Vec<ResumeEntry>>,
    /// The daemon's version, on a `version` reply.
    #[serde(default)]
    pub version: Option<String>,
}

/// Parse a reply frame line (as produced by the daemon) into a
/// [`BridgeReply`].
pub fn decode_reply(line: &str) -> Result<BridgeReply, String> {
    let json = line
        .strip_prefix(MAGIC)
        .map(str::trim_start)
        .ok_or_else(|| "not a bridge reply (missing BRIDGE1 prefix)".to_string())?;
    serde_json::from_str(json).map_err(|err| format!("bad bridge reply: {err}"))
}

/// The daemon's exit trailer, if this relayed line is one.
///
/// After the remote agent's stdout ends, the daemon appends a final frame
/// carrying the exit status and the agent's stderr tail before half-closing
/// — the remote equivalent of the stderr tail a locally spawned agent leaves
/// behind. Wire-protocol JSON never starts with the magic, so the prefix is
/// enough to tell the two apart.
pub fn exit_trailer(line: &str) -> Option<String> {
    if !line.starts_with(MAGIC) {
        return None;
    }
    Some(match decode_reply(line) {
        Ok(reply) => reply
            .error
            .filter(|reason| !reason.is_empty())
            .unwrap_or_else(|| "remote agent exited".to_string()),
        // A trailer we cannot read still means the agent is gone; say both.
        Err(err) => format!("remote agent exited (unreadable bridge trailer: {err})"),
    })
}

/// Connect to `endpoint` (`host:port`) with a bounded wait, trying every
/// address it resolves to.
pub fn connect(endpoint: &str, timeout: Duration) -> Result<TcpStream, String> {
    let addrs = endpoint
        .to_socket_addrs()
        .map_err(|err| format!("failed to resolve bridge `{endpoint}`: {err}"))?;
    let mut last = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(stream) => {
                let _ = stream.set_nodelay(true);
                return Ok(stream);
            }
            Err(err) => last = Some(err),
        }
    }
    Err(match last {
        Some(err) => format!("failed to connect to bridge `{endpoint}`: {err}"),
        None => format!("failed to connect to bridge `{endpoint}`: no address to try"),
    })
}

/// Read one `\n`-terminated frame line, bounded by [`MAX_LINE_BYTES`].
///
/// The daemon-side twin is `kimi_bridge::proto::read_line`: same cap, and
/// bytes the peer pipelined after the newline stay in the reader's buffer
/// for whoever owns the stream next (for `connect_tcp`, the agent's first
/// wire lines).
pub fn read_frame_line<R: BufRead>(reader: &mut R) -> Result<String, String> {
    let mut buf: Vec<u8> = Vec::new();
    let read = reader
        .take(MAX_LINE_BYTES as u64 + 1)
        .read_until(b'\n', &mut buf);
    match read {
        Ok(0) => Err("the daemon closed the connection without replying".to_string()),
        Ok(_) if !buf.ends_with(b"\n") => {
            if buf.len() > MAX_LINE_BYTES {
                Err("bridge frame exceeds size limit (is this a kimi-bridge daemon?)".to_string())
            } else {
                Err("the daemon closed the connection mid-frame".to_string())
            }
        }
        Ok(_) => Ok(String::from_utf8_lossy(&buf).trim_end().to_string()),
        Err(err) if is_timeout(&err) => Err(format!(
            "the daemon did not answer within {}s",
            HANDSHAKE_TIMEOUT.as_secs()
        )),
        Err(err) => Err(format!("bridge read failed: {err}")),
    }
}

/// A read that expired against `SO_RCVTIMEO`. Which kind that surfaces as is
/// platform-dependent (`WouldBlock` on unix, `TimedOut` on Windows).
fn is_timeout(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_header_shape() {
        assert_eq!(
            spawn_header(&["-w".into(), "/srv".into()]),
            r#"BRIDGE1 {"op":"spawn","args":["-w","/srv"]}"#
        );
        assert_eq!(list_sessions_header(), r#"BRIDGE1 {"op":"list_sessions"}"#);
    }

    #[test]
    fn reply_decodes() {
        let reply = decode_reply(r#"BRIDGE1 {"ok":true}"#).unwrap();
        assert!(reply.ok);
        assert!(reply.sessions.is_none());

        let reply = decode_reply(r#"BRIDGE1 {"ok":false,"error":"boom"}"#).unwrap();
        assert!(!reply.ok);
        assert_eq!(reply.error.as_deref(), Some("boom"));

        let reply = decode_reply(
            r#"BRIDGE1 {"ok":true,"sessions":[{"id":"a","title":"t","work_dir":"/w","updated_at":1.5}]}"#,
        )
        .unwrap();
        let sessions = reply.sessions.unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "a");
        assert_eq!(sessions[0].work_dir.to_string_lossy(), "/w");

        assert!(decode_reply(r#"{"ok":true}"#).is_err());
    }

    /// The client- and daemon-side framings must stay byte-compatible:
    /// what this module emits is exactly what `kimi_bridge::proto` parses,
    /// and its replies are exactly what [`decode_reply`] accepts.
    #[test]
    fn framing_matches_the_daemon_side() {
        let args = vec!["-w".to_string(), "/srv/proj".to_string()];
        let emitted = spawn_header(&args);
        let parsed: kimi_bridge::proto::Request =
            kimi_bridge::proto::decode(&emitted).expect("daemon must parse our header");
        assert_eq!(
            parsed,
            kimi_bridge::proto::Request::Spawn { args },
            "spawn header drifted from the daemon-side framing"
        );

        let daemon_reply = kimi_bridge::proto::encode(&kimi_bridge::proto::Reply::error("x"));
        assert!(
            decode_reply(&daemon_reply).is_ok(),
            "daemon reply not parseable by the client"
        );

        let daemon_listing =
            kimi_bridge::proto::encode(&kimi_bridge::proto::Reply::sessions(vec![
                kimi_bridge::proto::SessionEntry {
                    id: "a".into(),
                    title: "t".into(),
                    work_dir: "/w".into(),
                    updated_at: 1.5,
                },
            ]));
        let reply = decode_reply(&daemon_listing).expect("listing reply must parse");
        assert_eq!(reply.sessions.unwrap().len(), 1);
    }
}
