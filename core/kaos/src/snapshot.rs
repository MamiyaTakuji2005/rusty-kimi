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

/// What a file looked like immediately before a write, as far as undo is
/// concerned.
#[derive(Debug, Clone)]
pub enum PreWrite {
    /// The file did not exist — the write created it, so undo deletes it.
    Absent,
    /// Content before the write, held in memory until undone or evicted.
    Content(Arc<[u8]>),
    /// The file existed but its content was deliberately not retained: too
    /// large for the per-file cap, or unreadable at the moment of the write.
    /// Undo can neither restore nor delete it, so it reports it as skipped.
    Unrecorded,
}

/// One entry in the sequential write-undo history.
///
/// Captured automatically before every `write_bytes`/`write_text` call so
/// the agent can undo writes in reverse order without managing checkpoints.
#[derive(Debug, Clone)]
pub struct WriteEntry {
    pub path: PathBuf,
    pub original: PreWrite,
}

impl WriteEntry {
    /// Bytes this entry is holding in memory. The history is bounded by the
    /// sum of these, so anything that retains nothing is free to keep.
    pub fn retained_bytes(&self) -> usize {
        match &self.original {
            PreWrite::Content(bytes) => bytes.len(),
            PreWrite::Absent | PreWrite::Unrecorded => 0,
        }
    }
}

/// Result of an `undo(steps)` call.
#[derive(Debug, Default)]
pub struct UndoReport {
    /// How many writes were in the history before this call.
    pub steps_available: usize,
    /// How many history entries this call consumed.
    pub steps_applied: usize,
    /// Files restored to their pre-write content.
    pub restored: usize,
    /// New files deleted (they did not exist before the write).
    pub deleted: usize,
    /// Writes whose pre-write content was never retained, so the file was
    /// left exactly as it is.
    pub skipped: usize,
    pub errors: Vec<String>,
}
