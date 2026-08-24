mod tool_test_utils;

use kimi_agent::tools::dmail::SendDMail;
use kimi_agent::tools::file::{Glob, Grep, ReadFile, ReadMediaFile, StrReplaceFile, WriteFile};
use kimi_agent::tools::shell::Shell;
use kimi_agent::tools::think::Think;
use kimi_agent::tools::todo::SetTodoList;
use kimi_agent::tools::web::{FetchURL, SearchWeb};
use kosong::tooling::CallableTool;

use tool_test_utils::RuntimeFixture;

fn normalize_required(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::Array(required)) = map.get_mut("required") {
                required.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
            }
            for item in map.values_mut() {
                normalize_required(item);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                normalize_required(item);
            }
        }
        _ => {}
    }
}

fn assert_schema_eq(actual: serde_json::Value, expected: serde_json::Value) {
    let mut actual = actual;
    let mut expected = expected;
    normalize_required(&mut actual);
    normalize_required(&mut expected);
    assert_eq!(actual, expected);
}

#[test]
fn test_send_dmail_params_schema() {
    let fixture = RuntimeFixture::new();
    let tool = SendDMail::new(&fixture.runtime);
    let base = tool.base();
    assert_schema_eq(
        base.parameters,
        serde_json::json!({
            "properties": {
                "message": {"description": "The message to send.", "type": "string"},
                "checkpoint_id": {
                    "description": "The checkpoint to send the message back to.",
                    "minimum": 0,
                    "type": "integer",
                },
            },
            "required": ["message", "checkpoint_id"],
            "type": "object",
        }),
    );
}

#[test]
fn test_think_params_schema() {
    let fixture = RuntimeFixture::new();
    let tool = Think::new(&fixture.runtime);
    let base = tool.base();
    assert_schema_eq(
        base.parameters,
        serde_json::json!({
            "properties": {
                "thought": {
                    "description": "A thought to think about.",
                    "type": "string",
                }
            },
            "required": ["thought"],
            "type": "object",
        }),
    );
}

#[test]
fn test_set_todo_list_params_schema() {
    let fixture = RuntimeFixture::new();
    let tool = SetTodoList::new(&fixture.runtime);
    let base = tool.base();
    assert_schema_eq(
        base.parameters,
        serde_json::json!({
            "properties": {
                "todos": {
                    "description": "The updated todo list",
                    "items": {
                        "properties": {
                            "title": {
                                "description": "The title of the todo",
                                "minLength": 1,
                                "type": "string",
                            },
                            "status": {
                                "description": "The status of the todo",
                                "enum": ["pending", "in_progress", "done"],
                                "type": "string",
                            },
                        },
                        "required": ["title", "status"],
                        "type": "object",
                    },
                    "type": "array",
                }
            },
            "required": ["todos"],
            "type": "object",
        }),
    );
}

#[test]
fn test_shell_params_schema() {
    let fixture = RuntimeFixture::new();
    let tool = Shell::new(&fixture.runtime);
    let base = tool.base();
    assert_schema_eq(
        base.parameters,
        serde_json::json!({
            "properties": {
                "command": {
                    "description": "The command to execute.",
                    "type": "string",
                },
                "timeout": {
                    "default": 60,
                    "description": "The timeout in seconds for the command to execute. If the command takes longer than this, it will be killed.",
                    "maximum": 86400,
                    "minimum": 1,
                    "type": "integer",
                },
                "run_in_background": {
                    "default": false,
                    "description": "Whether to run the command as a background task.",
                    "type": "boolean",
                },
                "description": {
                    "default": "",
                    "description": "A short description for the background task. Required when run_in_background=true.",
                    "type": "string",
                },
            },
            "required": ["command"],
            "type": "object",
        }),
    );
}

#[test]
fn test_read_file_params_schema() {
    let fixture = RuntimeFixture::new();
    let tool = ReadFile::new(&fixture.runtime);
    let base = tool.base();
    assert_schema_eq(
        base.parameters,
        serde_json::json!({
            "properties": {
                "path": {
                    "description": "The path to the file to read. Relative unless when targeting a file outside of the workdir.",
                    "type": "string",
                },
                "line_offset": {
                    "default": 1,
                    "description": "The line number to start reading from. Default's to 1. Negative values read from the end of the file (e.g. -100 reads the last 100 lines).",
                    "type": "integer",
                },
                "n_lines": {
                    "default": 1000,
                    "description": "The number of lines to read. By default the maximum. Combine this value with the offset to make targeted readings.",
                    "minimum": 1,
                    "type": "integer",
                },
            },
            "required": ["path"],
            "type": "object",
        }),
    );
}

#[test]
fn test_read_media_file_params_schema() {
    let fixture = RuntimeFixture::new();
    let tool = ReadMediaFile::new(&fixture.runtime).expect("read media tool");
    let base = tool.base();
    assert_schema_eq(
        base.parameters,
        serde_json::json!({
            "properties": {
                "path": {
                    "description": "The path to the file to read. Relative unless when targeting a file outside of the workdir.",
                    "type": "string",
                }
            },
            "required": ["path"],
            "type": "object",
        }),
    );
}

#[test]
fn test_glob_params_schema() {
    let fixture = RuntimeFixture::new();
    let tool = Glob::new(&fixture.runtime);
    let base = tool.base();
    assert_schema_eq(
        base.parameters,
        serde_json::json!({
            "properties": {
                "pattern": {
                    "description": "Glob pattern to match files/directories.",
                    "type": "string",
                },
                "directory": {
                    "anyOf": [{"type": "string"}, {"type": "null"}],
                    "default": null,
                    "description": "Absolute path to the directory to search in (defaults to working directory).",
                },
                "include_dirs": {
                    "default": true,
                    "description": "Whether to include directories in results.",
                    "type": "boolean",
                },
            },
            "required": ["pattern"],
            "type": "object",
        }),
    );
}

#[test]
fn test_grep_params_schema() {
    let fixture = RuntimeFixture::new();
    let tool = Grep::new(&fixture.runtime);
    let base = tool.base();
    assert_schema_eq(
        base.parameters,
        serde_json::json!({
            "properties": {
                "pattern": {
                    "description": "Regex pattern to search for.",
                    "type": "string",
                },
                "path": {
                    "anyOf": [{"type": "string"}, {"type": "null"}],
                    "default": null,
                    "description": "Search root. Defaults to the working directory.",
                },
                "glob": {
                    "anyOf": [{"type": "string"}, {"type": "null"}],
                    "default": null,
                    "description": "Glob filter, e.g. `*.rs`.",
                },
                "output_mode": {
                    "default": "files_with_matches",
                    "description": "Output mode: `files_with_matches` (default), `content`, or `count_matches`.",
                    "type": "string",
                },
                "-B": {
                    "anyOf": [{"type": "integer"}, {"type": "null"}],
                    "default": null,
                    "description": "Lines of context before each match.",
                    "minimum": 0,
                },
                "-A": {
                    "anyOf": [{"type": "integer"}, {"type": "null"}],
                    "default": null,
                    "description": "Lines of context after each match.",
                    "minimum": 0,
                },
                "-C": {
                    "anyOf": [{"type": "integer"}, {"type": "null"}],
                    "default": null,
                    "description": "Lines of context before and after each match.",
                    "minimum": 0,
                },
                "-n": {
                    "default": false,
                    "description": "Show line numbers.",
                    "type": "boolean",
                },
                "-i": {
                    "default": false,
                    "description": "Case-insensitive search.",
                    "type": "boolean",
                },
                "-F": {
                    "default": false,
                    "description": "Treat pattern as a literal string.",
                    "type": "boolean",
                },
                "type": {
                    "anyOf": [{"type": "string"}, {"type": "null"}],
                    "default": null,
                    "description": "File type filter, e.g. `rust`, `python`.",
                },
                "head_limit": {
                    "anyOf": [{"type": "integer"}, {"type": "null"}],
                    "default": null,
                    "description": "Limit output to the first N lines.",
                    "minimum": 0,
                },
                "multiline": {
                    "default": false,
                    "description": "Enable multiline regex mode.",
                    "type": "boolean",
                },
            },
            "required": ["pattern"],
            "type": "object",
        }),
    );
}

#[test]
fn test_write_file_params_schema() {
    let fixture = RuntimeFixture::new();
    let tool = WriteFile::new(&fixture.runtime);
    let base = tool.base();
    assert_schema_eq(
        base.parameters,
        serde_json::json!({
            "properties": {
                "path": {
                    "description": "The path to the file to write. Absolute paths only required when working outside the workdir.",
                    "type": "string",
                },
                "content": {
                    "description": "The content to write to the file",
                    "type": "string",
                },
                "mode": {
                    "default": "overwrite",
                    "description": "The mode to use to write to the file. Two modes are supported: `overwrite` for overwriting the whole file and `append` for appending to the end of an existing file.",
                    "enum": ["overwrite", "append"],
                    "type": "string",
                },
            },
            "required": ["path", "content"],
            "type": "object",
        }),
    );
}

#[test]
fn test_str_replace_file_params_schema() {
    let fixture = RuntimeFixture::new();
    let tool = StrReplaceFile::new(&fixture.runtime);
    let base = tool.base();
    assert_schema_eq(
        base.parameters,
        serde_json::json!({
            "properties": {
                "path": {
                    "description": "The path to the target file. Relative unless outside the workdir.",
                    "type": "string",
                },
                "old": {
                    "description": "The string that you want to replace, supports multi-line.",
                    "type": "string",
                },
                "new": {
                    "description": "The replacement string, supports multi-line.",
                    "type": "string",
                },
                "replace_all": {
                    "default": false,
                    "description": "Whether to replace all matches.",
                    "type": "boolean",
                },
                "regex": {
                    "default": false,
                    "description": "Whether to treat the old string as a regex pattern.",
                    "type": "boolean",
                },
            },
            "required": ["path", "old", "new"],
            "type": "object",
        }),
    );
}

#[test]
fn test_search_web_params_schema() {
    let fixture = RuntimeFixture::new();
    let tool = SearchWeb::new(&fixture.runtime).expect("search web tool");
    let base = tool.base();
    assert_schema_eq(
        base.parameters,
        serde_json::json!({
            "properties": {
                "query": {
                    "description": "The query text to search for.",
                    "type": "string",
                },
                "limit": {
                    "default": 5,
                    "description": "The optional maximum returned results.",
                    "maximum": 20,
                    "minimum": 1,
                    "type": "integer",
                },
            },
            "required": ["query"],
            "type": "object",
        }),
    );
}

#[test]
fn test_fetch_url_params_schema() {
    let fixture = RuntimeFixture::new();
    let tool = FetchURL::new(&fixture.runtime);
    let base = tool.base();
    assert_schema_eq(
        base.parameters,
        serde_json::json!({
            "properties": {
                "url": {
                    "description": "The URL to fetch content from.",
                    "type": "string",
                }
            },
            "required": ["url"],
            "type": "object",
        }),
    );
}
