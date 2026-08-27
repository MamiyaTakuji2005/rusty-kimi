use std::path::PathBuf;

use schemars::JsonSchema;
use serde::Deserialize;

use kosong::tooling::{CallableTool2, ToolReturnValue};

use crate::soul::agent::Runtime;
use crate::tasks::BackgroundTaskManager;
use crate::tools::agent::{SpawnSubagentArgs, spawn_agent_subprocess};
use crate::tools::utils::ToolResultBuilder;

const FORK_DESC: &str = include_str!("desc/agent/fork.md");

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ForkParams {
    #[schemars(
        description = "The task or question to hand to the fork. The fork starts with a full copy of the current conversation, then receives this as the next user message."
    )]
    pub prompt: String,
    #[serde(default)]
    #[schemars(
        description = "Short label for the fork shown in task lists. Defaults to a generic label."
    )]
    pub description: String,
}

/// Spawn a copy of the current agent that shares this conversation's full
/// context and runs a sub-task concurrently in the background. Unlike `Agent`,
/// which starts a fresh subagent with an empty context, `Fork` seeds the child
/// with a snapshot of the parent's `context.jsonl`, so it picks up mid-session.
pub struct ForkTool {
    description: String,
    session_dir: PathBuf,
    parent_context_file: PathBuf,
    agent_file: PathBuf,
    work_dir: String,
    background_tasks: BackgroundTaskManager,
}

impl ForkTool {
    pub fn new(runtime: &Runtime) -> Self {
        let session_dir = runtime
            .session
            .context_file
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_path_buf();
        Self {
            description: FORK_DESC.to_string(),
            session_dir,
            parent_context_file: runtime.session.context_file.clone(),
            agent_file: runtime.agent_file.clone(),
            work_dir: runtime
                .builtin_args
                .KIMI_WORK_DIR
                .to_string_lossy()
                .to_string(),
            background_tasks: runtime.background_tasks.clone(),
        }
    }
}

#[async_trait::async_trait]
impl CallableTool2 for ForkTool {
    type Params = ForkParams;

    fn name(&self) -> &str {
        "Fork"
    }

    fn description(&self) -> &str {
        &self.description
    }

    async fn call_typed(&self, params: Self::Params) -> ToolReturnValue {
        let builder = ToolResultBuilder::default();

        if params.prompt.trim().is_empty() {
            return builder.error("prompt is required", "Missing prompt");
        }

        let fork_id = uuid::Uuid::new_v4().to_string();
        let fork_dir = self.session_dir.join("subagents").join(&fork_id);
        if let Err(err) = tokio::fs::create_dir_all(&fork_dir).await {
            return builder.error(
                &format!("Failed to create fork dir: {err}"),
                "Dir creation failed",
            );
        }
        let context_file = fork_dir.join("context.jsonl");

        // Seed the fork with a snapshot of the parent's context so it starts
        // mid-conversation. If the parent has no context file yet (empty
        // session), the fork simply starts fresh — the same agent file still
        // supplies the system prompt and toolset.
        if self.parent_context_file.exists() {
            if let Err(err) = tokio::fs::copy(&self.parent_context_file, &context_file).await {
                return builder.error(
                    &format!("Failed to copy parent context into fork: {err}"),
                    "Context copy failed",
                );
            }
        }

        let description = if params.description.trim().is_empty() {
            "Fork".to_string()
        } else {
            format!("Fork: {}", params.description.trim())
        };

        spawn_agent_subprocess(SpawnSubagentArgs {
            subagent_id: &fork_id,
            agent_file: &self.agent_file,
            context_file: &context_file,
            prompt: &params.prompt,
            work_dir: &self.work_dir,
            system_prompt_args: &[],
            description: &description,
            background_tasks: &self.background_tasks,
        })
        .await
    }
}
