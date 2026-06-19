use std::collections::HashMap;
use std::sync::Arc;

use kaos::{CachedKaos, KaosSnapshot};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::Mutex;

use kosong::tooling::{CallableTool2, ToolReturnValue, tool_error, tool_ok};

use crate::soul::agent::Runtime;

// ── SnapshotCreate ──────────────────────────────────────────────────────────

pub struct SnapshotCreate {
    cached_kaos: Arc<CachedKaos>,
    snapshots: Arc<Mutex<HashMap<String, KaosSnapshot>>>,
}

impl SnapshotCreate {
    pub fn new(runtime: &Runtime) -> Self {
        Self {
            cached_kaos: runtime.cached_kaos.clone(),
            snapshots: runtime.snapshots.clone(),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SnapshotCreateParams {
    #[serde(default)]
    #[schemars(description = "Optional human-readable label for this snapshot.")]
    pub label: Option<String>,
}

#[async_trait::async_trait]
impl CallableTool2 for SnapshotCreate {
    type Params = SnapshotCreateParams;

    fn name(&self) -> &str {
        "SnapshotCreate"
    }

    fn description(&self) -> &str {
        "Take a point-in-time snapshot of all tracked files in the working directory. \
         Returns a snapshot_id you can later pass to SnapshotRestore. \
         Coverage: all files respecting .gitignore and standard ignore rules. \
         Files over 10 MiB and ignored directories (target/, .git/, etc.) are excluded. \
         Shell-modified files within tracked paths ARE captured because the snapshot \
         reads from disk, not from an in-memory cache."
    }

    async fn call_typed(&self, params: Self::Params) -> ToolReturnValue {
        let id = uuid::Uuid::new_v4().to_string();
        let snapshot = match self.cached_kaos.take_snapshot(id.clone(), params.label.clone()).await {
            Ok(s) => s,
            Err(e) => {
                return tool_error("", format!("Failed to take snapshot: {e}"), "Snapshot failed")
            }
        };
        let file_count = snapshot.file_count();
        self.snapshots.lock().await.insert(id.clone(), snapshot);
        let label_str = params.label.as_deref().unwrap_or("(none)");
        tool_ok(
            format!("snapshot_id: {id}\nfiles_captured: {file_count}\nlabel: {label_str}"),
            &format!("Snapshot {id} created ({file_count} files)"),
            "",
        )
    }
}

// ── SnapshotList ────────────────────────────────────────────────────────────

pub struct SnapshotList {
    snapshots: Arc<Mutex<HashMap<String, KaosSnapshot>>>,
}

impl SnapshotList {
    pub fn new(runtime: &Runtime) -> Self {
        Self {
            snapshots: runtime.snapshots.clone(),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SnapshotListParams {}

#[async_trait::async_trait]
impl CallableTool2 for SnapshotList {
    type Params = SnapshotListParams;

    fn name(&self) -> &str {
        "SnapshotList"
    }

    fn description(&self) -> &str {
        "List all in-memory snapshots for this session."
    }

    async fn call_typed(&self, _params: Self::Params) -> ToolReturnValue {
        let guard = self.snapshots.lock().await;
        if guard.is_empty() {
            return tool_ok("No snapshots.", "No snapshots in this session.", "");
        }
        let mut lines: Vec<String> = guard
            .values()
            .map(|s| {
                let label = s.label.as_deref().unwrap_or("(none)");
                format!(
                    "id: {}\n  created_at: {}\n  label: {}\n  files: {}",
                    s.id,
                    s.created_at,
                    label,
                    s.file_count()
                )
            })
            .collect();
        lines.sort();
        tool_ok(lines.join("\n\n"), &format!("{} snapshot(s)", lines.len()), "")
    }
}

// ── SnapshotRestore ─────────────────────────────────────────────────────────

pub struct SnapshotRestore {
    cached_kaos: Arc<CachedKaos>,
    snapshots: Arc<Mutex<HashMap<String, KaosSnapshot>>>,
}

impl SnapshotRestore {
    pub fn new(runtime: &Runtime) -> Self {
        Self {
            cached_kaos: runtime.cached_kaos.clone(),
            snapshots: runtime.snapshots.clone(),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SnapshotRestoreParams {
    #[schemars(description = "Snapshot ID returned by SnapshotCreate.")]
    pub id: String,
}

#[async_trait::async_trait]
impl CallableTool2 for SnapshotRestore {
    type Params = SnapshotRestoreParams;

    fn name(&self) -> &str {
        "SnapshotRestore"
    }

    fn description(&self) -> &str {
        "Restore the working directory to a previously captured snapshot. \
         Files captured in the snapshot are written back to disk. \
         Files that exist now but were absent at snapshot time are deleted. \
         The snapshot is retained after restore so you can restore again if needed."
    }

    async fn call_typed(&self, params: Self::Params) -> ToolReturnValue {
        let guard = self.snapshots.lock().await;
        let snapshot = match guard.get(&params.id) {
            Some(s) => s,
            None => {
                return tool_error(
                    "",
                    format!("No snapshot with id: {}", params.id),
                    "Not found",
                )
            }
        };

        let report = match self.cached_kaos.restore_snapshot(snapshot).await {
            Ok(r) => r,
            Err(e) => {
                return tool_error("", format!("Restore failed: {e}"), "Restore error")
            }
        };
        drop(guard);

        let mut out = format!(
            "restored: {}\ndeleted: {}",
            report.restored, report.deleted
        );
        if !report.errors.is_empty() {
            out.push_str(&format!("\nerrors: {}", report.errors.len()));
            for e in &report.errors {
                out.push_str(&format!("\n  - {e}"));
            }
        }

        tool_ok(
            out,
            &format!(
                "Restored snapshot {} ({} written, {} deleted{})",
                params.id,
                report.restored,
                report.deleted,
                if report.errors.is_empty() {
                    String::new()
                } else {
                    format!(", {} errors", report.errors.len())
                }
            ),
            "",
        )
    }
}

// ── SnapshotDrop ────────────────────────────────────────────────────────────

pub struct SnapshotDrop {
    snapshots: Arc<Mutex<HashMap<String, KaosSnapshot>>>,
}

impl SnapshotDrop {
    pub fn new(runtime: &Runtime) -> Self {
        Self {
            snapshots: runtime.snapshots.clone(),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SnapshotDropParams {
    #[schemars(description = "Snapshot ID to remove from memory.")]
    pub id: String,
}

#[async_trait::async_trait]
impl CallableTool2 for SnapshotDrop {
    type Params = SnapshotDropParams;

    fn name(&self) -> &str {
        "SnapshotDrop"
    }

    fn description(&self) -> &str {
        "Free an in-memory snapshot. Use this after a successful restore or when \
         you no longer need the checkpoint to release memory."
    }

    async fn call_typed(&self, params: Self::Params) -> ToolReturnValue {
        let removed = self.snapshots.lock().await.remove(&params.id).is_some();
        if removed {
            tool_ok(
                format!("Dropped snapshot {}", params.id),
                &format!("Snapshot {} freed", params.id),
                "",
            )
        } else {
            tool_error(
                "",
                format!("No snapshot with id: {}", params.id),
                "Not found",
            )
        }
    }
}
