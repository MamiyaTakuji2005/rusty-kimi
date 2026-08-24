//! kimi-gui: a native egui frontend for the kimi-agent wire protocol.
//!
//! Usage:
//!   kimi-gui [--agent-bin <path>] [agent args...]
//!
//! The agent binary is taken from `--agent-bin` or the `KIMI_AGENT_BIN`
//! environment variable. All remaining arguments are passed through to the
//! agent verbatim (e.g. `-w <dir>`, `--session <id>`, `--continue`).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod client;
mod render;
mod session;
mod session_list;
mod transcript;

fn main() -> eframe::Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut agent_bin = std::env::var("KIMI_AGENT_BIN").ok();
    if let Some(pos) = args.iter().position(|a| a == "--agent-bin") {
        if pos + 1 < args.len() {
            args.remove(pos);
            agent_bin = Some(args.remove(pos));
        } else {
            eprintln!("--agent-bin requires a path argument");
            std::process::exit(2);
        }
    }
    let Some(agent_bin) = agent_bin else {
        eprintln!("no agent binary: pass --agent-bin <path> or set KIMI_AGENT_BIN");
        std::process::exit(2);
    };

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
