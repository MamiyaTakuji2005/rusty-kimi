//! Embeds the Windows icon resource, so the .exe carries the project's mark
//! in Explorer rather than the blank default. A console binary has no window
//! of its own, so this is the only icon it gets -- inkvizitor additionally
//! sets a window icon at run time. The mark comes from the workspace's
//! `assets/make_icon.py`, and all three binaries share the one file.

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
                "cargo:warning=could not embed the icon resource ({err}); \
                 the .exe will show the default one"
            );
        }
    }
}
