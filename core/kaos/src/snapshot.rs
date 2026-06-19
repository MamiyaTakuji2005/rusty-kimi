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
    /// Absolute path → file bytes for every captured file.
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
