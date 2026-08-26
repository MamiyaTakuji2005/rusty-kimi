//! Rendering `Block`s as pre-wrapped terminal lines.
//!
//! Every block is flattened into a fixed list of [`Line`]s *before* drawing:
//! wrapping happens once per content change (width-aware via
//! `unicode-width`), so scrolling is plain index arithmetic and redraws never
//! re-wrap. Markdown is rendered as plain text — headings get underlined by
//! position, code spans are shown verbatim; a rich markdown renderer can come
//! later without touching layout.

use std::fmt::Write as _;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Line as TuiLine, Span};
use similar::{ChangeTag, TextDiff};
use unicode_width::UnicodeWidthStr;

use kimi_agent::wire::ApprovalResponseKind;
use kosong::tooling::{DisplayBlock, ToolOutput, ToolReturnValue};

use crate::theme;
use wire_client::transcript::Block;

const ARGS_PREVIEW_CHARS: usize = 100;
const OUTPUT_PREVIEW_CHARS: usize = 2000;
const DIFF_MAX_LINES: usize = 300;

/// One styled run of text within a line.
struct Run {
    text: String,
    style: Style,
}

/// A fully wrapped visual row.
pub struct Row {
    runs: Vec<Run>,
}

impl Row {
    pub fn to_span_line(&self) -> TuiLine<'static> {
        let spans: Vec<Span> = self
            .runs
            .iter()
            .map(|run| Span::styled(run.text.clone(), run.style))
            .collect();
        TuiLine::from(spans)
    }
}

/// The scrollable transcript of one conversation view.
pub struct RenderedTranscript {
    rows: Vec<Row>,
}

impl RenderedTranscript {
    pub fn new() -> Self {
        Self { rows: Vec::new() }
    }

    /// Rebuild all rows from the visible blocks. Cheap enough per change at
    /// transcript scale (a few thousand blocks would still be milliseconds).
    pub fn rebuild(blocks: &[Block], width: u16, turn_running: bool) -> Self {
        let mut out = Self::new();
        for block in blocks {
            push_block(&mut out.rows, block, width, turn_running);
        }
        // Trailing blank row so the last line never hugs the input box.
        out.rows.push(Row { runs: Vec::new() });
        out
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// The window of rows ending at `bottom`, clamped into range.
    pub fn viewport(&self, bottom: usize, height: usize) -> &[Row] {
        let end = bottom.min(self.rows.len());
        let start = end.saturating_sub(height);
        &self.rows[start..end]
    }
}

impl Default for RenderedTranscript {
    fn default() -> Self {
        Self::new()
    }
}

fn blank(rows: &mut Vec<Row>) {
    rows.push(Row { runs: Vec::new() });
}

fn styled(rows: &mut Vec<Row>, text: &str, style: Style, width: u16) {
    wrap_runs(
        vec![Run {
            text: text.to_string(),
            style,
        }],
        width,
        rows,
    );
}

/// Greedy word-wrap of styled runs into width-bounded rows. Words are never
/// split mid-run unless a single word exceeds the whole width.
fn wrap_runs(runs: Vec<Run>, width: u16, rows: &mut Vec<Row>) {
    let max = width.max(4) as usize;
    let mut current: Vec<Run> = Vec::new();
    let mut current_w = 0usize;

    for run in runs {
        for word in split_words(&run.text) {
            let ww = word.width();
            if ww > max {
                // Oversized single word: hard-break it across rows.
                let mut rest = word.as_str();
                while rest.width() > max {
                    let cut = cut_at_width(rest, max);
                    flush_row(&mut current, &mut current_w, rows);
                    rows.push(Row {
                        runs: vec![Run {
                            text: cut.to_string(),
                            style: run.style,
                        }],
                    });
                    rest = &rest[cut.len()..];
                }
                if !rest.is_empty() {
                    current.push(Run {
                        text: rest.to_string(),
                        style: run.style,
                    });
                    current_w += rest.width();
                }
                continue;
            }
            if current_w + ww > max && current_w > 0 {
                flush_row(&mut current, &mut current_w, rows);
            }
            current.push(Run {
                text: word,
                style: run.style,
            });
            current_w += ww;
        }
    }
    flush_row(&mut current, &mut current_w, rows);
}

/// Emit `current` as one row and reset the accumulator.
fn flush_row(current: &mut Vec<Run>, width: &mut usize, rows: &mut Vec<Row>) {
    rows.push(Row {
        runs: std::mem::take(current),
    });
    *width = 0;
}

/// Split into words keeping trailing separators so spacing survives wrapping.
fn split_words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    for ch in text.chars() {
        word.push(ch);
        if ch.is_whitespace() {
            words.push(std::mem::take(&mut word));
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

/// Longest prefix of `text` whose display width fits `max`.
fn cut_at_width(text: &str, max: usize) -> &str {
    let mut w = 0usize;
    for (idx, ch) in text.char_indices() {
        let cw = UnicodeWidthStr::width(ch.to_string().as_str());
        if w + cw > max {
            return &text[..idx];
        }
        w += cw;
    }
    text
}

fn push_block(rows: &mut Vec<Row>, block: &Block, width: u16, turn_running: bool) {
    match block {
        Block::User { text, steer } => {
            // The Python shell echoed user input as plain text behind the
            // sparkle symbol; keep the symbol, drop the heavy color.
            let prefix = if *steer { "↪ " } else { "✨ " };
            styled(
                rows,
                &format!("{prefix}{text}"),
                Style::default().add_modifier(Modifier::BOLD),
                width,
            );
            blank(rows);
        }
        Block::Assistant { text } => {
            push_markdownish(rows, text, width);
            blank(rows);
        }
        Block::Thinking { text } => {
            // Rolling tail while live, collapsed marker otherwise.
            let body = if turn_running {
                let tail: Vec<&str> = text.lines().rev().take(2).collect();
                tail.into_iter().rev().collect::<Vec<_>>().join("\n")
            } else {
                String::new()
            };
            styled(
                rows,
                "· thinking",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
                width,
            );
            if !body.is_empty() {
                styled(rows, &body, theme::dim(), width);
            }
            if !turn_running {
                blank(rows);
            }
        }
        Block::ToolCall {
            call,
            result,
            subagent,
            abandoned,
        } => {
            let head_style = Style::default().add_modifier(Modifier::BOLD);
            let mut head = String::new();
            let mark = if result.is_some() {
                if result.as_ref().is_some_and(|r| r.is_error) {
                    "✗"
                } else {
                    "✓"
                }
            } else if *abandoned || !turn_running {
                "?"
            } else {
                "…"
            };
            let _ = write!(head, "{mark} {}", call.function.name);
            if let Some(args) = &call.function.arguments {
                let flat = args.replace(['\n', '\r'], " ");
                let preview: String = flat.chars().take(ARGS_PREVIEW_CHARS).collect();
                let _ = write!(head, " {preview}");
            }
            styled(rows, &head, head_style, width);
            if let Some(summary) = subagent {
                styled(
                    rows,
                    &format!(
                        "subagent · {} events · recent: {}",
                        summary.events,
                        summary.recent_tools.join(", ")
                    ),
                    theme::dim(),
                    width,
                );
            }
            if let Some(result) = result {
                push_tool_result(rows, result, width);
            }
            blank(rows);
        }
        Block::Approval { info, response } => {
            let verdict = match response {
                Some(ApprovalResponseKind::Approve) => "approved",
                Some(ApprovalResponseKind::ApproveForSession) => "approved for session",
                Some(ApprovalResponseKind::Reject) => "rejected",
                None => "pending",
            };
            let color = match response {
                Some(ApprovalResponseKind::Reject) => theme::ERROR,
                Some(_) => theme::SUCCESS,
                None => theme::WARNING,
            };
            styled(
                rows,
                &format!("approval · {} · {} · [{verdict}]", info.sender, info.action),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
                width,
            );
            styled(rows, &info.description, Style::default(), width);
            for display in &info.display {
                push_display_block(rows, display, width);
            }
            blank(rows);
        }
        Block::Info(text) => {
            styled(
                rows,
                &format!("· {text}"),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::ITALIC),
                width,
            );
            blank(rows);
        }
    }
}

/// Plain-text approximation of markdown: heading lines bold+underlined, list
/// bullets kept, everything else verbatim.
fn push_markdownish(rows: &mut Vec<Row>, text: &str, width: u16) {
    for line in text.lines() {
        let trimmed_start = line.trim_start();
        let is_heading = trimmed_start.starts_with('#')
            && trimmed_start
                .chars()
                .all(|c| c == '#' || c.is_alphanumeric() || c.is_whitespace());
        let style = if is_heading {
            Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            Style::default()
        };
        styled(rows, line.trim_end(), style, width);
    }
}

fn push_tool_result(rows: &mut Vec<Row>, result: &ToolReturnValue, width: u16) {
    if result.is_error && !result.message.is_empty() {
        styled(rows, &result.message, theme::error(), width);
    }
    for block in &result.display {
        push_display_block(rows, block, width);
    }
    let output_text = match &result.output {
        ToolOutput::Text(text) => text.clone(),
        ToolOutput::Parts(parts) => format!("[{} content parts]", parts.len()),
    };
    let output_text = output_text.trim();
    if !output_text.is_empty() {
        // Show only the tail: tool outputs grow and the interesting end is
        // where they stop.
        let lines: Vec<&str> = output_text.lines().collect();
        let skipped = lines.len().saturating_sub(8);
        if skipped > 0 {
            styled(
                rows,
                &format!("  … {skipped} more lines"),
                theme::dim(),
                width,
            );
        }
        for line in lines.iter().skip(skipped) {
            let truncated: String = line.chars().take(OUTPUT_PREVIEW_CHARS).collect();
            styled(rows, &format!("  {truncated}"), theme::dim(), width);
        }
    }
}

fn push_display_block(rows: &mut Vec<Row>, block: &DisplayBlock, width: u16) {
    match block {
        DisplayBlock::Brief(brief) => styled(
            rows,
            &brief.text,
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
            width,
        ),
        DisplayBlock::Shell(shell) => styled(
            rows,
            &format!("$ {}", shell.command),
            Style::default(),
            width,
        ),
        DisplayBlock::Todo(todo) => {
            for item in &todo.items {
                let icon = match item.status.as_str() {
                    "completed" | "done" => "☑",
                    "in_progress" => "▶",
                    _ => "☐",
                };
                styled(
                    rows,
                    &format!("{icon} {}", item.title),
                    Style::default(),
                    width,
                );
            }
        }
        DisplayBlock::Diff(diff) => push_diff(rows, diff, width),
        DisplayBlock::Unknown(unknown) => styled(
            rows,
            &format!(
                "[{}] {}",
                unknown.kind,
                truncate_chars(&unknown.data.to_string(), ARGS_PREVIEW_CHARS)
            ),
            theme::dim(),
            width,
        ),
    }
}

/// Flatten a display block into plain `Line`s for overlay popups (approval
/// previews), where the transcript row machinery is not in play.
pub fn push_display_block_lines(lines: &mut Vec<Line<'static>>, block: &DisplayBlock, width: u16) {
    let mut rows: Vec<Row> = Vec::new();
    push_display_block(&mut rows, block, width);
    for row in rows {
        lines.push(row.to_span_line());
    }
    let _ = width;
}

fn push_diff(rows: &mut Vec<Row>, diff: &kosong::tooling::DiffDisplayBlock, width: u16) {
    styled(
        rows,
        &diff.path,
        Style::default().add_modifier(Modifier::BOLD),
        width,
    );
    let d = TextDiff::from_lines(&diff.old_text, &diff.new_text);
    let mut shown = 0usize;
    'outer: for group in d.grouped_ops(2) {
        for op in group {
            for change in d.iter_changes(&op) {
                if shown >= DIFF_MAX_LINES {
                    styled(rows, "... diff truncated ...", theme::dim(), width);
                    break 'outer;
                }
                shown += 1;
                let line = change.value().trim_end_matches('\n');
                let (sign, color) = match change.tag() {
                    ChangeTag::Delete => ("-", theme::ERROR),
                    ChangeTag::Insert => ("+", theme::SUCCESS),
                    ChangeTag::Equal => (" ", theme::DIM),
                };
                styled(
                    rows,
                    &format!("{sign} {line}"),
                    Style::default().fg(color),
                    width,
                );
            }
        }
    }
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let cut: String = text.chars().take(max).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows_text(rendered: &RenderedTranscript) -> Vec<String> {
        rendered
            .rows
            .iter()
            .map(|row| {
                row.runs
                    .iter()
                    .map(|run| run.text.clone())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn wraps_long_lines_at_width() {
        let blocks = vec![Block::User {
            text: "aaaa bbbb cccc dddd".into(),
            steer: false,
        }];
        let rendered = RenderedTranscript::rebuild(&blocks, 10, false);
        let text = rows_text(&rendered);
        // "✨ " prefix counts toward the width.
        assert!(text.iter().all(|line| line.trim_end().width() <= 10));
        assert_eq!(
            text.first().map(|l| l.trim_end().to_string()),
            Some("✨ aaaa".to_string())
        );
        assert_eq!(text.join(" ").split_whitespace().count(), 5);
    }

    #[test]
    fn cjk_width_is_respected() {
        let blocks = vec![Block::User {
            text: "日本語テキスト".into(),
            steer: false,
        }];
        let rendered = RenderedTranscript::rebuild(&blocks, 8, false);
        let text = rows_text(&rendered);
        assert!(text.iter().all(|line| line.width() <= 8));
        assert_eq!(text.join(""), "✨ 日本語テキスト");
    }

    #[test]
    fn viewport_windows_from_the_end() {
        let blocks = vec![Block::Info("a".into()), Block::Info("b".into())];
        let rendered = RenderedTranscript::rebuild(&blocks, 40, false);
        let len = rendered.len();
        let tail = rendered.viewport(len, 2);
        assert_eq!(tail.len(), 2.min(len));
        // Bottom beyond the end clamps.
        assert_eq!(rendered.viewport(len + 100, 1).len(), 1);
    }

    #[test]
    fn info_block_gets_a_bullet() {
        let blocks = vec![Block::Info("wire error: bad json".into())];
        let rendered = RenderedTranscript::rebuild(&blocks, 60, false);
        let text = rows_text(&rendered);
        assert!(text[0].starts_with("· wire error"));
    }

    #[test]
    fn oversized_word_hard_breaks() {
        let blocks = vec![Block::User {
            text: "XXXXXXXXXX tail".into(),
            steer: false,
        }];
        let rendered = RenderedTranscript::rebuild(&blocks, 6, false);
        let text = rows_text(&rendered);
        assert!(text.iter().all(|line| line.trim_end().width() <= 6));
        // Content preserved across the break: prefix, all ten X, and "tail".
        let joined = text.join("");
        assert_eq!(joined.matches('X').count(), 10);
        assert!(joined.contains("tail"));
        // The last visible row holds the wrapped remainder.
        assert!(text.iter().any(|line| line.trim_end() == "tail"));
    }
}
