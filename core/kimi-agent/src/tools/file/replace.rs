use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use std::borrow::Cow;

use kaos::KaosPath;
use kosong::tooling::{CallableTool2, DisplayBlock, ToolReturnValue, tool_error};
use regex::Regex;

use crate::soul::agent::Runtime;
use crate::tools::utils::tool_rejected_error;
use crate::utils::{build_diff_blocks, is_within_directory};

use super::{
    FILE_ACTION_EDIT, FILE_ACTION_EDIT_OUTSIDE, REPLACE_DESC, read_text_lossy, resolve_tool_path,
    validate_absolute_path,
};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EditParams {
    #[schemars(description = "The string that you want to replace, suports multi-line.")]
    pub old: String,
    #[schemars(description = "The replacement string, suports multi-line.")]
    pub new: String,
    #[serde(default)]
    #[schemars(description = "Whether to replace all matches.", default)]
    pub replace_all: bool,
    #[serde(default)]
    #[schemars(description = "Whether to treat the old string as a regex pattern.", default)]
    pub regex: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StrReplaceParams {
    #[schemars(
        description = "The path to the target file. Relative unless outside the workdir."
    )]
    pub path: String,
    #[serde(deserialize_with = "deserialize_edit_list")]
    #[schemars(schema_with = "edit_schema")]
    pub edit: Vec<EditParams>,
}

fn deserialize_edit_list<'de, D>(deserializer: D) -> Result<Vec<EditParams>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    if value.is_array() {
        serde_json::from_value(value).map_err(serde::de::Error::custom)
    } else {
        let single: EditParams = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(vec![single])
    }
}

fn edit_schema(schema_gen: &mut SchemaGenerator) -> Schema {
    let edit_schema = EditParams::json_schema(schema_gen);
    let list_schema = Vec::<EditParams>::json_schema(schema_gen);
    let mut map = serde_json::Map::new();
    map.insert(
        "anyOf".to_string(),
        Value::Array(vec![
            serde_json::to_value(&edit_schema).unwrap_or(Value::Null),
            serde_json::to_value(&list_schema).unwrap_or(Value::Null),
        ]),
    );
    map.insert(
        "description".to_string(),
        Value::String(
            "The edit(s) to apply to the file. You can provide a single edit or a list of edits here.".to_string(),
        ),
    );
    Schema::from(map)
}

pub struct StrReplaceFile {
    description: String,
    work_dir: KaosPath,
    approval: std::sync::Arc<crate::soul::approval::Approval>,
}

impl StrReplaceFile {
    pub fn new(runtime: &Runtime) -> Self {
        Self {
            description: REPLACE_DESC.to_string(),
            work_dir: runtime.builtin_args.KIMI_WORK_DIR.clone(),
            approval: runtime.approval.clone(),
        }
    }
}

#[async_trait::async_trait]
impl CallableTool2 for StrReplaceFile {
    type Params = StrReplaceParams;

    fn name(&self) -> &str {
        "StrReplaceFile"
    }

    fn description(&self) -> &str {
        &self.description
    }

    async fn call_typed(&self, params: Self::Params) -> ToolReturnValue {
        if params.path.is_empty() {
            return tool_error("", "File path cannot be empty.", "Empty file path");
        }

        let mut path = KaosPath::new(params.path.as_str()).expanduser();
        if let Some(err) = validate_absolute_path(&path, &self.work_dir, "edit") {
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

        let original = match read_text_lossy(&path).await {
            Ok(text) => text,
            Err(err) => {
                return tool_error(
                    "",
                    format!("Failed to edit. Error: {err}"),
                    "Failed to edit file",
                );
            }
        };

        let mut content = original.clone();
        match apply_edits(&mut content, &params.edit) {
            Ok(true) => {}
            Ok(false) => {
                return tool_error(
                    "",
                    "No replacements were made. The old string was not found in the file.",
                    "No replacements made",
                );
            }
            Err(err) => {
                return tool_error("", err, "Edit failed");
            }
        }

        if content == original {
            return tool_error(
                "",
                "No replacements were made. The old string was not found in the file.",
                "No replacements made",
            );
        }

        let diff_blocks: Vec<DisplayBlock> =
            build_diff_blocks(&path.to_string_lossy(), &original, &content)
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
                &format!("Edit file `{}`", path),
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

        if let Err(err) = path.write_text(&content).await {
            return tool_error(
                "",
                format!("Failed to edit {}. Error: {err}", params.path),
                "Failed to edit file",
            );
        }

        ToolReturnValue {
            is_error: false,
            output: Default::default(),
            message: "File successfully edited.".to_string(),
            display: diff_blocks,
            extras: None,
        }
    }
}


fn replace_first_exact(text: &str, old: &str, new: &str) -> Option<String> {
    let pos = text.find(old)?;
    let mut result = String::with_capacity(text.len() - old.len() + new.len());
    result.push_str(&text[..pos]);
    result.push_str(new);
    result.push_str(&text[pos + old.len()..]);
    Some(result)
}

fn replace_all_exact(text: &str, old: &str, new: &str) -> Option<String> {
    let r = text.replace(old, &new);
    if r == text { None } else { Some(r) }
}

fn fuzzy_replace(text: &str, old: &str, new: &str) -> Option<String> {
    let pattern_lines: Vec<&str> = old.lines().collect();
    let content_lines: Vec<&str> = text.lines().collect();
    if pattern_lines.is_empty() || pattern_lines.len() > content_lines.len() {
        return None;
    }
    for start in 0..=content_lines.len() - pattern_lines.len() {
        if pattern_lines
            .iter()
            .zip(&content_lines[start..start + pattern_lines.len()])
            .all(|(p, c)| p.trim() == c.trim())
        {
            let mut combined: Vec<&str> = Vec::new();
            combined.extend_from_slice(&content_lines[..start]);
            combined.extend(new.lines());
            combined.extend_from_slice(&content_lines[start + pattern_lines.len()..]);
            return Some(combined.join("\n"));
        }
    }
    None
}

fn template_needs_captures(new: &str) -> bool {
    new.contains('$') || new.contains('\\')
}

fn replace_first_regex(text: &str, re: &Regex, new: &str) -> Option<String> {
    if !template_needs_captures(new) {
        return match re.replacen(text, 1, new) {
            Cow::Borrowed(_) => None,
            Cow::Owned(s) => Some(s),
        };
    }
    let caps = re.captures(text)?;
    let m = caps.get(0).expect("capture 0 always exists");
    let mut result = String::with_capacity(text.len() - m.len() + new.len());
    result.push_str(&text[..m.start()]);
    caps.expand(new, &mut result);
    result.push_str(&text[m.end()..]);
    Some(result)
}

fn replace_all_regex(text: &str, re: &Regex, new: &str) -> Option<String> {
    if !template_needs_captures(new) {
        return match re.replace_all(text, new) {
            Cow::Borrowed(_) => None,
            Cow::Owned(s) => Some(s),
        };
    }
    let mut result = String::with_capacity(text.len());
    let mut last = 0;
    let mut found = false;
    for caps in re.captures_iter(text) {
        found = true;
        let m = caps.get(0).expect("capture 0 always exists");
        result.push_str(&text[last..m.start()]);
        caps.expand(new, &mut result);
        last = m.end();
    }
    if !found { return None; }
    result.push_str(&text[last..]);
    Some(result)
}

fn apply_edits(content: &mut String, edits: &[EditParams]) -> Result<bool, String> {
    let mut changed = false;
    for e in edits {
        if e.old.is_empty() {
            return Err("empty search".into());
        }

        let maybe_new = if e.regex {
            let re = Regex::new(&e.old)
                .map_err(|err| format!("invalid regex: {err}"))?;
            if e.replace_all {
                replace_all_regex(content, &re, &e.new)
            } else {
                replace_first_regex(content, &re, &e.new)
            }
        } else if e.replace_all {
            replace_all_exact(content, &e.old, &e.new)
        } else {
            replace_first_exact(content, &e.old, &e.new)
        };

        if let Some(new) = maybe_new {
            *content = new;
            changed = true;
            continue;
        }

        // Fall back to fuzzy (whitespace-tolerant) match for single edits
        if !e.replace_all && !e.regex {
            if let Some(new) = fuzzy_replace(content, &e.old, &e.new) {
                *content = new;
                changed = true;
                continue;
            }
        }

        if changed {
            return Err(format!("edit failed for '{}'", &e.old[..e.old.len().min(40)]));
        }
        return Ok(false);
    }
    Ok(changed)
}
