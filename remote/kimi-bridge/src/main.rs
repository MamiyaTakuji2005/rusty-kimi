//! `kimi-bridge` binary: relay daemons for driving a remote `kimi-agent`.
//!
//! ```text
//! # on the agent machine (e.g. behind an ssh -L tunnel):
//! kimi-bridge remote --listen 127.0.0.1:9000
//! # on the frontend machine (optional hop; ssh -L can land on the
//! # remote daemon directly):
//! kimi-bridge local --upstream 127.0.0.1:9000
//! # then:
//! kimi-tui --remote 127.0.0.1:9000 -w /remote/dir
//! ```
//!
//! Security: the bridge carries shell commands and approval prompts with
//! no authentication or encryption of its own. Bind to loopback only and
//! cross the network through an ssh tunnel.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use tokio::net::TcpListener;

use kimi_bridge::{local_daemon, remote_daemon};

#[derive(Parser)]
#[command(
    name = "kimi-bridge",
    about = "Relay a kimi-agent wire connection across the network",
    long_about = "Relay a kimi-agent wire connection across the network.\n\
                  \n\
                  Two daemons, one binary: `remote` runs on the machine that hosts \
                  kimi-agent and spawns one agent per connection; `local` runs on the \
                  frontend machine and forwards connections upstream. Both are dumb \
                  byte relays with no authentication or encryption — bind them to \
                  loopback and cross the network through an ssh tunnel (`ssh -L`), \
                  never expose them directly."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run on the agent machine: spawn kimi-agent per connection and relay its stdio
    Remote {
        /// Address to listen on (keep it loopback)
        #[arg(long, default_value = "127.0.0.1:9000")]
        listen: String,
        /// kimi-agent binary to spawn; default: $KIMI_AGENT_BIN, a sibling
        /// of this executable, then $PATH
        #[arg(long)]
        agent_bin: Option<String>,
    },
    /// Run on the frontend machine: forward bridge connections upstream
    Local {
        /// Address to listen on (keep it loopback)
        #[arg(long, default_value = "127.0.0.1:9001")]
        listen: String,
        /// Upstream (remote) daemon address to forward to
        #[arg(long)]
        upstream: String,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Remote { listen, agent_bin } => {
            let agent_bin = agent_bin
                .or_else(env_agent_bin)
                .or_else(sibling_agent_bin)
                .unwrap_or_else(|| "kimi-agent".to_string());
            eprintln!("kimi-bridge: remote daemon listening on {listen}");
            eprintln!("kimi-bridge: spawning agent: {agent_bin}");
            eprintln!("kimi-bridge: no auth/TLS — keep this on loopback behind an ssh tunnel");
            run(&listen, |listener| {
                remote_daemon::serve(listener, agent_bin.clone())
            })
            .await
        }
        Command::Local { listen, upstream } => {
            eprintln!("kimi-bridge: local daemon listening on {listen}");
            eprintln!("kimi-bridge: forwarding to upstream {upstream}");
            run(&listen, |listener| {
                local_daemon::serve(listener, upstream.clone())
            })
            .await
        }
    };
    if let Err(err) = result {
        eprintln!("kimi-bridge: {err}");
        std::process::exit(1);
    }
}

async fn run<F>(listen: &str, serve: impl FnOnce(TcpListener) -> F) -> std::io::Result<()>
where
    F: std::future::Future<Output = std::io::Result<()>>,
{
    match TcpListener::bind(listen).await {
        Ok(listener) => serve(listener).await,
        Err(err) => Err(std::io::Error::new(
            err.kind(),
            format!("failed to listen on {listen}: {err}"),
        )),
    }
}

/// A `KIMI_AGENT_BIN` override, mirroring `wire_client::launch`.
fn env_agent_bin() -> Option<String> {
    std::env::var("KIMI_AGENT_BIN")
        .ok()
        .filter(|s| !s.is_empty())
}

/// A `kimi-agent` next to this executable — mirrors
/// `wire_client::launch` so a copied-around install works unchanged.
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
