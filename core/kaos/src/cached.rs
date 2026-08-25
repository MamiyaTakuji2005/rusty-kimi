use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, anyhow};
use tokio::sync::RwLock;

use crate::local::{ALWAYS_PRUNE_DIRS, LocalKaos};
use crate::snapshot::{KaosSnapshot, PreWrite, RestoreReport, UndoReport, WriteEntry};
use crate::{
    AsyncReadable, AsyncWritable, Kaos, KaosPath, KaosProcess, LineStream, StatResult,
    StrOrKaosPath,
};

/// Files larger than this are excluded from snapshots.
const MAX_SNAPSHOT_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// A file larger than this is not copied into the undo history. Restoring one
/// write is not worth holding a file of that size in memory for the rest of
/// the session; the write is still recorded, so undo can say it skipped it.
const MAX_UNDO_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// Caps on the undo history as a whole. Undo runs newest-first, so when the
/// history outgrows these the *oldest* entries are dropped — they are the ones
/// least likely to be reached, and without a bound the history grows with
/// every edit for the life of the session.
const MAX_UNDO_ENTRIES: usize = 64;
const MAX_UNDO_BYTES: usize = 32 * 1024 * 1024;

/// A completed scan of `work_dir`, together with the mutation count it is
/// valid as of.
struct Index {
    /// Full sorted list of paths relative to `work_dir`.
    paths: Arc<Vec<PathBuf>>,
    /// Value of [`CachedKaos::mutations`] when this scan *started*. The index
    /// is stale, and a rescan is due, as soon as the counter moves past it.
    generation: u64,
}

/// A `Kaos` decorator that caches an ignore-aware directory listing of
/// `work_dir` so `glob()` queries hit memory instead of walking the disk.
///
/// Staleness is a generation comparison rather than a flag. Every call that
/// mutates the filesystem (`write_bytes`, `write_text`, `mkdir`, `exec`) bumps
/// a counter on both sides of the change, and a scan may only be published as
/// current if the counter did not move while it ran. A flag cannot express
/// this: a walk that starts before a write and finishes after it observes a
/// clean flag at both ends and would install a listing that is missing the
/// file — permanently, until some unrelated mutation happens to mark it again.
///
/// All operations other than `glob` are delegated to the inner backend.
pub struct CachedKaos {
    inner: Arc<dyn Kaos>,
    work_dir: PathBuf,
    /// The most recent completed scan, or `None` when nothing has been scanned
    /// yet. Never holds an empty listing to mean "not scanned" — that is
    /// indistinguishable from an empty repo and silently breaks every glob.
    index: Arc<RwLock<Option<Index>>>,
    /// Bumped on both sides of every mutation. Being an atomic rather than a
    /// lock is what lets a process handle invalidate the index from `Drop`.
    mutations: Arc<AtomicU64>,
    /// Sequential log of pre-write content for every write_bytes/write_text call.
    /// Supports undo in reverse order, bounded by the `MAX_UNDO_*` caps.
    write_history: Arc<tokio::sync::Mutex<Vec<WriteEntry>>>,
}

fn scan_workdir(root: &Path) -> Result<Vec<PathBuf>> {
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .ignore(true)
        .parents(true)
        .require_git(false);
    builder.filter_entry(|entry| {
        if entry.file_type().is_some_and(|ft| ft.is_dir()) {
            if let Some(name) = entry.file_name().to_str() {
                return !ALWAYS_PRUNE_DIRS.contains(&name);
            }
        }
        true
    });

    let mut paths = Vec::new();
    for entry in builder.build() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if path == root {
            continue;
        }
        if let Ok(rel) = path.strip_prefix(root) {
            paths.push(rel.to_path_buf());
        }
    }
    paths.sort();
    Ok(paths)
}

impl CachedKaos {
    /// Build a `CachedKaos` without the initial scan. Synchronous, for
    /// tests and fixtures that need a value up front; the first `glob()`
    /// builds the index lazily, so globs still return real results.
    pub fn empty(work_dir: PathBuf) -> Self {
        Self::with_index(work_dir, None)
    }

    /// Build a `CachedKaos` over `work_dir`, performing the initial blocking
    /// scan immediately so the first `glob()` call hits the index.
    pub async fn new(work_dir: PathBuf) -> Self {
        let root = work_dir.clone();
        // A scan that fails leaves the index unbuilt rather than empty, so the
        // first glob retries it. Storing the empty result would make an
        // unreadable working directory look like one containing no files, and
        // every glob for the rest of the session would agree.
        let initial = tokio::task::spawn_blocking(move || scan_workdir(&root))
            .await
            .ok()
            .and_then(|result| result.ok())
            .map(|paths| Index {
                paths: Arc::new(paths),
                generation: 0,
            });

        Self::with_index(work_dir, initial)
    }

    fn with_index(work_dir: PathBuf, index: Option<Index>) -> Self {
        Self {
            inner: Arc::new(LocalKaos::new()),
            work_dir,
            index: Arc::new(RwLock::new(index)),
            mutations: Arc::new(AtomicU64::new(0)),
            write_history: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        }
    }

    /// Return the current listing, rescanning when the index is missing or has
    /// been outrun by a mutation.
    async fn get_paths(&self) -> Result<Arc<Vec<PathBuf>>> {
        // Fast path: a scan that nothing has invalidated since it started.
        {
            let guard = self.index.read().await;
            if let Some(index) = guard.as_ref()
                && index.generation == self.mutations.load(Ordering::SeqCst)
            {
                return Ok(Arc::clone(&index.paths));
            }
        }

        // Slow path. Read the counter *before* walking: anything that lands
        // while we walk may or may not be in the result, so the result must
        // not then be published as current.
        let generation = self.mutations.load(Ordering::SeqCst);
        let root = self.work_dir.clone();
        let paths = tokio::task::spawn_blocking(move || scan_workdir(&root))
            .await
            .map_err(|err| anyhow!(err))??;
        let paths = Arc::new(paths);

        if self.mutations.load(Ordering::SeqCst) == generation {
            let mut guard = self.index.write().await;
            // A concurrent rescan may have published a newer scan already;
            // that one saw at least as much as this one, so leave it alone.
            let newer = guard
                .as_ref()
                .is_some_and(|index| index.generation > generation);
            if !newer {
                *guard = Some(Index {
                    paths: Arc::clone(&paths),
                    generation,
                });
            }
        }
        // Otherwise this walk raced a mutation. The caller still gets the
        // best-effort listing, but nothing is published, so the next call
        // rescans instead of trusting a result that may be missing the change.

        Ok(paths)
    }

    /// Note that the filesystem is changing.
    ///
    /// Called on *both* sides of every mutation: the leading call stops a
    /// reader from publishing a scan taken while the write is in flight, and
    /// the trailing one invalidates a reader that walked past the path before
    /// it landed.
    fn mark_dirty(&self) {
        self.mutations.fetch_add(1, Ordering::SeqCst);
    }

    /// Record what `path` held before it is overwritten, so `undo` can put it
    /// back, and trim the history down to its caps.
    async fn record_write(&self, path: &Path) {
        let original = match tokio::fs::metadata(path).await {
            // Nothing there: the write creates the file, and undo deletes it.
            Err(_) => PreWrite::Absent,
            Ok(meta) if meta.len() > MAX_UNDO_FILE_BYTES => PreWrite::Unrecorded,
            Ok(_) => match tokio::fs::read(path).await {
                Ok(bytes) => PreWrite::Content(Arc::from(bytes.as_slice())),
                // It exists but cannot be read. Recording `Absent` here would
                // make undo *delete* a file it never captured.
                Err(_) => PreWrite::Unrecorded,
            },
        };

        let mut history = self.write_history.lock().await;
        history.push(WriteEntry {
            path: path.to_path_buf(),
            original,
        });
        trim_history(&mut history);
    }

    /// Undo the last `steps` writes made through this `CachedKaos` instance.
    ///
    /// Each `write_bytes`/`write_text` call pushed the pre-write content to the
    /// history.  This method pops up to `steps` entries (or all available) and
    /// restores them in reverse order.  Shell-written files are NOT tracked,
    /// and the history lives only as long as the process.
    pub async fn undo(&self, steps: usize) -> Result<UndoReport> {
        let mut history = self.write_history.lock().await;
        let steps_available = history.len();
        let mut report = UndoReport {
            steps_available,
            ..Default::default()
        };

        for _ in 0..steps.min(steps_available) {
            let Some(entry) = history.pop() else { break };
            // Counted per entry actually consumed: reporting the whole batch
            // up front claims work that a failure below never did.
            report.steps_applied += 1;
            match entry.original {
                PreWrite::Content(bytes) => {
                    if let Some(parent) = entry.path.parent() {
                        let _ = tokio::fs::create_dir_all(parent).await;
                    }
                    match tokio::fs::write(&entry.path, bytes.as_ref()).await {
                        Ok(()) => report.restored += 1,
                        Err(err) => report
                            .errors
                            .push(format!("{}: {err}", entry.path.display())),
                    }
                }
                // The write created the file, so undoing it removes the file.
                PreWrite::Absent => match tokio::fs::remove_file(&entry.path).await {
                    Ok(()) => report.deleted += 1,
                    // Already gone is the state undo was aiming for.
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        report.deleted += 1;
                    }
                    Err(err) => report
                        .errors
                        .push(format!("delete {}: {err}", entry.path.display())),
                },
                PreWrite::Unrecorded => {
                    report.skipped += 1;
                    report.errors.push(format!(
                        "{}: pre-write content was not retained (too large or unreadable); left as is",
                        entry.path.display()
                    ));
                }
            }
        }

        if report.steps_applied > 0 {
            self.mark_dirty();
        }
        Ok(report)
    }

    /// Capture a point-in-time snapshot of all non-ignored files in `work_dir`.
    ///
    /// Reads every eligible file from disk, so the snapshot is accurate
    /// regardless of Shell commands or subagent writes that the cache never saw.
    pub async fn take_snapshot(&self, id: String, label: Option<String>) -> Result<KaosSnapshot> {
        let root = self.work_dir.clone();
        let rel_paths = tokio::task::spawn_blocking(move || scan_workdir(&root))
            .await
            .map_err(|e| anyhow!(e))??;

        let mut files = HashMap::new();

        for rel in &rel_paths {
            let abs = self.work_dir.join(rel);
            let meta = match tokio::fs::metadata(&abs).await {
                Ok(m) => m,
                Err(_) => continue,
            };
            if !meta.is_file() || meta.len() > MAX_SNAPSHOT_FILE_BYTES {
                continue;
            }
            match tokio::fs::read(&abs).await {
                Ok(bytes) => {
                    files.insert(abs, Arc::from(bytes.as_slice()));
                }
                Err(_) => {}
            }
        }

        Ok(KaosSnapshot {
            id,
            created_at: chrono::Local::now().to_rfc3339(),
            label,
            files,
        })
    }

    /// Restore `work_dir` to the state captured in `snapshot`.
    ///
    /// Files in the snapshot are written back to disk.  Files that exist now
    /// but were not in the snapshot are deleted.  The path index is marked
    /// dirty so the next `glob()` call rescans.
    pub async fn restore_snapshot(&self, snapshot: &KaosSnapshot) -> Result<RestoreReport> {
        let mut report = RestoreReport::default();

        // Write back snapshot files.
        for (abs_path, bytes) in &snapshot.files {
            if let Some(parent) = abs_path.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            match tokio::fs::write(abs_path, bytes.as_ref()).await {
                Ok(_) => report.restored += 1,
                Err(e) => report.errors.push(format!("{}: {e}", abs_path.display())),
            }
        }

        // Delete files that exist now but were absent from the snapshot.
        let root = self.work_dir.clone();
        let current_rels = tokio::task::spawn_blocking(move || scan_workdir(&root))
            .await
            .map_err(|e| anyhow!(e))??;

        for rel in current_rels {
            let abs = self.work_dir.join(&rel);
            match tokio::fs::metadata(&abs).await {
                Ok(m) if m.is_file() => {
                    if !snapshot.files.contains_key(&abs) {
                        match tokio::fs::remove_file(&abs).await {
                            Ok(_) => report.deleted += 1,
                            Err(e) => report.errors.push(format!("delete {}: {e}", abs.display())),
                        }
                    }
                }
                _ => {}
            }
        }

        self.mark_dirty();
        Ok(report)
    }
}

#[async_trait::async_trait]
impl Kaos for CachedKaos {
    fn name(&self) -> &str {
        "local/cached"
    }

    fn normpath(&self, path: &StrOrKaosPath<'_>) -> KaosPath {
        self.inner.normpath(path)
    }

    fn home(&self) -> KaosPath {
        self.inner.home()
    }

    fn cwd(&self) -> KaosPath {
        self.inner.cwd()
    }

    async fn chdir(&self, path: &KaosPath) -> Result<()> {
        self.inner.chdir(path).await
    }

    async fn stat(&self, path: &KaosPath, follow_symlinks: bool) -> Result<StatResult> {
        self.inner.stat(path, follow_symlinks).await
    }

    async fn iterdir(&self, path: &KaosPath) -> Result<Vec<KaosPath>> {
        self.inner.iterdir(path).await
    }

    /// Glob against the in-memory index when `path` is within `work_dir`;
    /// fall back to the inner backend for any path outside `work_dir`.
    async fn glob(
        &self,
        path: &KaosPath,
        pattern: &str,
        case_sensitive: bool,
    ) -> Result<Vec<KaosPath>> {
        let root = path.as_path();

        // Compute the path relative to work_dir. Fails if path is outside —
        // fall through to a live walk in that case.
        let rel_prefix = match root.strip_prefix(&self.work_dir) {
            Ok(rel) => rel.to_path_buf(),
            Err(_) => return self.inner.glob(path, pattern, case_sensitive).await,
        };

        let paths = self.get_paths().await?;

        let matcher = globset::GlobBuilder::new(pattern)
            .literal_separator(true)
            .case_insensitive(!case_sensitive)
            .build()
            .map_err(|e| anyhow!(e))?
            .compile_matcher();

        let empty = Path::new("");
        let mut out = Vec::new();
        for entry_rel in paths.iter() {
            // Strip the subdirectory prefix so the pattern is matched relative
            // to `path`, not to `work_dir`.
            let within_root = if rel_prefix == empty {
                Some(entry_rel.as_path())
            } else {
                entry_rel.strip_prefix(&rel_prefix).ok()
            };

            if let Some(within) = within_root {
                if matcher.is_match(within) {
                    out.push(KaosPath::from(self.work_dir.join(entry_rel)));
                }
            }
        }

        Ok(out)
    }

    async fn read_bytes(&self, path: &KaosPath, limit: Option<usize>) -> Result<Vec<u8>> {
        self.inner.read_bytes(path, limit).await
    }

    async fn read_text(&self, path: &KaosPath) -> Result<String> {
        self.inner.read_text(path).await
    }

    async fn read_lines(&self, path: &KaosPath) -> Result<Vec<String>> {
        self.inner.read_lines(path).await
    }

    async fn read_lines_stream(&self, path: &KaosPath) -> Result<LineStream> {
        self.inner.read_lines_stream(path).await
    }

    async fn write_bytes(&self, path: &KaosPath, data: &[u8]) -> Result<usize> {
        self.record_write(path.as_path()).await;
        self.mark_dirty();
        let written = self.inner.write_bytes(path, data).await;
        self.mark_dirty();
        written
    }

    async fn write_text(&self, path: &KaosPath, data: &str, append: bool) -> Result<usize> {
        self.record_write(path.as_path()).await;
        self.mark_dirty();
        let written = self.inner.write_text(path, data, append).await;
        self.mark_dirty();
        written
    }

    async fn mkdir(&self, path: &KaosPath, parents: bool, exist_ok: bool) -> Result<()> {
        self.mark_dirty();
        let made = self.inner.mkdir(path, parents, exist_ok).await;
        self.mark_dirty();
        made
    }

    /// Spawning only marks the *start* of the change: the command goes on
    /// writing for its whole lifetime, so the handle carries the invalidation
    /// forward and marks again when the command is done.
    async fn exec(&self, args: &[String]) -> Result<Box<dyn KaosProcess>> {
        self.mark_dirty();
        let process = self.inner.exec(args).await?;
        Ok(Box::new(DirtyOnExit {
            inner: process,
            mutations: Arc::clone(&self.mutations),
        }))
    }
}

/// Drop the oldest entries until the history fits both caps.
///
/// Undo runs newest-first, so the oldest entries are the ones least likely to
/// be reached. Dropping them outright is the honest bound — keeping an entry
/// whose content has been evicted only defers the failure to undo time.
fn trim_history(history: &mut Vec<WriteEntry>) {
    let mut drop_count = history.len().saturating_sub(MAX_UNDO_ENTRIES);
    let mut retained: usize = history[drop_count..]
        .iter()
        .map(WriteEntry::retained_bytes)
        .sum();
    while retained > MAX_UNDO_BYTES && drop_count < history.len() {
        retained -= history[drop_count].retained_bytes();
        drop_count += 1;
    }
    history.drain(..drop_count);
}

/// A process handle that invalidates the path index when the command ends.
///
/// Both hooks matter: `wait` covers the callers that observe the exit, and
/// `Drop` covers the ones that abandon the handle. Marking twice is harmless —
/// the counter only ever has to move.
struct DirtyOnExit {
    inner: Box<dyn KaosProcess>,
    mutations: Arc<AtomicU64>,
}

impl Drop for DirtyOnExit {
    fn drop(&mut self) {
        self.mutations.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl KaosProcess for DirtyOnExit {
    fn pid(&self) -> u32 {
        self.inner.pid()
    }

    fn returncode(&mut self) -> Option<i32> {
        self.inner.returncode()
    }

    async fn wait(&mut self) -> Result<i32> {
        let code = self.inner.wait().await;
        self.mutations.fetch_add(1, Ordering::SeqCst);
        code
    }

    async fn kill(&mut self) -> Result<()> {
        self.inner.kill().await
    }

    fn stdin(&mut self) -> &mut dyn AsyncWritable {
        self.inner.stdin()
    }

    fn stdout(&mut self) -> &mut dyn AsyncReadable {
        self.inner.stdout()
    }

    fn stderr(&mut self) -> &mut dyn AsyncReadable {
        self.inner.stderr()
    }

    fn take_stdout(&mut self) -> Option<Box<dyn AsyncReadable>> {
        self.inner.take_stdout()
    }

    fn take_stderr(&mut self) -> Option<Box<dyn AsyncReadable>> {
        self.inner.take_stderr()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local::LocalKaos;

    async fn make_tree(root: &Path) {
        tokio::fs::create_dir_all(root.join("src")).await.unwrap();
        tokio::fs::write(root.join("src/main.rs"), b"fn main() {}")
            .await
            .unwrap();
        tokio::fs::write(root.join("src/lib.rs"), b"")
            .await
            .unwrap();
        tokio::fs::write(root.join("README.md"), b"# hello")
            .await
            .unwrap();
        tokio::fs::create_dir_all(root.join("target/debug"))
            .await
            .unwrap();
        tokio::fs::write(root.join("target/debug/out"), b"binary")
            .await
            .unwrap();
    }

    fn kp(root: &Path, rel: &str) -> KaosPath {
        KaosPath::from(root.join(rel))
    }

    /// A command that does nothing and exits cleanly, so a test can hold a
    /// live process handle without caring what it is.
    fn noop_command() -> Vec<String> {
        if cfg!(windows) {
            ["cmd", "/c", "exit", "0"]
        } else {
            ["sh", "-c", "exit 0", ""]
        }
        .iter()
        .map(|arg| (*arg).to_string())
        .filter(|arg| !arg.is_empty())
        .collect()
    }

    #[tokio::test]
    async fn test_parity_with_local_glob() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        make_tree(root).await;

        let local = LocalKaos::new();
        let cached = CachedKaos::new(root.to_path_buf()).await;
        let root_kp = KaosPath::from(root.to_path_buf());

        for pattern in ["**/*.rs", "*.md", "src/*.rs", "**/*"] {
            let mut local_out = local.glob(&root_kp, pattern, true).await.unwrap();
            let mut cached_out = cached.glob(&root_kp, pattern, true).await.unwrap();

            // Both return absolute paths — sort for deterministic comparison.
            local_out.sort_by(|a, b| a.to_string_lossy().cmp(&b.to_string_lossy()));
            cached_out.sort_by(|a, b| a.to_string_lossy().cmp(&b.to_string_lossy()));

            assert_eq!(
                local_out
                    .iter()
                    .map(|p| p.to_string_lossy())
                    .collect::<Vec<_>>(),
                cached_out
                    .iter()
                    .map(|p| p.to_string_lossy())
                    .collect::<Vec<_>>(),
                "pattern `{pattern}` diverged"
            );
        }
    }

    #[tokio::test]
    async fn test_subdirectory_glob() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        make_tree(root).await;

        let local = LocalKaos::new();
        let cached = CachedKaos::new(root.to_path_buf()).await;
        let src_kp = kp(root, "src");

        let mut local_out = local.glob(&src_kp, "*.rs", true).await.unwrap();
        let mut cached_out = cached.glob(&src_kp, "*.rs", true).await.unwrap();

        local_out.sort_by(|a, b| a.to_string_lossy().cmp(&b.to_string_lossy()));
        cached_out.sort_by(|a, b| a.to_string_lossy().cmp(&b.to_string_lossy()));

        assert_eq!(
            local_out
                .iter()
                .map(|p| p.to_string_lossy())
                .collect::<Vec<_>>(),
            cached_out
                .iter()
                .map(|p| p.to_string_lossy())
                .collect::<Vec<_>>(),
        );
        assert_eq!(cached_out.len(), 2);
    }

    #[tokio::test]
    async fn test_dirty_on_write_picks_up_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let root_kp = KaosPath::from(root.to_path_buf());
        let cached = CachedKaos::new(root.to_path_buf()).await;

        // Nothing yet.
        let before = cached.glob(&root_kp, "*.txt", true).await.unwrap();
        assert!(before.is_empty());

        // Write a file — marks dirty.
        let file_kp = kp(root, "note.txt");
        cached.write_text(&file_kp, "hello", false).await.unwrap();

        // Glob triggers a rescan and should find the new file.
        let after = cached.glob(&root_kp, "*.txt", true).await.unwrap();
        assert_eq!(after.len(), 1);
        assert!(after[0].to_string_lossy().ends_with("note.txt"));
    }

    #[tokio::test]
    async fn test_target_dir_skipped_by_both() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        make_tree(root).await;

        let cached = CachedKaos::new(root.to_path_buf()).await;
        let root_kp = KaosPath::from(root.to_path_buf());

        // `target/` is pruned — neither `out` nor any target file should appear.
        let results = cached.glob(&root_kp, "**/out", true).await.unwrap();
        assert!(
            results.is_empty(),
            "target/debug/out should be pruned: {results:?}"
        );
    }

    #[tokio::test]
    async fn test_unscanned_index_is_built_on_first_glob() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        make_tree(root).await;

        // No initial scan, and no mutation to trigger one — the first glob has
        // to build the index itself rather than report an empty tree.
        let cached = CachedKaos::empty(root.to_path_buf());
        let found = cached
            .glob(&KaosPath::from(root.to_path_buf()), "**/*.rs", true)
            .await
            .unwrap();

        assert_eq!(found.len(), 2, "first glob returned {found:?}");
    }

    #[tokio::test]
    async fn test_files_written_during_a_command_are_not_missed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let root_kp = KaosPath::from(root.to_path_buf());
        let cached = CachedKaos::new(root.to_path_buf()).await;

        // Warm the index.
        assert!(
            cached
                .glob(&root_kp, "*.txt", true)
                .await
                .unwrap()
                .is_empty()
        );

        let mut process = cached.exec(&noop_command()).await.unwrap();
        // A glob while the command is still alive. This is the one that used
        // to publish its listing as current and freeze out everything the
        // command wrote from here on.
        let _ = cached.glob(&root_kp, "*.txt", true).await.unwrap();
        // Stand in for the command's own output, landing after that glob.
        tokio::fs::write(root.join("late.txt"), b"late")
            .await
            .unwrap();
        process.wait().await.unwrap();
        drop(process);

        let after = cached.glob(&root_kp, "*.txt", true).await.unwrap();
        assert_eq!(after.len(), 1, "file written during the command was missed");
    }

    #[tokio::test]
    async fn test_undo_history_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let cached = CachedKaos::new(root.to_path_buf()).await;

        for index in 0..(MAX_UNDO_ENTRIES + 20) {
            let path = kp(root, &format!("file{index}.txt"));
            cached.write_text(&path, "x", false).await.unwrap();
        }

        // `undo(0)` applies nothing, so it reads the depth without spending it.
        let report = cached.undo(0).await.unwrap();
        assert_eq!(report.steps_available, MAX_UNDO_ENTRIES);
        assert_eq!(report.steps_applied, 0);
    }

    #[tokio::test]
    async fn test_undo_restores_content_and_deletes_new_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let existing = kp(root, "existing.txt");
        tokio::fs::write(existing.as_path(), b"before")
            .await
            .unwrap();

        let cached = CachedKaos::new(root.to_path_buf()).await;
        cached.write_text(&existing, "after", false).await.unwrap();
        cached
            .write_text(&kp(root, "fresh.txt"), "new", false)
            .await
            .unwrap();

        let report = cached.undo(2).await.unwrap();

        assert_eq!(report.steps_applied, 2);
        assert_eq!(report.restored, 1);
        assert_eq!(report.deleted, 1);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(
            tokio::fs::read_to_string(existing.as_path()).await.unwrap(),
            "before"
        );
        assert!(!root.join("fresh.txt").exists());
    }

    #[tokio::test]
    async fn test_oversized_file_is_not_retained_for_undo() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let big = kp(root, "big.bin");
        tokio::fs::write(big.as_path(), vec![b'a'; MAX_UNDO_FILE_BYTES as usize + 1])
            .await
            .unwrap();

        let cached = CachedKaos::new(root.to_path_buf()).await;
        cached.write_text(&big, "small", false).await.unwrap();

        let report = cached.undo(1).await.unwrap();

        assert_eq!(report.skipped, 1);
        assert_eq!(report.restored, 0);
        assert_eq!(report.deleted, 0);
        // Left exactly as it is — the danger is treating "not recorded" as
        // "did not exist" and deleting a file undo never captured.
        assert_eq!(
            tokio::fs::read_to_string(big.as_path()).await.unwrap(),
            "small"
        );
    }
}
