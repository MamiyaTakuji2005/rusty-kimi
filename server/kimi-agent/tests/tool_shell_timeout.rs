mod tool_test_utils;

use kaos::with_current_kaos_scope;
use kimi_agent::soul::toolset::with_current_tool_call;
use kimi_agent::tools::shell::{Shell, ShellParams};
use kosong::message::ToolCall;
use kosong::tooling::CallableTool2;

use tool_test_utils::{RuntimeFixture, TestKaosGuard};

/// A ~2s command requested with `timeout: 1` must still complete: the tool
/// floors any sub-30s timeout up to 30s, so short per-call timeouts no longer
/// kill legitimately slow commands.
#[tokio::test]
async fn short_timeout_is_floored_and_command_survives() {
    let fixture = RuntimeFixture::new();
    let work_dir = fixture.runtime.builtin_args.KIMI_WORK_DIR.clone();
    let tool = Shell::new(&fixture.runtime);

    // The fixture uses PowerShell on Windows and bash elsewhere.
    let command = if cfg!(windows) {
        "Start-Sleep -Seconds 2".to_string()
    } else {
        "sleep 2".to_string()
    };

    with_current_kaos_scope(async move {
        let _guard = TestKaosGuard::new(work_dir);

        let call = ToolCall::new("test-call-id", "Shell");
        let result = with_current_tool_call(
            call,
            tool.call_typed(ShellParams {
                command,
                timeout: 1,
                run_in_background: false,
                description: String::new(),
            }),
        )
        .await;

        // Without the floor this would be `Killed by timeout (1s)`.
        assert!(
            !result.is_error,
            "command should survive: {}",
            result.message
        );
        assert!(!result.message.contains("timeout"), "{}", result.message);
    })
    .await;
}
