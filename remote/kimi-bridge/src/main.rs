//! `kimi-bridge` binary: relay daemons for driving a remote `kimi-agent`.
//!
//! ```text
//! # on the agent machine (e.g. behind an ssh -L tunnel); with a
//! # [serve] section in ~/.kimi/bridge.toml, a bare `kimi-bridge remote`
//! # is the whole command:
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

use kimi_bridge::{config, local_daemon, remote_daemon};

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
        /// Address to listen on (keep it loopback).
        /// Default: [serve] listen in ~/.kimi/bridge.toml, else 127.0.0.1:9000
        #[arg(long)]
        listen: Option<String>,
        /// kimi-agent binary to spawn; default: [serve] agent_bin,
        /// $KIMI_AGENT_BIN, a sibling of this executable, then $PATH
        #[arg(long)]
        agent_bin: Option<String>,
        /// Work directory for agents whose spawn args name none; default:
        /// [serve] work_dir, else this user's home directory. A frontend on
        /// another OS then needs no `-w` at all — and cannot send it a path
        /// from its own machine.
        #[arg(long)]
        work_dir: Option<String>,
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

/// Where a resolved setting came from, for the startup banner: an operator
/// reading the log should be able to tell a default from a config file they
/// forgot they wrote.
fn source(flag: bool, config: bool) -> &'static str {
    match (flag, config) {
        (true, _) => "flag",
        (_, true) => "config",
        _ => "default",
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Remote {
            listen,
            agent_bin,
            work_dir,
        } => {
            // Flag beats config file beats built-in default, so a service
            // unit can be a bare `kimi-bridge remote` and a one-off run can
            // still override any of it.
            let serve = match config::load_serve() {
                Ok(serve) => serve,
                Err(err) => {
                    eprintln!("kimi-bridge: {err}");
                    std::process::exit(1);
                }
            };
            let listen_from = source(listen.is_some(), serve.listen.is_some());
            let listen = listen
                .or(serve.listen)
                .unwrap_or_else(|| "127.0.0.1:9000".to_string());
            let agent_from = source(agent_bin.is_some(), serve.agent_bin.is_some());
            let agent_bin = agent_bin
                .or(serve.agent_bin)
                .or_else(env_agent_bin)
                .or_else(sibling_agent_bin)
                .unwrap_or_else(|| "kimi-agent".to_string());
            let work_dir_from = source(work_dir.is_some(), serve.work_dir.is_some());
            let default_work_dir = match resolve_work_dir(work_dir.or(serve.work_dir)) {
                Ok(dir) => dir,
                Err(err) => {
                    eprintln!("kimi-bridge: {err}");
                    std::process::exit(1);
                }
            };
            eprintln!("kimi-bridge: config: {}", config::path().display());
            eprintln!("kimi-bridge: remote daemon listening on {listen} ({listen_from})");
            eprintln!("kimi-bridge: spawning agent: {agent_bin} ({agent_from})");
            match &default_work_dir {
                Some(dir) => eprintln!("kimi-bridge: default work dir: {dir} ({work_dir_from})"),
                None => eprintln!(
                    "kimi-bridge: no default work dir (no home directory found); \
                     clients must pass -w"
                ),
            }
            eprintln!("kimi-bridge: no auth/TLS — keep this on loopback behind an ssh tunnel");
            let config = remote_daemon::Config::new(agent_bin)
                .with_default_work_dir(default_work_dir.clone());
            run(&listen, |listener| {
                remote_daemon::serve(listener, config.clone())
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

/// The work directory agents get when the caller names none: the flag, else
/// this user's home directory. A frontend on another OS has no way to know a
/// path that exists here, so the daemon decides — and the home directory is
/// the right default for a box that exists to be experimented in.
///
/// An explicit `--work-dir` that does not exist is a startup error (the
/// alternative is every session failing later, one confusing tab at a time);
/// a missing home directory only costs the default.
fn resolve_work_dir(flag: Option<String>) -> Result<Option<String>, String> {
    if let Some(dir) = flag {
        let path = PathBuf::from(&dir);
        if !path.is_dir() {
            return Err(format!("--work-dir `{dir}` is not an existing directory"));
        }
        return Ok(Some(dir));
    }
    Ok(dirs::home_dir()
        .filter(|home| home.is_dir())
        .map(|home| home.to_string_lossy().into_owned()))
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
