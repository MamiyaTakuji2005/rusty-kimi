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

use serde::Deserialize;

use crate::session_list::ResumeEntry;

/// Magic prefix of every bridge frame — kept in sync with the daemon side.
pub const MAGIC: &str = "BRIDGE1";

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

/// The daemon's single reply frame to any request.
#[derive(Debug, Deserialize)]
pub struct BridgeReply {
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub sessions: Option<Vec<ResumeEntry>>,
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
