use std::collections::VecDeque;

use futures::StreamExt;
use kaos::KaosPath;
use schemars::JsonSchema;
use serde::Deserialize;

use kosong::tooling::error::tool_validate_error;
use kosong::tooling::{CallableTool2, ToolReturnValue, tool_error, tool_ok};

use crate::soul::agent::Runtime;
use crate::tools::utils::{load_desc, truncate_line};

use super::{
    FileKind, MAX_BYTES, MAX_LINE_LENGTH, MAX_LINES, MEDIA_SNIFF_BYTES, READ_DESC,
    detect_file_type, resolve_tool_path, validate_absolute_path,
};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadParams {
    #[schemars(
        description = "The path to the file to read. Relative unless when targeting a file outside of the workdir."
    )]
    pub path: String,
    #[serde(default = "default_line_offset")]
    #[schemars(
        description = "The line number to start reading from. Default's to 1. Negative values read from the end of the file (e.g. -100 reads the last 100 lines).",
        default = "default_line_offset"
    )]
    pub line_offset: i64,
    #[serde(default = "default_n_lines")]
    #[schemars(
        description = "The number of lines to read. By default the maximum. Combine this value with the offset to make targeted readings.",
        range(min = 1),
        default = "default_n_lines"
    )]
    pub n_lines: i64,
}

fn default_line_offset() -> i64 {
    1
}

fn default_n_lines() -> i64 {
    MAX_LINES as i64
}

pub struct ReadFile {
    description: String,
    work_dir: KaosPath,
}

impl ReadFile {
    pub fn new(runtime: &Runtime) -> Self {
        let desc = load_desc(
            READ_DESC,
            &[
                ("MAX_LINES", MAX_LINES.to_string()),
                ("MAX_LINE_LENGTH", MAX_LINE_LENGTH.to_string()),
                ("MAX_BYTES", MAX_BYTES.to_string()),
            ],
        );
        Self {
            description: desc,
            work_dir: runtime.builtin_args.KIMI_WORK_DIR.clone(),
        }
    }
}

#[async_trait::async_trait]
impl CallableTool2 for ReadFile {
    type Params = ReadParams;

    fn name(&self) -> &str {
        "ReadFile"
    }

    fn description(&self) -> &str {
        &self.description
    }

    async fn call_typed(&self, params: Self::Params) -> ToolReturnValue {
        if params.line_offset == 0 {
            return tool_validate_error(
                "line_offset cannot be 0; use 1 for the first line or -1 for the last line",
            );
        }
        if params.line_offset < -(MAX_LINES as i64) {
            return tool_validate_error(&format!(
                "line_offset cannot be less than -{}. Use a positive line_offset with the total line count to read from a specific position.",
                MAX_LINES
            ));
        }
        if params.n_lines < 1 {
            return tool_validate_error("n_lines must be >= 1");
        }
        if params.path.is_empty() {
            return tool_error("", "File path cannot be empty.", "Empty file path");
        }

        let mut path = KaosPath::new(params.path.as_str()).expanduser();
        if let Some(err) = validate_absolute_path(&path, &self.work_dir, "read") {
            return err;
        }
        path = resolve_tool_path(&path, &self.work_dir);

        if !path.exists(true).await {
            return tool_error(
                "",
                format!("`{}` does not exist.", params.path),
                "File not found",
            );
        }
        if !path.is_file(true).await {
            return tool_error(
                "",
                format!("`{}` is not a file.", params.path),
                "Invalid path",
            );
        }

        let header = match path.read_bytes(Some(MEDIA_SNIFF_BYTES)).await {
            Ok(bytes) => bytes,
            Err(err) => {
                return tool_error(
                    "",
                    format!("Failed to read {}. Error: {err}", params.path),
                    "Failed to read file",
                );
            }
        };
        let file_type = detect_file_type(&path.to_string_lossy(), Some(&header));
        match file_type.kind {
            FileKind::Image => {
                return tool_error(
                    "",
                    format!(
                        "`{}` is a image file. Use other appropriate tools to read image or video files.",
                        params.path
                    ),
                    "Unsupported file type",
                );
            }
            FileKind::Video => {
                return tool_error(
                    "",
                    format!(
                        "`{}` is a video file. Use other appropriate tools to read image or video files.",
                        params.path
                    ),
                    "Unsupported file type",
                );
            }
            FileKind::Unknown => {
                return tool_error(
                    "",
                    format!(
                        "`{}` seems not readable. You may need to read it with proper shell commands, Python tools or MCP tools if available. If you read/operate it with Python, you MUST ensure that any third-party packages are installed in a virtual environment (venv).",
                        params.path
                    ),
                    "File not readable",
                );
            }
            FileKind::Text => {}
        }

        let mut stream = match path.read_lines_stream().await {
            Ok(stream) => stream,
            Err(err) => {
                return tool_error(
                    "",
                    format!("Failed to read {}. Error: {err}", params.path),
                    "Failed to read file",
                );
            }
        };

        let (
            lines,
            start_line,
            total_lines,
            truncated_lines,
            max_lines_reached,
            max_bytes_reached,
            eof_reached,
        ) = if params.line_offset < 0 {
            read_tail(
                &mut stream,
                (-params.line_offset) as usize,
                params.n_lines as usize,
            )
            .await
        } else {
            read_forward(
                &mut stream,
                params.line_offset as usize,
                params.n_lines as usize,
            )
            .await
        };

        let numbered: Vec<String> = lines
            .iter()
            .enumerate()
            .map(|(idx, line)| format!("{:6}\t{}", start_line + idx, line))
            .collect();

        let mut message = if lines.is_empty() {
            "No lines read from file.".to_string()
        } else {
            format!(
                "{} lines read from file starting from line {}.",
                lines.len(),
                start_line
            )
        };
        message.push_str(&format!(" Total lines in file: {total_lines}."));

        if max_lines_reached {
            message.push_str(&format!(" Max {MAX_LINES} lines reached."));
        } else if max_bytes_reached {
            message.push_str(&format!(" Max {MAX_BYTES} bytes reached."));
        } else if eof_reached {
            message.push_str(" End of file reached.");
        }
        if !truncated_lines.is_empty() {
            message.push_str(&format!(" Lines {:?} were truncated.", truncated_lines));
        }

        tool_ok(numbered.join("\n"), message, "")
    }
}

/// Read from a positive line offset, counting total lines as we go.
async fn read_forward(
    stream: &mut kaos::LineStream,
    line_offset: usize,
    n_lines: usize,
) -> (Vec<String>, usize, usize, Vec<usize>, bool, bool, bool) {
    let mut lines = Vec::new();
    let mut truncated_lines = Vec::new();
    let mut n_bytes = 0usize;
    let mut max_lines_reached = false;
    let mut max_bytes_reached = false;
    let mut hit_n_lines = false;
    let mut current_line = 0usize;

    while let Some(line) = stream.next().await {
        let line = match line {
            Ok(line) => line,
            Err(_) => break,
        };
        let line = line.trim_end_matches('\n');
        current_line += 1;
        if current_line < line_offset {
            continue;
        }
        if !hit_n_lines && !max_lines_reached && !max_bytes_reached {
            let truncated = truncate_line(&line, MAX_LINE_LENGTH, "...");
            if truncated != line {
                truncated_lines.push(current_line);
            }
            n_bytes += truncated.as_bytes().len();
            lines.push(truncated);
            if lines.len() >= n_lines {
                hit_n_lines = true;
            } else if lines.len() >= MAX_LINES {
                max_lines_reached = true;
            } else if n_bytes >= MAX_BYTES {
                max_bytes_reached = true;
            }
        }
    }

    let eof_reached = !hit_n_lines && !max_lines_reached && !max_bytes_reached;

    (
        lines,
        line_offset,
        current_line,
        truncated_lines,
        max_lines_reached,
        max_bytes_reached,
        eof_reached,
    )
}

/// Read the last `tail_count` lines, then apply n_lines / MAX_LINES / MAX_BYTES limits.
async fn read_tail(
    stream: &mut kaos::LineStream,
    tail_count: usize,
    n_lines: usize,
) -> (Vec<String>, usize, usize, Vec<usize>, bool, bool, bool) {
    let mut tail_buf: VecDeque<(usize, String, bool)> = VecDeque::with_capacity(tail_count);
    let mut current_line = 0usize;

    while let Some(line) = stream.next().await {
        let line = match line {
            Ok(line) => line,
            Err(_) => break,
        };
        let line = line.trim_end_matches('\n');
        current_line += 1;
        let truncated = truncate_line(&line, MAX_LINE_LENGTH, "...");
        let was_truncated = truncated != line;
        tail_buf.push_back((current_line, truncated, was_truncated));
        if tail_buf.len() > tail_count {
            tail_buf.pop_front();
        }
    }

    let all_entries: Vec<_> = tail_buf.into();
    let line_limit = n_lines.min(MAX_LINES);
    let candidates = if all_entries.len() > line_limit {
        &all_entries[..line_limit]
    } else {
        &all_entries[..]
    };
    let max_lines_reached = all_entries.len() > MAX_LINES && candidates.len() == MAX_LINES;

    // Apply MAX_BYTES from the newest lines backward.
    let mut max_bytes_reached = false;
    let mut kept = candidates.len();
    let mut n_bytes = 0usize;
    for entry in candidates.iter().rev() {
        n_bytes += entry.1.as_bytes().len();
        if n_bytes > MAX_BYTES {
            max_bytes_reached = true;
            break;
        }
        kept -= 1;
    }
    let final_entries = &candidates[kept..];

    let start_line = final_entries
        .first()
        .map(|e| e.0)
        .unwrap_or(current_line + 1);
    let mut lines = Vec::with_capacity(final_entries.len());
    let mut truncated_lines = Vec::new();
    for (line_no, truncated, was_truncated) in final_entries {
        if *was_truncated {
            truncated_lines.push(*line_no);
        }
        lines.push(truncated.clone());
    }

    // Tail reads are inherently at EOF; don't append redundant "End of file reached".
    let eof_reached = false;

    (
        lines,
        start_line,
        current_line,
        truncated_lines,
        max_lines_reached,
        max_bytes_reached,
        eof_reached,
    )
}
