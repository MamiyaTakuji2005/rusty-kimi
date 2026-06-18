use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, anyhow};
use tokio::sync::RwLock;

use crate::local::{ALWAYS_PRUNE_DIRS, LocalKaos};
use crate::{Kaos, KaosPath, KaosProcess, LineStream, StatResult, StrOrKaosPath};

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
        self.mark_dirty().await;
        self.inner.write_bytes(path, data).await
    }

    async fn write_text(&self, path: &KaosPath, data: &str, append: bool) -> Result<usize> {
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
