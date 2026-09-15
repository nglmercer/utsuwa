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
/// a provider adapter immediately before serialization. Video never implies
/// a raw frame stream: providers without native video input receive
/// intelligently sampled frames selected at the tool layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ModelContentPart {
    Text(String),
    Image {
        artifact: ArtifactRef,
        detail: Option<ImageDetail>,
    },
    Audio {
        artifact: ArtifactRef,
        format: AudioFormat,
    },
    Video {
        artifact: ArtifactRef,
        format: VideoFormat,
    },
    Json(serde_json::Value),
}

/// Audio container supplied with an [`ModelContentPart::Audio`] artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioFormat {
    Wav,
    Mp3,
    Ogg,
    Flac,
    Webm,
    Other,
}

/// Video container supplied with an [`ModelContentPart::Video`] artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoFormat {
    Mp4,
    Webm,
    Mkv,
    Mov,
    Other,
}

/// Whether a provider API explicitly supports a capability.
///
/// `Unknown` means the catalog entry carried no metadata for the
/// capability. Unknown must never be treated as supported: runtime
/// booleans map it to `false` (text/tool fallback) unless an explicit
/// debug override forces the media path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilitySupport {
    Supported,
    Unsupported,
    Unknown,
}

impl CapabilitySupport {
    pub fn is_supported(self) -> bool {
        matches!(self, CapabilitySupport::Supported)
    }

    /// Runtime boolean: only explicit `Supported` enables the capability.
    pub fn as_bool(self) -> bool {
        self.is_supported()
    }

    pub fn from_advertised(advertised_known: bool, contains: bool) -> Self {
        if !advertised_known {
            CapabilitySupport::Unknown
        } else if contains {
            CapabilitySupport::Supported
        } else {
            CapabilitySupport::Unsupported
        }
    }
}

impl std::fmt::Display for CapabilitySupport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CapabilitySupport::Supported => write!(f, "supported"),
            CapabilitySupport::Unsupported => write!(f, "unsupported"),
            CapabilitySupport::Unknown => write!(f, "unknown"),
        }
    }
}

/// Input/output modality advertised by a provider catalog entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modality {
    Text,
    Image,
    Audio,
    Video,
    Pdf,
}

impl Modality {
    /// Parse one provider `input_modalities` token. Returns `None` for
    /// unrecognized tokens so future modalities fail open to unknown
    /// rather than corrupting the known set.
    pub fn parse(token: &str) -> Option<Self> {
        match token.trim().to_ascii_lowercase().as_str() {
            "text" | "txt" => Some(Modality::Text),
            "image" | "images" | "vision" => Some(Modality::Image),
            "audio" => Some(Modality::Audio),
            "video" => Some(Modality::Video),
            "pdf" | "document" | "documents" | "file" => Some(Modality::Pdf),
            _ => None,
        }
    }
}

/// Where a [`ResolvedModelInfo`] came from. Provider API metadata is the
/// source of truth; anything else is explicitly labeled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "name")]
pub enum CapabilitySource {
    ProviderApi,
    PublicCatalog(String),
    DebugOverride,
    Unknown,
}

impl std::fmt::Display for CapabilitySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CapabilitySource::ProviderApi => write!(f, "provider_api"),
            CapabilitySource::PublicCatalog(name) => write!(f, "public_catalog:{name}"),
            CapabilitySource::DebugOverride => write!(f, "debug_override"),
            CapabilitySource::Unknown => write!(f, "unknown"),
        }
    }
}

/// Normalized per-model capability record. Built only from real
/// provider/public API metadata — never from model-name substrings or
/// provider-id assumptions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedModelInfo {
    pub id: String,
    pub input_modalities: std::collections::HashSet<Modality>,
    pub output_modalities: std::collections::HashSet<Modality>,
    /// False when the catalog entry omitted modality metadata entirely.
    /// Guards the Supported/Unsupported vs Unknown distinction: an absent
    /// field yields Unknown, a present field without the modality yields
    /// Unsupported.
    #[serde(default)]
    pub modalities_known: bool,
    pub tool_calls: CapabilitySupport,
    pub parallel_tool_calls: CapabilitySupport,
    pub structured_output: CapabilitySupport,
    pub reasoning: CapabilitySupport,
    pub source: CapabilitySource,
}

impl ResolvedModelInfo {
    pub fn unknown(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            input_modalities: std::collections::HashSet::new(),
            output_modalities: std::collections::HashSet::new(),
            modalities_known: false,
            tool_calls: CapabilitySupport::Unknown,
            parallel_tool_calls: CapabilitySupport::Unknown,
            structured_output: CapabilitySupport::Unknown,
            reasoning: CapabilitySupport::Unknown,
            source: CapabilitySource::Unknown,
        }
    }

    fn modality_support(&self, modality: Modality) -> CapabilitySupport {
        if !self.modalities_known {
            return CapabilitySupport::Unknown;
        }
        if self.input_modalities.contains(&modality) {
            CapabilitySupport::Supported
        } else {
            CapabilitySupport::Unsupported
        }
    }

    pub fn image_input(&self) -> CapabilitySupport {
        self.modality_support(Modality::Image)
    }

    pub fn audio_input(&self) -> CapabilitySupport {
        self.modality_support(Modality::Audio)
    }

    pub fn video_input(&self) -> CapabilitySupport {
        self.modality_support(Modality::Video)
    }

    pub fn pdf_input(&self) -> CapabilitySupport {
        self.modality_support(Modality::Pdf)
    }

    /// Downgrade one modality to unsupported (stale-metadata recovery).
    /// Marks modalities known so the downgrade is explicit, not unknown.
    pub fn with_modality_unsupported(mut self, modality: Modality) -> Self {
        self.modalities_known = true;
        self.input_modalities.remove(&modality);
        self
    }
}

/// Explicit provider capability contract. Adapters declare what they
/// actually support; agent plumbing routes around the gaps (sampled video
/// frames, image fallback text) instead of assuming uniform support.
///
/// Runtime booleans derive from [`ResolvedModelInfo`] via
/// [`ModelCapabilities::from_resolved`]: only explicit `Supported` becomes
/// `true`. `Unknown` and `Unsupported` both disable the media path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCapabilities {
    pub tool_calls: bool,
    pub parallel_tool_calls: bool,
    pub image_input: bool,
    pub image_tool_results: bool,
    pub audio_input: bool,
    pub video_input: bool,
    pub pdf_input: bool,
    pub structured_output: bool,
    pub reasoning: bool,
    pub streaming: bool,
}

impl Default for ModelCapabilities {
    /// Conservative baseline: plain tools and text streaming only. A
    /// provider that accepts inline images in tool results must opt in via
    /// [`ModelCapabilities::with_image_tool_results`]; never assume an
    /// OpenAI-compatible endpoint supports multimodal tool results.
    fn default() -> Self {
        Self {
            tool_calls: true,
            parallel_tool_calls: false,
            image_input: false,
            image_tool_results: false,
            audio_input: false,
            video_input: false,
            pdf_input: false,
            structured_output: false,
            reasoning: false,
            streaming: true,
        }
    }
}

impl ModelCapabilities {
    /// Full multimodal support (first-party OpenAI/Anthropic endpoints).
    pub fn full() -> Self {
        Self {
            tool_calls: true,
            parallel_tool_calls: true,
            image_input: true,
            image_tool_results: true,
            audio_input: true,
            video_input: true,
            pdf_input: true,
            structured_output: true,
            reasoning: true,
            streaming: true,
        }
    }

    /// Build runtime booleans from API-resolved metadata. Only explicit
    /// `Supported` enables a capability; `Unknown` never becomes `true`.
    pub fn from_resolved(info: &ResolvedModelInfo) -> Self {
        let image = info.image_input().as_bool();
        Self {
            tool_calls: info.tool_calls.as_bool(),
            parallel_tool_calls: info.parallel_tool_calls.as_bool(),
            image_input: image,
            image_tool_results: image,
            audio_input: info.audio_input().as_bool(),
            video_input: info.video_input().as_bool(),
            pdf_input: info.pdf_input().as_bool(),
            structured_output: info.structured_output.as_bool(),
            reasoning: info.reasoning.as_bool(),
            streaming: true,
        }
    }

    /// Disable one modality after an explicit provider capability error
    /// (stale catalog recovery). Used for the single retry without the
    /// rejected media kind.
    pub fn without_modality(mut self, modality: Modality) -> Self {
        match modality {
            Modality::Image => {
                self.image_input = false;
                self.image_tool_results = false;
            }
            Modality::Audio => self.audio_input = false,
            Modality::Video => self.video_input = false,
            Modality::Pdf => self.pdf_input = false,
            Modality::Text => {}
        }
        self
    }

    pub fn with_image_tool_results(mut self, supported: bool) -> Self {
        self.image_tool_results = supported;
        if supported {
            self.image_input = true;
        }
        self
    }

    /// Rewrite a whole request for this provider's media support: every
    /// tool result passes through [`ModelCapabilities::apply_tool_result_fallback`].
    /// User-message media parts are left to the adapter wire format, which
    /// renders an explicit placeholder for unsupported kinds.
    pub fn apply_to_request(&self, request: &ModelRequest) -> ModelRequest {
        if self.image_tool_results && self.audio_input && self.video_input {
            return ModelRequest {
                messages: request.messages.clone(),
                tools: request.tools.clone(),
                max_tokens: request.max_tokens,
                temperature: request.temperature,
                artifact_store: request.artifact_store.clone(),
            };
        }
        let messages = request
            .messages
            .iter()
            .map(|message| {
                let Some(result) = message.tool_result.clone() else {
                    return message.clone();
                };
                let rewritten = self.apply_tool_result_fallback(&result);
                ModelMessage {
                    role: message.role,
                    content: rewritten.content.clone(),
                    content_value: message.content_value.clone(),
                    content_parts: message.content_parts.clone(),
                    tool_calls: message.tool_calls.clone(),
                    tool_result: Some(rewritten),
                }
            })
            .collect();
        ModelRequest {
            messages,
            tools: request.tools.clone(),
            max_tokens: request.max_tokens,
            temperature: request.temperature,
            artifact_store: request.artifact_store.clone(),
        }
    }

    /// Rewrite one tool result for a provider without native image
    /// tool-result support. Images become explicit metadata text — never
    /// silently dropped — so the model still learns a screenshot existed,
    /// its dimensions, and how to reference it.
    pub fn apply_tool_result_fallback(&self, result: &ToolResult) -> ToolResult {
        if self.image_tool_results && self.audio_input && self.video_input {
            return result.clone();
        }
        let mut parts = Vec::with_capacity(result.parts.len());
        for part in &result.parts {
            match part {
                ModelContentPart::Image { artifact, .. } if !self.image_tool_results => {
                    parts.push(ModelContentPart::Text(format!(
                        "[image tool result omitted: this provider does not accept images in tool results; artifact_id={} mime_type={} size_bytes={}]",
                        artifact.id, artifact.mime_type, artifact.size_bytes,
                    )));
                }
                ModelContentPart::Audio { artifact, .. } if !self.audio_input => {
                    parts.push(ModelContentPart::Text(format!(
                        "[audio tool result omitted: this provider does not accept audio input; artifact_id={} mime_type={} size_bytes={}]",
                        artifact.id, artifact.mime_type, artifact.size_bytes,
                    )));
                }
                ModelContentPart::Video { artifact, .. } if !self.video_input => {
                    parts.push(ModelContentPart::Text(format!(
                        "[video tool result omitted: this provider does not accept video input; artifact_id={} mime_type={} size_bytes={}]",
                        artifact.id, artifact.mime_type, artifact.size_bytes,
                    )));
                }
                other => parts.push(other.clone()),
            }
        }
        let mut rewritten = result.clone();
        if parts
            .iter()
            .all(|part| matches!(part, ModelContentPart::Text(_)))
        {
            rewritten.content = parts
                .iter()
                .filter_map(|part| match part {
                    ModelContentPart::Text(text) => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
        }
        rewritten.parts = parts;
        rewritten
    }
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

impl ModelError {
    /// Whether re-sending the same request later may succeed. Rate limits,
    /// request timeouts, and server-side failures are transient; transport
    /// failures stay fatal so a down local server fails fast instead of
    /// stalling the turn through a backoff ladder, and everything else
    /// (4xx validation, bad payloads, artifacts, cancellation) is
    /// deterministic and must surface immediately.
    ///
    /// A capability mismatch (provider explicitly rejects a media kind the
    /// catalog advertised) is never a normal retry: the adapter rebuilds
    /// the request without the rejected media and retries once instead of
    /// backing off on the identical payload.
    pub fn is_retryable(&self) -> bool {
        if self.capability_modality().is_some() {
            return false;
        }
        match self {
            ModelError::Provider { status, .. } => {
                *status == 429 || *status == 408 || (500..600).contains(status)
            }
            _ => false,
        }
    }

    /// Detect an explicit provider capability rejection and name the media
    /// kind it refuses. Matches provider phrasings such as "image input
    /// unsupported", "no endpoints support image input", "unsupported
    /// modality", "audio input unsupported", "video input unsupported".
    /// Returns `None` for ordinary errors (auth, rate limits, 5xx).
    pub fn capability_modality(&self) -> Option<Modality> {
        let message = match self {
            ModelError::Provider { message, .. } => message.as_str(),
            ModelError::InvalidResponse(message) => message.as_str(),
            _ => return None,
        };
        capability_modality_from_message(message)
    }
}

/// Classify a provider diagnostic as a media-capability rejection.
/// Case-insensitive substring match over the explicit phrasings providers
/// use when a model/route cannot accept a modality.
pub fn capability_modality_from_message(message: &str) -> Option<Modality> {
    let text = message.to_ascii_lowercase();
    let mentions = |words: &[&str]| words.iter().any(|word| text.contains(word));
    // Image phrasings first: the most common router failure ("No endpoints
    // found that support image input").
    if mentions(&["image input", "image_url", "image url"])
        && mentions(&[
            "unsupported",
            "not support",
            "no endpoint",
            "no endpoints",
            "does not support",
            "don't support",
            "cannot accept",
            "can't accept",
            "invalid",
            "rejected",
        ])
    {
        return Some(Modality::Image);
    }
    if mentions(&["audio input", "audio_url", "audio url"])
        && mentions(&[
            "unsupported",
            "not support",
            "no endpoint",
            "no endpoints",
            "does not support",
            "cannot accept",
            "invalid",
            "rejected",
        ])
    {
        return Some(Modality::Audio);
    }
    if mentions(&["video input", "video_url", "video url"])
        && mentions(&[
            "unsupported",
            "not support",
            "no endpoint",
            "no endpoints",
            "does not support",
            "cannot accept",
            "invalid",
            "rejected",
        ])
    {
        return Some(Modality::Video);
    }
    if mentions(&["pdf input", "pdf_url", "pdf url", "document input"])
        && mentions(&[
            "unsupported",
            "not support",
            "no endpoint",
            "no endpoints",
            "does not support",
            "cannot accept",
            "invalid",
            "rejected",
        ])
    {
        return Some(Modality::Pdf);
    }
    // Generic modality rejection without a named kind. Callers treat this
    // as an image-path failure (the only media kind the OpenAI-compatible
    // wire format serializes inline today) while still invalidating the
    // catalog entry.
    if mentions(&[
        "unsupported modality",
        "unsupported media",
        "modality not supported",
    ]) {
        return Some(Modality::Image);
    }
    None
}

pub type ModelStream =
    Pin<Box<dyn futures_core::Stream<Item = Result<ModelStreamEvent, ModelError>> + Send>>;

#[async_trait::async_trait]
pub trait ModelProvider: Send + Sync {
    /// What this provider actually supports. Defaults to the conservative
    /// baseline (no multimodal tool results); adapters opt in explicitly.
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    async fn stream(&self, request: ModelRequest) -> Result<ModelStream, ModelError>;

    /// Non-streaming completion assembled from [`ModelProvider::stream`].
    /// Adapters with a native non-streaming endpoint may override this.
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, ModelError> {
        use futures_util::StreamExt as _;
        let mut stream = self.stream(request).await?;
        let mut text = String::new();
        let mut tool_calls = Vec::new();
        let mut finish_reason = FinishReason::Stop;
        while let Some(event) = stream.next().await {
            match event? {
                ModelStreamEvent::TextDelta(delta) => text.push_str(&delta),
                ModelStreamEvent::ToolCall(call) => tool_calls.push(call),
                ModelStreamEvent::Done {
                    finish_reason: reason,
                } => finish_reason = reason,
            }
        }
        if finish_reason == FinishReason::Stop && !tool_calls.is_empty() {
            finish_reason = FinishReason::ToolCalls;
        }
        Ok(ModelResponse {
            text,
            tool_calls,
            finish_reason,
        })
    }
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
    fn providers_without_image_results_get_explicit_fallback_text() {
        let artifact = ArtifactRef::new(
            artifact_core::ArtifactId::new("shot-1"),
            "image/png",
            12_345,
        );
        let result = ToolResult::text("call-1", "screenshot taken").with_parts(vec![
            ModelContentPart::Text("screenshot taken".to_string()),
            ModelContentPart::Image {
                artifact: artifact.clone(),
                detail: None,
            },
        ]);
        // Capable providers keep the image part untouched.
        let full = ModelCapabilities::full();
        assert_eq!(full.apply_tool_result_fallback(&result), result);
        // Other providers get explicit metadata text, never a silent drop.
        let limited = ModelCapabilities::default();
        let rewritten = limited.apply_tool_result_fallback(&result);
        assert!(rewritten
            .parts
            .iter()
            .all(|part| matches!(part, ModelContentPart::Text(_))));
        assert!(
            rewritten.content.contains("shot-1"),
            "{}",
            rewritten.content
        );
        assert!(
            rewritten.content.contains("image/png"),
            "{}",
            rewritten.content
        );
    }

    #[test]
    fn fallback_matrix_covers_audio_video_and_mixed_media() {
        let image = ModelContentPart::Image {
            artifact: ArtifactRef::new(artifact_core::ArtifactId::new("shot-1"), "image/png", 100),
            detail: None,
        };
        let audio = ModelContentPart::Audio {
            artifact: ArtifactRef::new(artifact_core::ArtifactId::new("a1"), "audio/wav", 200),
            format: AudioFormat::Wav,
        };
        let video = ModelContentPart::Video {
            artifact: ArtifactRef::new(artifact_core::ArtifactId::new("v1"), "video/mp4", 300),
            format: VideoFormat::Mp4,
        };
        let mixed = ToolResult::text("call-9", "observe").with_parts(vec![
            ModelContentPart::Text("observe".to_string()),
            image.clone(),
            audio.clone(),
            video.clone(),
        ]);
        // Default caps: every media part becomes explicit omitted-text
        // carrying its artifact id — nothing is silently dropped.
        let rewritten = ModelCapabilities::default().apply_tool_result_fallback(&mixed);
        assert_eq!(rewritten.parts.len(), 4);
        let texts = rewritten
            .parts
            .iter()
            .filter_map(|part| match part {
                ModelContentPart::Text(text) => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        for id in ["shot-1", "a1", "v1"] {
            assert!(texts.contains(id), "{texts}");
        }
        assert!(texts.contains("observe"), "{texts}");
        // Full caps: the request passes through untouched.
        assert_eq!(
            ModelCapabilities::full().apply_tool_result_fallback(&mixed),
            mixed
        );
        // Image-only caps: the image survives, audio/video still degrade
        // to explicit metadata.
        let partial = ModelCapabilities::default().with_image_tool_results(true);
        let rewritten = partial.apply_tool_result_fallback(&mixed);
        assert!(rewritten.parts.contains(&image));
        assert!(!rewritten.parts.contains(&audio));
        assert!(!rewritten.parts.contains(&video));
        assert!(rewritten
            .parts
            .iter()
            .filter_map(|part| match part {
                ModelContentPart::Text(text) => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
            .contains("a1"),);
    }

    #[test]
    fn audio_video_parts_carry_format_metadata() {
        let audio = ModelContentPart::Audio {
            artifact: ArtifactRef::new(artifact_core::ArtifactId::new("a1"), "audio/wav", 100),
            format: AudioFormat::Wav,
        };
        let video = ModelContentPart::Video {
            artifact: ArtifactRef::new(artifact_core::ArtifactId::new("v1"), "video/mp4", 200),
            format: VideoFormat::Mp4,
        };
        // Serialization round-trips for conversation plumbing.
        for part in [audio, video] {
            let json = serde_json::to_value(&part).unwrap();
            let back: ModelContentPart = serde_json::from_value(json).unwrap();
            assert_eq!(back, part);
        }
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

    #[test]
    fn retryable_errors_are_only_transient_provider_failures() {
        for status in [408, 429, 500, 502, 503, 599] {
            assert!(
                ModelError::Provider {
                    status,
                    message: "x".to_string()
                }
                .is_retryable(),
                "status {status} should be retryable"
            );
        }
        for status in [400, 401, 403, 404, 422] {
            assert!(
                !ModelError::Provider {
                    status,
                    message: "x".to_string()
                }
                .is_retryable(),
                "status {status} should fail fast"
            );
        }
        assert!(!ModelError::Transport("down".to_string()).is_retryable());
        assert!(!ModelError::InvalidResponse("x".to_string()).is_retryable());
        assert!(!ModelError::Artifact("x".to_string()).is_retryable());
        assert!(!ModelError::Cancelled.is_retryable());
    }
}
