use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use tokio::io::AsyncBufReadExt;

use kaos::{AsyncReadable, KaosPath};
use kosong::tooling::error::tool_runtime_error;
use kosong::tooling::{CallableTool2, DisplayBlock, ShellDisplayBlock, ToolReturnValue};

use crate::soul::agent::Runtime;
use crate::soul::approval::Approval;
use crate::soul::get_current_wire_or_none;
use crate::tasks::{BackgroundTaskManager, TaskSpec, TaskStatus};
use crate::tools::utils::{ToolResultBuilder, load_desc, tool_rejected_error};

const DEFAULT_TIMEOUT: i64 = 60;

const BASH_DESC: &str = include_str!("desc/shell/bash.md");
const POWERSHELL_DESC: &str = include_str!("desc/shell/powershell.md");

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ShellParams {
    #[schemars(description = "The command to execute.")]
    pub command: String,
    #[serde(default = "default_timeout")]
    #[schemars(
        description = "The timeout in seconds for the command to execute. If the command takes longer than this, it will be killed.",
        range(min = 1, max = 86400),
        default = "default_timeout"
    )]
    pub timeout: i64,
    #[serde(default)]
    #[schemars(
        description = "Whether to run the command as a background task.",
        default
    )]
    pub run_in_background: bool,
    #[serde(default)]
    #[schemars(
        description = "A short description for the background task. Required when run_in_background=true."
    )]
    pub description: String,
}

fn default_timeout() -> i64 {
    DEFAULT_TIMEOUT
}

pub struct Shell {
    description: String,
    approval: std::sync::Arc<Approval>,
    shell_path: KaosPath,
    is_powershell: bool,
    background_tasks: BackgroundTaskManager,
    work_dir: KaosPath,
}

impl Shell {
    pub fn new(runtime: &Runtime) -> Self {
        let environment = runtime.environment.clone();
        let is_powershell = matches!(
            environment.shell_name.as_str(),
            "Windows PowerShell" | "PowerShell 7"
        );
        let shell_label = format!("{} (`{}`)", environment.shell_name, environment.shell_path);
        let template = if is_powershell {
            POWERSHELL_DESC
        } else {
            BASH_DESC
        };
        let desc = load_desc(template, &[("SHELL", shell_label)]);

        Self {
            description: desc,
            approval: runtime.approval.clone(),
            shell_path: environment.shell_path,
            is_powershell,
            background_tasks: runtime.background_tasks.clone(),
            work_dir: runtime.builtin_args.KIMI_WORK_DIR.clone(),
        }
    }

    async fn read_stream(
        &self,
        stream: &mut dyn AsyncReadable,
        builder: &mut ToolResultBuilder,
    ) -> anyhow::Result<()> {
        loop {
            let line = stream.readline().await?;
            if line.is_empty() {
                break;
            }
            let text = String::from_utf8_lossy(&line);
            builder.write(&text);
        }
        Ok(())
    }

    async fn read_streams(
        &self,
        mut stdout: Option<Box<dyn AsyncReadable>>,
        mut stderr: Option<Box<dyn AsyncReadable>>,
        builder: &mut ToolResultBuilder,
    ) -> anyhow::Result<()> {
        let mut stdout_done = stdout.is_none();
        let mut stderr_done = stderr.is_none();

        while !stdout_done || !stderr_done {
            tokio::select! {
                line = async {
                    match stdout.as_mut() {
                        Some(stream) => stream.readline().await,
                        None => Ok(Vec::new()),
                    }
                }, if !stdout_done => {
                    let line = line?;
                    if line.is_empty() {
                        stdout_done = true;
                    } else {
                        let text = String::from_utf8_lossy(&line);
                        builder.write(&text);
                    }
                }
                line = async {
                    match stderr.as_mut() {
                        Some(stream) => stream.readline().await,
                        None => Ok(Vec::new()),
                    }
                }, if !stderr_done => {
                    let line = line?;
                    if line.is_empty() {
                        stderr_done = true;
                    } else {
                        let text = String::from_utf8_lossy(&line);
                        builder.write(&text);
                    }
                }
            }
        }

        Ok(())
    }

    fn shell_args(&self, command: &str) -> Vec<String> {
        vec![
            self.shell_path.to_string_lossy(),
            "-c".to_string(),
            command.to_string(),
        ]
    }

    async fn run_foreground(&self, params: &ShellParams) -> ToolReturnValue {
        let mut builder = ToolResultBuilder::default();

        let approved = match self
            .approval
            .request(
                self.name(),
                "run command",
                &format!("Run command `{}`", params.command),
                Some(vec![DisplayBlock::Shell(ShellDisplayBlock::new(
                    if self.is_powershell {
                        "powershell"
                    } else {
                        "bash"
                    },
                    params.command.clone(),
                ))]),
            )
            .await
        {
            Ok(value) => value,
            Err(_) => false,
        };

        if !approved {
            return tool_rejected_error();
        }

        let args = self.shell_args(&params.command);
        let mut process = match kaos::exec(&args).await {
            Ok(process) => process,
            Err(err) => return tool_runtime_error(&err.to_string()),
        };

        let stdout = process.take_stdout();
        let stderr = process.take_stderr();

        let read_result = tokio::time::timeout(Duration::from_secs(params.timeout as u64), async {
            match (stdout, stderr) {
                (Some(stdout), Some(stderr)) => {
                    self.read_streams(Some(stdout), Some(stderr), &mut builder)
                        .await
                }
                (Some(stdout), None) => {
                    self.read_streams(Some(stdout), None, &mut builder).await?;
                    self.read_stream(process.stderr(), &mut builder).await
                }
                (None, Some(stderr)) => {
                    self.read_stream(process.stdout(), &mut builder).await?;
                    self.read_streams(None, Some(stderr), &mut builder).await
                }
                (None, None) => {
                    self.read_stream(process.stdout(), &mut builder).await?;
                    self.read_stream(process.stderr(), &mut builder).await
                }
            }
        })
        .await;

        match read_result {
            Ok(Ok(())) => {
                let exitcode = match process.wait().await {
                    Ok(code) => code,
                    Err(err) => return tool_runtime_error(&err.to_string()),
                };
                if exitcode == 0 {
                    builder.ok("Command executed successfully.", "")
                } else {
                    builder.error(
                        &format!("Command failed with exit code: {exitcode}."),
                        &format!("Failed with exit code: {exitcode}"),
                    )
                }
            }
            Ok(Err(err)) => tool_runtime_error(&err.to_string()),
            Err(_) => {
                if let Err(err) = process.kill().await {
                    return tool_runtime_error(&err.to_string());
                }
                builder.error(
                    &format!("Command killed by timeout ({}s)", params.timeout),
                    &format!("Killed by timeout ({}s)", params.timeout),
                )
            }
        }
    }

    async fn run_background(&self, params: &ShellParams) -> ToolReturnValue {
        let mut builder = ToolResultBuilder::default();

        if params.description.trim().is_empty() {
            return builder.error(
                "description is required when run_in_background is true",
                "Missing description",
            );
        }

        let approved = match self
            .approval
            .request(
                self.name(),
                "run background command",
                &format!("Run background command `{}`", params.command),
                Some(vec![DisplayBlock::Shell(ShellDisplayBlock::new(
                    if self.is_powershell {
                        "powershell"
                    } else {
                        "bash"
                    },
                    params.command.clone(),
                ))]),
            )
            .await
        {
            Ok(value) => value,
            Err(_) => false,
        };

        if !approved {
            return tool_rejected_error();
        }

        let task_id = self.background_tasks.generate_id();

        // Spawn the child process.
        let mut child = match tokio::process::Command::new(self.shell_path.to_string_lossy())
            .arg("-c")
            .arg(&params.command)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdin(std::process::Stdio::null())
            .current_dir(&self.work_dir.to_string_lossy())
            .spawn()
        {
            Ok(child) => child,
            Err(err) => {
                return builder.error(
                    &format!("Failed to spawn background command: {err}"),
                    "Spawn failed",
                );
            }
        };

        let stdout = child.stdout.take().expect("stdout configured but missing");
        let stderr = child.stderr.take().expect("stderr configured but missing");

        let spec = TaskSpec {
            id: task_id.clone(),
            description: params.description.trim().to_string(),
            command: params.command.clone(),
            shell_path: self.shell_path.to_string_lossy(),
            cwd: self.work_dir.to_string_lossy(),
            timeout_s: params.timeout,
            child_pid: child.id(),
        };

        // Kill channel: when kill() sends on this, the reader/spawned task stops.
        let (kill_tx, mut kill_rx) = tokio::sync::oneshot::channel::<()>();

        let bt = self.background_tasks.clone();
        let bt_spec = spec.clone();
        let (stdout_buf, stderr_buf) = bt.register(bt_spec, kill_tx);
        let output_log_path = self.background_tasks.output_log_path(&task_id);

        // Spawn a task that reads stdout/stderr and waits for completion.
        let background_tasks_clone = self.background_tasks.clone();
        let task_id_clone = task_id.clone();
        let timeout = Duration::from_secs(params.timeout as u64);

        // Capture the current wire so we can emit notifications from the background task.
        let current_wire = get_current_wire_or_none();

        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;

            // Open output.log for appending live output.
            let mut log_file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&output_log_path)
                .await
                .ok();

            // Read stdout and stderr concurrently.
            let mut stdout_reader = tokio::io::BufReader::new(stdout);
            let mut stderr_reader = tokio::io::BufReader::new(stderr);

            let read_stdout = async {
                let mut line = String::new();
                loop {
                    line.clear();
                    let n = stdout_reader.read_line(&mut line).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    let bytes = line.as_bytes();
                    if let Ok(mut buf) = stdout_buf.lock() {
                        buf.extend_from_slice(bytes);
                    }
                    if let Some(f) = log_file.as_mut() {
                        let _ = f.write_all(bytes).await;
                    }
                }
            };

            let read_stderr = async {
                let mut line = String::new();
                loop {
                    line.clear();
                    let n = stderr_reader.read_line(&mut line).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    if let Ok(mut buf) = stderr_buf.lock() {
                        buf.extend_from_slice(line.as_bytes());
                    }
                }
            };

            // Wait for: both streams to EOF, timeout, or kill signal.
            let deadline = tokio::time::sleep(timeout);
            tokio::pin!(deadline);
            tokio::pin!(read_stdout);
            tokio::pin!(read_stderr);

            loop {
                tokio::select! {
                    _ = &mut read_stdout => { break; }
                    _ = &mut read_stderr => {}
                    _ = &mut deadline => {
                        // Timeout reached
                        let _ = child.kill().await;
                        background_tasks_clone.complete(&task_id_clone, TaskStatus::TimedOut, None);
                        send_notification(&background_tasks_clone, &task_id_clone, &current_wire);
                        return;
                    }
                    _ = &mut kill_rx => {
                        // Kill signal received
                        let _ = child.kill().await;
                        background_tasks_clone.complete(&task_id_clone, TaskStatus::Killed, None);
                        send_notification(&background_tasks_clone, &task_id_clone, &current_wire);
                        return;
                    }
                }
            }

            // Streams reached EOF — wait for exit code.
            let exit_status = child.wait().await;
            let (status, exit_code) = match exit_status {
                Ok(status) => {
                    let code = status.code();
                    if code == Some(0) {
                        (TaskStatus::Completed, code)
                    } else {
                        (TaskStatus::Failed, code)
                    }
                }
                Err(_) => (TaskStatus::Failed, None),
            };
            background_tasks_clone.complete(&task_id_clone, status, exit_code);
            send_notification(&background_tasks_clone, &task_id_clone, &current_wire);
        });

        // Send notification
        fn send_notification(bt: &BackgroundTaskManager, id: &str, wire: &Option<std::sync::Arc<crate::wire::Wire>>) {
            if let Some(notification) = bt.build_notification(id) {
                if let Some(w) = wire {
                    w.soul_side().send(crate::wire::WireMessage::Notification(notification));
                }
            }
        }

        builder.write(&format!(
            "\
Task ID: {}
Description: {}
Command: `{}`
automatic_notification: true
next_step: You will be automatically notified when it completes.
next_step: Use TaskOutput with this task_id for a non-blocking status/output snapshot. Only set block=true when you intentionally want to wait.
next_step: Use TaskStop only if the task must be cancelled.
human_shell_hint: For users in the interactive shell, the only task-management slash command is /task. Do not suggest /task list, /task output, /task stop, or /tasks.
",
            task_id, params.description.trim(), params.command
        ));

        builder.ok("Background task started", &format!("Started {task_id}"))
    }
}

#[async_trait::async_trait]
impl CallableTool2 for Shell {
    type Params = ShellParams;

    fn name(&self) -> &str {
        "Shell"
    }

    fn description(&self) -> &str {
        &self.description
    }

    async fn call_typed(&self, params: Self::Params) -> ToolReturnValue {
        if params.command.is_empty() {
            return ToolResultBuilder::default().error("Command cannot be empty.", "Empty command");
        }

        if params.run_in_background {
            self.run_background(&params).await
        } else {
            self.run_foreground(&params).await
        }
    }
}
