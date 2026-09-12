//! Canonical model-provider types (plan Phase 13, Task 10).
//!
//! All providers speak these types. Provider-specific wire formats
//! (OpenAI, Anthropic, Ollama-native, …) are converted at the adapter
//! boundary and must never leak into `agent-core`.

use serde::{Deserialize, Serialize};
use std::pin::Pin;
use std::sync::Arc;

pub use artifact_core::ArtifactRef;

/// Provider-neutral multimodal content. Artifact bytes are resolved only by
/// a provider adapter immediately before serialization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ModelContentPart {
    Text(String),
    Image {
        artifact: ArtifactRef,
        detail: Option<ImageDetail>,
    },
    Json(serde_json::Value),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageDetail {
    Low,
    High,
    Auto,
}

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
    /// Backwards-compatible textual/JSON representation used by providers
    /// that only support string tool results and by existing host code.
    pub content: String,
    /// Typed content parts. An ordinary string result uses one `Text` part;
    /// an image result carries an artifact reference instead of base64 data.
    #[serde(default)]
    pub parts: Vec<ModelContentPart>,
    pub is_error: bool,
}

impl ToolResult {
    pub fn text(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        let content = content.into();
        Self {
            tool_call_id: tool_call_id.into(),
            parts: vec![ModelContentPart::Text(content.clone())],
            content,
            is_error: false,
        }
    }

    pub fn error(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        let mut result = Self::text(tool_call_id, content);
        result.is_error = true;
        result
    }

    pub fn with_parts(mut self, parts: Vec<ModelContentPart>) -> Self {
        self.parts = parts;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelMessage {
    pub role: ModelRole,
    pub content: String,
    /// Provider-neutral structured content for multimodal user messages.
    /// Plain text keeps this as `None`; adapters use this value when it is
    /// present instead of flattening images or other content parts.
    pub content_value: Option<serde_json::Value>,
    /// Typed content supplied by native tools or a frontend that already
    /// speaks the provider-neutral model contract. `content_value` remains
    /// for wire-compatible callers during migration.
    pub content_parts: Vec<ModelContentPart>,
    pub tool_calls: Vec<ToolCall>,
    pub tool_result: Option<ToolResult>,
}

impl ModelMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: ModelRole::System,
            content: content.into(),
            content_value: None,
            content_parts: vec![],
            tool_calls: vec![],
            tool_result: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: ModelRole::User,
            content: content.into(),
            content_value: None,
            content_parts: vec![],
            tool_calls: vec![],
            tool_result: None,
        }
    }

    pub fn assistant(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: ModelRole::Assistant,
            content: content.into(),
            content_value: None,
            content_parts: vec![],
            tool_calls,
            tool_result: None,
        }
    }

    pub fn tool_result(result: ToolResult) -> Self {
        let content = result.content.clone();
        let content_parts = result.parts.clone();
        Self {
            role: ModelRole::Tool,
            content,
            content_value: None,
            content_parts,
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
            content_parts: vec![],
            tool_calls: vec![],
            tool_result: None,
        }
    }

    pub fn with_content_parts(mut self, parts: Vec<ModelContentPart>) -> Self {
        self.content_parts = parts;
        self
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

#[derive(Clone)]
pub struct ModelRequest {
    pub messages: Vec<ModelMessage>,
    pub tools: Vec<ToolDefinition>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    /// Runtime-only artifact resolver. It is deliberately not serialized or
    /// stored in conversation history.
    pub artifact_store: Option<Arc<dyn artifact_core::ArtifactStore>>,
}

impl ModelRequest {
    pub fn new(messages: Vec<ModelMessage>) -> Self {
        Self {
            messages,
            tools: vec![],
            max_tokens: None,
            temperature: None,
            artifact_store: None,
        }
    }

    pub fn with_artifact_store(mut self, store: Arc<dyn artifact_core::ArtifactStore>) -> Self {
        self.artifact_store = Some(store);
        self
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
    #[error("artifact resolution failed: {0}")]
    Artifact(String),
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
