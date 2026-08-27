//! Widget rendering for transcript blocks — the egui counterpart of the
//! Python shell's `visualize/_blocks.py`.

use std::sync::Arc;

use eframe::egui::{self, RichText};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use similar::{ChangeTag, TextDiff};

use dvadva_agent::wire::ApprovalResponseKind;
use kosong::tooling::{DisplayBlock, ToolOutput, ToolReturnValue};

use crate::theme;
use wire_client::transcript::Block;

const ARGS_PREVIEW_CHARS: usize = 160;
const OUTPUT_PREVIEW_CHARS: usize = 4000;
const DIFF_MAX_LINES: usize = 300;

/// Draws one transcript block and returns its bounding response, so the
/// caller can scroll to it or — when `selected` — the outline below shows
/// which one the keyboard has climbed to.
pub fn block_ui(
    ui: &mut egui::Ui,
    index: usize,
    block: &Block,
    cache: &mut CommonMarkCache,
    is_tail: bool,
    turn_running: bool,
    selected: bool,
    toggle_fold: bool,
) -> egui::Response {
    let response = ui
        .push_id(index, |ui| {
            ui.scope(|ui| match block {
                Block::User { text, steer } => user_ui(ui, text, *steer),
                Block::Assistant { text } => {
                    CommonMarkViewer::new().show(ui, cache, text);
                }
                Block::Thinking { text } => {
                    thinking_ui(ui, text, is_tail && turn_running, toggle_fold);
                }
                Block::ToolCall {
                    call,
                    result,
                    subagent,
                    abandoned,
                } => tool_call_ui(
                    ui,
                    call,
                    result.as_ref(),
                    subagent.as_ref(),
                    turn_running && !abandoned,
                    toggle_fold,
                ),
                Block::Approval { info, response } => {
                    approval_block_ui(ui, info, response.as_ref());
                }
                Block::Info(text) => {
                    ui.label(RichText::new(text).weak().italics());
                }
            })
            .response
        })
        .inner;
    if selected {
        let accent = theme::colors(ui.ctx()).accent;
        ui.painter().rect_stroke(
            response.rect.expand(3.0),
            egui::CornerRadius::same(4),
            egui::Stroke::new(2.0, accent),
            egui::StrokeKind::Outside,
        );
    }
    response
}

/// The plain-text content Ctrl+C copies for a climbed-to block. A diff is
/// usually too big to want verbatim — the file it touched is the useful part.
pub fn block_copy_text(block: &Block) -> String {
    match block {
        Block::User { text, .. } | Block::Assistant { text } | Block::Thinking { text } => {
            text.clone()
        }
        Block::ToolCall { call, result, .. } => tool_call_copy_text(call, result.as_ref()),
        Block::Approval { info, .. } => info.description.clone(),
        Block::Info(text) => text.clone(),
    }
}

fn tool_call_copy_text(
    call: &dvadva_agent::wire::ToolCall,
    result: Option<&ToolReturnValue>,
) -> String {
    let Some(result) = result else {
        return format!(
            "{}({})",
            call.function.name,
            call.function.arguments.as_deref().unwrap_or_default()
        );
    };
    let diff_paths: Vec<&str> = result
        .display
        .iter()
        .filter_map(|block| match block {
            DisplayBlock::Diff(diff) => Some(diff.path.as_str()),
            _ => None,
        })
        .collect();
    if !diff_paths.is_empty() {
        return diff_paths.join("\n");
    }
    match &result.output {
        ToolOutput::Text(text) if !text.trim().is_empty() => text.clone(),
        _ if !result.message.is_empty() => result.message.clone(),
        _ => result.brief(),
    }
}

fn user_ui(ui: &mut egui::Ui, text: &str, steer: bool) {
    let fill = ui.visuals().faint_bg_color;
    egui::Frame::group(ui.style())
        .fill(fill)
        .inner_margin(8.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let prefix = if steer { "↪ " } else { "❯ " };
            ui.label(RichText::new(format!("{prefix}{text}")).strong());
        });
}

fn thinking_ui(ui: &mut egui::Ui, text: &str, live: bool, toggle: bool) {
    let id = ui.make_persistent_id("thinking_fold");
    let mut state =
        egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false);
    if toggle {
        state.toggle(ui);
    }
    state
        .show_header(ui, |ui| {
            ui.label(RichText::new("Thinking").weak().italics());
        })
        .body(|ui| {
            ui.label(RichText::new(text).weak());
        });
    if live {
        // Rolling tail preview while the model is still thinking.
        let tail: Vec<&str> = text.lines().rev().take(2).collect();
        for line in tail.iter().rev() {
            ui.label(RichText::new(*line).weak().small());
        }
    }
}

fn tool_call_ui(
    ui: &mut egui::Ui,
    call: &dvadva_agent::wire::ToolCall,
    result: Option<&ToolReturnValue>,
    subagent: Option<&wire_client::transcript::SubagentSummary>,
    live: bool,
    toggle: bool,
) {
    egui::Frame::group(ui.style())
        .inner_margin(6.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let colors = theme::colors(ui.ctx());
            ui.horizontal(|ui| {
                match result {
                    // Only show an indicator while the call can still produce
                    // a result: a spinner keeps asking for repaints, so an
                    // orphaned one would go on costing frames forever.
                    None if live => theme::spinner(ui),
                    None => {
                        ui.label(RichText::new("?").weak())
                            .on_hover_text("ended without a recorded result");
                    }
                    Some(r) if r.is_error => {
                        ui.label(RichText::new("✗").color(colors.error));
                    }
                    Some(_) => {
                        ui.label(RichText::new("✓").color(colors.success));
                    }
                }
                ui.label(RichText::new(&call.function.name).strong().monospace());
                if let Some(args) = &call.function.arguments {
                    // Ellipsized at the row's end: in a horizontal row a
                    // label extends instead of wrapping, which used to shoot
                    // long previews past the window edge, clipped raw. The
                    // header stays one line tall; the full text is a Ctrl+C
                    // away on the climbed block.
                    ui.add(
                        egui::Label::new(
                            RichText::new(truncate(&flatten(args), ARGS_PREVIEW_CHARS))
                                .weak()
                                .monospace(),
                        )
                        .truncate(),
                    );
                }
            });

            if let Some(summary) = subagent {
                ui.label(
                    RichText::new(format!(
                        "subagent · {} events · recent: {}",
                        summary.events,
                        summary.recent_tools.join(", ")
                    ))
                    .weak()
                    .small(),
                );
            }

            if let Some(result) = result {
                tool_result_ui(ui, result, toggle);
            }
        });
}

/// `toggle` is a keyboard equivalent of clicking the "output" header below —
/// not a separate fold of its own — so a block with no output (an edit whose
/// only result is a diff, say) has nothing for it to act on.
fn tool_result_ui(ui: &mut egui::Ui, result: &ToolReturnValue, toggle: bool) {
    if result.is_error && !result.message.is_empty() {
        ui.label(RichText::new(&result.message).color(theme::colors(ui.ctx()).error));
    }
    for block in &result.display {
        display_block_ui(ui, block);
    }
    let output_text = match &result.output {
        ToolOutput::Text(text) => text.clone(),
        ToolOutput::Parts(parts) => format!("[{} content parts]", parts.len()),
    };
    let output_text = output_text.trim();
    if !output_text.is_empty() {
        let id = ui.make_persistent_id("tool_output_fold");
        let mut state =
            egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false);
        if toggle {
            state.toggle(ui);
        }
        state
            .show_header(ui, |ui| {
                ui.label(RichText::new("output").weak().small());
            })
            .body(|ui| {
                ui.label(
                    RichText::new(truncate(output_text, OUTPUT_PREVIEW_CHARS))
                        .weak()
                        .monospace(),
                );
            });
    }
}

pub fn display_block_ui(ui: &mut egui::Ui, block: &DisplayBlock) {
    match block {
        DisplayBlock::Brief(brief) => {
            ui.label(RichText::new(&brief.text).weak().italics());
        }
        DisplayBlock::Shell(shell) => {
            ui.label(RichText::new(format!("$ {}", shell.command)).monospace());
        }
        DisplayBlock::Todo(todo) => {
            for item in &todo.items {
                let icon = match item.status.as_str() {
                    "completed" | "done" => "☑",
                    "in_progress" => "▶",
                    _ => "☐",
                };
                ui.label(format!("{icon} {}", item.title));
            }
        }
        DisplayBlock::Diff(diff) => diff_ui(ui, &diff.path, &diff.old_text, &diff.new_text),
        DisplayBlock::Unknown(unknown) => {
            ui.label(
                RichText::new(format!(
                    "[{}] {}",
                    unknown.kind,
                    truncate(&unknown.data.to_string(), ARGS_PREVIEW_CHARS)
                ))
                .weak()
                .monospace(),
            );
        }
    }
}

/// A diff worked out once and kept while the block stays on screen.
///
/// `similar`'s line diff is the most expensive thing in a transcript frame,
/// and a tool result never changes after it lands — so recomputing every
/// diff in the whole transcript on every repaint was pure waste. Each line
/// arrives with its `+`/`-`/space prefix already attached; only the colour
/// is left to the frame, because the theme can change under a diff that
/// cannot.
struct PreparedDiff {
    lines: Vec<(ChangeTag, String)>,
    /// Hit [`DIFF_MAX_LINES`] and stopped early.
    truncated: bool,
}

impl PreparedDiff {
    fn compute(old_text: &str, new_text: &str) -> Self {
        let diff = TextDiff::from_lines(old_text, new_text);
        let mut lines = Vec::new();
        let mut truncated = false;
        'groups: for group in diff.grouped_ops(2) {
            for op in group {
                for change in diff.iter_changes(&op) {
                    if lines.len() >= DIFF_MAX_LINES {
                        truncated = true;
                        break 'groups;
                    }
                    let sign = match change.tag() {
                        ChangeTag::Delete => "-",
                        ChangeTag::Insert => "+",
                        ChangeTag::Equal => " ",
                    };
                    let line = change.value().trim_end_matches('\n');
                    lines.push((change.tag(), format!("{sign} {line}")));
                }
            }
        }
        Self { lines, truncated }
    }
}

#[derive(Default)]
struct DiffComputer;

impl egui::cache::ComputerMut<(&str, &str), Arc<PreparedDiff>> for DiffComputer {
    fn compute(&mut self, (old_text, new_text): (&str, &str)) -> Arc<PreparedDiff> {
        Arc::new(PreparedDiff::compute(old_text, new_text))
    }
}

/// Keyed by the two texts themselves rather than by position, so one block's
/// diff is never mistaken for another's — a session and its subagent tabs
/// reuse the same block indices. egui drops whatever a frame did not ask
/// for, so closing a tab reclaims its diffs with no bookkeeping here.
type DiffCache = egui::cache::FrameCache<Arc<PreparedDiff>, DiffComputer>;

fn diff_ui(ui: &mut egui::Ui, path: &str, old_text: &str, new_text: &str) {
    ui.label(RichText::new(path).strong().monospace());
    let colors = theme::colors(ui.ctx());
    let prepared = ui
        .ctx()
        .memory_mut(|mem| mem.caches.cache::<DiffCache>().get((old_text, new_text)));
    for (tag, line) in &prepared.lines {
        let color = match tag {
            ChangeTag::Delete => colors.diff_del,
            ChangeTag::Insert => colors.diff_add,
            ChangeTag::Equal => ui.visuals().weak_text_color(),
        };
        ui.label(RichText::new(line).monospace().color(color));
    }
    if prepared.truncated {
        ui.label(RichText::new("... diff truncated ...").weak().italics());
    }
}

fn approval_block_ui(
    ui: &mut egui::Ui,
    info: &wire_client::transcript::ApprovalInfo,
    response: Option<&ApprovalResponseKind>,
) {
    let colors = theme::colors(ui.ctx());
    egui::Frame::group(ui.style())
        .inner_margin(6.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new("approval").strong());
                ui.label(RichText::new(format!("{} · {}", info.sender, info.action)).weak());
            });
            ui.label(&info.description);
            match response {
                Some(ApprovalResponseKind::Approve) => {
                    ui.label(RichText::new("approved").color(colors.success));
                }
                Some(ApprovalResponseKind::ApproveForSession) => {
                    ui.label(RichText::new("approved for session").color(colors.success));
                }
                Some(ApprovalResponseKind::Reject) => {
                    ui.label(RichText::new("rejected").color(colors.error));
                }
                None => {
                    ui.label(RichText::new("pending").weak().italics());
                }
            }
        });
}

fn flatten(text: &str) -> String {
    text.replace(['\n', '\r'], " ")
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let cut: String = text.chars().take(max_chars).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::{DIFF_MAX_LINES, PreparedDiff};
    use similar::ChangeTag;

    /// Every cached line arrives with its sign already attached, so drawing a
    /// frame is only a matter of picking the colour.
    #[test]
    fn test_prepared_diff_signs_its_lines() {
        let prepared = PreparedDiff::compute(
            "alpha
beta
",
            "alpha
gamma
",
        );
        let tags: Vec<ChangeTag> = prepared.lines.iter().map(|(tag, _)| *tag).collect();
        assert!(tags.contains(&ChangeTag::Delete), "{tags:?}");
        assert!(tags.contains(&ChangeTag::Insert), "{tags:?}");
        assert!(!prepared.truncated);
        for (tag, line) in &prepared.lines {
            let sign = match tag {
                ChangeTag::Delete => '-',
                ChangeTag::Insert => '+',
                ChangeTag::Equal => ' ',
            };
            assert_eq!(line.chars().next(), Some(sign), "{line:?}");
        }
    }

    /// Identical text has nothing to show: `grouped_ops` yields no groups at
    /// all, so the cached entry is empty rather than a wall of context.
    #[test]
    fn test_prepared_diff_of_identical_text_is_empty() {
        let prepared = PreparedDiff::compute(
            "same
", "same
",
        );
        assert!(prepared.lines.is_empty());
        assert!(!prepared.truncated);
    }

    /// The cap is what keeps one enormous rewrite from being cached in full.
    #[test]
    fn test_prepared_diff_stops_at_the_line_cap() {
        let old: String = (0..DIFF_MAX_LINES * 2)
            .map(|i| {
                format!(
                    "line {i}
"
                )
            })
            .collect();
        let new: String = (0..DIFF_MAX_LINES * 2)
            .map(|i| {
                format!(
                    "changed {i}
"
                )
            })
            .collect();
        let prepared = PreparedDiff::compute(&old, &new);
        assert_eq!(prepared.lines.len(), DIFF_MAX_LINES);
        assert!(prepared.truncated);
    }
}
