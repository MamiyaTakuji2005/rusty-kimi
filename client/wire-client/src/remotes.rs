//! `~/.kimi/bridge.toml` — the frontends' half of it.
//!
//! One file describes both roles a machine can play, and the two halves are
//! **disjoint sections**: the frontends read `[[remotes]]` (the remotes this
//! machine can connect to, and how to tunnel there), the daemon reads
//! `[serve]` (how this machine acts as a remote — see
//! `remote/kimi-bridge/src/config.rs`). Nothing is shared but the path and
//! the format, so neither crate has to depend on the other.
//!
//! ```toml
//! [[remotes]]
//! name = "vps"
//! endpoint = "127.0.0.1:9000"
//! tunnel = "ssh -N -L 9000:127.0.0.1:9000 user@vps"
//! default = true
//! ```
//!
//! `--remote` then takes a **name or a `host:port`**: a name is looked up
//! here, anything with a colon is used as-is, so nothing that worked before
//! the config file stops working.

use std::path::PathBuf;

use serde::Deserialize;

/// Name of the config file inside `~/.kimi`.
pub const FILE_NAME: &str = "bridge.toml";

/// One configured remote.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Remote {
    /// Short name for `--remote <name>` and the UI.
    pub name: String,
    /// Where the bridge daemon answers, from *this* machine's point of view
    /// — normally the local end of an ssh tunnel.
    pub endpoint: String,
    /// Command that opens the tunnel to `endpoint`, if one is needed. The
    /// GUI runs it as a child process and stops it again; it should stay in
    /// the foreground (`ssh -N -L …`, no `-f`).
    #[serde(default)]
    pub tunnel: Option<String>,
    /// Marks the remote the UI reaches for when none was named.
    #[serde(default)]
    pub default: bool,
}

impl Remote {
    /// An unnamed remote for a bare `host:port`, so a `--remote` that names
    /// no config entry still works exactly as it did.
    pub fn ad_hoc(endpoint: &str) -> Self {
        Self {
            name: endpoint.to_string(),
            endpoint: endpoint.to_string(),
            tunnel: None,
            default: false,
        }
    }
}

/// The file as the frontends see it. `[serve]` is the daemon's half and is
/// ignored here (serde skips unknown fields by default).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct BridgeFile {
    remotes: Vec<Remote>,
}

/// Path of the config file: `~/.kimi/bridge.toml`.
pub fn path() -> PathBuf {
    kimi_agent::share::get_share_dir().join(FILE_NAME)
}

/// Every configured remote, in file order. A missing file means none.
pub fn load() -> Result<Vec<Remote>, String> {
    load_from(&path())
}

fn load_from(path: &std::path::Path) -> Result<Vec<Remote>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
    };
    parse(&text).map_err(|err| format!("{}: {err}", path.display()))
}

fn parse(text: &str) -> Result<Vec<Remote>, String> {
    let remotes = toml::from_str::<BridgeFile>(text)
        .map(|file| file.remotes)
        .map_err(|err| format!("invalid bridge config: {err}"))?;
    for remote in &remotes {
        if remote.name.is_empty() {
            return Err("a [[remotes]] entry has an empty name".to_string());
        }
        if remote.endpoint.is_empty() {
            return Err(format!("remote `{}` has an empty endpoint", remote.name));
        }
    }
    Ok(remotes)
}

/// The remote a UI should reach for when the user named none: the one marked
/// `default`, else the first configured.
pub fn default_remote(remotes: &[Remote]) -> Option<&Remote> {
    remotes
        .iter()
        .find(|remote| remote.default)
        .or_else(|| remotes.first())
}

/// Resolve what `--remote` was given: a configured name, or a bare
/// `host:port`. Anything else is a usage error naming what *is* configured,
/// which is the whole reason a typo should not become a connect timeout.
pub fn resolve(spec: &str, remotes: &[Remote]) -> Result<Remote, String> {
    if let Some(remote) = remotes.iter().find(|remote| remote.name == spec) {
        return Ok(remote.clone());
    }
    if spec.contains(':') {
        return Ok(Remote::ad_hoc(spec));
    }
    let known: Vec<&str> = remotes.iter().map(|remote| remote.name.as_str()).collect();
    Err(if known.is_empty() {
        format!(
            "--remote `{spec}` is neither host:port nor a remote in {}",
            path().display()
        )
    } else {
        format!(
            "--remote `{spec}` is not one of the configured remotes ({})",
            known.join(", ")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<Remote> {
        parse(
            r#"
            [serve]
            listen = "127.0.0.1:9000"

            [[remotes]]
            name = "vps"
            endpoint = "127.0.0.1:9000"
            tunnel = "ssh -N -L 9000:127.0.0.1:9000 user@vps"

            [[remotes]]
            name = "buildbox"
            endpoint = "127.0.0.1:9010"
            default = true
            "#,
        )
        .expect("parses")
    }

    #[test]
    fn reads_the_remotes_and_ignores_the_daemon_half() {
        let remotes = sample();
        assert_eq!(remotes.len(), 2);
        assert_eq!(remotes[0].name, "vps");
        assert_eq!(remotes[0].endpoint, "127.0.0.1:9000");
        assert!(remotes[0].tunnel.as_deref().unwrap().starts_with("ssh -N"));
        assert!(!remotes[0].default);
        assert!(remotes[1].tunnel.is_none());
    }

    #[test]
    fn default_is_the_marked_one_then_the_first() {
        assert_eq!(default_remote(&sample()).unwrap().name, "buildbox");

        let unmarked = parse("[[remotes]]\nname = \"a\"\nendpoint = \"h:1\"\n").unwrap();
        assert_eq!(default_remote(&unmarked).unwrap().name, "a");
        assert!(default_remote(&[]).is_none());
    }

    #[test]
    fn resolve_takes_a_name_or_a_host_port() {
        let remotes = sample();
        assert_eq!(resolve("vps", &remotes).unwrap().endpoint, "127.0.0.1:9000");

        // A bare endpoint still works with no config at all.
        let ad_hoc = resolve("10.0.0.5:9000", &[]).unwrap();
        assert_eq!(ad_hoc.endpoint, "10.0.0.5:9000");
        assert!(ad_hoc.tunnel.is_none());
    }

    #[test]
    fn an_unknown_name_lists_what_is_configured() {
        let err = resolve("vpz", &sample()).unwrap_err();
        assert!(err.contains("vpz"), "{err}");
        assert!(err.contains("vps") && err.contains("buildbox"), "{err}");
    }

    #[test]
    fn empty_and_missing_are_no_remotes_but_broken_is_an_error() {
        assert!(parse("").unwrap().is_empty());
        assert!(parse("[serve]\nlisten = \"h:1\"").unwrap().is_empty());
        assert!(
            load_from(std::path::Path::new("no/such/bridge.toml"))
                .unwrap()
                .is_empty()
        );
        assert!(parse("[[remotes]]\nname = \"a\"").is_err(), "no endpoint");
        assert!(
            parse("[[remotes]]\nname = \"\"\nendpoint = \"h:1\"").is_err(),
            "empty name"
        );
    }
}
