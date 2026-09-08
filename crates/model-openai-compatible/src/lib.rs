//! OpenAI-compatible chat provider (plan Task 10).
//!
//! One client covers OpenAI, Ollama (`/v1`), LM Studio, and any other
//! OpenAI-compatible endpoint. Speaks `/chat/completions` with SSE
//! streaming; converts everything to `model-core` types at this boundary.

use futures_util::StreamExt;
use model_core::{
    FinishReason, ModelError, ModelMessage, ModelProvider, ModelRequest, ModelRole, ModelStream,
    ModelStreamEvent, ToolCall,
};
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};

const CHAT_COMPLETIONS_PATH: &str = "/chat/completions";

#[derive(Debug, Clone)]
pub struct OpenAICompatibleClient {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    model: String,
}

impl OpenAICompatibleClient {
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key,
            model: model.into(),
        }
    }

    /// Ollama's OpenAI-compatible endpoint (no key needed).
    pub fn ollama(model: impl Into<String>) -> Self {
        Self::new("http://localhost:11434/v1", None, model)
    }

    /// LM Studio's OpenAI-compatible endpoint (no key needed).
    pub fn lm_studio(model: impl Into<String>) -> Self {
        Self::new("http://localhost:1234/v1", None, model)
    }

    fn url(&self) -> String {
        format!("{}{}", self.base_url, CHAT_COMPLETIONS_PATH)
    }
}

#[async_trait::async_trait]
impl ModelProvider for OpenAICompatibleClient {
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream, ModelError> {
        let body = request_body(&self.model, &request);
        let mut req = self.http.post(self.url()).json(&body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let response = req
            .send()
            .await
            .map_err(|e| ModelError::Transport(e.to_string()))?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let message = response
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable error body>".to_string());
            return Err(ModelError::Provider { status, message });
        }
        let byte_stream = response.bytes_stream();
        let state = StreamState {
            buffer: String::new(),
            assembler: SseAssembler::default(),
            bytes: Box::pin(byte_stream),
            finished: false,
        };
        let stream = futures_util::stream::unfold(state, |mut state| async move {
            match next_event(&mut state).await {
                Ok(Some(event)) => Some((Ok(event), state)),
                Ok(None) => None,
                Err(err) => Some((Err(err), state)),
            }
        });
        Ok(Box::pin(stream))
    }
}

/// Anthropic Messages adapter. It lives beside the OpenAI-compatible adapter
/// because both produce the same provider-neutral stream consumed by the
/// agent loop.
#[derive(Debug, Clone)]
pub struct AnthropicClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
}

impl AnthropicClient {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            model: model.into(),
        }
    }

    fn url(&self) -> String {
        format!("{}/messages", self.base_url)
    }
}

#[async_trait::async_trait]
impl ModelProvider for AnthropicClient {
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream, ModelError> {
        let body = anthropic_request_body(&self.model, &request);
        let response = self
            .http
            .post(self.url())
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await
            .map_err(|e| ModelError::Transport(e.to_string()))?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let message = response
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable error body>".to_string());
            return Err(ModelError::Provider { status, message });
        }
        let state = AnthropicStreamState {
            buffer: String::new(),
            assembler: AnthropicAssembler::default(),
            bytes: Box::pin(response.bytes_stream()),
            finished: false,
        };
        let stream = futures_util::stream::unfold(state, |mut state| async move {
            match next_anthropic_event(&mut state).await {
                Ok(Some(event)) => Some((Ok(event), state)),
                Ok(None) => None,
                Err(err) => Some((Err(err), state)),
            }
        });
        Ok(Box::pin(stream))
    }
}

struct AnthropicStreamState {
    buffer: String,
    assembler: AnthropicAssembler,
    bytes: std::pin::Pin<
        Box<dyn futures_core::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send>,
    >,
    finished: bool,
}

async fn next_anthropic_event(
    state: &mut AnthropicStreamState,
) -> Result<Option<ModelStreamEvent>, ModelError> {
    loop {
        if let Some(event) = state.assembler.pop() {
            return Ok(Some(event));
        }
        if state.finished {
            return Ok(None);
        }
        match state.bytes.next().await {
            Some(Ok(chunk)) => {
                state.buffer.push_str(
                    std::str::from_utf8(&chunk)
                        .map_err(|e| ModelError::InvalidResponse(e.to_string()))?,
                );
                feed_anthropic_lines(state);
            }
            Some(Err(e)) => return Err(ModelError::Transport(e.to_string())),
            None => {
                state.finished = true;
                state.assembler.finish();
            }
        }
    }
}

fn feed_anthropic_lines(state: &mut AnthropicStreamState) {
    while let Some(pos) = state.buffer.find('\n') {
        let line = state.buffer[..pos].trim_end_matches('\r').to_string();
        state.buffer.drain(..=pos);
        state.assembler.feed_line(&line);
    }
}

#[derive(Debug, Default)]
struct AnthropicAssembler {
    queued: VecDeque<ModelStreamEvent>,
    tools: BTreeMap<u32, AnthropicToolFragment>,
    finish_reason: Option<FinishReason>,
    done: bool,
}

#[derive(Debug, Default)]
struct AnthropicToolFragment {
    id: String,
    name: String,
    input_json: String,
}

impl AnthropicAssembler {
    fn pop(&mut self) -> Option<ModelStreamEvent> {
        self.queued.pop_front()
    }

    fn feed_line(&mut self, line: &str) {
        if self.done {
            return;
        }
        let Some(data) = line.strip_prefix("data:") else {
            return;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            if data == "[DONE]" {
                self.finish();
            }
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            return;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("content_block_start") => {
                let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;
                let Some(block) = value.get("content_block") else {
                    return;
                };
                if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                    let fragment = self.tools.entry(index).or_default();
                    fragment.id = block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    fragment.name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                }
            }
            Some("content_block_delta") => {
                let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;
                let Some(delta) = value.get("delta") else {
                    return;
                };
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(text) = delta.get("text").and_then(Value::as_str) {
                            if !text.is_empty() {
                                self.queued
                                    .push_back(ModelStreamEvent::TextDelta(text.to_string()));
                            }
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(partial) = delta.get("partial_json").and_then(Value::as_str) {
                            self.tools
                                .entry(index)
                                .or_default()
                                .input_json
                                .push_str(partial);
                        }
                    }
                    _ => {}
                }
            }
            Some("message_delta") => {
                if let Some(reason) = value
                    .get("delta")
                    .and_then(|delta| delta.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    self.finish_reason = Some(map_anthropic_finish_reason(reason));
                }
            }
            Some("message_stop") => self.finish(),
            _ => {}
        }
    }

    fn finish(&mut self) {
        if self.done {
            return;
        }
        self.done = true;
        for fragment in std::mem::take(&mut self.tools).into_values() {
            if !fragment.name.is_empty() {
                self.queued.push_back(ModelStreamEvent::ToolCall(ToolCall {
                    id: fragment.id,
                    name: fragment.name,
                    arguments: if fragment.input_json.is_empty() {
                        "{}".to_string()
                    } else {
                        fragment.input_json
                    },
                }));
            }
        }
        self.queued.push_back(ModelStreamEvent::Done {
            finish_reason: self.finish_reason.unwrap_or(FinishReason::Stop),
        });
    }
}

fn map_anthropic_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "end_turn" | "stop_sequence" => FinishReason::Stop,
        "tool_use" => FinishReason::ToolCalls,
        "max_tokens" => FinishReason::Length,
        _ => FinishReason::Other,
    }
}

fn anthropic_request_body(model: &str, request: &ModelRequest) -> Value {
    let system = request
        .messages
        .iter()
        .filter(|message| message.role == ModelRole::System)
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut body = serde_json::json!({
        "model": model,
        "stream": true,
        "max_tokens": request.max_tokens.unwrap_or(1024),
        "messages": anthropic_messages(&request.messages),
        "tools": request.tools.iter().map(|tool| serde_json::json!({
            "name": tool.name,
            "description": tool.description,
            "input_schema": tool.input_schema,
        })).collect::<Vec<_>>(),
    });
    if !system.is_empty() {
        body["system"] = Value::String(system);
    }
    if let Some(temperature) = request.temperature {
        body["temperature"] = serde_json::json!(temperature);
    }
    body
}

fn anthropic_messages(messages: &[ModelMessage]) -> Vec<Value> {
    let mut output = Vec::new();
    for message in messages {
        match message.role {
            ModelRole::System => {}
            ModelRole::User => append_anthropic_message(
                &mut output,
                "user",
                anthropic_content(message.content_value.as_ref(), &message.content),
            ),
            ModelRole::Assistant => append_anthropic_message(
                &mut output,
                "assistant",
                anthropic_assistant_content(message),
            ),
            ModelRole::Tool => {
                if let Some(result) = &message.tool_result {
                    let mut block = serde_json::json!({
                        "type": "tool_result",
                        "tool_use_id": result.tool_call_id,
                        "content": result.content,
                    });
                    if result.is_error {
                        block["is_error"] = Value::Bool(true);
                    }
                    append_anthropic_message(&mut output, "user", Value::Array(vec![block]));
                }
            }
        }
    }
    output
}

fn anthropic_assistant_content(message: &ModelMessage) -> Value {
    if message.tool_calls.is_empty() {
        return anthropic_content(message.content_value.as_ref(), &message.content);
    }
    let mut blocks = Vec::new();
    if !message.content.is_empty() {
        blocks.push(serde_json::json!({
            "type": "text",
            "text": message.content,
        }));
    }
    for call in &message.tool_calls {
        let input = serde_json::from_str::<Value>(&call.arguments)
            .unwrap_or_else(|_| serde_json::json!({}));
        blocks.push(serde_json::json!({
            "type": "tool_use",
            "id": call.id,
            "name": call.name,
            "input": input,
        }));
    }
    Value::Array(blocks)
}

fn anthropic_content(value: Option<&Value>, fallback: &str) -> Value {
    let Some(value) = value else {
        return Value::String(fallback.to_string());
    };
    match value {
        Value::String(text) => Value::String(text.clone()),
        Value::Array(parts) => {
            let blocks = parts
                .iter()
                .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                    Some("text") => part
                        .get("text")
                        .and_then(Value::as_str)
                        .map(|text| serde_json::json!({"type":"text","text":text})),
                    Some("image_url") => {
                        let url = part
                            .get("image_url")
                            .and_then(|image| image.get("url"))
                            .and_then(Value::as_str)?;
                        let data = url.strip_prefix("data:")?;
                        let (media_type, data) = data.split_once(";base64,")?;
                        Some(serde_json::json!({
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": media_type,
                                "data": data,
                            },
                        }))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            if blocks.is_empty() {
                Value::String(fallback.to_string())
            } else {
                Value::Array(blocks)
            }
        }
        other => other.clone(),
    }
}

fn append_anthropic_message(messages: &mut Vec<Value>, role: &str, content: Value) {
    if let Some(last) = messages.last_mut() {
        if last.get("role").and_then(Value::as_str) == Some(role) {
            let existing = last
                .get_mut("content")
                .map(Value::take)
                .unwrap_or(Value::Null);
            let mut blocks = anthropic_blocks(existing);
            blocks.extend(anthropic_blocks(content));
            last["content"] = Value::Array(blocks);
            return;
        }
    }
    messages.push(serde_json::json!({ "role": role, "content": content }));
}

fn anthropic_blocks(content: Value) -> Vec<Value> {
    match content {
        Value::Array(blocks) => blocks,
        Value::String(text) => vec![serde_json::json!({"type":"text","text":text})],
        other => vec![other],
    }
}

struct StreamState {
    buffer: String,
    assembler: SseAssembler,
    bytes: std::pin::Pin<
        Box<dyn futures_core::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send>,
    >,
    finished: bool,
}

/// Pull the next stream event, buffering SSE lines across chunk splits.
async fn next_event(state: &mut StreamState) -> Result<Option<ModelStreamEvent>, ModelError> {
    loop {
        if let Some(event) = state.assembler.pop() {
            return Ok(Some(event));
        }
        if state.finished {
            return Ok(None);
        }
        match state.bytes.next().await {
            Some(Ok(chunk)) => {
                state.buffer.push_str(
                    std::str::from_utf8(&chunk)
                        .map_err(|e| ModelError::InvalidResponse(e.to_string()))?,
                );
                feed_lines(state);
            }
            Some(Err(e)) => return Err(ModelError::Transport(e.to_string())),
            None => {
                state.finished = true;
                state.assembler.finish();
            }
        }
    }
}

/// Move complete lines out of the buffer into the assembler.
fn feed_lines(state: &mut StreamState) {
    while let Some(pos) = state.buffer.find('\n') {
        let line = state.buffer[..pos].trim_end_matches('\r').to_string();
        state.buffer.drain(..=pos);
        state.assembler.feed_line(&line);
    }
}

/// Incremental SSE → `ModelStreamEvent` assembly. Text deltas stream
/// through; tool-call fragments accumulate per index and are emitted whole
/// when the turn finishes (`[DONE]` or `finish_reason`).
#[derive(Debug, Default)]
struct SseAssembler {
    queued: std::collections::VecDeque<ModelStreamEvent>,
    tool_fragments: BTreeMap<u32, ToolFragment>,
    finish_reason: Option<FinishReason>,
    done: bool,
}

#[derive(Debug, Default)]
struct ToolFragment {
    id: String,
    name: String,
    arguments: String,
}

impl SseAssembler {
    fn pop(&mut self) -> Option<ModelStreamEvent> {
        self.queued.pop_front()
    }

    fn feed_line(&mut self, line: &str) {
        if self.done {
            return;
        }
        let Some(data) = line.strip_prefix("data:") else {
            return;
        };
        let data = data.trim();
        if data.is_empty() {
            return;
        }
        if data == "[DONE]" {
            self.finish();
            return;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
            return;
        };
        let Some(choice) = value.get("choices").and_then(|c| c.get(0)) else {
            return;
        };
        if let Some(reason) = choice.get("finish_reason").and_then(|r| r.as_str()) {
            self.finish_reason = Some(map_finish_reason(reason));
        }
        let delta = choice.get("delta").or_else(|| choice.get("message"));
        let Some(delta) = delta else {
            if self.finish_reason.is_some() {
                self.finish();
            }
            return;
        };
        if let Some(text) = delta.get("content").and_then(|c| c.as_str()) {
            if !text.is_empty() {
                self.queued
                    .push_back(ModelStreamEvent::TextDelta(text.to_string()));
            }
        }
        if let Some(calls) = delta.get("tool_calls").and_then(|c| c.as_array()) {
            for call in calls {
                let index = call.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
                let fragment = self.tool_fragments.entry(index).or_default();
                if let Some(id) = call.get("id").and_then(|i| i.as_str()) {
                    fragment.id = id.to_string();
                }
                if let Some(function) = call.get("function") {
                    if let Some(name) = function.get("name").and_then(|n| n.as_str()) {
                        fragment.name = name.to_string();
                    }
                    if let Some(args) = function.get("arguments").and_then(|a| a.as_str()) {
                        fragment.arguments.push_str(args);
                    }
                }
            }
        }
        if self.finish_reason.is_some() {
            self.finish();
        }
    }

    fn finish(&mut self) {
        if self.done {
            return;
        }
        self.done = true;
        for fragment in std::mem::take(&mut self.tool_fragments).into_values() {
            if !fragment.name.is_empty() {
                self.queued.push_back(ModelStreamEvent::ToolCall(ToolCall {
                    id: fragment.id,
                    name: fragment.name,
                    arguments: fragment.arguments,
                }));
            }
        }
        self.queued.push_back(ModelStreamEvent::Done {
            finish_reason: self.finish_reason.unwrap_or(FinishReason::Stop),
        });
    }
}

fn map_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "stop" => FinishReason::Stop,
        "tool_calls" => FinishReason::ToolCalls,
        "length" => FinishReason::Length,
        _ => FinishReason::Other,
    }
}

fn request_body(model: &str, request: &ModelRequest) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "stream": true,
        "messages": request.messages.iter().map(wire_message).collect::<Vec<_>>(),
        "tools": request.tools.iter().map(|t| serde_json::json!({
            "type": "function",
            "function": {
                "name": t.name,
                "description": t.description,
                "parameters": t.input_schema,
            }
        })).collect::<Vec<_>>(),
        "max_tokens": request.max_tokens,
        "temperature": request.temperature,
    })
}

fn wire_message(message: &ModelMessage) -> serde_json::Value {
    let role = match message.role {
        ModelRole::System => "system",
        ModelRole::User => "user",
        ModelRole::Assistant => "assistant",
        ModelRole::Tool => "tool",
    };
    let content = message
        .content_value
        .clone()
        .unwrap_or_else(|| serde_json::Value::String(message.content.clone()));
    let mut value = serde_json::json!({ "role": role, "content": content });
    if !message.tool_calls.is_empty() {
        value["tool_calls"] = message
            .tool_calls
            .iter()
            .map(|c| {
                serde_json::json!({
                    "id": c.id,
                    "type": "function",
                    "function": { "name": c.name, "arguments": c.arguments },
                })
            })
            .collect();
    }
    if let Some(result) = &message.tool_result {
        value["tool_call_id"] = result.tool_call_id.clone().into();
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn assembler_streams_text_and_whole_tool_calls() {
        let mut asm = SseAssembler::default();
        asm.feed_line(r#"data: {"choices":[{"delta":{"content":"Hello"}}]}"#);
        asm.feed_line(r#"data: {"choices":[{"delta":{"content":" world"}}]}"#);
        // Tool-call arguments split across lines, like real chunked SSE.
        asm.feed_line(
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"system.echo","arguments":"{\"te"}}]}}]}"#,
        );
        asm.feed_line(
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"xt\":\"hi\"}"}}]},"finish_reason":"tool_calls"}]}"#,
        );
        assert_eq!(
            asm.pop(),
            Some(ModelStreamEvent::TextDelta("Hello".to_string()))
        );
        assert_eq!(
            asm.pop(),
            Some(ModelStreamEvent::TextDelta(" world".to_string()))
        );
        assert_eq!(
            asm.pop(),
            Some(ModelStreamEvent::ToolCall(ToolCall {
                id: "c1".to_string(),
                name: "system.echo".to_string(),
                arguments: "{\"text\":\"hi\"}".to_string(),
            }))
        );
        assert_eq!(
            asm.pop(),
            Some(ModelStreamEvent::Done {
                finish_reason: FinishReason::ToolCalls
            })
        );
        assert_eq!(asm.pop(), None);
    }

    #[test]
    fn openai_request_uses_standard_function_tool_shape() {
        let mut request = ModelRequest::new(vec![ModelMessage::user("inspect")]);
        request.tools.push(model_core::ToolDefinition {
            name: "filesystem.read".to_string(),
            description: "Read a file".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
        });

        let tool = &request_body("test-model", &request)["tools"][0];
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["function"]["name"], "filesystem.read");
        assert_eq!(tool["function"]["description"], "Read a file");
        assert_eq!(tool["function"]["parameters"]["required"][0], "path");
    }

    #[test]
    fn assembler_preserves_multiple_tool_calls_and_split_arguments() {
        let mut asm = SseAssembler::default();
        let line = |delta: serde_json::Value, finish_reason: Option<&str>| {
            let mut choice = serde_json::json!({"delta": delta});
            if let Some(reason) = finish_reason {
                choice["finish_reason"] = serde_json::Value::String(reason.to_string());
            }
            format!("data: {}", serde_json::json!({"choices": [choice]}))
        };

        asm.feed_line(&line(
            serde_json::json!({
                "tool_calls": [{
                    "index": 0,
                    "id": "read-1",
                    "function": {"name": "filesystem.read", "arguments": "{\"path\":\""}
                }]
            }),
            None,
        ));
        asm.feed_line(&line(
            serde_json::json!({
                "tool_calls": [{
                    "index": 1,
                    "id": "status-1",
                    "function": {"name": "process.status", "arguments": "{\"handle\":\""}
                }]
            }),
            None,
        ));
        asm.feed_line(&line(
            serde_json::json!({
                "tool_calls": [
                    {"index": 0, "function": {"arguments": "notes.txt\"}"}},
                    {"index": 1, "function": {"arguments": "proc-1\"}"}}
                ]
            }),
            Some("tool_calls"),
        ));

        assert_eq!(
            asm.pop(),
            Some(ModelStreamEvent::ToolCall(ToolCall {
                id: "read-1".to_string(),
                name: "filesystem.read".to_string(),
                arguments: r#"{"path":"notes.txt"}"#.to_string(),
            }))
        );
        assert_eq!(
            asm.pop(),
            Some(ModelStreamEvent::ToolCall(ToolCall {
                id: "status-1".to_string(),
                name: "process.status".to_string(),
                arguments: r#"{"handle":"proc-1"}"#.to_string(),
            }))
        );
        assert_eq!(
            asm.pop(),
            Some(ModelStreamEvent::Done {
                finish_reason: FinishReason::ToolCalls
            })
        );
    }

    #[test]
    fn done_marker_without_finish_reason_still_terminates() {
        let mut asm = SseAssembler::default();
        asm.feed_line(r#"data: {"choices":[{"delta":{"content":"x"}}]}"#);
        asm.feed_line("data: [DONE]");
        asm.feed_line(r#"data: {"choices":[{"delta":{"content":"late"}}]}"#);
        assert_eq!(
            asm.pop(),
            Some(ModelStreamEvent::TextDelta("x".to_string()))
        );
        assert_eq!(
            asm.pop(),
            Some(ModelStreamEvent::Done {
                finish_reason: FinishReason::Stop
            })
        );
        assert_eq!(asm.pop(), None);
    }

    #[test]
    fn wire_message_preserves_tool_result_content_and_multimodal_content() {
        let tool = ModelMessage::tool_result(model_core::ToolResult {
            tool_call_id: "call-1".to_string(),
            content: r#"{"ok":true}"#.to_string(),
            is_error: false,
        });
        let wire = wire_message(&tool);
        assert_eq!(wire["role"], "tool");
        assert_eq!(wire["tool_call_id"], "call-1");
        assert_eq!(wire["content"], r#"{"ok":true}"#);

        let multimodal = ModelMessage::from_wire(
            ModelRole::User,
            serde_json::json!([
                {"type": "text", "text": "look"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,abc"}}
            ]),
        );
        assert_eq!(wire_message(&multimodal)["content"][1]["type"], "image_url");
    }

    #[test]
    fn anthropic_request_preserves_tools_and_tool_results() {
        let mut request = ModelRequest::new(vec![
            ModelMessage::system("system prompt"),
            ModelMessage::user("run it"),
            ModelMessage::assistant(
                "",
                vec![ToolCall {
                    id: "call-1".to_string(),
                    name: "process.spawn".to_string(),
                    arguments: r#"{"executable":"echo"}"#.to_string(),
                }],
            ),
            ModelMessage::tool_result(model_core::ToolResult {
                tool_call_id: "call-1".to_string(),
                content: r#"{"ok":true}"#.to_string(),
                is_error: false,
            }),
        ]);
        request.tools.push(model_core::ToolDefinition {
            name: "process.spawn".to_string(),
            description: "run one command".to_string(),
            input_schema: serde_json::json!({"type":"object"}),
        });
        let body = anthropic_request_body("claude-test", &request);
        assert_eq!(body["system"], "system prompt");
        assert_eq!(body["tools"][0]["name"], "process.spawn");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(body["messages"][1]["content"][0]["type"], "tool_use");
        assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "call-1");
    }

    #[test]
    fn anthropic_stream_assembles_text_and_tool_input() {
        let mut assembler = AnthropicAssembler::default();
        assembler.feed_line(
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"c1","name":"system.echo","input":{}}}"#,
        );
        let input_delta = |partial_json: &str| {
            format!(
                "data: {}",
                serde_json::json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "input_json_delta", "partial_json": partial_json}
                })
            )
        };
        assembler.feed_line(&input_delta(r##"{"text":""##));
        assembler.feed_line(&input_delta(r#"hi"}"#));
        assembler.feed_line(r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#);
        assembler.feed_line(r#"data: {"type":"message_stop"}"#);
        assert_eq!(
            assembler.pop(),
            Some(ModelStreamEvent::ToolCall(ToolCall {
                id: "c1".to_string(),
                name: "system.echo".to_string(),
                arguments: r#"{"text":"hi"}"#.to_string(),
            }))
        );
        assert_eq!(
            assembler.pop(),
            Some(ModelStreamEvent::Done {
                finish_reason: FinishReason::ToolCalls
            })
        );
    }

    /// End-to-end against a mock OpenAI-compatible server: verifies the
    /// request shape (path, model, stream, tools) and chunked SSE parsing.
    #[tokio::test]
    async fn streams_against_mock_server() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = socket.read(&mut buf).await.unwrap();
            let head = String::from_utf8_lossy(&buf[..n]).to_string();
            let body_start = head.find("\r\n\r\n").map(|i| i + 4).unwrap_or(head.len());
            let len: usize = head
                .lines()
                .find_map(|l| {
                    l.strip_prefix("Content-Length:")
                        .or_else(|| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse().ok())
                })
                .unwrap_or(0);
            let mut body = head[body_start..].as_bytes().to_vec();
            while body.len() < len {
                let n = socket.read(&mut buf).await.unwrap();
                body.extend_from_slice(&buf[..n]);
            }
            let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(request["model"], "test-model");
            assert_eq!(request["stream"], true);
            assert_eq!(request["tools"][0]["function"]["name"], "system.echo");
            // Split mid-JSON to prove chunk-boundary buffering works.
            let chunks: &[&[u8]] = &[
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"Hel",
                b"lo\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            ];
            for chunk in chunks {
                socket.write_all(chunk).await.unwrap();
            }
        });

        let client = OpenAICompatibleClient::new(format!("http://{addr}"), None, "test-model");
        let mut request = ModelRequest::new(vec![ModelMessage::user("hi")]);
        request.tools.push(model_core::ToolDefinition {
            name: "system.echo".to_string(),
            description: "echo".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
        });
        let mut stream = client.stream(request).await.unwrap();
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event.unwrap());
        }
        assert_eq!(
            events,
            vec![
                ModelStreamEvent::TextDelta("Hello".to_string()),
                ModelStreamEvent::Done {
                    finish_reason: FinishReason::Stop
                },
            ]
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn provider_errors_are_typed() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = socket.read(&mut buf).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 25\r\n\r\n{\"error\":\"bad key\"}")
                .await
                .unwrap();
        });
        let client = OpenAICompatibleClient::new(
            format!("http://{addr}"),
            Some("wrong".to_string()),
            "test-model",
        );
        let err = match client
            .stream(ModelRequest::new(vec![ModelMessage::user("hi")]))
            .await
        {
            Ok(_) => panic!("expected provider error"),
            Err(err) => err,
        };
        assert!(
            matches!(err, ModelError::Provider { status: 401, .. }),
            "unexpected: {err:?}"
        );
        server.await.unwrap();
    }
}
