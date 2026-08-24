use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rand::Rng;
use serde::Serialize;
use tokio::sync::{Notify, oneshot};
use tracing::error;

use crate::wire::Notification;

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn rand_task_id() -> String {
    let val: u32 = rand::rng().random();
    format!("bash-{val:08x}")
}

fn rand_notification_id() -> String {
    let val: u32 = rand::rng().random();
    format!("n{:07x}", val & 0x0fffffff)
}

#[derive(Clone, Debug)]
pub struct TaskSpec {
    pub id: String,
    pub description: String,
    pub command: String,
    pub shell_path: String,
    pub cwd: String,
    pub timeout_s: i64,
    pub child_pid: Option<u32>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TaskStatus {
    Running,
    Completed,
    Failed,
    Killed,
    TimedOut,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::Running => "running",
            TaskStatus::Completed => "completed",
            TaskStatus::Failed => "failed",
            TaskStatus::Killed => "killed",
            TaskStatus::TimedOut => "timed_out",
        }
    }

    pub(crate) fn is_terminal(&self) -> bool {
        !matches!(self, TaskStatus::Running)
    }
}

#[derive(Clone, Debug)]
pub struct TaskView {
    pub spec: TaskSpec,
    pub status: TaskStatus,
    pub exit_code: Option<i32>,
    pub created_at: f64,
    pub completed_at: Option<f64>,
    pub stdout_len: usize,
    pub stderr_len: usize,
}

struct TaskEntry {
    spec: TaskSpec,
    status: TaskStatus,
    exit_code: Option<i32>,
    stdout: Arc<Mutex<Vec<u8>>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    created_at: f64,
    completed_at: Option<f64>,
    notify: Arc<Notify>,
    kill_sender: Option<oneshot::Sender<()>>,
}

struct TaskRegistry {
    tasks: HashMap<String, TaskEntry>,
}

#[derive(Serialize)]
struct TaskSpecFile {
    version: u32,
    id: String,
    kind: String,
    session_id: String,
    description: String,
    owner_role: &'static str,
    created_at: f64,
    command: String,
    shell_name: String,
    shell_path: String,
    cwd: String,
    timeout_s: i64,
    child_pid: Option<u32>,
    kind_payload: serde_json::Value,
}

#[derive(Serialize)]
struct TaskRuntimeFile {
    status: String,
    worker_pid: u32,
    child_pid: Option<u32>,
    exit_code: Option<i32>,
    started_at: f64,
    updated_at: f64,
    finished_at: Option<f64>,
    interrupted: bool,
    timed_out: bool,
    failure_reason: Option<String>,
}

/// Background task manager — registry of running/completed shell processes.
#[derive(Clone)]
pub struct BackgroundTaskManager {
    inner: Arc<Mutex<TaskRegistry>>,
    pub tasks_dir: PathBuf,
    session_id: String,
}

impl BackgroundTaskManager {
    pub fn new(tasks_dir: PathBuf, session_id: String) -> Self {
        Self {
            inner: Arc::new(Mutex::new(TaskRegistry {
                tasks: HashMap::new(),
            })),
            tasks_dir,
            session_id,
        }
    }

    /// Generate a unique task ID (format: bash-xxxxxxxx).
    pub fn generate_id(&self) -> String {
        rand_task_id()
    }

    pub fn task_dir(&self, id: &str) -> PathBuf {
        self.tasks_dir.join(id)
    }

    pub fn output_log_path(&self, id: &str) -> PathBuf {
        self.task_dir(id).join("output.log")
    }

    fn notifications_dir(&self) -> PathBuf {
        self.tasks_dir
            .parent()
            .map(|p| p.join("notifications"))
            .unwrap_or_else(|| self.tasks_dir.join("../notifications"))
    }

    fn write_spec_files(&self, spec: &TaskSpec, created_at: f64) {
        let dir = self.task_dir(&spec.id);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            error!("Failed to create task dir {}: {e}", dir.display());
            return;
        }
        let shell_name = std::path::Path::new(&spec.shell_path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(&spec.shell_path)
            .to_string();
        let spec_file = TaskSpecFile {
            version: 1,
            id: spec.id.clone(),
            kind: "bash".to_string(),
            session_id: self.session_id.clone(),
            description: spec.description.clone(),
            owner_role: "assistant",
            created_at,
            command: spec.command.clone(),
            shell_name,
            shell_path: spec.shell_path.clone(),
            cwd: spec.cwd.clone(),
            timeout_s: spec.timeout_s,
            child_pid: spec.child_pid,
            kind_payload: serde_json::json!({}),
        };
        if let Ok(json) = serde_json::to_string_pretty(&spec_file) {
            if let Err(e) = std::fs::write(dir.join("spec.json"), json) {
                error!("Failed to write spec.json for {}: {e}", spec.id);
            }
        }
        let consumer = serde_json::json!({
            "last_seen_output_size": 0,
            "last_viewed_at": null
        });
        if let Ok(json) = serde_json::to_string_pretty(&consumer) {
            let _ = std::fs::write(dir.join("consumer.json"), json);
        }
        let control = serde_json::json!({
            "kill_requested_at": null,
            "kill_reason": null,
            "force": false
        });
        if let Ok(json) = serde_json::to_string_pretty(&control) {
            let _ = std::fs::write(dir.join("control.json"), json);
        }
        self.write_runtime_file(
            &spec.id,
            "running",
            spec.child_pid,
            None,
            created_at,
            None,
            false,
            false,
        );
    }

    fn write_runtime_file(
        &self,
        id: &str,
        status: &str,
        child_pid: Option<u32>,
        exit_code: Option<i32>,
        started_at: f64,
        finished_at: Option<f64>,
        interrupted: bool,
        timed_out: bool,
    ) {
        let file = TaskRuntimeFile {
            status: status.to_string(),
            worker_pid: std::process::id(),
            child_pid,
            exit_code,
            started_at,
            updated_at: unix_now(),
            finished_at,
            interrupted,
            timed_out,
            failure_reason: None,
        };
        if let Ok(json) = serde_json::to_string_pretty(&file) {
            if let Err(e) = std::fs::write(self.task_dir(id).join("runtime.json"), json) {
                error!("Failed to write runtime.json for {id}: {e}");
            }
        }
    }

    fn write_notification_files(&self, view: &TaskView) {
        let notif_id = rand_notification_id();
        let notif_dir = self.notifications_dir().join(&notif_id);
        if let Err(e) = std::fs::create_dir_all(&notif_dir) {
            error!("Failed to create notification dir: {e}");
            return;
        }
        let type_name = match view.status {
            TaskStatus::Completed => "task.completed",
            TaskStatus::Failed => "task.failed",
            TaskStatus::Killed => "task.killed",
            TaskStatus::TimedOut => "task.timed_out",
            TaskStatus::Running => return,
        };
        let severity = match view.status {
            TaskStatus::Completed => "info",
            TaskStatus::Failed | TaskStatus::TimedOut => "error",
            TaskStatus::Killed => "warning",
            TaskStatus::Running => return,
        };
        let duration_s = view
            .completed_at
            .map(|c| c - view.created_at)
            .unwrap_or(0.0);
        let created_at = unix_now();

        let event = serde_json::json!({
            "version": 1,
            "id": notif_id,
            "category": "task",
            "type": type_name,
            "source_kind": "bash",
            "source_id": view.spec.id,
            "title": format!("Background task {}", view.status.as_str()),
            "body": format!("Task `{}`: {}", view.spec.description, view.status.as_str()),
            "severity": severity,
            "created_at": created_at,
            "payload": {
                "task_id": view.spec.id,
                "task_kind": "bash",
                "status": view.status.as_str(),
                "description": view.spec.description,
                "exit_code": view.exit_code,
                "interrupted": matches!(view.status, TaskStatus::Killed),
                "timed_out": matches!(view.status, TaskStatus::TimedOut),
                "terminal_reason": view.status.as_str(),
                "failure_reason": null,
                "finished_at": view.completed_at,
                "duration_s": duration_s,
            },
            "targets": ["llm", "wire", "shell"],
            "dedupe_key": format!("{}:{}", type_name, view.spec.id),
        });

        if let Ok(json) = serde_json::to_string_pretty(&event) {
            let _ = std::fs::write(notif_dir.join("event.json"), json);
        }
        let delivery = serde_json::json!({
            "sinks": {
                "llm": {"status": "pending", "claimed_at": null, "acked_at": null},
                "wire": {"status": "pending", "claimed_at": null, "acked_at": null},
                "shell": {"status": "pending", "claimed_at": null, "acked_at": null}
            }
        });
        if let Ok(json) = serde_json::to_string_pretty(&delivery) {
            let _ = std::fs::write(notif_dir.join("delivery.json"), json);
        }
    }

    /// Register a new background task. Returns the (stdout, stderr) output buffers.
    pub fn register(
        &self,
        spec: TaskSpec,
        kill_sender: oneshot::Sender<()>,
    ) -> (Arc<Mutex<Vec<u8>>>, Arc<Mutex<Vec<u8>>>) {
        let stdout = Arc::new(Mutex::new(Vec::new()));
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let created_at = unix_now();
        let task_id = spec.id.clone();

        {
            let mut registry = self.inner.lock().unwrap();
            registry.tasks.insert(
                task_id.clone(),
                TaskEntry {
                    spec: spec.clone(),
                    status: TaskStatus::Running,
                    exit_code: None,
                    stdout: stdout.clone(),
                    stderr: stderr.clone(),
                    created_at,
                    completed_at: None,
                    notify: Arc::new(Notify::new()),
                    kill_sender: Some(kill_sender),
                },
            );
        }

        self.write_spec_files(&spec, created_at);
        (stdout, stderr)
    }

    /// Mark a task as completed/failed and write filesystem state.
    pub fn complete(&self, id: &str, status: TaskStatus, exit_code: Option<i32>) {
        let result = {
            let mut registry = self.inner.lock().unwrap();
            if let Some(entry) = registry.tasks.get_mut(id) {
                let started_at = entry.created_at;
                let child_pid = entry.spec.child_pid;
                let timed_out = status == TaskStatus::TimedOut;
                let interrupted = status == TaskStatus::Killed;
                entry.status = status.clone();
                entry.exit_code = exit_code;
                let finished_at = unix_now();
                entry.completed_at = Some(finished_at);
                entry.notify.notify_waiters();
                let view = TaskView {
                    spec: entry.spec.clone(),
                    status: status.clone(),
                    exit_code,
                    created_at: entry.created_at,
                    completed_at: Some(finished_at),
                    stdout_len: entry.stdout.lock().unwrap().len(),
                    stderr_len: entry.stderr.lock().unwrap().len(),
                };
                Some((
                    view,
                    started_at,
                    child_pid,
                    finished_at,
                    timed_out,
                    interrupted,
                ))
            } else {
                None
            }
        };

        if let Some((view, started_at, child_pid, finished_at, timed_out, interrupted)) = result {
            self.write_runtime_file(
                id,
                view.status.as_str(),
                child_pid,
                exit_code,
                started_at,
                Some(finished_at),
                interrupted,
                timed_out,
            );
            self.write_notification_files(&view);
        }
    }

    /// Get stdout buffer for a task.
    pub fn stdout(&self, id: &str) -> Option<Arc<Mutex<Vec<u8>>>> {
        let registry = self.inner.lock().unwrap();
        registry.tasks.get(id).map(|e| e.stdout.clone())
    }

    /// Get stderr buffer for a task.
    pub fn stderr(&self, id: &str) -> Option<Arc<Mutex<Vec<u8>>>> {
        let registry = self.inner.lock().unwrap();
        registry.tasks.get(id).map(|e| e.stderr.clone())
    }

    /// Get the `Notify` for waiting on a task.
    pub fn notify(&self, id: &str) -> Option<Arc<Notify>> {
        let registry = self.inner.lock().unwrap();
        registry.tasks.get(id).map(|e| e.notify.clone())
    }

    /// Get a snapshot view of a task.
    pub fn get(&self, id: &str) -> Option<TaskView> {
        let registry = self.inner.lock().unwrap();
        registry.tasks.get(id).map(|e| {
            let stdout_len = e.stdout.lock().unwrap().len();
            let stderr_len = e.stderr.lock().unwrap().len();
            TaskView {
                spec: e.spec.clone(),
                status: e.status.clone(),
                exit_code: e.exit_code,
                created_at: e.created_at,
                completed_at: e.completed_at,
                stdout_len,
                stderr_len,
            }
        })
    }

    /// List all tasks matching an optional filter.
    pub fn list(&self, active_only: bool) -> Vec<TaskView> {
        let registry = self.inner.lock().unwrap();
        registry
            .tasks
            .values()
            .filter(|e| !active_only || !e.status.is_terminal())
            .map(|e| {
                let stdout_len = e.stdout.lock().unwrap().len();
                let stderr_len = e.stderr.lock().unwrap().len();
                TaskView {
                    spec: e.spec.clone(),
                    status: e.status.clone(),
                    exit_code: e.exit_code,
                    created_at: e.created_at,
                    completed_at: e.completed_at,
                    stdout_len,
                    stderr_len,
                }
            })
            .collect()
    }

    /// Send kill signal to a running background task.
    pub fn kill(&self, id: &str) -> Result<(), String> {
        let (started_at, child_pid) = {
            let mut registry = self.inner.lock().unwrap();
            let entry = registry
                .tasks
                .get_mut(id)
                .ok_or_else(|| format!("Task {id} not found"))?;
            if entry.status.is_terminal() {
                return Err(format!("Task {id} is already in terminal state"));
            }
            let started_at = entry.created_at;
            let child_pid = entry.spec.child_pid;
            if let Some(sender) = entry.kill_sender.take() {
                let _ = sender.send(());
            }
            entry.status = TaskStatus::Killed;
            entry.completed_at = Some(unix_now());
            entry.notify.notify_waiters();
            (started_at, child_pid)
        };

        let control = serde_json::json!({
            "kill_requested_at": unix_now(),
            "kill_reason": "Stopped by TaskStop",
            "force": false
        });
        if let Ok(json) = serde_json::to_string_pretty(&control) {
            let _ = std::fs::write(self.task_dir(id).join("control.json"), json);
        }
        self.write_runtime_file(
            id,
            "killed",
            child_pid,
            None,
            started_at,
            Some(unix_now()),
            true,
            false,
        );

        Ok(())
    }

    /// Build a Notification wire event for a terminal task.
    pub fn build_notification(&self, id: &str) -> Option<Notification> {
        let view = self.get(id)?;
        if !view.status.is_terminal() {
            return None;
        }
        let (title, severity) = match view.status {
            TaskStatus::Completed => ("Background task completed".into(), "info".into()),
            TaskStatus::Failed => ("Background task failed".into(), "error".into()),
            TaskStatus::Killed => ("Background task killed".into(), "warning".into()),
            TaskStatus::TimedOut => ("Background task timed out".into(), "error".into()),
            TaskStatus::Running => return None,
        };
        Some(Notification {
            id: id.to_string(),
            category: "task".to_string(),
            type_name: "background_task_completed".to_string(),
            source_kind: "background_task".to_string(),
            source_id: id.to_string(),
            title,
            body: format!("Task `{}`: {}", view.spec.description, view.status.as_str()),
            severity,
            created_at: unix_now(),
            payload: serde_json::Map::new(),
        })
    }
}
