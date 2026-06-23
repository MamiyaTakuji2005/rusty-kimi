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

#[derive(Debug, JsonSchema)]
#[schemars(schema_with = "str_replace_params_schema")]
pub struct StrReplaceParams {
    #[schemars(
        description = "The path to the target file. Relative unless outside the workdir."
    )]
    pub path: String,
    pub edit: Vec<EditParams>,
}

impl<'de> Deserialize<'de> for StrReplaceParams {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let path = value
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| serde::de::Error::custom("`path` is required"))?
            .to_string();

        let edit = if let Some(edit_value) = value.get("edit") {
            if edit_value.is_array() {
                serde_json::from_value(edit_value.clone()).map_err(serde::de::Error::custom)?
            } else {
                let single: EditParams = serde_json::from_value(edit_value.clone())
                    .map_err(serde::de::Error::custom)?;
                vec![single]
            }
        } else {
            let old = value
                .get("old")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    serde::de::Error::custom("either `edit` or `old`+`new` is required")
                })?
                .to_string();
            let new = value
                .get("new")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    serde::de::Error::custom("`new` is required when `old` is provided")
                })?
                .to_string();
            let replace_all = value.get("replace_all").and_then(Value::as_bool).unwrap_or(false);
            let regex = value.get("regex").and_then(Value::as_bool).unwrap_or(false);
            vec![EditParams {
                old,
                new,
                replace_all,
                regex,
            }]
        };

        Ok(StrReplaceParams { path, edit })
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

fn str_replace_params_schema(schema_gen: &mut SchemaGenerator) -> Schema {
    let path_schema = serde_json::to_value(schema_gen.subschema_for::<String>()).unwrap_or(Value::Null);
    let edit_property = serde_json::to_value(edit_schema(schema_gen)).unwrap_or(Value::Null);

    let mut root = serde_json::Map::new();
    root.insert("type".to_string(), Value::String("object".to_string()));
    let mut props = serde_json::Map::new();
    props.insert("path".to_string(), path_schema);
    props.insert("edit".to_string(), edit_property);
    root.insert("properties".to_string(), Value::Object(props));
    root.insert(
        "required".to_string(),
        Value::Array(vec![
            Value::String("path".to_string()),
            Value::String("edit".to_string()),
        ]),
    );
    Schema::from(root)
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

/// Whitespace-tolerant fallback: match `old` against `text` line-by-line
/// comparing trimmed lines, then splice `new` back in using the original byte
/// ranges. Everything outside the matched block — including the file's line
/// endings (CRLF), its trailing newline, and unmatched lines — is preserved
/// verbatim, so a single fuzzy edit never rewrites the whole file.
fn fuzzy_replace(text: &str, old: &str, new: &str) -> Option<String> {
    let pattern_lines: Vec<&str> = old.lines().collect();
    if pattern_lines.is_empty() {
        return None;
    }

    // Byte spans of each line in `text` (content only, excluding the line
    // terminator), matching `str::lines()` segmentation so a trailing newline
    // does not produce a phantom empty final line.
    let bytes = text.as_bytes();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut line_start = 0;
    for i in 0..bytes.len() {
        if bytes[i] == b'\n' {
            let mut end = i;
            if end > line_start && bytes[end - 1] == b'\r' {
                end -= 1;
            }
            spans.push((line_start, end));
            line_start = i + 1;
        }
    }
    if line_start < bytes.len() {
        spans.push((line_start, bytes.len()));
    }

    let plen = pattern_lines.len();
    if plen > spans.len() {
        return None;
    }
    for start in 0..=spans.len() - plen {
        let matched = pattern_lines.iter().enumerate().all(|(k, p)| {
            let (s, e) = spans[start + k];
            p.trim() == text[s..e].trim()
        });
        if matched {
            let replace_start = spans[start].0;
            let replace_end = spans[start + plen - 1].1;
            let mut result =
                String::with_capacity(text.len() - (replace_end - replace_start) + new.len());
            result.push_str(&text[..replace_start]);
            result.push_str(new);
            result.push_str(&text[replace_end..]);
            return Some(result);
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

fn ref_exists(name: &str, group_count: usize, names: &[Option<&str>]) -> bool {
    if let Ok(idx) = name.parse::<usize>() {
        idx < group_count
    } else {
        names.iter().any(|n| *n == Some(name))
    }
}

/// Build the error for an undefined capture reference. `group_count` includes
/// the implicit whole-match group 0, so user-defined groups = `group_count - 1`.
/// Naming the real group count makes the common "I forgot to escape `[]`, so my
/// `(...)` became a character class with no group" mistake self-diagnosable.
fn undefined_ref_error(reference: &str, group_count: usize) -> String {
    let n = group_count - 1;
    format!(
        "regex replacement references undefined capture group `{reference}`; \
         the pattern defines {n} capture group(s). Escape brackets (`\\[`, `\\]`) \
         if you meant a literal match, or write `$$` for a literal '$'."
    )
}

/// Validate that every capture reference in a regex replacement template
/// (`$N`, `${name}`) refers to a group that exists in `re`.
///
/// The `regex` crate silently expands an unknown reference to the empty
/// string, which turns a typo'd group number — or a `$` that was meant
/// literally — into silent data loss (e.g. `new = "$5"` wipes the match).
/// We mirror the crate's template grammar and reject undefined references so
/// the model gets an actionable error instead. A literal `$` is written `$$`.
fn validate_capture_refs(re: &Regex, new: &str) -> Result<(), String> {
    let names: Vec<Option<&str>> = re.capture_names().collect();
    let group_count = re.captures_len(); // includes the implicit whole-match group 0
    let bytes = new.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'$' {
            i += 1;
            continue;
        }
        // `$$` is an escaped literal dollar sign.
        if bytes.get(i + 1) == Some(&b'$') {
            i += 2;
            continue;
        }
        // `${name}` form.
        if bytes.get(i + 1) == Some(&b'{') {
            let Some(close) = new[i + 2..].find('}').map(|off| i + 2 + off) else {
                // Unterminated `${`: the crate treats `$` as a literal here.
                i += 2;
                continue;
            };
            let name = &new[i + 2..close];
            if !ref_exists(name, group_count, &names) {
                return Err(undefined_ref_error(&format!("${{{name}}}"), group_count));
            }
            i = close + 1;
            continue;
        }
        // `$name` form: greedily consume ASCII alphanumerics / underscores.
        let start = i + 1;
        let mut end = start;
        while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
            end += 1;
        }
        if end == start {
            // `$` not followed by a name (e.g. "$ "): the crate keeps it literal.
            i += 1;
            continue;
        }
        let name = &new[start..end];
        if !ref_exists(name, group_count, &names) {
            return Err(undefined_ref_error(&format!("${name}"), group_count));
        }
        i = end;
    }
    Ok(())
}

/// Fallback parser for the common case where the model emits malformed JSON
/// for `StrReplaceFile` (usually unescaped newlines or backslashes inside
/// `old`/`new`).
///
/// Instead of trying to repair arbitrary JSON, we exploit the known structure:
/// `path`, `old`, and `new` are extracted raw, and the actual file on disk is
/// used as the validator via `apply_edits`/`fuzzy_replace`.
pub(crate) fn try_parse_str_replace_fallback(raw: &str) -> Option<StrReplaceParams> {
    // `path` is always a short string without the escaping problems that break
    // `old`/`new`, so a simple regex is enough.
    let path_re = Regex::new(r#""path"\s*:\s*"([^"]*)""#).ok()?;
    let path = path_re.captures(raw)?.get(1)?.as_str().to_string();

    // Extract `old` (must be followed by `,"new"`) and `new` (must be the last
    // field before the closing brace). We search `new` starting after `old` so
    // that an `old` value containing the literal substring `"new":` does not
    // confuse us.
    let (old, old_close) = extract_string_field(raw, "old", 0, Some("new"))?;
    let (new, _) = extract_string_field(raw, "new", old_close + 1, None)?;

    let replace_all = parse_bool_field(raw, "replace_all");
    let regex = parse_bool_field(raw, "regex");

    Some(StrReplaceParams {
        path,
        edit: vec![EditParams {
            old: unescape_json(&old),
            new: unescape_json(&new),
            replace_all,
            regex,
        }],
    })
}

fn parse_bool_field(raw: &str, key: &str) -> bool {
    let pat = format!(r#""{}"\s*:\s*(true|false)"#, regex::escape(key));
    Regex::new(&pat)
        .ok()
        .and_then(|re| re.captures(raw))
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str() == "true")
        .unwrap_or(false)
}

fn extract_string_field(
    raw: &str,
    key: &str,
    search_from: usize,
    next_key: Option<&str>,
) -> Option<(String, usize)> {
    let key_pat = format!(r#""{}"\s*:\s*""#, regex::escape(key));
    let key_re = Regex::new(&key_pat).ok()?;

    let window = &raw[search_from..];
    let key_match = key_re.find(window)?;
    let open_quote = search_from + key_match.end() - 1;
    let close_quote = find_unescaped_quote(raw, open_quote + 1)?;

    // The character immediately after the closing quote must be the expected
    // structural delimiter. This rejects ambiguous cases such as an unescaped
    // quote inside the value: `{"old": "say "hello", "new": "bye"}`.
    let after = &raw[close_quote + 1..];
    match next_key {
        Some(next) => {
            let next_pat = format!(r#"^\s*,\s*"{}"\s*:"#, regex::escape(next));
            let next_re = Regex::new(&next_pat).ok()?;
            next_re.find(after)?;
        }
        None => {
            // `new` may be the last field or may be followed by other flags
            // such as `replace_all`. Either `}` or `,` is acceptable here.
            let tail_re = Regex::new(r#"^\s*[},]"#).ok()?;
            tail_re.find(after)?;
        }
    }

    Some((raw[open_quote + 1..close_quote].to_string(), close_quote))
}

fn find_unescaped_quote(s: &str, start: usize) -> Option<usize> {
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

/// Undo the JSON escape sequences that are still present in a raw extracted
/// substring. This matters when the model correctly escaped some characters
/// but the overall JSON is still unparseable because of, e.g., an unescaped
/// newline elsewhere.
fn unescape_json(s: &str) -> String {
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
            Some('b') => out.push('\u{0008}'),
            Some('f') => out.push('\u{000C}'),
            Some('/') => out.push('/'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('u') => {
                let mut hex = String::with_capacity(4);
                for _ in 0..4 {
                    match chars.next() {
                        Some(h) => hex.push(h),
                        None => break,
                    }
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
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
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
            validate_capture_refs(&re, &e.new)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_literal_newlines() {
        let raw = r#"{"path": "foo.txt", "edit": {"old": "line1
line2", "new": "line3
line4"}}"#;
        let params = try_parse_str_replace_fallback(raw).unwrap();
        assert_eq!(params.path, "foo.txt");
        assert_eq!(params.edit.len(), 1);
        assert_eq!(params.edit[0].old, "line1\nline2");
        assert_eq!(params.edit[0].new, "line3\nline4");
    }

    #[test]
    fn fallback_escaped_quotes() {
        let raw = r#"{"path": "foo.txt", "edit": {"old": "say \"hello\"", "new": "say \"bye\""}}"#;
        let params = try_parse_str_replace_fallback(raw).unwrap();
        assert_eq!(params.edit[0].old, r#"say "hello""#);
        assert_eq!(params.edit[0].new, r#"say "bye""#);
    }

    #[test]
    fn fallback_unescaped_quote_is_rejected() {
        let raw = r#"{"path": "foo.txt", "edit": {"old": "say "hello"", "new": "bye"}}"#;
        assert!(try_parse_str_replace_fallback(raw).is_none());
    }

    #[test]
    fn fallback_array_edit_wrapper() {
        let raw = r#"{"path": "foo.txt", "edit": [{"old": "a", "new": "b"}]}"#;
        let params = try_parse_str_replace_fallback(raw).unwrap();
        assert_eq!(params.path, "foo.txt");
        assert_eq!(params.edit[0].old, "a");
        assert_eq!(params.edit[0].new, "b");
    }

    #[test]
    fn fallback_preserves_replace_all() {
        let raw = r#"{"path": "foo.txt", "edit": {"old": "a", "new": "b", "replace_all": true}}"#;
        let params = try_parse_str_replace_fallback(raw).unwrap();
        assert!(params.edit[0].replace_all);
    }
}


#[cfg(test)]
mod flat_shape_tests {
    use super::*;

    #[test]
    fn flat_shape_single_edit() {
        let raw = r#"{"path": "foo.txt", "old": "a", "new": "b"}"#;
        let params: StrReplaceParams = serde_json::from_str(raw).unwrap();
        assert_eq!(params.path, "foo.txt");
        assert_eq!(params.edit.len(), 1);
        assert_eq!(params.edit[0].old, "a");
        assert_eq!(params.edit[0].new, "b");
        assert!(!params.edit[0].replace_all);
        assert!(!params.edit[0].regex);
    }

    #[test]
    fn flat_shape_with_flags() {
        let raw = r#"{"path": "foo.txt", "old": "a", "new": "b", "replace_all": true, "regex": true}"#;
        let params: StrReplaceParams = serde_json::from_str(raw).unwrap();
        assert!(params.edit[0].replace_all);
        assert!(params.edit[0].regex);
    }

    #[test]
    fn wrapper_shape_still_works() {
        let raw = r#"{"path": "foo.txt", "edit": {"old": "a", "new": "b"}}"#;
        let params: StrReplaceParams = serde_json::from_str(raw).unwrap();
        assert_eq!(params.path, "foo.txt");
        assert_eq!(params.edit.len(), 1);
        assert_eq!(params.edit[0].old, "a");
        assert_eq!(params.edit[0].new, "b");
    }
}
