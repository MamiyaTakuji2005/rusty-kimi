//! Widget rendering for transcript blocks — the egui counterpart of the
//! Python shell's `visualize/_blocks.py`.

use eframe::egui::{self, RichText};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use similar::{ChangeTag, TextDiff};

use kimi_agent::wire::ApprovalResponseKind;
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
    call: &kimi_agent::wire::ToolCall,
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
    call: &kimi_agent::wire::ToolCall,
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
                    ui.label(
                        RichText::new(truncate(&flatten(args), ARGS_PREVIEW_CHARS))
                            .weak()
                            .monospace(),
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

fn diff_ui(ui: &mut egui::Ui, path: &str, old_text: &str, new_text: &str) {
    ui.label(RichText::new(path).strong().monospace());
    let colors = theme::colors(ui.ctx());
    let diff = TextDiff::from_lines(old_text, new_text);
    let mut lines_shown = 0usize;
    for group in diff.grouped_ops(2) {
        for op in group {
            for change in diff.iter_changes(&op) {
                if lines_shown >= DIFF_MAX_LINES {
                    ui.label(RichText::new("... diff truncated ...").weak().italics());
                    return;
                }
                lines_shown += 1;
                let line = change.value().trim_end_matches('\n');
                let (sign, color) = match change.tag() {
                    ChangeTag::Delete => ("-", colors.diff_del),
                    ChangeTag::Insert => ("+", colors.diff_add),
                    ChangeTag::Equal => (" ", ui.visuals().weak_text_color()),
                };
                ui.label(
                    RichText::new(format!("{sign} {line}"))
                        .monospace()
                        .color(color),
                );
            }
        }
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
