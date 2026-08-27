//! inkvizitor: a native egui frontend for the dvadva-agent wire protocol.
//!
//! Usage:
//!   inkvizitor [--agent-bin <path>] [--remote <host:port>] [agent args...]
//!
//! The agent binary is resolved by [`wire_client::launch`] (flag, then
//! environment, then a sibling executable, then `PATH`); everything else on
//! the command line goes to the agent verbatim (e.g. `-w <dir>`,
//! `--session <id>`, `--continue`).
//!
//! `--remote <host:port>` (or `$KIMI_REMOTE`) connects through a
//! `dvadva-bridge` daemon instead: the agent — and its sessions, config and
//! skills — live on the daemon's machine, and agent arguments like `-w`
//! name paths *there*.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod os;
mod palette;
mod remote_link;
mod render;
mod session;
mod theme;

use wire_client::launch::AgentLaunch;

/// The window icon: two squared, the arithmetic the project is named for.
///
/// Raw RGBA rather than a PNG — eframe wants pixels, and decoding one would
/// mean pulling the `image` crate in for 16 KB of them. The .exe's *file*
/// icon is a separate thing that no run-time call can set; `build.rs`
/// embeds that as a Windows resource. Both come from the workspace's
/// `assets/make_icon.py`, and all three binaries share the one mark.
const ICON: &[u8] = include_bytes!("../../../assets/icon-64.rgba");
const ICON_SIDE: u32 = 64;
const _: () = assert!(
    ICON.len() == (ICON_SIDE * ICON_SIDE * 4) as usize,
    "icon-64.rgba is not 64x64 RGBA — regenerate it, or fix ICON_SIDE"
);

fn main() -> eframe::Result<()> {
    let launch = match AgentLaunch::from_env() {
        Ok(launch) => launch,
        Err(message) => fatal(&message),
    };
    let agent_bin = launch.agent_bin;
    let remote = launch.remote;
    let args = launch.agent_args;

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([980.0, 760.0])
            .with_min_inner_size([480.0, 360.0])
            .with_icon(eframe::egui::IconData {
                rgba: ICON.to_vec(),
                width: ICON_SIDE,
                height: ICON_SIDE,
            }),
        ..Default::default()
    };
    eframe::run_native(
        "Inkvizitor",
        options,
        Box::new(move |cc| {
            let app = app::InkvizitorApp::new(cc, &agent_bin, remote, &args)
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
        .set_title("Inkvizitor")
        .set_description(message)
        .set_level(rfd::MessageLevel::Error)
        .show();
    std::process::exit(2)
}
