use std::path::PathBuf;

use kaos::KaosPath;

#[derive(Clone, Debug)]
pub struct Environment {
    pub os_kind: String,
    pub os_arch: String,
    pub os_version: String,
    pub shell_name: String,
    pub shell_path: KaosPath,
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Locate Git Bash (`bash.exe` from Git for Windows).
///
/// We deliberately never trust a bare `bash.exe` found on `PATH`: on Windows
/// that is almost always the WSL launcher (`System32\bash.exe`) or its
/// WindowsApps alias, which drops into a Linux distro rather than Git Bash.
/// Instead we look in the well-known Git for Windows install roots and derive
/// the root from `git.exe` on `PATH` (covers portable / custom installs).
#[cfg(windows)]
fn find_git_bash() -> Option<PathBuf> {
    // Explicit override wins, for non-standard setups.
    if let Some(raw) = std::env::var_os("KIMI_SHELL_BIN") {
        let path = PathBuf::from(raw);
        if path.is_file() {
            return Some(path);
        }
    }

    let mut roots: Vec<PathBuf> = Vec::new();
    for var in ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(dir) = std::env::var_os(var) {
            roots.push(PathBuf::from(dir).join("Git"));
        }
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        roots.push(PathBuf::from(local).join("Programs").join("Git"));
    }
    // Derive the Git root from git.exe on PATH: it lives at either
    // `<root>\cmd\git.exe` or `<root>\mingw64\bin\git.exe`.
    if let Some(git) = find_on_path("git.exe") {
        let mut ancestors = git.ancestors();
        ancestors.next(); // git.exe
        if let Some(dir) = ancestors.next() {
            // `<root>\cmd` -> `<root>`
            if let Some(root) = dir.parent() {
                roots.push(root.to_path_buf());
            }
        }
        // `<root>\mingw64\bin\git.exe` -> `<root>`
        if let Some(root) = git.ancestors().nth(3) {
            roots.push(root.to_path_buf());
        }
    }

    for root in roots {
        // `bin\bash.exe` is the launcher wrapper; prefer it over the raw
        // `usr\bin\bash.exe`. Both accept `-c`.
        for sub in [&["bin", "bash.exe"][..], &["usr", "bin", "bash.exe"][..]] {
            let mut candidate = root.clone();
            for part in sub {
                candidate.push(part);
            }
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(windows)]
fn find_windows_shell() -> (String, KaosPath) {
    // Prefer Git Bash when available — most CLIs and POSIX-style commands
    // behave better there than under PowerShell.
    if let Some(path) = find_git_bash() {
        return ("bash".to_string(), KaosPath::from(path));
    }

    // Otherwise prefer PowerShell 7 (pwsh.exe) over legacy Windows PowerShell 5.1.
    for name in ["pwsh.exe", "powershell.exe"] {
        if let Some(path) = find_on_path(name) {
            let label = if name == "pwsh.exe" {
                "PowerShell 7"
            } else {
                "Windows PowerShell"
            };
            return (label.to_string(), KaosPath::from(path));
        }
    }
    // Last-resort fallback — should never be reached on a normal Windows install.
    (
        "Windows PowerShell".to_string(),
        KaosPath::from("powershell.exe".into()),
    )
}

#[cfg(not(windows))]
fn find_windows_shell() -> (String, KaosPath) {
    unreachable!()
}

impl Environment {
    pub async fn detect() -> Self {
        let os_kind = match std::env::consts::OS {
            "macos" => "macOS",
            "windows" => "Windows",
            "linux" => "Linux",
            other => other,
        }
        .to_string();

        let os_arch = std::env::consts::ARCH.to_string();
        let os_version = sysinfo::System::long_os_version().unwrap_or_default();

        if os_kind == "Windows" {
            let (shell_name, shell_path) = find_windows_shell();
            return Environment {
                os_kind,
                os_arch,
                os_version,
                shell_name,
                shell_path,
            };
        }

        let mut shell_name = "sh".to_string();
        let mut shell_path = KaosPath::from("/bin/sh".into());
        for candidate in ["/bin/bash", "/usr/bin/bash", "/usr/local/bin/bash"] {
            let path = KaosPath::from(candidate.into());
            if path.is_file(true).await {
                shell_name = "bash".to_string();
                shell_path = path;
                break;
            }
        }

        Environment {
            os_kind,
            os_arch,
            os_version,
            shell_name,
            shell_path,
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// On a machine with Git for Windows installed, detection must resolve to
    /// Git Bash — never the WSL launcher in System32 — and report it as `bash`
    /// so the Shell tool uses the bash description and `-c` invocation.
    #[tokio::test]
    async fn detect_prefers_git_bash_over_wsl() {
        let env = Environment::detect().await;

        // Skip on CI runners without Git installed; there we fall back to PowerShell.
        if find_git_bash().is_none() {
            return;
        }

        assert_eq!(env.shell_name, "bash", "shell_name should be bash");
        let path = env.shell_path.to_string_lossy().to_lowercase();
        assert!(path.ends_with("bash.exe"), "unexpected shell path: {path}");
        assert!(
            !path.contains("system32"),
            "must not pick the WSL launcher: {path}"
        );
    }
}
