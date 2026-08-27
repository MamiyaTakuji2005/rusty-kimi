use schemars::JsonSchema;
use serde::Deserialize;

use kosong::tooling::{CallableTool2, ToolReturnValue, tool_error, tool_ok};

use crate::tasks::BackgroundTaskManager;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskStopParams {
    #[schemars(description = "The background task ID to stop.")]
    pub task_id: String,
    #[serde(default = "default_reason")]
    #[schemars(
        description = "Short reason recorded when the task is stopped.",
        default = "default_reason"
    )]
    pub reason: String,
}

fn default_reason() -> String {
    "Stopped by TaskStop".to_string()
}

const STOP_DESC: &str = include_str!("../desc/task/stop.md");

pub struct TaskStop {
    description: String,
    background_tasks: BackgroundTaskManager,
}

impl TaskStop {
    pub fn new(runtime: &crate::soul::agent::Runtime) -> Self {
        Self {
            description: STOP_DESC.to_string(),
            background_tasks: runtime.background_tasks.clone(),
        }
    }
}

#[async_trait::async_trait]
impl CallableTool2 for TaskStop {
    type Params = TaskStopParams;

    fn name(&self) -> &str {
        "TaskStop"
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

        if view.status.is_terminal() {
            return tool_ok(
                "",
                &format!(
                    "Task `{}` is already in terminal state ({}).",
                    params.task_id,
                    view.status.as_str()
                ),
                "",
            );
        }

        match self.background_tasks.kill(&params.task_id) {
            Ok(()) => tool_ok("", &format!("Task `{}` stopped.", params.task_id), ""),
            Err(err) => tool_error("", &err, "Failed to stop"),
        }
    }
}
