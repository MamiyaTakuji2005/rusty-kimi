use std::time::UNIX_EPOCH;

use serde::Serialize;
use tracing::error;

use crate::session::Session;
use crate::soul::agent::Runtime;
use crate::tools::todo::TodoItem;

#[derive(Serialize)]
struct ApprovalSnapshot {
    yolo: bool,
    afk: bool,
    auto_approve_actions: Vec<String>,
}

#[derive(Serialize)]
struct TodoSnapshot {
    title: String,
    status: String,
}

impl From<&TodoItem> for TodoSnapshot {
    fn from(item: &TodoItem) -> Self {
        Self {
            title: item.title.clone(),
            status: item.status.as_str().to_string(),
        }
    }
}

#[derive(Serialize)]
struct SessionState {
    version: u32,
    approval: ApprovalSnapshot,
    additional_dirs: Vec<String>,
    custom_title: String,
    title_generated: bool,
    title_generate_attempts: u32,
    plan_mode: bool,
    plan_session_id: Option<String>,
    plan_slug: Option<String>,
    wire_mtime: Option<f64>,
    archived: bool,
    archived_at: Option<f64>,
    auto_archive_exempt: bool,
    todos: Vec<TodoSnapshot>,
}

pub async fn write_state(session: &Session, runtime: &Runtime) {
    let state_path = match session.context_file.parent() {
        Some(dir) => dir.to_path_buf().join("state.json"),
        None => {
            error!("Cannot determine session dir for state.json");
            return;
        }
    };

    let wire_mtime = wire_mtime(session.wire_file.path()).await;

    let mut auto_approve_actions = runtime.approval.auto_approve_actions();
    auto_approve_actions.sort();

    let todos = runtime
        .todos
        .lock()
        .unwrap()
        .iter()
        .map(TodoSnapshot::from)
        .collect();

    let state = SessionState {
        version: 1,
        approval: ApprovalSnapshot {
            yolo: runtime.approval.is_yolo(),
            afk: false,
            auto_approve_actions,
        },
        additional_dirs: vec![],
        custom_title: session.title.clone(),
        title_generated: false,
        title_generate_attempts: 0,
        plan_mode: false,
        plan_session_id: None,
        plan_slug: None,
        wire_mtime,
        archived: false,
        archived_at: None,
        auto_archive_exempt: false,
        todos,
    };

    match serde_json::to_string_pretty(&state) {
        Ok(json) => {
            if let Err(err) = tokio::fs::write(&state_path, json).await {
                error!(error = ?err, "Failed to write state.json to {}", state_path.display());
            }
        }
        Err(err) => {
            error!(error = ?err, "Failed to serialize state.json");
        }
    }
}

async fn wire_mtime(path: &std::path::Path) -> Option<f64> {
    let meta = tokio::fs::metadata(path).await.ok()?;
    let mtime = meta.modified().ok()?;
    Some(
        mtime
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64(),
    )
}
