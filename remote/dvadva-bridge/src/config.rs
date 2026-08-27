//! `~/.kimi/bridge.toml` — the daemon's half of it.
//!
//! One file describes both roles a machine can play, and the two halves are
//! **disjoint sections**: the daemon reads `[serve]` (how this machine acts
//! as a remote), the frontends read `[[remotes]]` (which remotes this
//! machine connects to, and how to tunnel there — see
//! `client/wire-client/src/remotes.rs`). Nothing is shared but the path and
//! the file format, which is what keeps the daemon free of any dependency on
//! the frontend kit. Unknown sections are ignored on both sides, so neither
//! half can break the other by growing.
//!
//! It is deliberately *not* a section in `~/.kimi/config.toml`: the agent
//! rewrites that file by serializing its own `Config` struct
//! (`dvadva_agent::config::save_config`), which would silently drop anything
//! it does not know about.
//!
//! Every field is optional. A missing file, a missing section, and an empty
//! section all mean the same thing: use the built-in defaults, which is why
//! `dvadva-bridge remote` works on a machine with no config at all.

use std::path::PathBuf;

use serde::Deserialize;

/// Name of the config file inside `~/.kimi`.
pub const FILE_NAME: &str = "bridge.toml";

/// The `[serve]` section: how this machine acts as a remote.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct ServeConfig {
    /// Address to listen on. Keep it loopback.
    pub listen: Option<String>,
    /// `dvadva-agent` binary to spawn.
    pub agent_bin: Option<String>,
    /// Work directory for agents whose spawn args name none.
    pub work_dir: Option<String>,
}

/// The file as the daemon sees it. `[[remotes]]` is the frontends' half and
/// is ignored here (serde skips unknown fields by default).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct BridgeFile {
    serve: ServeConfig,
}

/// Path of the config file: `~/.kimi/bridge.toml`.
pub fn path() -> PathBuf {
    dvadva_agent::share::get_share_dir().join(FILE_NAME)
}

/// Read the `[serve]` section from `~/.kimi/bridge.toml`.
///
/// A missing file is not an error — it is the normal case for a machine that
/// only ever runs frontends. A malformed one is: silently falling back to
/// defaults would start a daemon listening somewhere the operator did not
/// ask for.
pub fn load_serve() -> Result<ServeConfig, String> {
    load_serve_from(&path())
}

fn load_serve_from(path: &std::path::Path) -> Result<ServeConfig, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(ServeConfig::default()),
        Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
    };
    parse_serve(&text).map_err(|err| format!("{}: {err}", path.display()))
}

fn parse_serve(text: &str) -> Result<ServeConfig, String> {
    toml::from_str::<BridgeFile>(text)
        .map(|file| file.serve)
        .map_err(|err| format!("invalid bridge config: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_serve_section_and_ignores_the_other_half() {
        let config = parse_serve(
            r#"
            [serve]
            listen = "127.0.0.1:9100"
            work_dir = "/home/kimi"

            [[remotes]]
            name = "vps"
            endpoint = "127.0.0.1:9000"
            "#,
        )
        .expect("parses");
        assert_eq!(config.listen.as_deref(), Some("127.0.0.1:9100"));
        assert_eq!(config.work_dir.as_deref(), Some("/home/kimi"));
        assert_eq!(config.agent_bin, None);
    }

    #[test]
    fn every_shape_of_empty_means_defaults() {
        assert_eq!(parse_serve("").unwrap(), ServeConfig::default());
        assert_eq!(parse_serve("[serve]").unwrap(), ServeConfig::default());
        // A file that only configures the frontends' half.
        assert_eq!(
            parse_serve("[[remotes]]\nname = \"vps\"\nendpoint = \"h:1\"").unwrap(),
            ServeConfig::default()
        );
    }

    #[test]
    fn a_missing_file_is_not_an_error_but_a_broken_one_is() {
        let missing = std::path::Path::new("this/does/not/exist/bridge.toml");
        assert_eq!(load_serve_from(missing).unwrap(), ServeConfig::default());
        assert!(parse_serve("[serve]\nlisten = ").is_err());
    }
}
