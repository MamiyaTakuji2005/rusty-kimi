//! The ssh tunnel a remote is reached through, as a managed child process.
//!
//! A remote in `~/.kimi/bridge.toml` may carry a `tunnel` command
//! (`ssh -N -L 9000:127.0.0.1:9000 user@vps`). The frontend runs it, keeps
//! it alive for as long as the connection is wanted, and kills it on the way
//! out — so "connect" is one action instead of a terminal the user has to
//! remember to keep open.
//!
//! Two things this deliberately does not do:
//!
//! - **Interpret the command.** It is split into program and arguments and
//!   run as-is; it is not a shell, so no pipes, no `&&`, no globbing. That
//!   keeps a config file from being a place where a shell command runs with
//!   whatever the shell happens to expand.
//! - **Talk to the user.** The child gets no console (a windowed frontend
//!   would otherwise sprout one), so an ssh that wants a password or a
//!   host-key confirmation will sit there unanswered. Tunnel commands must
//!   be non-interactive — key auth, a known host, ideally
//!   `-o BatchMode=yes` — and when one is not, its complaint shows up in
//!   [`Tunnel::stderr_tail`] rather than in a window nobody can see.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

/// How many stderr lines to keep from the tunnel process. ssh says what is
/// wrong in one or two ("Permission denied (publickey)", "Address already in
/// use"), and those are exactly what a stuck connection needs to show.
const STDERR_TAIL_LINES: usize = 10;

/// A running tunnel process.
#[derive(Debug)]
pub struct Tunnel {
    command: String,
    child: Child,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
}

/// What a tunnel is doing right now.
#[derive(Clone, Debug, PartialEq)]
pub enum TunnelState {
    /// Still running.
    Running,
    /// Exited on its own — with ssh, always a failure: `-N` never finishes.
    Exited(String),
}

impl Tunnel {
    /// Spawn `command` (split on whitespace, honouring double quotes).
    pub fn spawn(command: &str) -> Result<Self, String> {
        let parts = split_command(command)?;
        let (program, args) = parts
            .split_first()
            .ok_or_else(|| "tunnel command is empty".to_string())?;

        let mut builder = Command::new(program);
        builder
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            builder.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = builder
            .spawn()
            .map_err(|err| format!("failed to run tunnel `{program}`: {err}"))?;

        let stderr_tail = Arc::new(Mutex::new(VecDeque::new()));
        if let Some(stderr) = child.stderr.take() {
            let collector = Arc::clone(&stderr_tail);
            std::thread::Builder::new()
                .name("tunnel-stderr".into())
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
                .expect("spawn tunnel-stderr thread");
        }

        Ok(Self {
            command: command.to_string(),
            child,
            stderr_tail,
        })
    }

    /// The command line this tunnel was started from.
    pub fn command(&self) -> &str {
        &self.command
    }

    /// Whether the process is still up, without blocking on it.
    pub fn state(&mut self) -> TunnelState {
        match self.child.try_wait() {
            Ok(None) => TunnelState::Running,
            Ok(Some(status)) => TunnelState::Exited(format!("{status}")),
            Err(err) => TunnelState::Exited(format!("wait failed: {err}")),
        }
    }

    /// The tunnel's last words — where ssh explains itself.
    pub fn stderr_tail(&self) -> Vec<String> {
        self.stderr_tail
            .lock()
            .map(|tail| tail.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Stop the tunnel. Called on disconnect and on the way out of the app.
    pub fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Split a command line into program and arguments on whitespace, keeping
/// double-quoted runs together. Not a shell: no escapes, no substitution,
/// no operators — just enough to let a Windows path with spaces be quoted.
fn split_command(command: &str) -> Result<Vec<String>, String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut has_current = false;
    for ch in command.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                has_current = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if has_current {
                    parts.push(std::mem::take(&mut current));
                    has_current = false;
                }
            }
            c => {
                current.push(c);
                has_current = true;
            }
        }
    }
    if in_quotes {
        return Err("tunnel command has an unclosed quote".to_string());
    }
    if has_current {
        parts.push(current);
    }
    if parts.is_empty() {
        return Err("tunnel command is empty".to_string());
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_a_plain_ssh_command() {
        assert_eq!(
            split_command("ssh -N -L 9000:127.0.0.1:9000 user@vps").unwrap(),
            vec!["ssh", "-N", "-L", "9000:127.0.0.1:9000", "user@vps"]
        );
    }

    #[test]
    fn keeps_quoted_runs_together() {
        assert_eq!(
            split_command(r#""C:\Program Files\ssh.exe" -N vps"#).unwrap(),
            vec![r"C:\Program Files\ssh.exe", "-N", "vps"]
        );
        // An empty quoted argument is still an argument.
        assert_eq!(split_command(r#"cmd "" x"#).unwrap(), vec!["cmd", "", "x"]);
    }

    #[test]
    fn rejects_what_it_cannot_run() {
        assert!(split_command("").is_err());
        assert!(split_command("   ").is_err());
        assert!(split_command(r#"ssh "unclosed"#).is_err());
    }

    #[test]
    fn a_missing_program_is_an_error_not_a_panic() {
        let err = Tunnel::spawn("kimi-no-such-tunnel-program --flag").unwrap_err();
        assert!(err.contains("failed to run tunnel"), "{err}");
    }

    #[test]
    fn a_process_that_exits_is_reported_as_exited() {
        // Any binary that exits promptly; the state must flip off Running.
        let program = if cfg!(windows) { "cmd" } else { "true" };
        let args = if cfg!(windows) { " /c exit 0" } else { "" };
        let Ok(mut tunnel) = Tunnel::spawn(&format!("{program}{args}")) else {
            return; // no such shell on this machine: nothing to assert
        };
        for _ in 0..50 {
            if let TunnelState::Exited(_) = tunnel.state() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("the tunnel process never reported as exited");
    }
}
