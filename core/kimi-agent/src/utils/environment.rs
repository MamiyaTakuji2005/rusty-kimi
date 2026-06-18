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

#[cfg(windows)]
fn find_windows_shell() -> (String, KaosPath) {
    // Prefer PowerShell 7 (pwsh.exe) over the legacy Windows PowerShell 5.1.
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
