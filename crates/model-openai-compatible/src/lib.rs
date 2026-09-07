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
use std::collections::BTreeMap;

const CHAT_COMPLETIONS_PATH: &str = "/chat/completions";

#[derive(Debug, Clone)]
pub struct OpenAICompatibleClient {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    model: String,
}

impl OpenAICompatibleClient {
    pub fn new(base_url: impl Into<String>, api_key: Option<String>, model: impl Into<String>) -> Self {
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
                self.queued.push_back(ModelStreamEvent::TextDelta(text.to_string()));
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
    let mut value = serde_json::json!({ "role": role, "content": message.content });
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
        assert_eq!(asm.pop(), Some(ModelStreamEvent::TextDelta("Hello".to_string())));
        assert_eq!(asm.pop(), Some(ModelStreamEvent::TextDelta(" world".to_string())));
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
    fn done_marker_without_finish_reason_still_terminates() {
        let mut asm = SseAssembler::default();
        asm.feed_line(r#"data: {"choices":[{"delta":{"content":"x"}}]}"#);
        asm.feed_line("data: [DONE]");
        asm.feed_line(r#"data: {"choices":[{"delta":{"content":"late"}}]}"#);
        assert_eq!(asm.pop(), Some(ModelStreamEvent::TextDelta("x".to_string())));
        assert_eq!(
            asm.pop(),
            Some(ModelStreamEvent::Done {
                finish_reason: FinishReason::Stop
            })
        );
        assert_eq!(asm.pop(), None);
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
