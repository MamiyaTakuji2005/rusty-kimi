use schemars::JsonSchema;
use serde::Deserialize;

use kosong::tooling::{CallableTool2, ToolReturnValue, tool_ok};

use crate::tasks::BackgroundTaskManager;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskListParams {
    #[serde(default = "default_active_only")]
    #[schemars(
        description = "Whether to list only active (non-terminal) background tasks.",
        default = "default_active_only"
    )]
    pub active_only: bool,
    #[serde(default = "default_limit")]
    #[schemars(
        description = "Maximum number of tasks to return.",
        default = "default_limit",
        range(min = 1, max = 100)
    )]
    pub limit: i64,
}

fn default_active_only() -> bool {
    true
}

fn default_limit() -> i64 {
    20
}

pub struct TaskList {
    description: String,
    background_tasks: BackgroundTaskManager,
}

impl TaskList {
    pub fn new(runtime: &crate::soul::agent::Runtime) -> Self {
        Self {
            description: "List background tasks from the current session.\n\nUse this when you need to re-enumerate which background tasks still exist, especially after context compaction or when you are no longer confident which task IDs are still active.\n\nGuidelines:\n\n- Prefer the default `active_only=true` unless you specifically need completed or failed tasks.\n- Use `TaskOutput` to inspect one task in detail after you have identified the correct task ID.\n- Do not guess which tasks are still running when you can call this tool directly.\n- This tool is read-only and safe to use in plan mode.".to_string(),
            background_tasks: runtime.background_tasks.clone(),
        }
    }
}

#[async_trait::async_trait]
impl CallableTool2 for TaskList {
    type Params = TaskListParams;

    fn name(&self) -> &str {
        "TaskList"
    }

    fn description(&self) -> &str {
        &self.description
    }

    async fn call_typed(&self, params: Self::Params) -> ToolReturnValue {
        let tasks = self.background_tasks.list(params.active_only);

        if tasks.is_empty() {
            return tool_ok(
                "",
                if params.active_only {
                    "No active background tasks."
                } else {
                    "No background tasks."
                },
                "",
            );
        }

        let mut lines = Vec::new();
        for task in tasks.iter().take(params.limit as usize) {
            let exit = task
                .exit_code
                .map(|c| format!(", exit_code={c}"))
                .unwrap_or_default();
            let elapsed = task
                .completed_at
                .map(|t| format!(", elapsed={:.1}s", t - task.created_at))
                .unwrap_or_default();
            lines.push(format!(
                "{}: {} [{}]{}{}",
                task.spec.id, task.spec.description, task.status.as_str(), exit, elapsed
            ));
        }

        let summary = if tasks.len() > params.limit as usize {
            format!("Showing {} of {} tasks.", params.limit, tasks.len())
        } else {
            format!("{} task(s) found.", tasks.len())
        };

        tool_ok(lines.join("\n"), summary, "")
    }
}
