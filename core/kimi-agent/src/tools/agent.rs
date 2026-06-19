use std::path::PathBuf;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tracing::{debug, info, warn};

use kosong::tooling::{CallableTool2, ToolReturnValue};

use crate::soul::agent::Runtime;
use crate::soul::approval::Approval;
use crate::soul::get_current_wire_or_none;
use crate::soul::toolset::get_current_tool_call_or_none;
use crate::tools::utils::{ToolResultBuilder, tool_rejected_error};
use crate::wire::{
    ContentPart, SubagentEvent, WIRE_PROTOCOL_VERSION, Wire, WireMessage,
    deserialize_wire_message,
};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AgentParams {
    #[schemars(description = "Path to the agent spec YAML file.")]
    pub agent_file: String,
    #[schemars(description = "The task or prompt to send to the subagent.")]
    pub prompt: String,
    #[serde(default)]
    #[schemars(description = "Extra system prompt template args as KEY=VALUE strings.")]
    pub system_prompt_args: Vec<String>,
}

pub struct AgentTool {
    session_dir: PathBuf,
    approval: Arc<Approval>,
    work_dir: String,
}

impl AgentTool {
    pub fn new(runtime: &Runtime) -> Self {
        let session_dir = runtime
            .session
            .context_file
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_path_buf();
        Self {
            session_dir,
            approval: runtime.approval.clone(),
            work_dir: runtime.builtin_args.KIMI_WORK_DIR.to_string_lossy(),
        }
    }
}

async fn run_subagent(
    binary: PathBuf,
    agent_file: PathBuf,
    context_file: PathBuf,
    system_prompt_args: Vec<String>,
    prompt: String,
    work_dir: String,
    parent_wire: Option<Arc<Wire>>,
    tool_call_id: String,
) -> anyhow::Result<String> {
    let mut cmd = tokio::process::Command::new(&binary);
    cmd.arg("--yolo")
        .arg("--agent-file")
        .arg(&agent_file)
        .arg("--context-file")
        .arg(&context_file)
        .arg("--work-dir")
        .arg(&work_dir);
    for arg in &system_prompt_args {
        cmd.arg("--system-prompt-arg").arg(arg);
    }
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn()?;
    let mut stdin = child.stdin.take().expect("stdin piped");
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let mut reader = BufReader::new(stdout);
    let mut stderr_reader = BufReader::new(stderr);

    // Send initialize
    let init = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": "1",
        "method": "initialize",
        "params": { "protocol_version": WIRE_PROTOCOL_VERSION }
    }))?;
    stdin.write_all(init.as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await?;

    // Wait for initialize response (id="1")
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            let mut err_output = String::new();
            let _ = stderr_reader.read_to_string(&mut err_output).await;
            let err_output = err_output.trim().to_string();
            if err_output.is_empty() {
                anyhow::bail!("Subagent closed stdout before initialize response (no stderr)");
            } else {
                anyhow::bail!("Subagent died before initialize response:\n{err_output}");
            }
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let msg: Value = serde_json::from_str(trimmed)?;
        if msg.get("id").and_then(|v| v.as_str()) == Some("1") && msg.get("method").is_none() {
            if msg.get("error").is_some() {
                let err_msg = msg["error"]["message"]
                    .as_str()
                    .unwrap_or("unknown")
                    .to_string();
                anyhow::bail!("Subagent initialize failed: {err_msg}");
            }
            break;
        }
    }

    // Send prompt
    let prompt_msg = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": "2",
        "method": "prompt",
        "params": { "user_input": prompt }
    }))?;
    stdin.write_all(prompt_msg.as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await?;

    // Read events until prompt response (id="2"), forwarding each to the parent wire.
    let mut output_parts: Vec<String> = Vec::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let msg_json: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                warn!("Subagent invalid JSON: {e}");
                continue;
            }
        };
        // Prompt response: has id="2" and no method field
        if msg_json.get("id").and_then(|v| v.as_str()) == Some("2")
            && msg_json.get("method").is_none()
        {
            break;
        }
        // Event notification: method="event"
        if msg_json.get("method").and_then(|v| v.as_str()) == Some("event") {
            if let Some(params_val) = msg_json.get("params") {
                match deserialize_wire_message(params_val.clone()) {
                    Ok(event) => {
                        if let WireMessage::ContentPart(ContentPart::Text(ref part)) = event {
                            output_parts.push(part.text.clone());
                        }
                        if let Some(ref wire) = parent_wire {
                            if let Ok(subagent_event) =
                                SubagentEvent::new(&tool_call_id, event)
                            {
                                wire.soul_side()
                                    .send(WireMessage::SubagentEvent(subagent_event));
                            }
                        }
                    }
                    Err(e) => {
                        debug!("Failed to deserialize subagent event: {e}");
                    }
                }
            }
        }
    }

    drop(stdin);
    drop(stderr_reader);
    let _ = child.wait().await;

    Ok(output_parts.join(""))
}

#[async_trait::async_trait]
impl CallableTool2 for AgentTool {
    type Params = AgentParams;

    fn name(&self) -> &str {
        "Agent"
    }

    fn description(&self) -> &str {
        "Run a separate agent process to handle a task and return its text output. \
         The child agent gets its own session and tool access. \
         Prefer this only when the task genuinely benefits from isolation; \
         for most things, just do it directly."
    }

    async fn call_typed(&self, params: Self::Params) -> ToolReturnValue {
        let mut builder = ToolResultBuilder::default();

        if params.agent_file.trim().is_empty() {
            return builder.error("agent_file is required", "Missing agent_file");
        }
        if params.prompt.trim().is_empty() {
            return builder.error("prompt is required", "Missing prompt");
        }

        let agent_file_path = PathBuf::from(&params.agent_file);
        if !agent_file_path.exists() {
            return builder.error(
                &format!("agent_file not found: {}", params.agent_file),
                "Agent file not found",
            );
        }

        let approved = match self
            .approval
            .request(
                self.name(),
                "spawn subagent",
                &format!("Spawn subagent: {}", params.agent_file),
                None,
            )
            .await
        {
            Ok(v) => v,
            Err(_) => false,
        };
        if !approved {
            return tool_rejected_error();
        }

        // Capture parent wire and tool call ID now, while task-locals are still in scope.
        let parent_wire = get_current_wire_or_none();
        let tool_call_id = get_current_tool_call_or_none()
            .map(|tc| tc.id)
            .unwrap_or_default();

        let subagent_id = uuid::Uuid::new_v4().to_string();
        let subagent_dir = self.session_dir.join("subagents").join(&subagent_id);
        if let Err(err) = tokio::fs::create_dir_all(&subagent_dir).await {
            return builder.error(
                &format!("Failed to create subagent dir: {err}"),
                "Dir creation failed",
            );
        }
        let context_file = subagent_dir.join("context.jsonl");

        let binary = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                return builder.error(
                    &format!("Cannot locate kimi-agent binary: {e}"),
                    "Binary not found",
                )
            }
        };

        info!(
            "Spawning subagent {} agent_file={}",
            subagent_id, params.agent_file
        );

        match run_subagent(
            binary,
            agent_file_path,
            context_file,
            params.system_prompt_args,
            params.prompt,
            self.work_dir.clone(),
            parent_wire,
            tool_call_id,
        )
        .await
        {
            Ok(output) => {
                if output.is_empty() {
                    return builder.error("Subagent produced no text output", "Empty response");
                }
                builder.write(&output);
                builder.ok("Subagent completed", &format!("subagent={subagent_id}"))
            }
            Err(err) => builder.error(&err.to_string(), "Subagent error"),
        }
    }
}
