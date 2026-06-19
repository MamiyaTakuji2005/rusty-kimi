use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use globset::GlobBuilder;
use ignore::WalkBuilder;
use ignore::types::TypesBuilder;
use regex::RegexBuilder;
use schemars::JsonSchema;
use serde::Deserialize;

use kosong::tooling::{CallableTool2, ToolReturnValue, tool_error};

use crate::soul::agent::Runtime;
use crate::tools::utils::ToolResultBuilder;

use super::GREP_DESC;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GrepParams {
    #[schemars(description = "The regular expression pattern to search for in file contents.")]
    pub pattern: String,
    #[serde(default)]
    #[schemars(
        description = "File or directory to search in. Defaults to the working directory. Accepts absolute or relative paths."
    )]
    pub path: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Glob pattern to filter files (e.g. `*.js`, `*.{ts,tsx}`). No filter by default."
    )]
    pub glob: Option<String>,
    #[serde(default = "default_output_mode")]
    #[schemars(
        description = "`content`: show matching lines (supports `-B`/`-A`/`-C`/`-n`/`head_limit`); `files_with_matches`: one file path per match (default); `count_matches`: per-file match count.",
        default = "default_output_mode"
    )]
    pub output_mode: String,
    #[serde(default, rename = "-B")]
    #[schemars(
        description = "Lines of context before each match. Requires `output_mode: content`."
    )]
    pub before_context: Option<usize>,
    #[serde(default, rename = "-A")]
    #[schemars(
        description = "Lines of context after each match. Requires `output_mode: content`."
    )]
    pub after_context: Option<usize>,
    #[serde(default, rename = "-C")]
    #[schemars(
        description = "Lines of context before AND after each match. Requires `output_mode: content`."
    )]
    pub context: Option<usize>,
    #[serde(default, rename = "-n")]
    #[schemars(
        description = "Show line numbers in output. Requires `output_mode: content`."
    )]
    pub line_number: bool,
    #[serde(default, rename = "-i")]
    #[schemars(description = "Case-insensitive search.")]
    pub ignore_case: bool,
    #[serde(default, rename = "-F")]
    #[schemars(
        description = "Treat `pattern` as a literal string rather than a regex (fixed strings). Useful when searching for text that contains regex metacharacters."
    )]
    pub fixed_strings: bool,
    #[serde(default, rename = "type")]
    #[schemars(
        description = "File type to search. Examples: rust, python, js, ts, go, java, cpp, c. More efficient than `glob` for standard file types."
    )]
    pub file_type: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Limit output to first N lines. Works across all output modes."
    )]
    pub head_limit: Option<usize>,
    #[serde(default)]
    #[schemars(
        description = "Enable multiline mode: `.` matches newlines and the pattern can span lines."
    )]
    pub multiline: bool,
}

fn default_output_mode() -> String {
    "files_with_matches".to_string()
}

pub struct Grep {
    description: String,
    work_dir: PathBuf,
}

impl Grep {
    pub fn new(runtime: &Runtime) -> Self {
        Self {
            description: GREP_DESC.to_string(),
            work_dir: runtime.builtin_args.KIMI_WORK_DIR.as_path().to_path_buf(),
        }
    }
}

#[async_trait::async_trait]
impl CallableTool2 for Grep {
    type Params = GrepParams;

    fn name(&self) -> &str {
        "Grep"
    }

    fn description(&self) -> &str {
        &self.description
    }

    async fn call_typed(&self, params: Self::Params) -> ToolReturnValue {
        let root = match &params.path {
            Some(p) if !p.is_empty() => {
                let candidate = Path::new(p);
                if candidate.is_absolute() {
                    candidate.to_path_buf()
                } else {
                    self.work_dir.join(candidate)
                }
            }
            _ => self.work_dir.clone(),
        };

        let before_ctx = params.context.unwrap_or(0).max(params.before_context.unwrap_or(0));
        let after_ctx = params.context.unwrap_or(0).max(params.after_context.unwrap_or(0));

        let cfg = GrepConfig {
            pattern: params.pattern,
            root,
            glob: params.glob,
            output_mode: params.output_mode,
            before_context: before_ctx,
            after_context: after_ctx,
            line_number: params.line_number,
            ignore_case: params.ignore_case,
            fixed_strings: params.fixed_strings,
            file_type: params.file_type,
            head_limit: params.head_limit,
            multiline: params.multiline,
        };

        let result = tokio::task::spawn_blocking(move || run_grep(cfg))
            .await
            .map_err(|e| e.to_string());

        match result {
            Ok(Ok(GrepOutput { lines, truncated })) => {
                if lines.is_empty() {
                    return ToolResultBuilder::default().ok("No matches found", "");
                }
                let mut builder = ToolResultBuilder::default();
                builder.write(&lines.join("\n"));
                let msg = if truncated {
                    format!("Results truncated to first {} lines", lines.len())
                } else {
                    String::new()
                };
                builder.ok(&msg, "")
            }
            Ok(Err(e)) => tool_error("", e, "Grep failed"),
            Err(e) => tool_error("", e, "Grep failed"),
        }
    }
}

struct GrepConfig {
    pattern: String,
    root: PathBuf,
    glob: Option<String>,
    output_mode: String,
    before_context: usize,
    after_context: usize,
    line_number: bool,
    ignore_case: bool,
    fixed_strings: bool,
    file_type: Option<String>,
    head_limit: Option<usize>,
    multiline: bool,
}

struct GrepOutput {
    lines: Vec<String>,
    truncated: bool,
}

fn run_grep(cfg: GrepConfig) -> Result<GrepOutput, String> {
    // Build the regex from the pattern.
    let pattern = if cfg.fixed_strings {
        regex::escape(&cfg.pattern)
    } else {
        cfg.pattern.clone()
    };
    let re = RegexBuilder::new(&pattern)
        .case_insensitive(cfg.ignore_case)
        .multi_line(true)
        .dot_matches_new_line(cfg.multiline)
        .build()
        .map_err(|e| format!("Invalid pattern `{}`: {e}", cfg.pattern))?;

    // Build the file type filter.
    let types = if let Some(ref type_name) = cfg.file_type {
        let mut tb = TypesBuilder::new();
        tb.add_defaults();
        tb.select(type_name);
        Some(
            tb.build()
                .map_err(|e| format!("Unknown file type `{type_name}`: {e}"))?,
        )
    } else {
        None
    };

    // Build the glob filter (matches against full relative path so `src/*.rs` works).
    let glob_matcher = if let Some(ref glob_pattern) = cfg.glob {
        let m = GlobBuilder::new(glob_pattern)
            .literal_separator(false)
            .build()
            .map_err(|e| format!("Invalid glob `{glob_pattern}`: {e}"))?
            .compile_matcher();
        Some(m)
    } else {
        None
    };

    // Build the directory walker.
    let mut wb = WalkBuilder::new(&cfg.root);
    wb.hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .ignore(true)
        .parents(true)
        .require_git(false);
    if let Some(types) = types {
        wb.types(types);
    }

    let limit = cfg.head_limit.unwrap_or(usize::MAX);
    let mut out: Vec<String> = Vec::new();
    let mut truncated = false;

    'walk: for entry in wb.build() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            continue;
        }

        let path = entry.path();

        // Apply glob filter against the path relative to the search root.
        if let Some(ref m) = glob_matcher {
            let rel = path.strip_prefix(&cfg.root).unwrap_or(path);
            if !m.is_match(rel) {
                continue;
            }
        }

        // Read file — skip on error or binary content.
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        if is_binary(&bytes) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);

        match cfg.output_mode.as_str() {
            "files_with_matches" => {
                if re.is_match(&text) {
                    out.push(relative_path(path, &cfg.root));
                    if out.len() >= limit {
                        truncated = true;
                        break 'walk;
                    }
                }
            }
            "count_matches" => {
                let count = re.find_iter(&text).count();
                if count > 0 {
                    out.push(format!("{}:{count}", relative_path(path, &cfg.root)));
                    if out.len() >= limit {
                        truncated = true;
                        break 'walk;
                    }
                }
            }
            _ => {
                // content mode
                if cfg.multiline {
                    // Multiline: match across lines, output each matched span.
                    for m in re.find_iter(&text) {
                        let snippet = m.as_str().replace('\n', "↵");
                        out.push(format!("{}:{snippet}", relative_path(path, &cfg.root)));
                        if out.len() >= limit {
                            truncated = true;
                            break 'walk;
                        }
                    }
                } else {
                    let mut pre_buf: VecDeque<(usize, &str)> =
                        VecDeque::with_capacity(cfg.before_context + 1);
                    let mut after_remaining = 0usize;

                    for (idx, line) in text.lines().enumerate() {
                        let line_no = idx + 1;
                        let is_match = re.is_match(line);

                        if is_match {
                            // Flush pre-context.
                            for (n, pre) in pre_buf.drain(..) {
                                let formatted = if cfg.line_number {
                                    format!("{}-{pre}", display_path(path, &cfg.root, n))
                                } else {
                                    format!("{}-{pre}", relative_path(path, &cfg.root))
                                };
                                out.push(formatted);
                                if out.len() >= limit {
                                    truncated = true;
                                    break 'walk;
                                }
                            }
                            // Output matched line.
                            let formatted = if cfg.line_number {
                                format!("{}:{line}", display_path(path, &cfg.root, line_no))
                            } else {
                                format!("{}:{line}", relative_path(path, &cfg.root))
                            };
                            out.push(formatted);
                            if out.len() >= limit {
                                truncated = true;
                                break 'walk;
                            }
                            after_remaining = cfg.after_context;
                        } else if after_remaining > 0 {
                            let formatted = if cfg.line_number {
                                format!("{}-{line}", display_path(path, &cfg.root, line_no))
                            } else {
                                format!("{}-{line}", relative_path(path, &cfg.root))
                            };
                            out.push(formatted);
                            if out.len() >= limit {
                                truncated = true;
                                break 'walk;
                            }
                            after_remaining -= 1;
                            pre_buf.clear();
                        } else {
                            if cfg.before_context > 0 {
                                pre_buf.push_back((line_no, line));
                                if pre_buf.len() > cfg.before_context {
                                    pre_buf.pop_front();
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(GrepOutput { lines: out, truncated })
}

/// Return a path string relative to the search root when possible.
fn relative_path(path: &Path, root: &Path) -> String {
    if root.is_file() {
        path.to_string_lossy().into_owned()
    } else {
        path.strip_prefix(root)
            .map(|r| r.to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.to_string_lossy().into_owned())
    }
}

/// Format `path:line_no` using a path relative to root when possible.
fn display_path(path: &Path, root: &Path, line_no: usize) -> String {
    format!("{}:{line_no}", relative_path(path, root))
}

/// Returns true if `bytes` looks like binary data (contains a null byte in the first 8 KB).
fn is_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(8192)].contains(&0u8)
}
