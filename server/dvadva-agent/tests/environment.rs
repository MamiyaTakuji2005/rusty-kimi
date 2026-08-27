use dvadva_agent::utils::Environment;

#[tokio::test]
async fn test_environment_detection() {
    let env = Environment::detect().await;

    assert!(!env.os_kind.is_empty());
    assert!(!env.os_arch.is_empty());
    assert!(!env.os_version.is_empty());

    if env.os_kind == "Windows" {
        // Windows prefers Git Bash; the fallback is PowerShell.
        let is_bash =
            env.shell_name == "bash" && env.shell_path.to_string_lossy().ends_with("bash.exe");
        let is_pwsh = env.shell_name == "Windows PowerShell"
            && env.shell_path.to_string_lossy() == "powershell.exe";
        assert!(is_bash || is_pwsh, "unexpected shell: {env:?}");
    } else {
        assert!(env.shell_name == "bash" || env.shell_name == "sh");
        let shell_path = env.shell_path.to_string_lossy();
        assert!(!shell_path.is_empty());
        if env.shell_name == "bash" {
            assert!(shell_path.ends_with("bash"));
        } else {
            assert!(shell_path.ends_with("sh"));
        }
    }
}
