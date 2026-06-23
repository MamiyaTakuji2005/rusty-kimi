use kaos::KaosPath;
use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use kosong::tooling::{CallableTool2, DisplayBlock, ToolOutput, ToolReturnValue, tool_error};

use crate::soul::agent::Runtime;
use crate::tools::utils::tool_rejected_error;
use crate::utils::{build_diff_blocks, is_within_directory};

use super::{
    FILE_ACTION_EDIT, FILE_ACTION_EDIT_OUTSIDE, WRITE_DESC, read_text_lossy, resolve_tool_path,
    validate_absolute_path,
};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteParams {
    #[schemars(
        description = "The path to the file to write. Absolute paths only required when working outside the workdir."
    )]
    pub path: String,
    #[schemars(description = "The content to write to the file")]
    pub content: String,
    #[serde(default = "default_write_mode")]
    #[schemars(
        description = "The mode to use to write to the file. Two modes are supported: `overwrite` for overwriting the whole file and `append` for appending to the end of an existing file.",
        default = "default_write_mode"
    )]
    pub mode: WriteMode,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum WriteMode {
    Overwrite,
    Append,
}

fn default_write_mode() -> WriteMode {
    WriteMode::Overwrite
}

pub struct WriteFile {
    description: String,
    work_dir: KaosPath,
    approval: std::sync::Arc<crate::soul::approval::Approval>,
}

impl WriteFile {
    pub fn new(runtime: &Runtime) -> Self {
        Self {
            description: WRITE_DESC.to_string(),
            work_dir: runtime.builtin_args.KIMI_WORK_DIR.clone(),
            approval: runtime.approval.clone(),
        }
    }
}

#[async_trait::async_trait]
impl CallableTool2 for WriteFile {
    type Params = WriteParams;

    fn name(&self) -> &str {
        "WriteFile"
    }

    fn description(&self) -> &str {
        &self.description
    }

    async fn call_typed(&self, params: Self::Params) -> ToolReturnValue {
        if params.path.is_empty() {
            return tool_error("", "File path cannot be empty.", "Empty file path");
        }

        let mut path = KaosPath::new(params.path.as_str()).expanduser();
        if let Some(err) = validate_absolute_path(&path, &self.work_dir, "write") {
            return err;
        }
        path = resolve_tool_path(&path, &self.work_dir);

        if !path.parent().exists(true).await {
            return tool_error(
                "",
                format!("`{}` parent directory does not exist.", params.path),
                "Parent directory not found",
            );
        }

        let append = matches!(params.mode, WriteMode::Append);

        let file_existed = path.exists(true).await;
        let old_text = if file_existed {
            read_text_lossy(&path).await.ok()
        } else {
            None
        };

        let new_text = if append {
            format!("{}{}", old_text.clone().unwrap_or_default(), params.content)
        } else {
            params.content.clone()
        };

        let diff_blocks: Vec<DisplayBlock> = build_diff_blocks(
            &path.to_string_lossy(),
            &old_text.unwrap_or_default(),
            &new_text,
        )
        .into_iter()
        .map(DisplayBlock::Diff)
        .collect();

        let action = if is_within_directory(&path, &self.work_dir) {
            FILE_ACTION_EDIT
        } else {
            FILE_ACTION_EDIT_OUTSIDE
        };

        let approved = match self
            .approval
            .request(
                self.name(),
                action,
                &format!("Write file `{}`", path),
                Some(diff_blocks.clone()),
            )
            .await
        {
            Ok(value) => value,
            Err(_) => false,
        };
        if !approved {
            return tool_rejected_error();
        }

        let write_result = if append {
            path.append_text(&params.content).await
        } else {
            path.write_text(&params.content).await
        };
        if let Err(err) = write_result {
            return tool_error(
                "",
                format!("Failed to write to {}. Error: {err}", params.path),
                "Failed to write file",
            );
        }

        let size = path.stat(true).await.map(|s| s.st_size).unwrap_or(0);
        let action = if append { "appended to" } else { "overwritten" };
        ToolReturnValue {
            is_error: false,
            output: ToolOutput::Text(String::new()),
            message: format!("File successfully {action}. Current size: {size} bytes."),
            display: diff_blocks,
            extras: None,
        }
    }
}

/// Fallback parser for when the model emits malformed JSON for `WriteFile`
/// (typically unescaped newlines or backslashes inside `content`).
///
/// `path` and `mode` are short fields that never cause escaping issues.
/// `content` is extracted raw using an escape-aware quote scan, then
/// partially unescaped to handle the mixed case where the model correctly
/// escaped some sequences but left others bare.
pub(crate) fn try_parse_write_file_fallback(raw: &str) -> Option<WriteParams> {
    let path_re = Regex::new(r#""path"\s*:\s*"([^"]*)""#).ok()?;
    let path = path_re.captures(raw)?.get(1)?.as_str().to_string();

    let content = extract_content_field(raw)?;

    let mode = Regex::new(r#""mode"\s*:\s*"(overwrite|append)""#)
        .ok()
        .and_then(|re| re.captures(raw))
        .and_then(|caps| caps.get(1))
        .map(|m| if m.as_str() == "append" { WriteMode::Append } else { WriteMode::Overwrite })
        .unwrap_or(WriteMode::Overwrite);

    Some(WriteParams { path, content, mode })
}

fn extract_content_field(raw: &str) -> Option<String> {
    let key_re = Regex::new(r#""content"\s*:\s*""#).ok()?;
    let m = key_re.find(raw)?;
    let open = m.end() - 1;
    let close = write_find_close_quote(raw, open + 1)?;
    // Structural check: after the closing " must come whitespace then } or ,
    let after = raw[close + 1..].trim_start();
    if !after.starts_with('}') && !after.starts_with(',') {
        return None;
    }
    Some(write_unescape(&raw[open + 1..close]))
}

fn write_find_close_quote(s: &str, start: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = start;
    let mut escaped = false;
    while i < bytes.len() {
        if escaped {
            escaped = false;
        } else if bytes[i] == b'\\' {
            escaped = true;
        } else if bytes[i] == b'"' {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn write_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('/') => out.push('/'),
            Some('b') => out.push('\u{0008}'),
            Some('f') => out.push('\u{000C}'),
            Some('u') => {
                let mut hex = String::with_capacity(4);
                for _ in 0..4 {
                    if let Some(h) = chars.next() { hex.push(h); }
                }
                if hex.len() == 4 {
                    if let Ok(code) = u32::from_str_radix(&hex, 16) {
                        if let Some(ch) = char::from_u32(code) {
                            out.push(ch);
                            continue;
                        }
                    }
                }
                out.push('\\');
                out.push('u');
                out.push_str(&hex);
            }
            Some(other) => { out.push('\\'); out.push(other); }
            None => out.push('\\'),
        }
    }
    out
}
