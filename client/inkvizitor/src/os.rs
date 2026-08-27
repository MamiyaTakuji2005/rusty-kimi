//! Handing a path to the desktop: "open this in whatever the user configured".

use std::path::Path;
use std::process::{Command, Stdio};

/// Open a file or directory with the system's default handler — the editor
/// registered for `.toml`, the file manager for a directory.
///
/// Fire and forget: the handler outlives this call, and nothing about the GUI
/// depends on how it goes, so only the failure to *launch* it is reported.
pub fn open_in_default_app(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Err(format!("{} does not exist", path.display()));
    }
    let mut command = launcher(path);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Without this the `cmd` shim flashes up a console window — exactly
        // the one suppressed for the agent in `client.rs`.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("could not open {}: {err}", path.display()))
}

#[cfg(windows)]
fn launcher(path: &Path) -> Command {
    let mut command = Command::new("cmd");
    // `start` is a shell builtin, hence `cmd /c`. Its first quoted argument is
    // the window title, and omitting it would swallow a quoted path as one.
    command.arg("/c").arg("start").arg("").arg(path);
    command
}

#[cfg(target_os = "macos")]
fn launcher(path: &Path) -> Command {
    let mut command = Command::new("open");
    command.arg(path);
    command
}

#[cfg(all(unix, not(target_os = "macos")))]
fn launcher(path: &Path) -> Command {
    let mut command = Command::new("xdg-open");
    command.arg(path);
    command
}
