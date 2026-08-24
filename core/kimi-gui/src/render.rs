//! Widget rendering for transcript blocks — the egui counterpart of the
//! Python shell's `visualize/_blocks.py`.

use eframe::egui::{self, Color32, RichText};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use similar::{ChangeTag, TextDiff};

use kimi_agent::wire::ApprovalResponseKind;
use kosong::tooling::{DisplayBlock, ToolOutput, ToolReturnValue};

use crate::transcript::Block;

const ARGS_PREVIEW_CHARS: usize = 160;
const OUTPUT_PREVIEW_CHARS: usize = 4000;
const DIFF_MAX_LINES: usize = 300;

pub fn block_ui(
    ui: &mut egui::Ui,
    index: usize,
    block: &Block,
    cache: &mut CommonMarkCache,
    is_tail: bool,
    turn_running: bool,
) {
    ui.push_id(index, |ui| match block {
        Block::User { text, steer } => user_ui(ui, text, *steer),
        Block::Assistant { text } => {
            CommonMarkViewer::new().show(ui, cache, text);
        }
        Block::Thinking { text } => thinking_ui(ui, text, is_tail && turn_running),
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
        ),
        Block::Approval { info, response } => approval_block_ui(ui, info, response.as_ref()),
        Block::Info(text) => {
            ui.label(RichText::new(text).weak().italics());
        }
    });
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

fn thinking_ui(ui: &mut egui::Ui, text: &str, live: bool) {
    egui::CollapsingHeader::new(RichText::new("Thinking").weak().italics())
        .default_open(false)
        .show(ui, |ui| {
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
    subagent: Option<&crate::transcript::SubagentSummary>,
    live: bool,
) {
    egui::Frame::group(ui.style())
        .inner_margin(6.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                match result {
                    // Only spin while the call can still produce a result:
                    // egui's spinner forces a repaint every frame it is
                    // drawn, so an orphaned one would pin the render loop.
                    None if live => {
                        ui.spinner();
                    }
                    None => {
                        ui.label(RichText::new("?").weak())
                            .on_hover_text("ended without a recorded result");
                    }
                    Some(r) if r.is_error => {
                        ui.label(RichText::new("✗").color(Color32::from_rgb(200, 80, 80)));
                    }
                    Some(_) => {
                        ui.label(RichText::new("✓").color(Color32::from_rgb(80, 170, 80)));
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
                tool_result_ui(ui, result);
            }
        });
}

fn tool_result_ui(ui: &mut egui::Ui, result: &ToolReturnValue) {
    if result.is_error && !result.message.is_empty() {
        ui.label(RichText::new(&result.message).color(Color32::from_rgb(200, 80, 80)));
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
        egui::CollapsingHeader::new(RichText::new("output").weak().small())
            .default_open(false)
            .show(ui, |ui| {
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
                    ChangeTag::Delete => ("-", Color32::from_rgb(200, 90, 90)),
                    ChangeTag::Insert => ("+", Color32::from_rgb(90, 170, 90)),
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
    info: &crate::transcript::ApprovalInfo,
    response: Option<&ApprovalResponseKind>,
) {
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
                    ui.label(RichText::new("approved").color(Color32::from_rgb(80, 170, 80)));
                }
                Some(ApprovalResponseKind::ApproveForSession) => {
                    ui.label(
                        RichText::new("approved for session").color(Color32::from_rgb(80, 170, 80)),
                    );
                }
                Some(ApprovalResponseKind::Reject) => {
                    ui.label(RichText::new("rejected").color(Color32::from_rgb(200, 80, 80)));
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
