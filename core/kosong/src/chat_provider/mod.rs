use std::any::Any;
use std::fmt;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::message::{Message, StreamedMessagePart, ToolCall, ToolCallFunction};
use crate::tooling::Tool;

pub mod echo;
pub mod kimi;
pub mod openai_compatible;

/// Merge one streamed `tool_calls` delta entry into an index-keyed accumulator.
///
/// OpenAI-style streaming sends tool calls as deltas correlated by an `index`
/// field: the first delta for an index carries `id`/`function.name`, and later
/// deltas carry `function.arguments` fragments. Assembling by adjacency (relying
/// on the name landing in the first fragment) silently loses calls when a
/// provider fragments differently; keying by `index` is robust to that and to
/// interleaved parallel tool calls.
pub(crate) fn accumulate_tool_call_delta(acc: &mut Vec<ToolCall>, tc: &serde_json::Value) {
    let index = tc
        .get("index")
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
        .max(0) as usize;
    while acc.len() <= index {
        acc.push(ToolCall {
            kind: "function".to_string(),
            id: String::new(),
            function: ToolCallFunction {
                name: String::new(),
                arguments: None,
            },
            extras: None,
        });
    }
    let call = &mut acc[index];
    if let Some(id) = tc
        .get("id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        call.id = id.to_string();
    }
    if let Some(function) = tc.get("function") {
        if let Some(name) = function
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            call.function.name = name.to_string();
        }
        if let Some(args) = function.get("arguments").and_then(|v| v.as_str()) {
            match &mut call.function.arguments {
                Some(existing) => existing.push_str(args),
                None => call.function.arguments = Some(args.to_string()),
            }
        }
    }
}

#[async_trait]
pub trait StreamedMessage: Send {
    async fn next_part(&mut self) -> Result<Option<StreamedMessagePart>, ChatProviderError>;
    fn id(&self) -> Option<String>;
    fn usage(&self) -> Option<TokenUsage>;
}

#[async_trait]
pub trait ChatProvider: Send + Sync {
    fn name(&self) -> &str;
    fn model_name(&self) -> &str;
    fn thinking_effort(&self) -> Option<ThinkingEffort>;
    async fn generate(
        &self,
        system_prompt: &str,
        tools: &[Tool],
        history: &[Message],
    ) -> Result<Box<dyn StreamedMessage>, ChatProviderError>;
    fn with_thinking(&self, effort: ThinkingEffort) -> Box<dyn ChatProvider>;
    fn as_any(&self) -> &dyn Any;
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_other: i64,
    pub output: i64,
    #[serde(default)]
    pub input_cache_read: i64,
    #[serde(default)]
    pub input_cache_creation: i64,
}

impl TokenUsage {
    pub fn total(&self) -> i64 {
        self.input() + self.output
    }

    pub fn input(&self) -> i64 {
        self.input_other + self.input_cache_read + self.input_cache_creation
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThinkingEffort {
    Off,
    Low,
    Medium,
    High,
}

#[derive(Debug)]
pub struct ChatProviderError {
    pub message: String,
    pub kind: ChatProviderErrorKind,
}

#[derive(Debug)]
pub enum ChatProviderErrorKind {
    Connection,
    Timeout,
    Status(u16),
    EmptyResponse,
    Other,
}

impl ChatProviderError {
    pub fn new(kind: ChatProviderErrorKind, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind,
        }
    }
}

impl fmt::Display for ChatProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ChatProviderError {}
