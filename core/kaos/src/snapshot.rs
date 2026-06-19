use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// A point-in-time snapshot of all non-ignored files in the working directory.
///
/// Coverage: files readable by `scan_workdir` (respects .gitignore, prunes
/// `target/` etc.).  Files modified by Shell commands in ignored paths are
/// outside the snapshot boundary.  Files larger than the per-file size cap
/// (10 MiB) are skipped.
#[derive(Debug, Clone)]
pub struct KaosSnapshot {
    pub id: String,
    pub created_at: String,
    pub label: Option<String>,
    /// Absolute path -> file bytes for every captured file.
    pub(crate) files: HashMap<PathBuf, Arc<[u8]>>,
}

impl KaosSnapshot {
    pub fn file_count(&self) -> usize {
        self.files.len()
    }
}

#[derive(Debug, Default)]
pub struct RestoreReport {
    pub restored: usize,
    pub deleted: usize,
    pub errors: Vec<String>,
}

/// One entry in the sequential write-undo history.
///
/// Captured automatically before every `write_bytes`/`write_text` call so
/// the agent can undo writes in reverse order without managing checkpoints.
#[derive(Debug, Clone)]
pub struct WriteEntry {
    pub path: PathBuf,
    /// Content before the write.  `None` means the file did not exist
    /// (the write created it), so undo should delete it.
    pub original: Option<Arc<[u8]>>,
}

/// Result of an `undo(steps)` call.
#[derive(Debug, Default)]
pub struct UndoReport {
    /// How many writes were in the history before this call.
    pub steps_available: usize,
    /// How many writes were actually undone (steps requested capped at steps_available).
    pub steps_applied: usize,
    /// Files restored to their pre-write content.
    pub restored: usize,
    /// New files deleted (they did not exist before the write).
    pub deleted: usize,
    pub errors: Vec<String>,
}
