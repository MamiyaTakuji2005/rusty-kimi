use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use kosong::tooling::{CallableTool2, ToolReturnValue, tool_error};

use crate::tasks::BackgroundTaskManager;
use crate::tools::utils::ToolResultBuilder;

const DEFAULT_MAX_OUTPUT: usize = 50_000;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskOutputParams {
    #[schemars(description = "The background task ID to inspect.")]
    pub task_id: String,
    #[serde(default)]
    #[schemars(
        description = "Whether to wait for the task to finish before returning.",
        default
    )]
    pub block: bool,
    #[serde(default = "default_timeout")]
    #[schemars(
        description = "Maximum number of seconds to wait when block=true.",
        default = "default_timeout",
        range(min = 0, max = 3600)
    )]
    pub timeout: i64,
}

fn default_timeout() -> i64 {
    30
}

pub struct TaskOutput {
    description: String,
    background_tasks: BackgroundTaskManager,
}

impl TaskOutput {
    pub fn new(runtime: &crate::soul::agent::Runtime) -> Self {
        Self {
            description: "Retrieve output from a running or completed background task.\n\nUse this after `Shell(run_in_background=true)` when you need to inspect progress or explicitly wait for completion.\n\nGuidelines:\n- Prefer relying on automatic completion notifications. Use this tool only when you need task output before the automatic notification arrives.\n- By default this tool is non-blocking and returns a current status/output snapshot.\n- Use `block=true` only when you intentionally want to wait for completion or timeout.".to_string(),
            background_tasks: runtime.background_tasks.clone(),
        }
    }
}

#[async_trait::async_trait]
impl CallableTool2 for TaskOutput {
    type Params = TaskOutputParams;

    fn name(&self) -> &str {
        "TaskOutput"
    }

    fn description(&self) -> &str {
        &self.description
    }

    async fn call_typed(&self, params: Self::Params) -> ToolReturnValue {
        let view = match self.background_tasks.get(&params.task_id) {
            Some(view) => view,
            None => {
                return tool_error(
                    "",
                    &format!("Task '{}' not found.", params.task_id),
                    "Task not found",
                );
            }
        };

        let mut builder = ToolResultBuilder::new(DEFAULT_MAX_OUTPUT, None);

        // If block is requested, wait for the completion notification — but
        // close the TOCTOU race: `notify_waiters()` only wakes already-registered
        // waiters, so a task that completes between the status check and
        // registration would otherwise hang until timeout. Arm the notified
        // future first, THEN re-check terminal status; if it's already done (or
        // finishes after arming), we don't block.
        let _ = view; // initial snapshot only used for the not-found check above
        if params.block {
            if let Some(notify) = self.background_tasks.notify(&params.task_id) {
                let notified = notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let still_running = self
                    .background_tasks
                    .get(&params.task_id)
                    .map(|v| !v.status.is_terminal())
                    .unwrap_or(false);
                if still_running {
                    let _ = tokio::time::timeout(
                        Duration::from_secs(params.timeout as u64),
                        notified,
                    )
                    .await;
                }
            }
        }

        // Read output buffers.
        let stdout = self.background_tasks.stdout(&params.task_id);
        let stderr = self.background_tasks.stderr(&params.task_id);

        if let Some(buf) = stdout {
            let data = buf.lock().unwrap().clone();
            if !data.is_empty() {
                builder.write("[stdout]\n");
                if let Ok(text) = String::from_utf8(data) {
                    builder.write(&text);
                }
            }
        }
        if let Some(buf) = stderr {
            let data = buf.lock().unwrap().clone();
            if !data.is_empty() {
                builder.write("[stderr]\n");
                if let Ok(text) = String::from_utf8(data) {
                    builder.write(&text);
                }
            }
        }

        let view = self.background_tasks.get(&params.task_id);
        let status_str = view
            .as_ref()
            .map(|v| v.status.as_str())
            .unwrap_or("unknown");
        let summary = format!("Task `{}`: status={}", params.task_id, status_str);

        builder.ok(&summary, "")
    }
}
