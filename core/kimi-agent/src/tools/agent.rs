use std::path::PathBuf;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tracing::{debug, info, warn};

use kosong::tooling::{CallableTool2, ToolReturnValue};

use crate::agentspec::get_agents_dir;
use crate::soul::agent::Runtime;
use crate::soul::approval::Approval;
use crate::soul::get_current_wire_or_none;
use crate::tasks::{BackgroundTaskManager, TaskSpec, TaskStatus};
use crate::tools::utils::{ToolResultBuilder, tool_rejected_error};
use crate::wire::{
    ContentPart, SubagentEvent, WIRE_PROTOCOL_VERSION, Wire, WireMessage, deserialize_wire_message,
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
    description: String,
    session_dir: PathBuf,
    approval: Arc<Approval>,
    work_dir: String,
    background_tasks: BackgroundTaskManager,
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
            description: build_agent_description(),
            session_dir,
            approval: runtime.approval.clone(),
            work_dir: runtime.builtin_args.KIMI_WORK_DIR.to_string_lossy(),
            background_tasks: runtime.background_tasks.clone(),
        }
    }
}

/// Build the Agent tool description, injecting the list of available built-in
/// agents from the agents/default/agent.yaml subagents map.
fn build_agent_description() -> String {
    let agents_dir = get_agents_dir().join("default");
    let base_yaml = agents_dir.join("agent.yaml");

    let mut desc = "Run a separate agent process to handle a task and return its text output. \
         The child agent gets its own session and tool access. \
         Prefer this only when the task genuinely benefits from isolation; \
         for most things, just do it directly."
        .to_string();

    if let Some(agents) = read_subagents_from_yaml(&base_yaml, &agents_dir) {
        if !agents.is_empty() {
            desc.push_str("\n\nAvailable agents (pass the path as agent_file):");
            for (name, path, description) in agents {
                desc.push_str(&format!("\n- {name} ({path}): {description}"));
            }
        }
    }

    desc
}

/// Synchronously read the subagents map from a base agent YAML and return
/// (name, resolved_path, description) tuples.
fn read_subagents_from_yaml(
    base_yaml: &std::path::Path,
    agents_dir: &std::path::Path,
) -> Option<Vec<(String, String, String)>> {
    let content = std::fs::read_to_string(base_yaml).ok()?;
    let data: serde_yaml::Value = serde_yaml::from_str(&content).ok()?;
    let mapping = data.get("agent")?.get("subagents")?.as_mapping()?;

    let mut result = Vec::new();
    for (key, value) in mapping {
        let name = key.as_str().unwrap_or("").to_string();
        if name.is_empty() {
            continue;
        }
        let rel_path = value.get("path").and_then(|p| p.as_str()).unwrap_or("");
        let description = value
            .get("description")
            .and_then(|d| d.as_str())
            .unwrap_or("")
            .to_string();
        let resolved = if std::path::Path::new(rel_path).is_absolute() {
            rel_path.to_string()
        } else {
            agents_dir.join(rel_path).to_string_lossy().into_owned()
        };
        result.push((name, resolved, description));
    }
    Some(result)
}

/// Format a subagent wire event for inclusion in the TaskOutput buffer.
/// Returns None for events that should not appear in output (think blocks,
/// structural events, tool results, etc.).
fn format_event_for_output(event: &WireMessage) -> Option<Vec<u8>> {
    match event {
        WireMessage::ContentPart(ContentPart::Text(part)) => Some(part.text.as_bytes().to_vec()),
        WireMessage::ToolCall(call) => Some(format!("\n• {}\n", call.function.name).into_bytes()),
        _ => None,
    }
}

fn send_notification(bt: &BackgroundTaskManager, id: &str, wire: &Option<Arc<Wire>>) {
    if let Some(notification) = bt.build_notification(id) {
        if let Some(w) = wire {
            w.soul_side().send(WireMessage::Notification(notification));
        }
    }
}

#[async_trait::async_trait]
impl CallableTool2 for AgentTool {
    type Params = AgentParams;

    fn name(&self) -> &str {
        "Agent"
    }

    fn description(&self) -> &str {
        &self.description
    }

    async fn call_typed(&self, params: Self::Params) -> ToolReturnValue {
        let builder = ToolResultBuilder::default();

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

        let subagent_id = uuid::Uuid::new_v4().to_string();
        let subagent_dir = self.session_dir.join("subagents").join(&subagent_id);
        if let Err(err) = tokio::fs::create_dir_all(&subagent_dir).await {
            return builder.error(
                &format!("Failed to create subagent dir: {err}"),
                "Dir creation failed",
            );
        }
        let context_file = subagent_dir.join("context.jsonl");

        spawn_agent_subprocess(SpawnSubagentArgs {
            subagent_id: &subagent_id,
            agent_file: &agent_file_path,
            context_file: &context_file,
            prompt: &params.prompt,
            work_dir: &self.work_dir,
            system_prompt_args: &params.system_prompt_args,
            description: &format!("Subagent: {}", params.agent_file),
            background_tasks: &self.background_tasks,
        })
        .await
    }
}

/// Arguments for [`spawn_agent_subprocess`]. The caller prepares `context_file`
/// (empty for a fresh subagent, or a copy of the parent context for a fork).
pub(crate) struct SpawnSubagentArgs<'a> {
    pub subagent_id: &'a str,
    pub agent_file: &'a std::path::Path,
    pub context_file: &'a std::path::Path,
    pub prompt: &'a str,
    pub work_dir: &'a str,
    pub system_prompt_args: &'a [String],
    pub description: &'a str,
    pub background_tasks: &'a BackgroundTaskManager,
}

/// Spawn a child kimi-agent subprocess, perform the initialize handshake, send
/// the prompt, register it as a background task, and monitor it in a detached
/// task. Shared by the `Agent` and `Fork` tools.
pub(crate) async fn spawn_agent_subprocess(args: SpawnSubagentArgs<'_>) -> ToolReturnValue {
    let mut builder = ToolResultBuilder::default();

    let binary = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            return builder.error(
                &format!("Cannot locate kimi-agent binary: {e}"),
                "Binary not found",
            );
        }
    };

    // Spawn child process.
    let mut cmd = tokio::process::Command::new(&binary);
    cmd.arg("--yolo")
        .arg("--agent-file")
        .arg(args.agent_file)
        .arg("--context-file")
        .arg(args.context_file)
        .arg("--work-dir")
        .arg(args.work_dir);
    for arg in args.system_prompt_args {
        cmd.arg("--system-prompt-arg").arg(arg);
    }
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return builder.error(&format!("Failed to spawn subagent: {e}"), "Spawn failed"),
    };

    let mut stdin = child.stdin.take().expect("stdin piped");
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr_handle = child.stderr.take().expect("stderr piped");
    let mut reader = BufReader::new(stdout);
    let mut stderr_reader = BufReader::new(stderr_handle);

    // Do the initialize handshake synchronously so startup failures surface immediately.
    let init = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": "1",
        "method": "initialize",
        "params": { "protocol_version": WIRE_PROTOCOL_VERSION }
    }))
    .unwrap();
    if let Err(e) = stdin.write_all(init.as_bytes()).await {
        return builder.error(&format!("Failed to write initialize: {e}"), "Write error");
    }
    let _ = stdin.write_all(b"\n").await;
    let _ = stdin.flush().await;

    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await.unwrap_or(0);
        if n == 0 {
            let mut err_output = String::new();
            let _ = stderr_reader.read_to_string(&mut err_output).await;
            let err_output = err_output.trim().to_string();
            let detail = if err_output.is_empty() {
                "(no stderr)".to_string()
            } else {
                format!("\n{err_output}")
            };
            return builder.error(
                &format!("Subagent died before initialize response{detail}"),
                "Startup failed",
            );
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if msg.get("id").and_then(|v| v.as_str()) == Some("1") && msg.get("method").is_none() {
            if msg.get("error").is_some() {
                let err_msg = msg["error"]["message"]
                    .as_str()
                    .unwrap_or("unknown")
                    .to_string();
                return builder.error(
                    &format!("Subagent initialize failed: {err_msg}"),
                    "Initialize error",
                );
            }
            break;
        }
    }

    // Send prompt.
    let prompt_msg = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": "2",
        "method": "prompt",
        "params": { "user_input": args.prompt }
    }))
    .unwrap();
    if let Err(e) = stdin.write_all(prompt_msg.as_bytes()).await {
        return builder.error(&format!("Failed to send prompt: {e}"), "Write error");
    }
    let _ = stdin.write_all(b"\n").await;
    let _ = stdin.flush().await;

    // Register as a background task so TaskOutput / TaskStop work.
    let task_id = args.background_tasks.generate_id();
    info!("Spawning subagent {} task={}", args.subagent_id, task_id);

    let spec = TaskSpec {
        id: task_id.clone(),
        description: args.description.to_string(),
        command: args.agent_file.to_string_lossy().to_string(),
        shell_path: binary.to_string_lossy().to_string(),
        cwd: args.work_dir.to_string(),
        timeout_s: 3600,
        child_pid: child.id(),
    };

    let (kill_tx, mut kill_rx) = tokio::sync::oneshot::channel::<()>();
    let bt = args.background_tasks.clone();
    let (stdout_buf, _stderr_buf) = bt.register(spec, kill_tx);

    let parent_wire = get_current_wire_or_none();

    // Key for SubagentEvent forwarding: the invoking tool call's id when the
    // spawn came from a Fork/Agent tool call, or a task-derived id for
    // /fork-style spawns that have no tool call on the wire.
    let subagent_key = crate::soul::toolset::get_current_tool_call_or_none()
        .map(|call| call.id)
        .unwrap_or_else(|| format!("task-{task_id}"));

    let bt_clone = args.background_tasks.clone();
    let task_id_clone = task_id.clone();

    tokio::spawn(async move {
        // Keep stderr_reader alive until child exits so the child's stderr pipe stays open.
        let _stderr = stderr_reader;
        let mut line = String::new();
        loop {
            line.clear();
            tokio::select! {
                result = reader.read_line(&mut line) => {
                    let n = result.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let msg_json: Value = match serde_json::from_str(trimmed) {
                        Ok(v) => v,
                        Err(e) => { warn!("Subagent invalid JSON: {e}"); continue; }
                    };
                    // Prompt response signals end of turn.
                    if msg_json.get("id").and_then(|v| v.as_str()) == Some("2")
                        && msg_json.get("method").is_none()
                    {
                        break;
                    }
                    if msg_json.get("method").and_then(|v| v.as_str()) == Some("event") {
                        if let Some(params_val) = msg_json.get("params") {
                            match deserialize_wire_message(params_val.clone()) {
                                Ok(event) => {
                                    if let Some(bytes) = format_event_for_output(&event) {
                                        if let Ok(mut buf) = stdout_buf.lock() {
                                            buf.extend_from_slice(&bytes);
                                        }
                                    }
                                    // Mirror the child's event stream onto the
                                    // parent wire so clients can render the
                                    // subagent live (e.g. as its own tab).
                                    if let Some(w) = &parent_wire {
                                        if let Ok(sub) =
                                            SubagentEvent::new(subagent_key.clone(), event)
                                        {
                                            w.soul_side().send(sub.into());
                                        }
                                    }
                                }
                                Err(e) => { debug!("Failed to deserialize subagent event: {e}"); }
                            }
                        }
                    }
                }
                _ = &mut kill_rx => {
                    let _ = child.kill().await;
                    bt_clone.complete(&task_id_clone, TaskStatus::Killed, None);
                    send_notification(&bt_clone, &task_id_clone, &parent_wire);
                    return;
                }
            }
        }

        drop(stdin);
        let exit_status = child.wait().await;
        let (status, code) = match exit_status {
            Ok(s) => {
                let code = s.code();
                if code == Some(0) {
                    (TaskStatus::Completed, code)
                } else {
                    (TaskStatus::Failed, code)
                }
            }
            Err(_) => (TaskStatus::Failed, None),
        };
        bt_clone.complete(&task_id_clone, status, code);
        send_notification(&bt_clone, &task_id_clone, &parent_wire);
    });

    builder.write(&format!(
        "\
Task ID: {task_id}
Description: {description}
automatic_notification: true
next_step: You will be automatically notified when it completes.
next_step: Call TaskOutput with task_id={task_id} and block=true to wait for the result.
next_step: Use TaskStop only if the task must be cancelled.
",
        description = args.description,
    ));
    builder.ok("Subagent started", &format!("task={task_id}"))
}
