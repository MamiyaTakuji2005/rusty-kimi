//! kimi-gui: a native egui frontend for the kimi-agent wire protocol.
//!
//! Usage:
//!   kimi-gui [--agent-bin <path>] [agent args...]
//!
//! The agent binary is taken from `--agent-bin`, the `KIMI_AGENT_BIN`
//! environment variable, or a `kimi-agent` sitting next to this executable —
//! in that order, so a double-clicked install needs no setup. All remaining
//! arguments are passed through to the agent verbatim (e.g. `-w <dir>`,
//! `--session <id>`, `--continue`).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod client;
mod render;
mod session;
mod session_list;
mod transcript;

/// The agent binary that ships alongside this one. This is what makes a
/// double-clicked `kimi-gui` work: no environment, no arguments, no console
/// to have read an error from if it had refused to start.
fn sibling_agent_bin() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let name = if cfg!(windows) {
        "kimi-agent.exe"
    } else {
        "kimi-agent"
    };
    let candidate = exe.parent()?.join(name);
    candidate
        .is_file()
        .then(|| candidate.to_string_lossy().into_owned())
}

/// Report a startup problem where the user can actually see it. Launched from
/// Explorer there is no console, so `eprintln!` alone goes nowhere.
fn fatal(message: &str) -> ! {
    eprintln!("{message}");
    rfd::MessageDialog::new()
        .set_title("Kimi")
        .set_description(message)
        .set_level(rfd::MessageLevel::Error)
        .show();
    std::process::exit(2)
}

fn main() -> eframe::Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut agent_bin = std::env::var("KIMI_AGENT_BIN").ok().filter(|s| !s.is_empty());
    if let Some(pos) = args.iter().position(|a| a == "--agent-bin") {
        if pos + 1 < args.len() {
            args.remove(pos);
            agent_bin = Some(args.remove(pos));
        } else {
            fatal("--agent-bin requires a path argument");
        }
    }
    // Falling back to the bare name lets PATH have the last word, and keeps
    // the window opening either way: a missing agent then surfaces as the
    // normal spawn-error modal rather than an invisible exit.
    let agent_bin = agent_bin
        .or_else(sibling_agent_bin)
        .unwrap_or_else(|| "kimi-agent".to_string());

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([980.0, 760.0])
            .with_min_inner_size([480.0, 360.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Kimi",
        options,
        Box::new(move |cc| {
            let app = app::KimiGuiApp::new(cc, &agent_bin, &args)
                .map_err(|err| -> Box<dyn std::error::Error + Send + Sync> { err.into() })?;
            Ok(Box::new(app))
        }),
    )
}
