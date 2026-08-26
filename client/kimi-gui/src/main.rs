//! kimi-gui: a native egui frontend for the kimi-agent wire protocol.
//!
//! Usage:
//!   kimi-gui [--agent-bin <path>] [agent args...]
//!
//! The agent binary is resolved by [`wire_client::launch`] (flag, then
//! environment, then a sibling executable, then `PATH`); everything else on
//! the command line goes to the agent verbatim (e.g. `-w <dir>`,
//! `--session <id>`, `--continue`).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod os;
mod palette;
mod render;
mod session;
mod theme;

use wire_client::launch::AgentLaunch;

fn main() -> eframe::Result<()> {
    let launch = match AgentLaunch::from_env() {
        Ok(launch) => launch,
        Err(message) => fatal(&message),
    };
    let agent_bin = launch.agent_bin;
    let args = launch.agent_args;

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
