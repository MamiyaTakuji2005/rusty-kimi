//! Transcript state: folds the wire event stream into renderable blocks.
//! Frontend-agnostic — the egui GUI renders it today, a terminal frontend
//! renders the same blocks its own way.

use kimi_agent::wire::{
    ApprovalResponseKind, ContentPart, StatusUpdate, ToolCall, UserInput, WireMessage,
};
use kosong::tooling::ToolReturnValue;

pub const MAX_SUBAGENT_TOOLS_SHOWN: usize = 4;
const SUBAGENT_TITLE_MAX_CHARS: usize = 28;

pub struct SubagentSummary {
    pub events: u64,
    /// Names of the most recent subagent tool calls (capped).
    pub recent_tools: Vec<String>,
}

/// A subagent's own event stream, folded into a nested transcript.
/// Rendered as a second-layer tab under the session tab.
pub struct SubagentTranscript {
    pub task_tool_call_id: String,
    pub title: String,
    pub transcript: Transcript,
    /// Set when the subagent's own turn ends (its inner TurnEnd event).
    pub done: bool,
}

pub struct ApprovalInfo {
    pub request_id: String,
    pub sender: String,
    pub action: String,
    pub description: String,
    pub display: Vec<kosong::tooling::DisplayBlock>,
}

pub enum Block {
    User {
        text: String,
        steer: bool,
    },
    Assistant {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolCall {
        call: ToolCall,
        result: Option<ToolReturnValue>,
        subagent: Option<SubagentSummary>,
        /// The turn ended (or the step was interrupted) with no result for
        /// this call; render a "no result" marker instead of a live spinner.
        abandoned: bool,
    },
    Approval {
        info: ApprovalInfo,
        response: Option<ApprovalResponseKind>,
    },
    Info(String),
}

/// Live status, delta-merged from `StatusUpdate` events (None = no change).
#[derive(Default)]
pub struct Status {
    pub model: Option<String>,
    pub context_usage: Option<f64>,
    pub context_tokens: Option<i64>,
    pub max_context_tokens: Option<i64>,
    pub yolo_enabled: Option<bool>,
    pub thinking: Option<bool>,
}

impl Status {
    pub fn merge(&mut self, update: &StatusUpdate) {
        if update.model.is_some() {
            self.model = update.model.clone();
        }
        if update.context_usage.is_some() {
            self.context_usage = update.context_usage;
        }
        if update.context_tokens.is_some() {
            self.context_tokens = update.context_tokens;
        }
        if update.max_context_tokens.is_some() {
            self.max_context_tokens = update.max_context_tokens;
        }
        if update.yolo_enabled.is_some() {
            self.yolo_enabled = update.yolo_enabled;
        }
        if update.thinking.is_some() {
            self.thinking = update.thinking;
        }
    }

    /// How full the context is, 0.0–1.0. Derived from the token counts when
    /// both are known, so the figure can't lag behind a model switch that
    /// changed the window size; the agent-supplied ratio is the fallback.
    pub fn context_ratio(&self) -> Option<f64> {
        match (self.context_tokens, self.max_context_tokens) {
            (Some(tokens), Some(max)) if max > 0 => Some((tokens as f64 / max as f64).min(1.0)),
            _ => self.context_usage.map(|usage| usage.clamp(0.0, 1.0)),
        }
    }

    /// Status-bar text for the context, mirroring the Python shell's
    /// `format_context_status`. `None` before the agent has reported anything.
    pub fn context_label(&self) -> Option<String> {
        let ratio = self.context_ratio()?;
        match self.max_context_tokens {
            Some(max) if max > 0 => Some(format!(
                "context: {:.1}% ({}/{})",
                ratio * 100.0,
                format_token_count(self.context_tokens.unwrap_or(0)),
                format_token_count(max),
            )),
            _ => Some(format!("context: {:.1}%", ratio * 100.0)),
        }
    }
}

/// Compact token count, e.g. `28.5k`, `128k`, `1.2m` — same rendering as the
/// Python shell's `format_token_count`.
pub fn format_token_count(n: i64) -> String {
    let (value, suffix) = if n >= 1_000_000 {
        (n as f64 / 1_000_000.0, "m")
    } else if n >= 1_000 {
        (n as f64 / 1_000.0, "k")
    } else {
        return n.to_string();
    };
    // Keep one decimal when it says something, but drop a trailing ".0".
    let compact = format!("{value:.1}");
    let compact = compact.trim_end_matches('0').trim_end_matches('.');
    format!("{compact}{suffix}")
}

#[derive(Default)]
pub struct Transcript {
    pub blocks: Vec<Block>,
    pub status: Status,
    /// One nested transcript per subagent, keyed by the Task tool call id.
    pub subagents: Vec<SubagentTranscript>,
    /// Monotonic change counter. Streaming mutates blocks *in place* (text
    /// deltas append into the last Assistant/Thinking block, streamed tool
    /// arguments merge into an existing ToolCall), so block count is not a
    /// change signal — frontends compare this to decide when to re-render.
    pub version: u64,
}

impl Transcript {
    /// Fold one wire event into the block list. Returns true if anything
    /// visible changed (callers can use this to keep scroll pinned).
    ///
    /// Any change also bumps [`Self::version`] — including the streaming
    /// paths that mutate the newest block in place rather than pushing one.
    pub fn apply_event(&mut self, msg: WireMessage) -> bool {
        let changed = self.apply_event_inner(msg);
        if changed {
            self.version += 1;
        }
        changed
    }

    fn apply_event_inner(&mut self, msg: WireMessage) -> bool {
        match msg {
            WireMessage::TurnBegin(turn) => {
                self.blocks.push(Block::User {
                    text: user_input_text(&turn.user_input),
                    steer: false,
                });
                true
            }
            WireMessage::SteerInput(steer) => {
                self.blocks.push(Block::User {
                    text: user_input_text(&steer.user_input),
                    steer: true,
                });
                true
            }
            WireMessage::TurnEnd(_) => self.abandon_open_tool_calls(),
            WireMessage::StepBegin(_) => false,
            WireMessage::StepInterrupted(_) => {
                self.abandon_open_tool_calls();
                self.blocks.push(Block::Info("step interrupted".into()));
                true
            }
            WireMessage::CompactionBegin(_) => {
                self.blocks
                    .push(Block::Info("compacting context...".into()));
                true
            }
            WireMessage::CompactionEnd(_) => {
                self.blocks.push(Block::Info("context compacted".into()));
                true
            }
            WireMessage::StatusUpdate(update) => {
                self.status.merge(&update);
                true
            }
            WireMessage::ContentPart(part) => {
                self.apply_content_part(part);
                true
            }
            WireMessage::ToolCall(call) => {
                self.blocks.push(Block::ToolCall {
                    call,
                    result: None,
                    subagent: None,
                    abandoned: false,
                });
                true
            }
            WireMessage::ToolCallPart(part) => {
                // Streamed argument fragments merge into the newest open call.
                for block in self.blocks.iter_mut().rev() {
                    if let Block::ToolCall {
                        call, result: None, ..
                    } = block
                    {
                        call.merge_in_place(&part);
                        return true;
                    }
                }
                false
            }
            WireMessage::ToolResult(tool_result) => {
                for block in self.blocks.iter_mut().rev() {
                    if let Block::ToolCall { call, result, .. } = block
                        && call.id == tool_result.tool_call_id
                        && result.is_none()
                    {
                        *result = Some(tool_result.return_value);
                        return true;
                    }
                }
                false
            }
            WireMessage::SubagentEvent(sub) => {
                let task_id = sub.task_tool_call_id;
                let inner = *sub.event;
                // Inline summary on the parent Task tool-call block.
                let mut title_hint = None;
                for block in self.blocks.iter_mut().rev() {
                    if let Block::ToolCall { call, subagent, .. } = block
                        && call.id == task_id
                    {
                        let summary = subagent.get_or_insert_with(|| SubagentSummary {
                            events: 0,
                            recent_tools: Vec::new(),
                        });
                        summary.events += 1;
                        if let WireMessage::ToolCall(inner_call) = &inner {
                            summary.recent_tools.push(inner_call.function.name.clone());
                            if summary.recent_tools.len() > MAX_SUBAGENT_TOOLS_SHOWN {
                                summary.recent_tools.remove(0);
                            }
                        }
                        title_hint = call.function.arguments.as_deref().and_then(subagent_title);
                        break;
                    }
                }
                // Full event stream into the subagent's own transcript
                // (creates the second-layer tab on first event). `/fork`-style
                // spawns have no parent tool call, so the first TurnBegin's
                // prompt supplies the tab title instead.
                let sub_transcript = match self
                    .subagents
                    .iter_mut()
                    .find(|s| s.task_tool_call_id == task_id)
                {
                    Some(existing) => existing,
                    None => {
                        let title = title_hint
                            .or_else(|| match &inner {
                                WireMessage::TurnBegin(turn) => {
                                    clip_title(&user_input_text(&turn.user_input))
                                }
                                _ => None,
                            })
                            .unwrap_or_else(|| {
                                format!("task {}", task_id.chars().take(8).collect::<String>())
                            });
                        self.subagents.push(SubagentTranscript {
                            task_tool_call_id: task_id,
                            title,
                            transcript: Transcript::default(),
                            done: false,
                        });
                        self.subagents.last_mut().expect("just pushed")
                    }
                };
                if matches!(inner, WireMessage::TurnEnd(_)) {
                    // The subagent's turn is its whole life; TurnEnd means done.
                    sub_transcript.done = true;
                }
                sub_transcript.transcript.apply_event(inner);
                true
            }
            WireMessage::Notification(note) => {
                self.blocks
                    .push(Block::Info(format!("{}: {}", note.title, note.body)));
                true
            }
            WireMessage::ApprovalResponse(resp) => {
                for block in self.blocks.iter_mut().rev() {
                    if let Block::Approval { info, response } = block
                        && info.request_id == resp.request_id
                    {
                        *response = Some(resp.response);
                        return true;
                    }
                }
                false
            }
            // Requests arrive via `Inbound::Request` and are pushed by the app;
            // they never come through the event path.
            WireMessage::ApprovalRequest(_) | WireMessage::ToolCallRequest(_) => false,
        }
    }

    fn apply_content_part(&mut self, part: ContentPart) {
        match part {
            ContentPart::Text(text_part) => {
                if let Some(Block::Assistant { text }) = self.blocks.last_mut() {
                    text.push_str(&text_part.text);
                } else {
                    self.blocks.push(Block::Assistant {
                        text: text_part.text,
                    });
                }
            }
            ContentPart::Think(think_part) => {
                if let Some(Block::Thinking { text }) = self.blocks.last_mut() {
                    text.push_str(&think_part.think);
                } else {
                    self.blocks.push(Block::Thinking {
                        text: think_part.think,
                    });
                }
            }
            ContentPart::ImageUrl(_) => self.blocks.push(Block::Info("[image]".into())),
            ContentPart::AudioUrl(_) => self.blocks.push(Block::Info("[audio]".into())),
            ContentPart::VideoUrl(_) => self.blocks.push(Block::Info("[video]".into())),
        }
    }

    /// Mark result-less tool calls as abandoned so they stop rendering as
    /// live (a perpetual spinner forces a repaint every frame). Returns true
    /// if any call was marked.
    fn abandon_open_tool_calls(&mut self) -> bool {
        let mut changed = false;
        for block in &mut self.blocks {
            if let Block::ToolCall {
                result: None,
                abandoned: abandoned @ false,
                ..
            } = block
            {
                *abandoned = true;
                changed = true;
            }
        }
        changed
    }

    pub fn push_approval(&mut self, info: ApprovalInfo) -> usize {
        self.blocks.push(Block::Approval {
            info,
            response: None,
        });
        self.version += 1;
        self.blocks.len() - 1
    }
}

/// Derive a tab title from the spawning tool call's JSON arguments
/// (Fork has `description`/`prompt`, Agent has `agent_file`/`prompt`).
fn subagent_title(args: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(args).ok()?;
    for key in ["description", "agent_name", "subagent_type", "name"] {
        if let Some(title) = value.get(key).and_then(|v| v.as_str()).and_then(clip_title) {
            return Some(title);
        }
    }
    if let Some(file) = value.get("agent_file").and_then(|v| v.as_str())
        && let Some(stem) = std::path::Path::new(file).file_stem()
        && let Some(title) = clip_title(&stem.to_string_lossy())
    {
        return Some(title);
    }
    value
        .get("prompt")
        .and_then(|v| v.as_str())
        .and_then(clip_title)
}

/// First line of `text`, trimmed and capped, or None if empty.
fn clip_title(text: &str) -> Option<String> {
    let line = text.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        return None;
    }
    let mut title: String = line.chars().take(SUBAGENT_TITLE_MAX_CHARS).collect();
    if title.chars().count() < line.chars().count() {
        title.push('…');
    }
    Some(title)
}

pub fn user_input_text(input: &UserInput) -> String {
    match input {
        UserInput::Text(text) => text.clone(),
        UserInput::Parts(parts) => parts
            .iter()
            .map(|part| match part {
                ContentPart::Text(p) => p.text.clone(),
                ContentPart::Think(_) => "[thinking]".to_string(),
                ContentPart::ImageUrl(_) => "[image]".to_string(),
                ContentPart::AudioUrl(_) => "[audio]".to_string(),
                ContentPart::VideoUrl(_) => "[video]".to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status_update() -> StatusUpdate {
        StatusUpdate {
            context_usage: None,
            context_tokens: None,
            max_context_tokens: None,
            token_usage: None,
            message_id: None,
            model: None,
            yolo_enabled: None,
            thinking: None,
        }
    }

    #[test]
    fn test_format_token_count() {
        assert_eq!(format_token_count(0), "0");
        assert_eq!(format_token_count(999), "999");
        assert_eq!(format_token_count(1_000), "1k");
        assert_eq!(format_token_count(28_500), "28.5k");
        assert_eq!(format_token_count(128_000), "128k");
        assert_eq!(format_token_count(1_200_000), "1.2m");
    }

    /// A brand-new session reports zero tokens against a real window, and the
    /// status bar must show that rather than nothing at all.
    #[test]
    fn test_empty_context_still_renders() {
        let mut status = Status::default();
        status.merge(&StatusUpdate {
            context_usage: Some(0.0),
            context_tokens: Some(0),
            max_context_tokens: Some(256_000),
            ..status_update()
        });
        assert_eq!(
            status.context_label().as_deref(),
            Some("context: 0.0% (0/256k)")
        );
    }

    /// The percentage comes from the counts, not from a ratio that may have
    /// been computed against a different model's window size.
    #[test]
    fn test_ratio_recomputed_from_counts() {
        let mut status = Status::default();
        status.merge(&StatusUpdate {
            context_usage: Some(0.9),
            context_tokens: Some(64_000),
            max_context_tokens: Some(256_000),
            ..status_update()
        });
        assert_eq!(
            status.context_label().as_deref(),
            Some("context: 25.0% (64k/256k)")
        );
        assert_eq!(status.context_ratio(), Some(0.25));
    }

    /// An agent that only reports a ratio still gets a percentage shown.
    #[test]
    fn test_ratio_only_status() {
        let mut status = Status::default();
        status.merge(&StatusUpdate {
            context_usage: Some(0.125),
            ..status_update()
        });
        assert_eq!(status.context_label().as_deref(), Some("context: 12.5%"));
    }

    #[test]
    fn test_nothing_reported_yet() {
        assert_eq!(Status::default().context_label(), None);
    }
}
