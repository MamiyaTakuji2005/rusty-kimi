use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, anyhow};
use tokio::sync::RwLock;

use crate::local::{ALWAYS_PRUNE_DIRS, LocalKaos};
use crate::snapshot::{KaosSnapshot, RestoreReport, UndoReport, WriteEntry};
use crate::{Kaos, KaosPath, KaosProcess, LineStream, StatResult, StrOrKaosPath};

/// Files larger than this are excluded from snapshots.
const MAX_SNAPSHOT_FILE_BYTES: u64 = 10 * 1024 * 1024;

enum IndexState {
    /// Full sorted list of paths relative to `work_dir`.
    Ready(Arc<Vec<PathBuf>>),
    /// Any FS-mutating call happened since the last scan; rescan on next glob.
    Dirty,
}

/// A `Kaos` decorator that caches an ignore-aware directory listing of
/// `work_dir` so `glob()` queries hit memory instead of walking the disk.
///
/// The index is built once at construction and marked Dirty on any call that
/// mutates the filesystem (`write_bytes`, `write_text`, `mkdir`, `exec`).
/// The next `glob()` call on a Dirty index does a lazy blocking rescan before
/// matching.  All other operations are delegated to the inner backend.
pub struct CachedKaos {
    inner: Arc<dyn Kaos>,
    work_dir: PathBuf,
    index: Arc<RwLock<IndexState>>,
    /// Sequential log of pre-write content for every write_bytes/write_text call.
    /// Supports undo in reverse order.
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
    /// Build a `CachedKaos` over `work_dir`, performing the initial blocking
    /// scan immediately so the first `glob()` call hits the index.
    pub async fn new(work_dir: PathBuf) -> Self {
        let root = work_dir.clone();
        let initial = tokio::task::spawn_blocking(move || scan_workdir(&root))
            .await
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or_default();

        Self {
            inner: Arc::new(LocalKaos::new()),
            work_dir,
            index: Arc::new(RwLock::new(IndexState::Ready(Arc::new(initial)))),
            write_history: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        }
    }

    /// Return the current index, rescanning if dirty.
    async fn get_paths(&self) -> Result<Arc<Vec<PathBuf>>> {
        // Fast path: read lock — returns immediately when Ready.
        {
            let guard = self.index.read().await;
            if let IndexState::Ready(ref arc) = *guard {
                return Ok(Arc::clone(arc));
            }
        }

        // Slow path: rescan.
        let root = self.work_dir.clone();
        let paths = tokio::task::spawn_blocking(move || scan_workdir(&root))
            .await
            .map_err(|err| anyhow!(err))??;
        let arc = Arc::new(paths);

        let mut guard = self.index.write().await;
        // Another concurrent rescan may have already updated the state — overwrite
        // with fresh data regardless, since idempotent rescans are always correct.
        *guard = IndexState::Ready(Arc::clone(&arc));
        Ok(arc)
    }

    async fn mark_dirty(&self) {
        let mut guard = self.index.write().await;
        *guard = IndexState::Dirty;
    }

    /// Undo the last `steps` writes made through this `CachedKaos` instance.
    ///
    /// Each `write_bytes`/`write_text` call pushed the pre-write content to the
    /// history.  This method pops up to `steps` entries (or all available) and
    /// restores them in reverse order.  Shell-written files are NOT tracked.
    pub async fn undo(&self, steps: usize) -> Result<UndoReport> {
        let mut history = self.write_history.lock().await;
        let steps_available = history.len();
        let steps_to_apply = steps.min(steps_available);
        let mut report = UndoReport {
            steps_available,
            steps_applied: steps_to_apply,
            ..Default::default()
        };

        for _ in 0..steps_to_apply {
            let entry = history.pop().unwrap();
            match entry.original {
                Some(bytes) => {
                    if let Some(parent) = entry.path.parent() {
                        let _ = tokio::fs::create_dir_all(parent).await;
                    }
                    match tokio::fs::write(&entry.path, bytes.as_ref()).await {
                        Ok(_) => report.restored += 1,
                        Err(e) => report.errors.push(format!("{}: {e}", entry.path.display())),
                    }
                }
                None => {
                    // File was created by the write — delete it on undo.
                    match tokio::fs::remove_file(&entry.path).await {
                        Ok(_) => report.deleted += 1,
                        Err(_) => {}
                    }
                }
            }
        }

        if steps_to_apply > 0 {
            self.mark_dirty().await;
        }
        Ok(report)
    }

    /// Capture a point-in-time snapshot of all non-ignored files in `work_dir`.
    ///
    /// Reads every eligible file from disk, so the snapshot is accurate
    /// regardless of Shell commands or subagent writes that the cache never saw.
    pub async fn take_snapshot(
        &self,
        id: String,
        label: Option<String>,
    ) -> Result<KaosSnapshot> {
        let root = self.work_dir.clone();
        let rel_paths =
            tokio::task::spawn_blocking(move || scan_workdir(&root)).await.map_err(|e| anyhow!(e))??;

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
        let current_rels =
            tokio::task::spawn_blocking(move || scan_workdir(&root)).await.map_err(|e| anyhow!(e))??;

        for rel in current_rels {
            let abs = self.work_dir.join(&rel);
            match tokio::fs::metadata(&abs).await {
                Ok(m) if m.is_file() => {
                    if !snapshot.files.contains_key(&abs) {
                        match tokio::fs::remove_file(&abs).await {
                            Ok(_) => report.deleted += 1,
                            Err(e) => {
                                report.errors.push(format!("delete {}: {e}", abs.display()))
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        self.mark_dirty().await;
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
        let abs = path.as_path().to_path_buf();
        let original = tokio::fs::read(&abs).await.ok().map(|b| Arc::from(b.as_slice()));
        self.write_history.lock().await.push(WriteEntry { path: abs, original });
        self.mark_dirty().await;
        self.inner.write_bytes(path, data).await
    }

    async fn write_text(&self, path: &KaosPath, data: &str, append: bool) -> Result<usize> {
        let abs = path.as_path().to_path_buf();
        let original = tokio::fs::read(&abs).await.ok().map(|b| Arc::from(b.as_slice()));
        self.write_history.lock().await.push(WriteEntry { path: abs, original });
        self.mark_dirty().await;
        self.inner.write_text(path, data, append).await
    }

    async fn mkdir(&self, path: &KaosPath, parents: bool, exist_ok: bool) -> Result<()> {
        self.mark_dirty().await;
        self.inner.mkdir(path, parents, exist_ok).await
    }

    async fn exec(&self, args: &[String]) -> Result<Box<dyn KaosProcess>> {
        self.mark_dirty().await;
        self.inner.exec(args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local::LocalKaos;

    async fn make_tree(root: &Path) {
        tokio::fs::create_dir_all(root.join("src")).await.unwrap();
        tokio::fs::write(root.join("src/main.rs"), b"fn main() {}").await.unwrap();
        tokio::fs::write(root.join("src/lib.rs"), b"").await.unwrap();
        tokio::fs::write(root.join("README.md"), b"# hello").await.unwrap();
        tokio::fs::create_dir_all(root.join("target/debug")).await.unwrap();
        tokio::fs::write(root.join("target/debug/out"), b"binary").await.unwrap();
    }

    fn kp(root: &Path, rel: &str) -> KaosPath {
        KaosPath::from(root.join(rel))
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
                local_out.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>(),
                cached_out.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>(),
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
            local_out.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>(),
            cached_out.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>(),
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
        assert!(results.is_empty(), "target/debug/out should be pruned: {results:?}");
    }
}
