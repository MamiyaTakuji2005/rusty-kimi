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
//! starts with the magic, and an agent's first message never does.
//!
//! **Versioning.** The digit in the magic is this protocol's *major*, and it
//! is a hard gate: a frame carrying any other digit is refused outright, by
//! both halves, with a message that says so. [`BRIDGE_PROTOCOL_VERSION`]
//! carries that same major plus a *minor*, which a `version` reply hands to
//! the client, so an additive op (a new `op`, a new reply field) can be
//! introduced without cutting BRIDGE2 and breaking every deployed daemon at
//! once. Same rule as the wire protocol next door
//! (`server/dvadva-agent/src/wire/protocol.rs`), different clock: the two
//! version independently, and the daemons never parse the wire at all.
//!
//! The client-side twin of this framing lives in
//! `client/wire-client/src/bridge.rs`; a dev-dependency test there asserts
//! the two stay byte-compatible.

use std::io;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::AsyncBufReadExt;

/// Magic prefix of every bridge frame. The trailing digit is the frame
/// protocol's major version.
pub const MAGIC: &str = "BRIDGE1";

/// The magic without its version digit, for telling "a bridge frame from a
/// build we cannot talk to" apart from "not a bridge frame at all".
pub const MAGIC_FAMILY: &str = "BRIDGE";

/// This build's frame protocol version, `major.minor`. The major must equal
/// the digit in [`MAGIC`]; the minor rises with additive ops.
///
/// 1.1 added [`Request::Attach`] and the two fields that answer it —
/// [`Reply::session`] and [`SessionEntry::live`]. A 1.0 daemon refuses an
/// `attach` frame as an unknown op, which is why the minor is worth
/// reporting: a client can ask before it asks for something new.
pub const BRIDGE_PROTOCOL_VERSION: &str = "1.1";

/// Upper bound on a frame line. Real frames are a few hundred bytes; the
/// cap keeps a hostile or confused client from buffering unbounded memory.
pub const MAX_LINE_BYTES: usize = 64 * 1024;

/// A parsed bridge request — the first line of a connection.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Spawn an agent with these CLI arguments (verbatim, e.g. `-w`,
    /// `--session`), then relay bytes both ways until either side closes.
    ///
    /// The one-shot path: this connection's lifetime *is* the agent's, and
    /// the daemon kills it on the way out. [`Request::Attach`] is the other
    /// one, where the agent outlives whoever is looking at it.
    Spawn { args: Vec<String> },
    /// Attach to the agent hosting `session`, starting one with these CLI
    /// arguments if none is live, then relay bytes both ways.
    ///
    /// The supervised path. The connection closing is a *detach*: the agent
    /// keeps its turn, its context and its pid, and the next `attach` for
    /// the same session reaches the same process.
    ///
    /// `session` is the registry key and nothing else — the daemon does not
    /// synthesize agent arguments from it. A caller resuming a cold session
    /// passes both: the id here, and its own `--session <id> -w <dir>` in
    /// `args`. A caller starting a fresh session passes no id, gets whatever
    /// the agent mints, and is told which in the ack.
    Attach {
        #[serde(default)]
        session: Option<String>,
        #[serde(default)]
        args: Vec<String>,
    },
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
    /// Whether an agent is hosting this session right now — an `attach`
    /// would join it rather than start one. Absent on the wire when false,
    /// so a listing from a 1.0 daemon reads as "all cold", which is what a
    /// daemon that cannot supervise anything means.
    #[serde(default, skip_serializing_if = "is_false")]
    pub live: bool,
}

/// `skip_serializing_if` for a `bool` that defaults to false.
fn is_false(value: &bool) -> bool {
    !*value
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
    /// The daemon's own build version, on a `version` reply. Says which
    /// binary is running, never whether it is compatible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The daemon's frame protocol version, on a `version` reply. Absent
    /// from a daemon built before this field existed, which is itself the
    /// answer: frame protocol 1.0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proto: Option<String>,
    /// Which session the relay that follows is attached to, on an `attach`
    /// ack. The only way a caller who asked for a *new* session learns its
    /// id before the wire starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

impl Reply {
    /// The `spawn` acknowledgement: the agent exists, relay starts now.
    pub fn spawn_ok() -> Self {
        Self {
            ok: true,
            error: None,
            sessions: None,
            version: None,
            proto: None,
            session: None,
        }
    }

    /// A `list_sessions` result.
    pub fn sessions(entries: Vec<SessionEntry>) -> Self {
        Self {
            ok: true,
            error: None,
            sessions: Some(entries),
            version: None,
            proto: None,
            session: None,
        }
    }

    /// A `version` result: this daemon is alive, this is the build, and
    /// this is the frame protocol it speaks.
    pub fn version(version: impl Into<String>) -> Self {
        Self {
            ok: true,
            error: None,
            sessions: None,
            version: Some(version.into()),
            proto: Some(BRIDGE_PROTOCOL_VERSION.to_string()),
            session: None,
        }
    }

    /// The `attach` acknowledgement: an agent is on the other end of this
    /// relay, and this is which session it hosts. Named even when the caller
    /// asked for a session by id, so an ack always reads the same way.
    pub fn attach_ok(session: impl Into<String>) -> Self {
        Self {
            ok: true,
            error: None,
            sessions: None,
            version: None,
            proto: None,
            session: Some(session.into()),
        }
    }

    /// A failure the client should see (bad frame, spawn failure, …).
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(message.into()),
            sessions: None,
            version: None,
            proto: None,
            session: None,
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
        .ok_or_else(|| magic_mismatch(line))?;
    serde_json::from_str(json).map_err(|err| format!("bad bridge frame: {err}"))
}

/// Why a line did not start with our magic. A frame from a *different* bridge
/// major is a version mismatch and has to say so: reporting it as "not a
/// bridge frame" would send whoever hit it looking for a networking fault
/// instead of for a stale binary.
fn magic_mismatch(line: &str) -> String {
    match line.split_whitespace().next() {
        Some(word) if word.starts_with(MAGIC_FAMILY) && word != MAGIC => format!(
            "bridge frame protocol `{word}` is not compatible with this build's \
             `{MAGIC}`: the two binaries need to match"
        ),
        _ => format!("not a bridge frame (missing {MAGIC} prefix)"),
    }
}

/// Read one `\n`-terminated line, bounded by [`MAX_LINE_BYTES`].
///
/// Hand-rolled over `fill_buf`/`consume` rather than `read_until` so that
/// (a) the size cap actually applies, and (b) bytes a peer pipelined
/// *after* the newline stay buffered in the reader for whoever owns the
/// stream next. Errors on close-before-delimiter: the caller treats that
/// as "no usable frame".
///
/// Used for bridge frames in both directions, and by the supervisor for the
/// one line an agent answers its attach handshake with — same shape, same
/// cap, same need to leave the wire bytes behind it alone. Its messages are
/// therefore about *lines*, and the caller says what it was reading.
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
                "connection closed before a complete line",
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
                        "line exceeds size limit",
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
    fn an_attach_frame_carries_a_session_or_asks_for_a_new_one() {
        let resume = Request::Attach {
            session: Some("abc".into()),
            args: vec!["--session".into(), "abc".into()],
        };
        let encoded = encode(&resume);
        assert_eq!(decode::<Request>(&encoded).unwrap(), resume);

        // A fresh session names no id: the agent mints one and the ack
        // reports it.
        let fresh = decode::<Request>(r#"BRIDGE1 {"op":"attach"}"#).unwrap();
        assert_eq!(
            fresh,
            Request::Attach {
                session: None,
                args: Vec::new()
            }
        );

        let ack = Reply::attach_ok("abc");
        assert_eq!(encode(&ack), r#"BRIDGE1 {"ok":true,"session":"abc"}"#);
        assert_eq!(decode::<Reply>(&encode(&ack)).unwrap(), ack);
    }

    #[test]
    fn a_listing_says_which_sessions_are_live_and_stays_quiet_about_the_rest() {
        // The `live` flag is additive both ways: a 1.0 daemon's listing has
        // no such field and reads as cold, and a cold session in a 1.1
        // listing puts nothing on the wire either.
        let cold = SessionEntry {
            id: "a".into(),
            title: "t".into(),
            work_dir: "/w".into(),
            updated_at: 1.5,
            live: false,
        };
        let encoded = encode(&Reply::sessions(vec![cold]));
        assert!(!encoded.contains("live"), "{encoded}");

        let live = decode::<Reply>(
            r#"BRIDGE1 {"ok":true,"sessions":[{"id":"a","title":"t","work_dir":"/w","updated_at":1.5,"live":true}]}"#,
        )
        .unwrap();
        assert!(live.sessions.unwrap()[0].live);

        let old = decode::<Reply>(
            r#"BRIDGE1 {"ok":true,"sessions":[{"id":"a","title":"t","work_dir":"/w","updated_at":1.5}]}"#,
        )
        .unwrap();
        assert!(!old.sessions.unwrap()[0].live);
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
            live: true,
        }]);
        let encoded = encode(&sessions);
        assert_eq!(decode::<Reply>(&encoded).unwrap(), sessions);

        let error = Reply::error("boom");
        assert_eq!(decode::<Reply>(&encode(&error)).unwrap(), error);

        let version = Reply::version("1.8.0");
        assert_eq!(
            encode(&version),
            r#"BRIDGE1 {"ok":true,"version":"1.8.0","proto":"1.1"}"#
        );
        assert_eq!(decode::<Reply>(&encode(&version)).unwrap(), version);
    }

    #[test]
    fn a_version_reply_separates_the_build_from_the_protocol() {
        // Two numbers, two jobs: `version` says which binary is running,
        // `proto` says whether it can be talked to. Conflating them is the
        // whole thing this field exists to prevent.
        let reply = Reply::version("9.9.9");
        assert_eq!(reply.version.as_deref(), Some("9.9.9"));
        assert_eq!(reply.proto.as_deref(), Some(BRIDGE_PROTOCOL_VERSION));
        assert!(
            MAGIC.ends_with(BRIDGE_PROTOCOL_VERSION.split('.').next().unwrap()),
            "the magic's digit is the frame protocol's major"
        );
    }

    #[test]
    fn a_reply_from_before_the_proto_field_still_decodes() {
        // The deployed daemon this build has to keep talking to. Its silence
        // means frame 1.0, and must not read as a broken frame.
        let old = r#"BRIDGE1 {"ok":true,"version":"1.8.0"}"#;
        let reply = decode::<Reply>(old).unwrap();
        assert_eq!(reply.version.as_deref(), Some("1.8.0"));
        assert_eq!(reply.proto, None);
    }

    #[test]
    fn a_foreign_magic_is_reported_as_a_version_mismatch() {
        // Not "not a bridge frame": the far end *is* a bridge, from a major
        // this build cannot speak, and saying so is the whole diagnosis.
        let err = decode::<Reply>(r#"BRIDGE2 {"ok":true}"#).unwrap_err();
        assert!(err.contains("BRIDGE2"), "{err}");
        assert!(err.contains(MAGIC), "{err}");
        assert!(err.contains("not compatible"), "{err}");

        // Something that is not a bridge at all still reads as before.
        let plain = decode::<Reply>(r#"{"ok":true}"#).unwrap_err();
        assert!(plain.contains("not a bridge frame"), "{plain}");
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
