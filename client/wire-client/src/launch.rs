//! Locating the agent binary — the one piece of process setup every
//! frontend shares.
//!
//! The lookup order is fixed so an install "just works" without setup:
//!
//! 1. `--agent-bin <path>` on the command line (removed from `args` when
//!    found),
//! 2. the `KIMI_AGENT_BIN` environment variable,
//! 3. a `kimi-agent` sitting next to the frontend executable — what makes a
//!    double-clicked or copied-around install work with no environment at all,
//! 4. the bare name `kimi-agent`, letting `PATH` have the last word.
//!
//! The last fallback is deliberate rather than an error: frontends surface a
//! missing agent through their normal failure paths (spawn error → transcript
//! info / exit message) instead of refusing to start.

use std::path::PathBuf;

/// The resolved launch configuration of a frontend.
#[derive(Debug)]
pub struct AgentLaunch {
    /// Path or bare name to spawn.
    pub agent_bin: String,
    /// Remaining command-line arguments for the agent, verbatim.
    pub agent_args: Vec<String>,
}

impl AgentLaunch {
    /// Resolve from this process's arguments (`argv[1..]`) and the
    /// environment. Returns the usage error for a malformed `--agent-bin`.
    pub fn from_env() -> Result<Self, String> {
        let args: Vec<String> = std::env::args().skip(1).collect();
        Self::from_args(args, std::env::var("KIMI_AGENT_BIN").ok())
    }

    /// Resolve from explicit pieces; exposed for tests and for frontends that
    /// pre-parse their own flags.
    pub fn from_args(mut args: Vec<String>, env_bin: Option<String>) -> Result<Self, String> {
        let mut agent_bin = env_bin.filter(|s| !s.is_empty());
        if let Some(pos) = args.iter().position(|a| a == "--agent-bin") {
            if pos + 1 < args.len() {
                args.remove(pos);
                agent_bin = Some(args.remove(pos));
            } else {
                return Err("--agent-bin requires a path argument".into());
            }
        }
        Ok(Self {
            agent_bin: agent_bin
                .or_else(sibling_agent_bin)
                .unwrap_or_else(|| "kimi-agent".to_string()),
            agent_args: args,
        })
    }
}

/// A `kimi-agent` next to this executable, if one exists.
fn sibling_agent_bin() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let name = if cfg!(windows) {
        "kimi-agent.exe"
    } else {
        "kimi-agent"
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
        );
        assert_eq!(bin(&r), "/x/a");
        assert!(r.unwrap().agent_args.is_empty());

        let r = AgentLaunch::from_args(vec![], Some("/y/b".into()));
        assert_eq!(bin(&r), "/y/b");

        let r = AgentLaunch::from_args(vec![], None);
        assert_eq!(bin(&r), "kimi-agent");
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
        );
        let launch = r.expect("resolved");
        assert_eq!(launch.agent_bin, "/x/a");
        assert_eq!(launch.agent_args, vec!["-w", "/tmp"]);
    }

    #[test]
    fn dangling_flag_is_a_usage_error() {
        let err = AgentLaunch::from_args(vec!["--agent-bin".into()], None).unwrap_err();
        assert!(err.contains("--agent-bin"));
    }

    #[test]
    fn empty_env_var_is_ignored() {
        let r = AgentLaunch::from_args(vec![], Some(String::new()));
        assert_eq!(bin(&r), "kimi-agent");
    }
}
