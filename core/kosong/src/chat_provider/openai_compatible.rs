use std::any::Any;
use std::collections::VecDeque;
use std::env;
use std::pin::Pin;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use reqwest::{Client, Url};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::chat_provider::{
    ChatProvider, ChatProviderError, ChatProviderErrorKind, StreamedMessage, ThinkingEffort,
    TokenUsage,
};
use crate::message::{
    ContentPart, Message, StreamedMessagePart, TextPart, ThinkPart, ToolCall, ToolCallFunction,
};
use crate::tooling::Tool;

/// Generic OpenAI-compatible chat provider.
///
/// Works with any API that follows the OpenAI chat completions format:
/// - OpenAI (GPT-4, o1, etc.)
/// - DeepSeek (V3, R1, etc.)
/// - Groq, Together, Together AI
/// - Local models (Ollama, LM Studio, vLLM)
/// - Any other OpenAI-compatible endpoint
#[derive(Clone)]
pub struct OpenAiCompatible {
    model: String,
    api_key: String,
    base_url: Url,
    stream: bool,
    client: Client,
    generation_kwargs: Map<String, Value>,
    user_agent: String,
}

impl OpenAiCompatible {
    pub fn new(
        model: impl Into<String>,
        api_key: Option<String>,
        base_url: Option<String>,
        default_headers: Option<HeaderMap>,
    ) -> Result<Self, ChatProviderError> {
        let api_key = api_key
            .or_else(|| env::var("OPENAI_API_KEY").ok())
            .ok_or_else(|| {
                ChatProviderError::new(
                    ChatProviderErrorKind::Other,
                    "The api_key client option or the OPENAI_API_KEY environment variable is not set",
                )
            })?;
        let mut base_url = base_url
            .or_else(|| env::var("OPENAI_BASE_URL").ok())
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
        if !base_url.ends_with('/') {
            base_url.push('/');
        }
        let base_url = Url::parse(&base_url).map_err(|err| {
            ChatProviderError::new(
                ChatProviderErrorKind::Other,
                format!("Invalid base URL: {err}"),
            )
        })?;

        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(USER_AGENT, HeaderValue::from_static("KimiCLI"));
        if let Some(extra) = default_headers {
            for (k, v) in extra.iter() {
                if let Some(value) = v.to_str().ok() {
                    headers.insert(
                        k,
                        HeaderValue::from_str(value).unwrap_or_else(|_| v.clone()),
                    );
                } else {
                    headers.insert(k, v.clone());
                }
            }
        }

        let client = Client::builder()
            .default_headers(headers)
            .build()
            .map_err(|err| ChatProviderError::new(ChatProviderErrorKind::Other, err.to_string()))?;

        Ok(Self {
            model: model.into(),
            api_key,
            base_url,
            stream: true,
            client,
            generation_kwargs: Map::new(),
            user_agent: "KimiCLI".to_string(),
        })
    }

    pub fn with_stream(mut self, stream: bool) -> Self {
        self.stream = stream;
        self
    }

    pub fn with_generation_kwargs(mut self, kwargs: Map<String, Value>) -> Self {
        for (k, v) in kwargs {
            self.generation_kwargs.insert(k, v);
        }
        self
    }

    pub fn with_extra_body(mut self, extra_body: Value) -> Self {
        let mut merged = Map::new();
        if let Some(Value::Object(existing)) = self.generation_kwargs.get("extra_body") {
            for (k, v) in existing {
                merged.insert(k.clone(), v.clone());
            }
        }
        if let Value::Object(extra) = extra_body {
            for (k, v) in extra {
                merged.insert(k, v);
            }
        }
        self.generation_kwargs
            .insert("extra_body".to_string(), Value::Object(merged));
        self
    }

    pub fn with_user_agent(mut self, user_agent: impl Into<String>) -> Self {
        self.user_agent = user_agent.into();
        self
    }

    pub fn model_parameters(&self) -> Map<String, Value> {
        let mut params = Map::new();
        params.insert(
            "base_url".to_string(),
            Value::String(self.base_url.to_string()),
        );
        for (k, v) in &self.generation_kwargs {
            params.insert(k.clone(), v.clone());
        }
        params
    }
}

#[async_trait]
impl ChatProvider for OpenAiCompatible {
    fn name(&self) -> &str {
        "openai_compatible"
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    fn thinking_effort(&self) -> Option<ThinkingEffort> {
        // Check for OpenAI o1-series reasoning_effort
        match self.generation_kwargs.get("reasoning_effort") {
            Some(Value::String(value)) => match value.as_str() {
                "low" => Some(ThinkingEffort::Low),
                "medium" => Some(ThinkingEffort::Medium),
                "high" => Some(ThinkingEffort::High),
                _ => Some(ThinkingEffort::Off),
            },
            // Absent or Null reasoning_effort (the latter is what with_thinking(Off)
            // writes) means thinking is off — not "unknown" — so callers (and the UI
            // thinking indicator) can distinguish off from on instead of going stale.
            _ => Some(ThinkingEffort::Off),
        }
    }

    async fn generate(
        &self,
        system_prompt: &str,
        tools: &[Tool],
        history: &[Message],
    ) -> Result<Box<dyn StreamedMessage>, ChatProviderError> {
        let mut messages = Vec::new();
        if !system_prompt.is_empty() {
            messages.push(json!({"role": "system", "content": system_prompt}));
        }
        for message in history {
            messages.push(convert_message(message)?);
        }

        let mut tool_defs = Vec::new();
        for tool in tools {
            tool_defs.push(convert_tool(tool));
        }

        let mut body = Map::new();
        body.insert("model".to_string(), Value::String(self.model.clone()));
        body.insert("messages".to_string(), Value::Array(messages));
        if !tool_defs.is_empty() {
            body.insert("tools".to_string(), Value::Array(tool_defs));
        }
        body.insert("stream".to_string(), Value::Bool(self.stream));
        if self.stream {
            body.insert("stream_options".to_string(), json!({"include_usage": true}));
        }
        let mut generation_kwargs = Map::new();
        generation_kwargs.insert("max_tokens".to_string(), Value::from(32768));
        for (k, v) in &self.generation_kwargs {
            generation_kwargs.insert(k.clone(), v.clone());
        }
        let extra_body = match generation_kwargs.remove("extra_body") {
            Some(Value::Object(map)) => Some(map),
            _ => None,
        };

        for (k, v) in generation_kwargs {
            body.insert(k, v);
        }
        if let Some(extra_body) = extra_body {
            for (k, v) in extra_body {
                body.insert(k, v);
            }
        }

        let url = self
            .base_url
            .join("chat/completions")
            .map_err(|err| ChatProviderError::new(ChatProviderErrorKind::Other, err.to_string()))?;

        let resp = self
            .client
            .post(url)
            .header(AUTHORIZATION, format!("Bearer {}", self.api_key))
            .json(&body)
            .send()
            .await
            .map_err(map_reqwest_error)?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(ChatProviderError::new(
                ChatProviderErrorKind::Status(status.as_u16()),
                format!("API error ({status}): {text}"),
            ));
        }

        if self.stream {
            Ok(Box::new(OpenAiCompatibleStreamedMessage::new_stream(resp)))
        } else {
            let value: Value = resp.json().await.map_err(map_reqwest_error)?;
            let (parts, message_id, usage) = parse_non_stream_response(&value)?;
            Ok(Box::new(OpenAiCompatibleStreamedMessage::new_parts(
                parts, message_id, usage,
            )))
        }
    }

    fn with_thinking(&self, effort: ThinkingEffort) -> Box<dyn ChatProvider> {
        let mut kwargs = Map::new();
        let reasoning_effort = match effort {
            ThinkingEffort::Off => None,
            ThinkingEffort::Low => Some("low"),
            ThinkingEffort::Medium => Some("medium"),
            ThinkingEffort::High => Some("high"),
        };
        if let Some(value) = reasoning_effort {
            kwargs.insert(
                "reasoning_effort".to_string(),
                Value::String(value.to_string()),
            );
        } else {
            kwargs.insert("reasoning_effort".to_string(), Value::Null);
        }

        let provider = self.clone().with_generation_kwargs(kwargs);
        Box::new(provider)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub struct OpenAiCompatibleStreamedMessage {
    stream: Option<Pin<Box<dyn futures::Stream<Item = Result<Bytes, reqwest::Error>> + Send>>>,
    buffer: String,
    parts: VecDeque<StreamedMessagePart>,
    id: Option<String>,
    usage: Option<TokenUsage>,
    /// True once the provider signalled completion (a `finish_reason` chunk or
    /// `[DONE]`). If the byte stream ends without this, the connection was cut
    /// early (e.g. a slow response dropped by the provider gateway) and the
    /// partial response must NOT be treated as complete.
    finished: bool,
    /// Tool calls accumulated by their streaming `index`, assembled here rather
    /// than by adjacency in `generate.rs` (robust to late names / interleaved
    /// parallel calls). Flushed as complete `ToolCall` parts once `finished`.
    tool_acc: Vec<ToolCall>,
    tools_flushed: bool,
}

impl OpenAiCompatibleStreamedMessage {
    pub fn new_stream(resp: reqwest::Response) -> Self {
        let stream = resp.bytes_stream();
        Self {
            stream: Some(Box::pin(stream)),
            buffer: String::new(),
            parts: VecDeque::new(),
            id: None,
            usage: None,
            finished: false,
            tool_acc: Vec::new(),
            tools_flushed: false,
        }
    }

    pub fn new_parts(
        parts: Vec<StreamedMessagePart>,
        id: Option<String>,
        usage: Option<TokenUsage>,
    ) -> Self {
        Self {
            stream: None,
            buffer: String::new(),
            parts: parts.into(),
            id,
            usage,
            finished: true,
            tool_acc: Vec::new(),
            tools_flushed: true,
        }
    }

    /// Emit the index-accumulated tool calls as complete `ToolCall` parts.
    fn flush_tool_calls(&mut self) {
        for mut call in self.tool_acc.drain(..) {
            if call.function.name.is_empty() {
                continue; // padding/malformed slot with no name
            }
            if call.id.is_empty() {
                call.id = Uuid::new_v4().to_string();
            }
            self.parts.push_back(StreamedMessagePart::ToolCall(call));
        }
    }

    fn ingest_chunk(&mut self, value: &Value) -> Result<(), ChatProviderError> {
        if let Some(id) = value.get("id").and_then(|v| v.as_str()) {
            self.id = Some(id.to_string());
        }
        let usage_value = value.get("usage").or_else(|| {
            value
                .get("choices")
                .and_then(|v| v.as_array())
                .and_then(|choices| choices.first())
                .and_then(|choice| choice.get("usage"))
        });
        if let Some(usage) = usage_value {
            if let Some(parsed) = parse_usage(usage) {
                self.usage = Some(parsed);
            }
        }
        if let Some(choices) = value.get("choices").and_then(|v| v.as_array()) {
            for choice in choices {
                if let Some(fr) = choice
                    .get("finish_reason")
                    .filter(|v| !v.is_null())
                    .and_then(|v| v.as_str())
                {
                    self.finished = true;
                    tracing::debug!("openai_compatible stream finish_reason={fr}");
                } else if choice
                    .get("finish_reason")
                    .map(|v| !v.is_null())
                    .unwrap_or(false)
                {
                    self.finished = true;
                }
                if let Some(delta) = choice.get("delta") {
                    ingest_delta(delta, &mut self.parts);
                    if let Some(tool_calls) =
                        delta.get("tool_calls").and_then(|v| v.as_array())
                    {
                        for tc in tool_calls {
                            super::accumulate_tool_call_delta(&mut self.tool_acc, tc);
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

#[async_trait]
impl StreamedMessage for OpenAiCompatibleStreamedMessage {
    async fn next_part(&mut self) -> Result<Option<StreamedMessagePart>, ChatProviderError> {
        loop {
            if let Some(part) = self.parts.pop_front() {
                return Ok(Some(part));
            }
            // Parts drained. If the provider signalled completion, flush the
            // index-accumulated tool calls (once) then we're done.
            if self.finished {
                if !self.tools_flushed {
                    self.tools_flushed = true;
                    self.flush_tool_calls();
                    continue;
                }
                self.stream = None;
                return Ok(None);
            }
            let stream = match &mut self.stream {
                Some(stream) => stream,
                None => {
                    // Stream gone but never marked finished: the connection was
                    // cut early (slow responses dropped by the provider gateway).
                    // Surface as a retryable Connection error instead of silently
                    // returning a truncated response.
                    return Err(ChatProviderError::new(
                        ChatProviderErrorKind::Connection,
                        "stream ended before completion (no finish_reason or [DONE]); \
                         connection closed early"
                            .to_string(),
                    ));
                }
            };
            match stream.next().await {
                Some(Ok(bytes)) => {
                    let chunk = String::from_utf8_lossy(&bytes);
                    self.buffer.push_str(&chunk);
                    while let Some(pos) = self.buffer.find('\n') {
                        let line = self.buffer[..pos].trim().to_string();
                        self.buffer = self.buffer[pos + 1..].to_string();
                        if line.is_empty() {
                            continue;
                        }
                        if let Some(data) = line.strip_prefix("data: ") {
                            if data.trim() == "[DONE]" {
                                self.finished = true;
                                self.stream = None;
                                break;
                            }
                            let value: Value = serde_json::from_str(data).map_err(|err| {
                                ChatProviderError::new(
                                    ChatProviderErrorKind::Other,
                                    err.to_string(),
                                )
                            })?;
                            self.ingest_chunk(&value)?;
                        }
                    }
                }
                Some(Err(err)) => return Err(map_reqwest_error(err)),
                None => {
                    // Mark the stream gone; the top-of-loop `finished` check
                    // decides clean completion (flush) vs premature close (error).
                    self.stream = None;
                }
            }
        }
    }

    fn id(&self) -> Option<String> {
        self.id.clone()
    }

    fn usage(&self) -> Option<TokenUsage> {
        self.usage.clone()
    }
}

fn convert_message(message: &Message) -> Result<Value, ChatProviderError> {
    let mut reasoning_content = String::new();
    let mut content_parts = Vec::new();
    for part in &message.content {
        match part {
            ContentPart::Think(think) => {
                reasoning_content.push_str(&think.think);
            }
            _ => content_parts.push(part.clone()),
        }
    }

    let payload = serde_json::to_value(Message {
        role: message.role.clone(),
        content: content_parts,
        name: message.name.clone(),
        tool_calls: message.tool_calls.clone(),
        tool_call_id: message.tool_call_id.clone(),
        partial: message.partial,
    })
    .map_err(|err| ChatProviderError::new(ChatProviderErrorKind::Other, err.to_string()))?;

    let mut payload = strip_nulls(payload);
    if !reasoning_content.is_empty() {
        if let Value::Object(map) = &mut payload {
            map.insert(
                "reasoning_content".to_string(),
                Value::String(reasoning_content),
            );
        }
    }
    Ok(payload)
}

fn strip_nulls(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut cleaned = serde_json::Map::new();
            for (key, val) in map {
                if val.is_null() {
                    continue;
                }
                cleaned.insert(key, strip_nulls(val));
            }
            Value::Object(cleaned)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(strip_nulls).collect()),
        other => other,
    }
}

fn convert_tool(tool: &Tool) -> Value {
    if tool.name.starts_with('$') {
        json!({
            "type": "builtin_function",
            "function": {"name": tool.name},
        })
    } else {
        json!({
            "type": "function",
            "function": {
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters,
            }
        })
    }
}

fn parse_non_stream_response(
    value: &Value,
) -> Result<(Vec<StreamedMessagePart>, Option<String>, Option<TokenUsage>), ChatProviderError> {
    let message_id = value
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let usage = value.get("usage").and_then(parse_usage);

    let choices = value
        .get("choices")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            ChatProviderError::new(ChatProviderErrorKind::Other, "Missing choices in response")
        })?;
    if choices.is_empty() {
        return Err(ChatProviderError::new(
            ChatProviderErrorKind::EmptyResponse,
            "The API returned an empty response.",
        ));
    }
    let message = choices[0].get("message").ok_or_else(|| {
        ChatProviderError::new(ChatProviderErrorKind::Other, "Missing message in response")
    })?;

    let mut parts = Vec::new();
    if let Some(reasoning) = message
        .get("reasoning_content")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        parts.push(StreamedMessagePart::Content(ContentPart::Think(
            ThinkPart {
                kind: "think".to_string(),
                think: reasoning.to_string(),
                encrypted: None,
            },
        )));
    }
    if let Some(content) = message.get("content").and_then(|v| v.as_str()) {
        if !content.is_empty() {
            parts.push(StreamedMessagePart::Content(ContentPart::Text(
                TextPart::new(content),
            )));
        }
    }
    if let Some(tool_calls) = message.get("tool_calls").and_then(|v| v.as_array()) {
        for tool_call in tool_calls {
            if let Some(call) = parse_tool_call(tool_call) {
                parts.push(StreamedMessagePart::ToolCall(call));
            }
        }
    }

    Ok((parts, message_id, usage))
}

fn ingest_delta(delta: &Value, parts: &mut VecDeque<StreamedMessagePart>) {
    if let Some(reasoning) = delta
        .get("reasoning_content")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        parts.push_back(StreamedMessagePart::Content(ContentPart::Think(
            ThinkPart {
                kind: "think".to_string(),
                think: reasoning.to_string(),
                encrypted: None,
            },
        )));
    }
    if let Some(content) = delta
        .get("content")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        parts.push_back(StreamedMessagePart::Content(ContentPart::Text(
            TextPart::new(content),
        )));
    }
    // Tool-call deltas are NOT handled here — they are accumulated by `index`
    // in OpenAiCompatibleStreamedMessage::ingest_chunk and flushed as complete
    // ToolCall parts on completion (see accumulate_tool_call_delta).
}

fn parse_tool_call(tool_call: &Value) -> Option<ToolCall> {
    let function = tool_call.get("function")?;
    let name = function.get("name")?.as_str()?.to_string();
    let arguments = function
        .get("arguments")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let id = tool_call
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    Some(ToolCall {
        kind: "function".to_string(),
        id,
        function: ToolCallFunction { name, arguments },
        extras: None,
    })
}

fn parse_usage(value: &Value) -> Option<TokenUsage> {
    let prompt_tokens = value.get("prompt_tokens")?.as_i64()?;
    let completion_tokens = value.get("completion_tokens")?.as_i64()?;
    let mut cached = 0i64;
    if let Some(cached_tokens) = value.get("cached_tokens").and_then(|v| v.as_i64()) {
        cached = cached_tokens;
    } else if let Some(details) = value.get("prompt_tokens_details") {
        if let Some(cached_tokens) = details.get("cached_tokens").and_then(|v| v.as_i64()) {
            cached = cached_tokens;
        }
    }
    let input_other = if prompt_tokens >= cached {
        prompt_tokens - cached
    } else {
        0
    };
    Some(TokenUsage {
        input_other,
        output: completion_tokens,
        input_cache_read: cached,
        input_cache_creation: 0,
    })
}

fn map_reqwest_error(err: reqwest::Error) -> ChatProviderError {
    if err.is_timeout() {
        ChatProviderError::new(ChatProviderErrorKind::Timeout, err.to_string())
    } else if err.is_connect() {
        ChatProviderError::new(ChatProviderErrorKind::Connection, err.to_string())
    } else {
        ChatProviderError::new(ChatProviderErrorKind::Other, err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_accumulate_tool_call_delta_by_index() {
        let mut acc = Vec::new();
        crate::chat_provider::accumulate_tool_call_delta(
            &mut acc,
            &json!({"index": 0, "id": "call_1", "function": {"name": "test_func", "arguments": "{\"a\":"}}),
        );
        crate::chat_provider::accumulate_tool_call_delta(
            &mut acc,
            &json!({"index": 0, "function": {"arguments": "1}"}}),
        );
        assert_eq!(acc.len(), 1);
        assert_eq!(acc[0].id, "call_1");
        assert_eq!(acc[0].function.name, "test_func");
        assert_eq!(acc[0].function.arguments.as_deref(), Some("{\"a\":1}"));
    }

    #[test]
    fn test_accumulate_tool_call_delta_late_name() {
        // The bug this fix addresses: the name does not arrive in the first
        // fragment. Adjacency-based assembly dropped these; index-keying keeps them.
        let mut acc = Vec::new();
        crate::chat_provider::accumulate_tool_call_delta(
            &mut acc,
            &json!({"index": 0, "function": {"arguments": "{}"}}),
        );
        crate::chat_provider::accumulate_tool_call_delta(
            &mut acc,
            &json!({"index": 0, "id": "call_2", "function": {"name": "late"}}),
        );
        assert_eq!(acc.len(), 1);
        assert_eq!(acc[0].function.name, "late");
        assert_eq!(acc[0].function.arguments.as_deref(), Some("{}"));
    }

    #[test]
    fn test_parse_usage() {
        let usage = json!({
            "prompt_tokens": 100,
            "completion_tokens": 50
        });
        let result = parse_usage(&usage);
        assert!(result.is_some());
        let token_usage = result.unwrap();
        assert_eq!(token_usage.input(), 100);
        assert_eq!(token_usage.output, 50);
        assert_eq!(token_usage.total(), 150);
    }
}
