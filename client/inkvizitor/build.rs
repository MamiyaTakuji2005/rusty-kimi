//! Embeds the Windows icon resource, so the .exe carries the mark in
//! Explorer, the taskbar and Alt+Tab — the file's own icon, which no
//! run-time call can set. The *window* icon is a separate thing that
//! `main.rs` sets. Both come from the workspace's `assets/make_icon.py`,
//! and `dvadva-agent` and `dvadva-tui` embed the same mark the same way.

fn main() {
    println!("cargo:rerun-if-changed=../../assets/dvadva.ico");

    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../assets/dvadva.ico");
        // Cosmetic, and it needs the SDK's `rc.exe`: a machine without one
        // should still get a working binary, just a blank-looking one.
        if let Err(err) = res.compile() {
            println!(
                "cargo:warning=inkvizitor: could not embed the icon resource ({err}); \
                 the .exe will show the default one"
            );
        }
    }
}
