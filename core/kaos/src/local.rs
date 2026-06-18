use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use futures::stream;
use tokio::fs;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::{
    AsyncReadable, AsyncWritable, Kaos, KaosPath, KaosProcess, LineStream, StatResult,
    StrOrKaosPath,
};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(not(unix))]
use std::time::{SystemTime, UNIX_EPOCH};

pub struct LocalKaos {
    /// When true, `glob` honors ignore rules (`.gitignore`, hidden files, and
    /// well-known heavy dirs like `node_modules`/`target`). Not exposed on the
    /// tool surface yet — it's the internal escape hatch for getting the raw,
    /// unfiltered tree back.
    respect_ignores: bool,
}

impl LocalKaos {
    pub fn new() -> Self {
        Self {
            respect_ignores: true,
        }
    }

    /// Construct a backend with explicit control over ignore handling.
    pub fn with_respect_ignores(respect_ignores: bool) -> Self {
        Self { respect_ignores }
    }
}

/// Directory names always pruned when `respect_ignores` is on, so the walk
/// stays fast and relevant even in a repo without a `.gitignore`.
const ALWAYS_PRUNE_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    ".venv",
    "venv",
    "__pycache__",
    "dist",
    "build",
    ".mypy_cache",
    ".pytest_cache",
];

/// Walk `root` (ignore-aware when `respect_ignores`) and return every entry
/// whose path relative to `root` matches `pattern`. `**` crosses directory
/// separators; `*`/`?` do not (literal separator), matching the previous glob
/// semantics. Brace alternation (`{a,b}`) is supported.
fn glob_walk(
    root: &Path,
    pattern: &str,
    case_sensitive: bool,
    respect_ignores: bool,
) -> Result<Vec<PathBuf>> {
    let matcher = globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .case_insensitive(!case_sensitive)
        .build()
        .map_err(|err| anyhow!(err))?
        .compile_matcher();

    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(respect_ignores)
        .git_ignore(respect_ignores)
        .git_global(respect_ignores)
        .git_exclude(respect_ignores)
        .ignore(respect_ignores)
        .parents(respect_ignores)
        .require_git(false);
    if respect_ignores {
        builder.filter_entry(|entry| {
            // Prune well-known heavy dirs regardless of git status.
            if entry.file_type().is_some_and(|ft| ft.is_dir()) {
                if let Some(name) = entry.file_name().to_str() {
                    return !ALWAYS_PRUNE_DIRS.contains(&name);
                }
            }
            true
        });
    }

    let mut out = Vec::new();
    for entry in builder.build() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue, // skip unreadable entries rather than aborting
        };
        let path = entry.path();
        if path == root {
            continue;
        }
        if let Ok(rel) = path.strip_prefix(root) {
            if matcher.is_match(rel) {
                out.push(path.to_path_buf());
            }
        }
    }
    Ok(out)
}

struct LocalProcess {
    child: tokio::process::Child,
    stdin: StdIoWriter<tokio::process::ChildStdin>,
    stdout: Option<StdIoReader<tokio::process::ChildStdout>>,
    stderr: Option<StdIoReader<tokio::process::ChildStderr>>,
    null_stdout: StdIoReader<tokio::io::Empty>,
    null_stderr: StdIoReader<tokio::io::Empty>,
    exit_status: Option<i32>,
}

#[async_trait::async_trait]
impl KaosProcess for LocalProcess {
    fn pid(&self) -> u32 {
        self.child.id().unwrap_or(0)
    }

    fn returncode(&mut self) -> Option<i32> {
        if self.exit_status.is_some() {
            return self.exit_status;
        }
        if let Ok(Some(status)) = self.child.try_wait() {
            self.exit_status = Some(status.code().unwrap_or(0));
        }
        self.exit_status
    }

    async fn wait(&mut self) -> Result<i32> {
        let status = self.child.wait().await?;
        let code = status.code().unwrap_or(0);
        self.exit_status = Some(code);
        Ok(code)
    }

    async fn kill(&mut self) -> Result<()> {
        self.child.kill().await.map_err(|e| anyhow!(e))
    }

    fn stdin(&mut self) -> &mut dyn AsyncWritable {
        &mut self.stdin
    }

    fn stdout(&mut self) -> &mut dyn AsyncReadable {
        self.stdout
            .as_mut()
            .map(|stream| stream as &mut dyn AsyncReadable)
            .unwrap_or(&mut self.null_stdout)
    }

    fn stderr(&mut self) -> &mut dyn AsyncReadable {
        self.stderr
            .as_mut()
            .map(|stream| stream as &mut dyn AsyncReadable)
            .unwrap_or(&mut self.null_stderr)
    }

    fn take_stdout(&mut self) -> Option<Box<dyn AsyncReadable>> {
        self.stdout
            .take()
            .map(|stream| Box::new(stream) as Box<dyn AsyncReadable>)
    }

    fn take_stderr(&mut self) -> Option<Box<dyn AsyncReadable>> {
        self.stderr
            .take()
            .map(|stream| Box::new(stream) as Box<dyn AsyncReadable>)
    }
}

struct StdIoReader<T>
where
    T: AsyncRead + Unpin + Send,
{
    inner: BufReader<T>,
    eof: bool,
}

impl<T> StdIoReader<T>
where
    T: AsyncRead + Unpin + Send,
{
    fn new(inner: T) -> Self {
        Self {
            inner: BufReader::new(inner),
            eof: false,
        }
    }
}

#[async_trait::async_trait]
impl<T> AsyncReadable for StdIoReader<T>
where
    T: AsyncRead + Unpin + Send + Sync,
{
    async fn read(&mut self, n: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; n];
        let size = self.inner.read(&mut buf).await?;
        if size == 0 {
            self.eof = true;
            buf.clear();
        } else {
            buf.truncate(size);
        }
        Ok(buf)
    }

    async fn readline(&mut self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        let size = self.inner.read_until(b'\n', &mut buf).await?;
        if size == 0 {
            self.eof = true;
        }
        Ok(buf)
    }

    fn is_eof(&self) -> bool {
        self.eof
    }
}

struct StdIoWriter<T>
where
    T: AsyncWrite + Unpin + Send,
{
    inner: T,
}

impl<T> StdIoWriter<T>
where
    T: AsyncWrite + Unpin + Send,
{
    fn new(inner: T) -> Self {
        Self { inner }
    }
}

#[async_trait::async_trait]
impl<T> AsyncWritable for StdIoWriter<T>
where
    T: AsyncWrite + Unpin + Send + Sync,
{
    async fn write(&mut self, data: &[u8]) -> Result<()> {
        self.inner.write_all(data).await?;
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        self.inner.flush().await?;
        Ok(())
    }

    async fn close(&mut self) -> Result<()> {
        self.inner.shutdown().await?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl Kaos for LocalKaos {
    fn name(&self) -> &str {
        "local"
    }

    fn normpath(&self, path: &StrOrKaosPath<'_>) -> KaosPath {
        let path = match path {
            StrOrKaosPath::Str(s) => PathBuf::from(s),
            StrOrKaosPath::KaosPath(p) => p.as_path().to_path_buf(),
        };
        KaosPath::from(normalize_path(&path))
    }

    fn home(&self) -> KaosPath {
        KaosPath::from(dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")))
    }

    fn cwd(&self) -> KaosPath {
        KaosPath::from(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }

    async fn chdir(&self, path: &KaosPath) -> Result<()> {
        std::env::set_current_dir(path.as_path())?;
        Ok(())
    }

    async fn stat(&self, path: &KaosPath, follow_symlinks: bool) -> Result<StatResult> {
        let metadata = if follow_symlinks {
            fs::metadata(path.as_path()).await?
        } else {
            fs::symlink_metadata(path.as_path()).await?
        };

        let st_size = metadata.len();

        #[cfg(unix)]
        let (st_mode, st_ino, st_dev, st_nlink, st_uid, st_gid, st_atime, st_mtime, st_ctime) = {
            let st_mode = metadata.mode();
            let st_ino = metadata.ino();
            let st_dev = metadata.dev();
            let st_nlink = metadata.nlink();
            let st_uid = metadata.uid();
            let st_gid = metadata.gid();
            let st_atime = metadata.atime() as f64 + (metadata.atime_nsec() as f64 / 1e9);
            let st_mtime = metadata.mtime() as f64 + (metadata.mtime_nsec() as f64 / 1e9);
            let st_ctime = metadata.ctime() as f64 + (metadata.ctime_nsec() as f64 / 1e9);
            (
                st_mode as u32,
                st_ino as u64,
                st_dev as u64,
                st_nlink as u64,
                st_uid as u32,
                st_gid as u32,
                st_atime,
                st_mtime,
                st_ctime,
            )
        };

        #[cfg(not(unix))]
        let (st_mode, st_ino, st_dev, st_nlink, st_uid, st_gid, st_atime, st_mtime, st_ctime) = {
            let st_mode = if metadata.is_dir() {
                0o040000 | 0o777
            } else if metadata.is_file() {
                0o100000 | 0o666
            } else {
                0
            };
            let st_atime = metadata
                .accessed()
                .ok()
                .map(system_time_to_f64)
                .unwrap_or(0.0);
            let st_mtime = metadata
                .modified()
                .ok()
                .map(system_time_to_f64)
                .unwrap_or(0.0);
            let st_ctime = metadata
                .created()
                .ok()
                .map(system_time_to_f64)
                .unwrap_or(st_mtime);
            (st_mode, 0, 0, 0, 0, 0, st_atime, st_mtime, st_ctime)
        };

        Ok(StatResult {
            st_mode,
            st_ino,
            st_dev,
            st_nlink,
            st_uid,
            st_gid,
            st_size,
            st_atime,
            st_mtime,
            st_ctime,
        })
    }

    async fn iterdir(&self, path: &KaosPath) -> Result<Vec<KaosPath>> {
        let mut entries = Vec::new();
        let mut dir = fs::read_dir(path.as_path()).await?;
        while let Some(entry) = dir.next_entry().await? {
            entries.push(KaosPath::from(entry.path()));
        }
        Ok(entries)
    }

    async fn glob(
        &self,
        path: &KaosPath,
        pattern: &str,
        case_sensitive: bool,
    ) -> Result<Vec<KaosPath>> {
        let root = path.as_path().to_path_buf();
        let pattern = pattern.to_string();
        let respect_ignores = self.respect_ignores;
        let paths = tokio::task::spawn_blocking(move || {
            glob_walk(&root, &pattern, case_sensitive, respect_ignores)
        })
        .await
        .map_err(|err| anyhow!(err))??;
        Ok(paths.into_iter().map(KaosPath::from).collect())
    }

    async fn read_bytes(&self, path: &KaosPath, limit: Option<usize>) -> Result<Vec<u8>> {
        let mut data = fs::read(path.as_path()).await?;
        if let Some(n) = limit {
            data.truncate(n);
        }
        Ok(data)
    }

    async fn read_text(&self, path: &KaosPath) -> Result<String> {
        Ok(fs::read_to_string(path.as_path()).await?)
    }

    async fn read_lines(&self, path: &KaosPath) -> Result<Vec<String>> {
        let text = fs::read_to_string(path.as_path()).await?;
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        Ok(normalized
            .split_inclusive('\n')
            .map(|s| s.to_string())
            .collect())
    }

    async fn read_lines_stream(&self, path: &KaosPath) -> Result<LineStream> {
        struct LineStreamState<R>
        where
            R: AsyncRead + Unpin + Send,
        {
            reader: BufReader<R>,
            buf: Vec<u8>,
            pending: VecDeque<String>,
            done: bool,
        }

        let file = fs::File::open(path.as_path()).await?;
        let state = LineStreamState {
            reader: BufReader::new(file),
            buf: Vec::new(),
            pending: VecDeque::new(),
            done: false,
        };

        let stream = stream::unfold(state, |mut state| async move {
            loop {
                if let Some(line) = state.pending.pop_front() {
                    return Some((Ok(line), state));
                }
                if state.done {
                    return None;
                }

                state.buf.clear();
                match state.reader.read_until(b'\n', &mut state.buf).await {
                    Ok(0) => {
                        state.done = true;
                    }
                    Ok(_) => {
                        let text = String::from_utf8_lossy(&state.buf);
                        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
                        let lines: Vec<String> = normalized
                            .split_inclusive('\n')
                            .map(str::to_string)
                            .collect();
                        if lines.is_empty() {
                            continue;
                        }
                        for line in lines {
                            state.pending.push_back(line);
                        }
                    }
                    Err(err) => return Some((Err(err.into()), state)),
                }
            }
        });

        Ok(Box::pin(stream))
    }

    async fn write_bytes(&self, path: &KaosPath, data: &[u8]) -> Result<usize> {
        fs::write(path.as_path(), data).await?;
        Ok(data.len())
    }

    async fn write_text(&self, path: &KaosPath, data: &str, append: bool) -> Result<usize> {
        if append {
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path.as_path())
                .await?;
            file.write_all(data.as_bytes()).await?;
        } else {
            fs::write(path.as_path(), data).await?;
        }
        Ok(data.len())
    }

    async fn mkdir(&self, path: &KaosPath, parents: bool, exist_ok: bool) -> Result<()> {
        if parents {
            if let Err(err) = fs::create_dir_all(path.as_path()).await {
                if !exist_ok {
                    return Err(err.into());
                }
            }
        } else if let Err(err) = fs::create_dir(path.as_path()).await {
            if !exist_ok {
                return Err(err.into());
            }
        }
        Ok(())
    }

    async fn exec(&self, args: &[String]) -> Result<Box<dyn KaosProcess>> {
        if args.is_empty() {
            return Err(anyhow!("missing command"));
        }
        let mut command = Command::new(&args[0]);
        if args.len() > 1 {
            command.args(&args[1..]);
        }
        command.stdin(std::process::Stdio::piped());
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());
        let mut child = command.spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("missing stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("missing stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("missing stderr"))?;
        Ok(Box::new(LocalProcess {
            child,
            stdin: StdIoWriter::new(stdin),
            stdout: Some(StdIoReader::new(stdout)),
            stderr: Some(StdIoReader::new(stderr)),
            null_stdout: StdIoReader::new(tokio::io::empty()),
            null_stderr: StdIoReader::new(tokio::io::empty()),
            exit_status: None,
        }))
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    use std::ffi::OsString;
    use std::path::Component;

    let mut parts: Vec<OsString> = Vec::new();
    let mut prefix: Option<OsString> = None;
    let mut has_root = false;

    for component in path.components() {
        match component {
            Component::Prefix(prefix_comp) => {
                prefix = Some(prefix_comp.as_os_str().to_os_string());
            }
            Component::RootDir => {
                has_root = true;
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if let Some(last) = parts.last() {
                    if last != ".." {
                        parts.pop();
                    } else if !has_root {
                        parts.push(OsString::from(".."));
                    }
                } else if !has_root {
                    parts.push(OsString::from(".."));
                }
            }
            Component::Normal(part) => parts.push(part.to_os_string()),
        }
    }

    let mut out = PathBuf::new();
    if let Some(prefix) = prefix {
        out.push(prefix);
    }
    if has_root {
        out.push(std::path::MAIN_SEPARATOR.to_string());
    }
    for part in parts {
        out.push(part);
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

#[cfg(not(unix))]
fn system_time_to_f64(time: SystemTime) -> f64 {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}
