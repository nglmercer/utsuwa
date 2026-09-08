//! Canonical model-provider types (plan Phase 13, Task 10).
//!
//! All providers speak these types. Provider-specific wire formats
//! (OpenAI, Anthropic, Ollama-native, …) are converted at the adapter
//! boundary and must never leak into `agent-core`.

use serde::{Deserialize, Serialize};
use std::pin::Pin;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ModelRole {
    System,
    User,
    Assistant,
    Tool,
}

/// A tool call requested by the assistant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// JSON-encoded arguments (assembled from streaming deltas by adapters).
    pub arguments: String,
}

/// Result of executing a tool call, fed back to the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub tool_call_id: String,
    pub content: String,
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelMessage {
    pub role: ModelRole,
    pub content: String,
    /// Provider-neutral structured content for multimodal user messages.
    /// Plain text keeps this as `None`; adapters use this value when it is
    /// present instead of flattening images or other content parts.
    pub content_value: Option<serde_json::Value>,
    pub tool_calls: Vec<ToolCall>,
    pub tool_result: Option<ToolResult>,
}

impl ModelMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: ModelRole::System,
            content: content.into(),
            content_value: None,
            tool_calls: vec![],
            tool_result: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: ModelRole::User,
            content: content.into(),
            content_value: None,
            tool_calls: vec![],
            tool_result: None,
        }
    }

    pub fn assistant(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: ModelRole::Assistant,
            content: content.into(),
            content_value: None,
            tool_calls,
            tool_result: None,
        }
    }

    pub fn tool_result(result: ToolResult) -> Self {
        let content = result.content.clone();
        Self {
            role: ModelRole::Tool,
            content,
            content_value: None,
            tool_calls: vec![],
            tool_result: Some(result),
        }
    }

    /// Construct a message from the JSON content shape used by the frontend
    /// and OpenAI-compatible providers. Arrays are retained verbatim so
    /// image parts survive the native runtime boundary.
    pub fn from_wire(role: ModelRole, content: serde_json::Value) -> Self {
        let text = match &content {
            serde_json::Value::String(text) => text.clone(),
            serde_json::Value::Array(parts) => parts
                .iter()
                .filter_map(|part| part.get("text").and_then(|text| text.as_str()))
                .collect::<Vec<_>>()
                .join(""),
            other => other.to_string(),
        };
        Self {
            role,
            content: text,
            content_value: Some(content),
            tool_calls: vec![],
            tool_result: None,
        }
    }
}

/// A tool the model may call, in provider-neutral form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

impl ToolDefinition {
    pub fn from_metadata(meta: &tool_core::ToolMetadata) -> Self {
        Self {
            name: meta.id.0.clone(),
            description: meta.description.clone(),
            input_schema: meta.input_schema.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub messages: Vec<ModelMessage>,
    pub tools: Vec<ToolDefinition>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
}

impl ModelRequest {
    pub fn new(messages: Vec<ModelMessage>) -> Self {
        Self {
            messages,
            tools: vec![],
            max_tokens: None,
            temperature: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelResponse {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: FinishReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Length,
    Other,
}

/// Streaming events. Adapters assemble provider deltas into these;
/// `agent-core` only ever sees this enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelStreamEvent {
    TextDelta(String),
    /// A complete tool call assembled from provider deltas.
    ToolCall(ToolCall),
    Done {
        finish_reason: FinishReason,
    },
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ModelError {
    #[error("transport error: {0}")]
    Transport(String),
    #[error("provider error {status}: {message}")]
    Provider { status: u16, message: String },
    #[error("invalid provider response: {0}")]
    InvalidResponse(String),
    #[error("cancelled")]
    Cancelled,
}

pub type ModelStream =
    Pin<Box<dyn futures_core::Stream<Item = Result<ModelStreamEvent, ModelError>> + Send>>;

#[async_trait::async_trait]
pub trait ModelProvider: Send + Sync {
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream, ModelError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_definition_converts_from_registry_metadata() {
        let meta = tool_core::ToolMetadata {
            id: capability_core::ToolId::new("system.echo"),
            description: "echo".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
            effects: vec![tool_core::ToolEffect::ReadOnly],
        };
        let def = ToolDefinition::from_metadata(&meta);
        assert_eq!(def.name, "system.echo");
        assert_eq!(def.description, "echo");
    }

    #[test]
    fn message_constructors_carry_roles() {
        assert_eq!(ModelMessage::system("s").role, ModelRole::System);
        assert_eq!(ModelMessage::user("u").role, ModelRole::User);
        let m = ModelMessage::assistant(
            "a",
            vec![ToolCall {
                id: "1".to_string(),
                name: "system.echo".to_string(),
                arguments: "{}".to_string(),
            }],
        );
        assert_eq!(m.tool_calls.len(), 1);
    }
}
