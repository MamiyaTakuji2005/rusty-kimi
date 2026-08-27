//! Locating the agent binary — the one piece of process setup every
//! frontend shares.
//!
//! The lookup order is fixed so an install "just works" without setup:
//!
//! 1. `--agent-bin <path>` on the command line (removed from `args` when
//!    found),
//! 2. the `KIMI_AGENT_BIN` environment variable,
//! 3. a `dvadva-agent` sitting next to the frontend executable — what makes a
//!    double-clicked or copied-around install work with no environment at all,
//! 4. the bare name `dvadva-agent`, letting `PATH` have the last word.
//!
//! The last fallback is deliberate rather than an error: frontends surface a
//! missing agent through their normal failure paths (spawn error → transcript
//! info / exit message) instead of refusing to start.
//!
//! The same resolution handles **remote** connections: `--remote <name |
//! host:port>` (or `KIMI_REMOTE`) routes the frontend through a
//! `dvadva-bridge` daemon instead of spawning a local agent. Both flags are
//! stripped from the agent args; everything else is forwarded verbatim —
//! over remote, `-w <dir>` and friends name paths on the *remote* machine.
//!
//! What `--remote` names is resolved by [`crate::remotes`], not here: a name
//! from `~/.kimi/bridge.toml` or a bare `host:port`. This module stays pure
//! (no file reads) so it can be tested as the argument parser it is, and so
//! an unknown name produces one good error listing what *is* configured
//! rather than a syntax complaint.

use std::path::PathBuf;

/// The resolved launch configuration of a frontend.
#[derive(Debug)]
pub struct AgentLaunch {
    /// Path or bare name to spawn.
    pub agent_bin: String,
    /// Remaining command-line arguments for the agent, verbatim.
    pub agent_args: Vec<String>,
    /// What `--remote` / `KIMI_REMOTE` named — a configured remote's name
    /// or a bare `host:port`, resolved through [`crate::remotes::resolve`].
    /// When set, frontends connect through a `dvadva-bridge` daemon instead of
    /// spawning, and `agent_bin` is unused.
    pub remote: Option<String>,
}

/// The session id in agent arguments, if they name one.
///
/// The **attach key** and the agent's `--session` are two different things
/// that happen to be the same string on a resume: one says which live agent to
/// look for, the other is what a cold start should be told to open. The daemon
/// deliberately does not derive one from the other
/// (`remote/dvadva-bridge/src/proto.rs`), so a frontend that generates
/// `--session` has to read its own argv to know what to attach to — otherwise
/// `--session abc` starts a *second* agent on a session that already has one.
pub fn session_arg(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if let Some(value) = arg.strip_prefix("--session=") {
            return Some(value.to_string());
        }
        if arg == "--session" || arg == "-S" {
            return iter.next().cloned();
        }
    }
    None
}

impl AgentLaunch {
    /// Resolve from this process's arguments (`argv[1..]`) and the
    /// environment. Returns the usage error for a malformed flag.
    pub fn from_env() -> Result<Self, String> {
        let args: Vec<String> = std::env::args().skip(1).collect();
        Self::from_args(
            args,
            std::env::var("KIMI_AGENT_BIN").ok(),
            std::env::var("KIMI_REMOTE").ok(),
        )
    }

    /// Resolve from explicit pieces; exposed for tests and for frontends that
    /// pre-parse their own flags.
    pub fn from_args(
        mut args: Vec<String>,
        env_bin: Option<String>,
        env_remote: Option<String>,
    ) -> Result<Self, String> {
        let mut agent_bin = env_bin.filter(|s| !s.is_empty());
        if let Some(pos) = args.iter().position(|a| a == "--agent-bin") {
            let value = args
                .get(pos + 1)
                .cloned()
                .ok_or("--agent-bin requires a path argument")?;
            args.drain(pos..pos + 2);
            agent_bin = Some(value);
        }

        let mut remote = env_remote.filter(|s| !s.is_empty());
        if let Some(pos) = args.iter().position(|a| a == "--remote") {
            let value = args
                .get(pos + 1)
                .cloned()
                .ok_or("--remote requires a name or host:port argument")?;
            args.drain(pos..pos + 2);
            remote = Some(value);
        }
        if remote.as_deref() == Some("") {
            return Err("--remote expects a name or host:port".to_string());
        }

        Ok(Self {
            agent_bin: agent_bin
                .or_else(sibling_agent_bin)
                .unwrap_or_else(|| "dvadva-agent".to_string()),
            agent_args: args,
            remote,
        })
    }
}

/// A `dvadva-agent` next to this executable, if one exists.
fn sibling_agent_bin() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let name = if cfg!(windows) {
        "dvadva-agent.exe"
    } else {
        "dvadva-agent"
    };
    let candidate: PathBuf = exe.parent()?.join(name);
    candidate
        .is_file()
        .then(|| candidate.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bin(result: &Result<AgentLaunch, String>) -> &str {
        result.as_ref().expect("resolved").agent_bin.as_str()
    }

    #[test]
    fn flag_beats_env_beats_default() {
        let r = AgentLaunch::from_args(
            vec!["--agent-bin".into(), "/x/a".into()],
            Some("/y/b".into()),
            None,
        );
        assert_eq!(bin(&r), "/x/a");
        assert!(r.unwrap().agent_args.is_empty());

        let r = AgentLaunch::from_args(vec![], Some("/y/b".into()), None);
        assert_eq!(bin(&r), "/y/b");

        let r = AgentLaunch::from_args(vec![], None, None);
        assert_eq!(bin(&r), "dvadva-agent");
    }

    #[test]
    fn flag_is_removed_from_agent_args() {
        let r = AgentLaunch::from_args(
            vec![
                "--agent-bin".into(),
                "/x/a".into(),
                "-w".into(),
                "/tmp".into(),
            ],
            None,
            None,
        );
        let launch = r.expect("resolved");
        assert_eq!(launch.agent_bin, "/x/a");
        assert_eq!(launch.agent_args, vec!["-w", "/tmp"]);
    }

    #[test]
    fn dangling_flag_is_a_usage_error() {
        let err = AgentLaunch::from_args(vec!["--agent-bin".into()], None, None).unwrap_err();
        assert!(err.contains("--agent-bin"));
    }

    #[test]
    fn empty_env_var_is_ignored() {
        let r = AgentLaunch::from_args(vec![], Some(String::new()), Some(String::new()));
        assert_eq!(bin(&r), "dvadva-agent");
        assert!(r.unwrap().remote.is_none());
    }

    #[test]
    fn remote_flag_beats_env_and_is_stripped() {
        let r = AgentLaunch::from_args(
            vec![
                "--remote".into(),
                "127.0.0.1:9000".into(),
                "-w".into(),
                "/remote/dir".into(),
            ],
            None,
            Some("10.0.0.1:1".into()),
        );
        let launch = r.expect("resolved");
        assert_eq!(launch.remote.as_deref(), Some("127.0.0.1:9000"));
        assert_eq!(launch.agent_args, vec!["-w", "/remote/dir"]);

        let r = AgentLaunch::from_args(vec![], None, Some("10.0.0.1:1".into()));
        assert_eq!(r.unwrap().remote.as_deref(), Some("10.0.0.1:1"));
    }

    #[test]
    fn a_bare_name_is_accepted_for_remotes_to_resolve() {
        // `--remote vps` names an entry in ~/.kimi/bridge.toml; whether it
        // exists is `remotes::resolve`'s question, not this parser's.
        let launch = AgentLaunch::from_args(vec!["--remote".into(), "vps".into()], None, None)
            .expect("resolved");
        assert_eq!(launch.remote.as_deref(), Some("vps"));

        let err =
            AgentLaunch::from_args(vec!["--remote".into(), String::new()], None, None).unwrap_err();
        assert!(err.contains("name or host:port"), "{err}");
    }

    #[test]
    fn dangling_remote_flag_is_a_usage_error() {
        let err = AgentLaunch::from_args(vec!["--remote".into()], None, None).unwrap_err();
        assert!(err.contains("--remote"));
    }

    #[test]
    fn the_attach_key_is_read_out_of_the_agent_arguments() {
        // Every spelling, because a person types these and a frontend's
        // find-or-start turns on getting one.
        assert_eq!(
            session_arg(&["--session".into(), "abc".into()]),
            Some("abc".to_string())
        );
        assert_eq!(
            session_arg(&["-w".into(), "/p".into(), "-S".into(), "abc".into()]),
            Some("abc".to_string())
        );
        assert_eq!(
            session_arg(&["--session=abc".into()]),
            Some("abc".to_string())
        );
        // A new session names none, and must not be handed one by accident:
        // that is how a caller ends up attached to somebody else's agent.
        assert_eq!(session_arg(&["-w".into(), "/p".into()]), None);
        assert_eq!(session_arg(&["--session".into()]), None);
    }
}
