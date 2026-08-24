//! Transcript state: folds the wire event stream into renderable blocks.
//! This is the egui-side equivalent of the Python shell's `_LiveView`.

use kimi_agent::wire::{
    ApprovalResponseKind, ContentPart, StatusUpdate, ToolCall, UserInput, WireMessage,
};
use kosong::tooling::ToolReturnValue;

pub const MAX_SUBAGENT_TOOLS_SHOWN: usize = 4;

pub struct SubagentSummary {
    pub events: u64,
    /// Names of the most recent subagent tool calls (capped).
    pub recent_tools: Vec<String>,
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
}

#[derive(Default)]
pub struct Transcript {
    pub blocks: Vec<Block>,
    pub status: Status,
}

impl Transcript {
    /// Fold one wire event into the block list. Returns true if anything
    /// visible changed (callers can use this to keep scroll pinned).
    pub fn apply_event(&mut self, msg: WireMessage) -> bool {
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
            WireMessage::TurnEnd(_) | WireMessage::StepBegin(_) => false,
            WireMessage::StepInterrupted(_) => {
                self.blocks.push(Block::Info("step interrupted".into()));
                true
            }
            WireMessage::CompactionBegin(_) => {
                self.blocks.push(Block::Info("compacting context...".into()));
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
                });
                true
            }
            WireMessage::ToolCallPart(part) => {
                // Streamed argument fragments merge into the newest open call.
                for block in self.blocks.iter_mut().rev() {
                    if let Block::ToolCall { call, result: None, .. } = block {
                        call.merge_in_place(&part);
                        return true;
                    }
                }
                false
            }
            WireMessage::ToolResult(tool_result) => {
                for block in self.blocks.iter_mut().rev() {
                    if let Block::ToolCall { call, result, .. } = block {
                        if call.id == tool_result.tool_call_id && result.is_none() {
                            *result = Some(tool_result.return_value);
                            return true;
                        }
                    }
                }
                false
            }
            WireMessage::SubagentEvent(sub) => {
                for block in self.blocks.iter_mut().rev() {
                    if let Block::ToolCall { call, subagent, .. } = block {
                        if call.id == sub.task_tool_call_id {
                            let summary = subagent.get_or_insert_with(|| SubagentSummary {
                                events: 0,
                                recent_tools: Vec::new(),
                            });
                            summary.events += 1;
                            if let WireMessage::ToolCall(inner) = sub.event.as_ref() {
                                summary.recent_tools.push(inner.function.name.clone());
                                if summary.recent_tools.len() > MAX_SUBAGENT_TOOLS_SHOWN {
                                    summary.recent_tools.remove(0);
                                }
                            }
                            return true;
                        }
                    }
                }
                false
            }
            WireMessage::Notification(note) => {
                self.blocks
                    .push(Block::Info(format!("{}: {}", note.title, note.body)));
                true
            }
            WireMessage::ApprovalResponse(resp) => {
                for block in self.blocks.iter_mut().rev() {
                    if let Block::Approval { info, response } = block {
                        if info.request_id == resp.request_id {
                            *response = Some(resp.response);
                            return true;
                        }
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

    pub fn push_approval(&mut self, info: ApprovalInfo) -> usize {
        self.blocks.push(Block::Approval {
            info,
            response: None,
        });
        self.blocks.len() - 1
    }
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
