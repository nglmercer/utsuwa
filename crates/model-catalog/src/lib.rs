//! Dynamic model capability discovery from real provider/public APIs.
//!
//! Capabilities are resolved only from catalog metadata returned by a
//! provider's official model endpoint (or an explicitly configured public
//! model API). There is intentionally no model-name heuristic anywhere in
//! this crate: no `VISION_MODEL_HINTS`, no `TEXT_ONLY_MODELS`, no
//! provider-id modality assumptions. When the API says nothing about a
//! capability, it resolves to [`CapabilitySupport::Unknown`], and the
//! runtime maps that to the safe text/tool fallback.
//!
//! Resolution order for one model:
//!
//! ```text
//! 1. Selected provider's official API (via [`ProviderCatalog`])
//! 2. Explicit public metadata API configured for that provider
//!    ([`OpenRouterCatalog`], [`ModelsDevCatalog`], …)
//! 3. Unknown ([`ResolvedModelInfo::unknown`])
//! ```
//!
//! Future providers and public capability APIs plug in as new
//! [`ModelCatalogProvider`] implementations without touching `agent-core`
//! or `model-core`.

use model_core::{CapabilitySource, CapabilitySupport, Modality, ResolvedModelInfo};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Request identity for one catalog lookup: provider + base URL + model +
/// optional API key. The key itself never enters logs or cache keys in the
/// clear; only an [`auth_identity`] hash does.
#[derive(Debug, Clone)]
pub struct CatalogRequest {
    pub provider: String,
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    /// Explicit public metadata source for this provider, consulted after
    /// the provider's official API. `None` means official-API-or-Unknown.
    pub public_catalog: Option<PublicCatalog>,
}

impl CatalogRequest {
    pub fn new(
        provider: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: Option<String>,
    ) -> Self {
        Self {
            provider: provider.into(),
            base_url: base_url.into(),
            model: model.into(),
            api_key,
            public_catalog: None,
        }
    }

    pub fn with_public_catalog(mut self, catalog: PublicCatalog) -> Self {
        self.public_catalog = Some(catalog);
        self
    }
}

/// Explicit opt-in public metadata source for one provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PublicCatalog {
    OpenRouter,
    ModelsDev,
}

/// Catalog fetch failure. Failures are never cached: the next resolution
/// retries the provider API instead of serving a stale negative.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CatalogError {
    #[error("catalog transport error: {0}")]
    Transport(String),
    #[error("catalog provider error {status}: {message}")]
    Provider { status: u16, message: String },
    #[error("invalid catalog response: {0}")]
    InvalidResponse(String),
}

/// One external model-metadata source. Implementations fetch a raw model
/// list over HTTP and normalize it to [`ResolvedModelInfo`]. Adding a new
/// source means adding a new implementation — never editing the service,
/// `agent-core`, or `model-core`.
#[async_trait::async_trait]
pub trait ModelCatalogProvider: Send + Sync {
    fn id(&self) -> &'static str;

    async fn fetch_models(
        &self,
        config: &CatalogRequest,
    ) -> Result<Vec<ResolvedModelInfo>, CatalogError>;
}

/// The selected provider's official model/catalog endpoint.
///
/// Endpoints per provider family (transport only — capability semantics
/// always come from the response body):
///
/// ```text
/// OpenAI-compatible / Kilo / OpenRouter-as-provider
///     GET {base_url}/models
/// LM Studio
///     GET {root}/api/v1/models, fallback {root}/api/v0/models,
///     fallback {root}/v1/models
/// Ollama
///     GET {root}/api/tags (+ best-effort POST {root}/api/show enrichment)
/// Anthropic / Google
///     GET {base_url}/models with provider auth headers
/// ```
#[derive(Debug, Default)]
pub struct ProviderCatalog {
    http: reqwest::Client,
}

impl ProviderCatalog {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl ModelCatalogProvider for ProviderCatalog {
    fn id(&self) -> &'static str {
        "provider"
    }

    async fn fetch_models(
        &self,
        config: &CatalogRequest,
    ) -> Result<Vec<ResolvedModelInfo>, CatalogError> {
        let provider = config.provider.trim().to_ascii_lowercase();
        if provider == "ollama" {
            return fetch_ollama_models(&self.http, config).await;
        }
        if provider == "lmstudio" {
            return fetch_lmstudio_models(&self.http, config).await;
        }
        fetch_openai_compatible_models(&self.http, config, &provider).await
    }
}

/// Public OpenRouter model catalog (`https://openrouter.ai/api/v1/models`).
/// Same record shape as OpenAI-compatible providers. Only consulted when a
/// [`CatalogRequest`] explicitly selects it — never implicitly.
#[derive(Debug, Default)]
pub struct OpenRouterCatalog {
    http: reqwest::Client,
    endpoint: Option<String>,
}

impl OpenRouterCatalog {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
            endpoint: None,
        }
    }

    /// Override the endpoint (tests point this at a local fixture server).
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }
}

#[async_trait::async_trait]
impl ModelCatalogProvider for OpenRouterCatalog {
    fn id(&self) -> &'static str {
        "openrouter"
    }

    async fn fetch_models(
        &self,
        config: &CatalogRequest,
    ) -> Result<Vec<ResolvedModelInfo>, CatalogError> {
        let endpoint = self
            .endpoint
            .clone()
            .unwrap_or_else(|| "https://openrouter.ai/api/v1/models".to_string());
        let mut request = self.http.get(&endpoint).timeout(Duration::from_secs(15));
        if let Some(key) = config
            .api_key
            .as_deref()
            .filter(|key| !key.trim().is_empty())
        {
            request = request.bearer_auth(key.trim());
        }
        let response = request
            .send()
            .await
            .map_err(|error| CatalogError::Transport(error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(CatalogError::Provider {
                status: status.as_u16(),
                message: truncate_body_for_error(&body),
            });
        }
        let data: serde_json::Value = response
            .json()
            .await
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
        Ok(normalize_openai_compatible_list(
            &data,
            CapabilitySource::PublicCatalog("openrouter".to_string()),
        ))
    }
}

/// Public models.dev catalog. Best-effort: the aggregate shape varies, so
/// unrecognized shapes resolve to Unknown rather than guessing.
#[derive(Debug, Default)]
pub struct ModelsDevCatalog {
    http: reqwest::Client,
    endpoint: Option<String>,
}

impl ModelsDevCatalog {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
            endpoint: None,
        }
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }
}

#[async_trait::async_trait]
impl ModelCatalogProvider for ModelsDevCatalog {
    fn id(&self) -> &'static str {
        "modelsdev"
    }

    async fn fetch_models(
        &self,
        _config: &CatalogRequest,
    ) -> Result<Vec<ResolvedModelInfo>, CatalogError> {
        let endpoint = self
            .endpoint
            .clone()
            .unwrap_or_else(|| "https://models.dev/api.json".to_string());
        let response = self
            .http
            .get(&endpoint)
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(|error| CatalogError::Transport(error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(CatalogError::Provider {
                status: status.as_u16(),
                message: truncate_body_for_error(&body),
            });
        }
        let data: serde_json::Value = response
            .json()
            .await
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
        Ok(normalize_models_dev_list(&data))
    }
}

// ---------------------------------------------------------------------------
// Normalization: provider metadata -> ResolvedModelInfo.
// ---------------------------------------------------------------------------

fn string_array(value: Option<&serde_json::Value>) -> Option<Vec<String>> {
    let values = value?.as_array()?;
    Some(
        values
            .iter()
            .filter_map(|item| item.as_str().map(str::to_string))
            .collect(),
    )
}

fn lowercase_set(values: &[String]) -> HashSet<String> {
    values
        .iter()
        .map(|value| value.to_ascii_lowercase())
        .collect()
}

/// Normalize one OpenAI-compatible model record. Reads only real metadata:
///
/// - `architecture.input_modalities` / `output_modalities` (fallback to
///   top-level `input_modalities` / `output_modalities`)
/// - `supported_parameters` (`tools`, `parallel_tool_calls`,
///   `response_format`, `reasoning`)
///
/// Absent fields yield [`CapabilitySupport::Unknown`]; present fields
/// without the token yield `Unsupported`. The model id is used only as an
/// identifier — never inspected for capability hints.
pub fn normalize_openai_compatible_record(
    id: &str,
    raw: &serde_json::Value,
    source: CapabilitySource,
) -> ResolvedModelInfo {
    let architecture = raw.get("architecture");
    let input_raw = string_array(
        architecture
            .and_then(|arch| arch.get("input_modalities"))
            .or_else(|| raw.get("input_modalities")),
    );
    let output_raw = string_array(
        architecture
            .and_then(|arch| arch.get("output_modalities"))
            .or_else(|| raw.get("output_modalities")),
    );
    let modalities_known = input_raw.is_some() || output_raw.is_some();
    let input_modalities = input_raw
        .unwrap_or_default()
        .iter()
        .filter_map(|token| Modality::parse(token))
        .collect();
    let output_modalities = output_raw
        .unwrap_or_default()
        .iter()
        .filter_map(|token| Modality::parse(token))
        .collect();

    let supported = string_array(raw.get("supported_parameters"));
    let parameters_known = supported.is_some();
    let set = lowercase_set(&supported.unwrap_or_default());
    // `tool_choice` without `tools` still proves the tools protocol: a
    // provider advertising tool choice necessarily accepts tools.
    let tools = set.contains("tools") || set.contains("tool_choice");

    ResolvedModelInfo {
        id: id.to_string(),
        input_modalities,
        output_modalities,
        modalities_known,
        tool_calls: CapabilitySupport::from_advertised(parameters_known, tools),
        parallel_tool_calls: CapabilitySupport::from_advertised(
            parameters_known,
            set.contains("parallel_tool_calls"),
        ),
        structured_output: CapabilitySupport::from_advertised(
            parameters_known,
            set.contains("response_format")
                || set.contains("structured_output")
                || set.contains("json_schema"),
        ),
        reasoning: CapabilitySupport::from_advertised(
            parameters_known,
            set.contains("reasoning") || set.contains("reasoning_effort"),
        ),
        source,
    }
}

/// Normalize a standard `{ data: [...] }` OpenAI-compatible catalog.
/// Records without a string `id` are skipped.
pub fn normalize_openai_compatible_list(
    data: &serde_json::Value,
    source: CapabilitySource,
) -> Vec<ResolvedModelInfo> {
    let records = data
        .get("data")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    records
        .iter()
        .filter_map(|record| {
            let id = record.get("id")?.as_str()?;
            if id.trim().is_empty() {
                return None;
            }
            Some(normalize_openai_compatible_record(
                id,
                record,
                source.clone(),
            ))
        })
        .collect()
}

/// Normalize one LM Studio model record. LM Studio reports capabilities as
/// an object (`{ vision, trained_for_tool_use }`), a legacy string array
/// (`["vision", "tool_use"]`), or root-level fields — plus a `type` of
/// `llm` / `vlm` / `embeddings`, which is API metadata (not a name hint).
pub fn normalize_lmstudio_record(
    id: &str,
    raw: &serde_json::Value,
    source: CapabilitySource,
) -> ResolvedModelInfo {
    let capabilities = raw.get("capabilities");
    let mut vision: Option<bool> = None;
    let mut native_tools: Option<bool> = None;
    let mut compatible_tools = false;

    if let Some(object) = capabilities.and_then(|value| value.as_object()) {
        if let Some(value) = object.get("vision").and_then(|value| value.as_bool()) {
            vision = Some(value);
        }
        if let Some(value) = object
            .get("trained_for_tool_use")
            .and_then(|value| value.as_bool())
        {
            native_tools = Some(value);
        }
        if object.get("tool_use").and_then(|value| value.as_bool()) == Some(true) {
            compatible_tools = true;
        }
    } else if let Some(list) = capabilities.and_then(|value| value.as_array()) {
        let tokens: HashSet<String> = list
            .iter()
            .filter_map(|item| item.as_str().map(|token| token.to_ascii_lowercase()))
            .collect();
        if tokens.contains("vision") {
            vision = Some(true);
        }
        if tokens.contains("tool_use")
            || tokens.contains("tool-use")
            || tokens.contains("trained_for_tool_use")
        {
            native_tools = Some(true);
        }
    }
    // Older responses put these at the model root.
    if vision.is_none() {
        if let Some(value) = raw.get("vision").and_then(|value| value.as_bool()) {
            vision = Some(value);
        }
    }
    if native_tools.is_none() {
        if let Some(value) = raw
            .get("trained_for_tool_use")
            .and_then(|value| value.as_bool())
        {
            native_tools = Some(value);
        } else if raw.get("tool_use").and_then(|value| value.as_bool()) == Some(true) {
            compatible_tools = true;
        }
    }
    // The server-declared model type is authoritative metadata: a `vlm`
    // accepts images, other types without a vision flag stay unknown.
    let model_type = raw
        .get("type")
        .and_then(|value| value.as_str())
        .map(|value| value.to_ascii_lowercase());
    if vision.is_none() && model_type.as_deref() == Some("vlm") {
        vision = Some(true);
    }

    let modalities_known = vision.is_some();
    let mut input_modalities = HashSet::new();
    input_modalities.insert(Modality::Text);
    if vision == Some(true) {
        input_modalities.insert(Modality::Image);
    }
    let tool_calls = match native_tools {
        Some(true) => CapabilitySupport::Supported,
        Some(false) => CapabilitySupport::Unsupported,
        None if compatible_tools => CapabilitySupport::Supported,
        None => CapabilitySupport::Unknown,
    };

    ResolvedModelInfo {
        id: id.to_string(),
        input_modalities,
        output_modalities: HashSet::from([Modality::Text]),
        modalities_known,
        tool_calls,
        parallel_tool_calls: CapabilitySupport::Unknown,
        structured_output: CapabilitySupport::Unknown,
        reasoning: CapabilitySupport::Unknown,
        source,
    }
}

/// Normalize an LM Studio catalog (`{ models: [...] }` or `{ data: [...] }`).
/// Non-LLM entries (embeddings) are skipped.
pub fn normalize_lmstudio_list(
    data: &serde_json::Value,
    source: CapabilitySource,
) -> Vec<ResolvedModelInfo> {
    let records = data
        .get("models")
        .or_else(|| data.get("data"))
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    records
        .iter()
        .filter_map(|record| {
            if !record.is_object() {
                return None;
            }
            let model_type = record
                .get("type")
                .and_then(|value| value.as_str())
                .map(|value| value.to_ascii_lowercase());
            if let Some(kind) = model_type {
                if kind != "llm" && kind != "vlm" {
                    return None;
                }
            }
            let id = record
                .get("key")
                .or_else(|| record.get("id"))
                .or_else(|| record.get("name"))
                .and_then(|value| value.as_str())?;
            if id.trim().is_empty() {
                return None;
            }
            Some(normalize_lmstudio_record(id, record, source.clone()))
        })
        .collect()
}

/// Normalize one Ollama `/api/show` response for `model`. Ollama reports
/// `capabilities` (e.g. `["completion"]` or `["completion", "vision"]`,
/// newer servers add `"tools"`). A present list is authoritative; an absent
/// field yields Unknown.
pub fn normalize_ollama_show(
    model: &str,
    raw: &serde_json::Value,
    source: CapabilitySource,
) -> ResolvedModelInfo {
    let capabilities = string_array(raw.get("capabilities"));
    let known = capabilities.is_some();
    let set = lowercase_set(&capabilities.unwrap_or_default());
    let mut input_modalities = HashSet::new();
    if known {
        input_modalities.insert(Modality::Text);
        if set.contains("vision") || set.contains("image") {
            input_modalities.insert(Modality::Image);
        }
        if set.contains("audio") {
            input_modalities.insert(Modality::Audio);
        }
        if set.contains("video") {
            input_modalities.insert(Modality::Video);
        }
        if set.contains("pdf") || set.contains("document") {
            input_modalities.insert(Modality::Pdf);
        }
    }
    ResolvedModelInfo {
        id: model.to_string(),
        input_modalities,
        output_modalities: if known {
            HashSet::from([Modality::Text])
        } else {
            HashSet::new()
        },
        modalities_known: known,
        tool_calls: CapabilitySupport::from_advertised(known, set.contains("tools")),
        parallel_tool_calls: CapabilitySupport::Unknown,
        structured_output: CapabilitySupport::from_advertised(
            known,
            set.contains("structured_output") || set.contains("response_format"),
        ),
        reasoning: CapabilitySupport::from_advertised(known, set.contains("reasoning")),
        source,
    }
}

/// Best-effort models.dev aggregate parser. Recognized per-model entries
/// are normalized; anything unrecognized is skipped (Unknown) rather than
/// guessed. Expected shape: `{ provider: { models: { id: entry } } }` with
/// entries carrying `modalities: { input: [...], output: [...] }` and
/// `tool_call: bool` / `structured_output: bool` / `reasoning: bool`.
pub fn normalize_models_dev_list(data: &serde_json::Value) -> Vec<ResolvedModelInfo> {
    let source = CapabilitySource::PublicCatalog("models.dev".to_string());
    let mut models = Vec::new();
    let Some(top) = data.as_object() else {
        return models;
    };
    for provider_entry in top.values() {
        let entries = provider_entry
            .get("models")
            .and_then(|value| value.as_object());
        let Some(entries) = entries else {
            continue;
        };
        for (id, entry) in entries {
            let modalities = entry.get("modalities");
            let input_raw = string_array(
                modalities
                    .and_then(|value| value.get("input"))
                    .or_else(|| entry.get("input_modalities")),
            );
            let output_raw = string_array(
                modalities
                    .and_then(|value| value.get("output"))
                    .or_else(|| entry.get("output_modalities")),
            );
            let modalities_known = input_raw.is_some() || output_raw.is_some();
            let input_modalities = input_raw
                .unwrap_or_default()
                .iter()
                .filter_map(|token| Modality::parse(token))
                .collect();
            let output_modalities = output_raw
                .unwrap_or_default()
                .iter()
                .filter_map(|token| Modality::parse(token))
                .collect();
            let flag = |key: &str| match entry.get(key).and_then(|value| value.as_bool()) {
                Some(true) => CapabilitySupport::Supported,
                Some(false) => CapabilitySupport::Unsupported,
                None => CapabilitySupport::Unknown,
            };
            // Skip entries with no usable metadata at all: they would only
            // shadow the provider API's own Unknown with another Unknown.
            if !modalities_known
                && flag("tool_call") == CapabilitySupport::Unknown
                && flag("structured_output") == CapabilitySupport::Unknown
                && flag("reasoning") == CapabilitySupport::Unknown
            {
                continue;
            }
            models.push(ResolvedModelInfo {
                id: id.clone(),
                input_modalities,
                output_modalities,
                modalities_known,
                tool_calls: flag("tool_call"),
                parallel_tool_calls: flag("parallel_tool_calls"),
                structured_output: flag("structured_output"),
                reasoning: flag("reasoning"),
                source: source.clone(),
            });
        }
    }
    models
}

// ---------------------------------------------------------------------------
// HTTP fetch: official provider endpoints.
// ---------------------------------------------------------------------------

fn truncate_body_for_error(body: &str) -> String {
    let cleaned: String = body
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .take(512)
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        "provider returned no diagnostic body".to_string()
    } else {
        collapsed
    }
}

fn trim_base(base_url: &str) -> String {
    base_url.trim().trim_end_matches('/').to_string()
}

/// Server root for providers whose metadata API lives above `/v1`
/// (LM Studio `/api/...`, Ollama `/api/tags`).
fn server_root(base_url: &str) -> String {
    let trimmed = trim_base(base_url);
    let lower = trimmed.to_ascii_lowercase();
    if lower.ends_with("/v1") {
        trimmed[..trimmed.len() - 3].to_string()
    } else {
        trimmed
    }
}

fn bearer(request: reqwest::RequestBuilder, api_key: Option<&str>) -> reqwest::RequestBuilder {
    match api_key.map(str::trim).filter(|key| !key.is_empty()) {
        Some(key) => request.bearer_auth(key),
        None => request,
    }
}

fn provider_auth_headers(
    request: reqwest::RequestBuilder,
    provider: &str,
    api_key: Option<&str>,
) -> reqwest::RequestBuilder {
    let key = api_key.map(str::trim).filter(|key| !key.is_empty());
    match provider {
        "anthropic" => {
            let mut request = request.header("anthropic-version", "2023-06-01");
            if let Some(key) = key {
                request = request.header("x-api-key", key);
            }
            request
        }
        "google" => {
            let mut request = request;
            if let Some(key) = key {
                request = request.header("x-goog-api-key", key);
            }
            request
        }
        _ => bearer(request, key),
    }
}

async fn get_json(
    http: &reqwest::Client,
    url: &str,
    provider: &str,
    api_key: Option<&str>,
    timeout: Duration,
) -> Result<serde_json::Value, CatalogError> {
    let response = provider_auth_headers(http.get(url).timeout(timeout), provider, api_key)
        .send()
        .await
        .map_err(|error| CatalogError::Transport(error.to_string()))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(CatalogError::Provider {
            status: status.as_u16(),
            message: truncate_body_for_error(&body),
        });
    }
    response
        .json()
        .await
        .map_err(|error| CatalogError::InvalidResponse(error.to_string()))
}

async fn fetch_openai_compatible_models(
    http: &reqwest::Client,
    config: &CatalogRequest,
    provider: &str,
) -> Result<Vec<ResolvedModelInfo>, CatalogError> {
    let url = format!("{}/models", trim_base(&config.base_url));
    let data = get_json(
        http,
        &url,
        provider,
        config.api_key.as_deref(),
        Duration::from_secs(15),
    )
    .await?;
    // Anthropic/Google list shapes differ slightly (`{ data: [...] }` with
    // bare ids); the shared normalizer yields Unknown for entries without
    // modality metadata, which is the honest answer.
    if provider == "google" {
        let records = data
            .get("models")
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default();
        return Ok(records
            .iter()
            .filter_map(|record| {
                let name = record.get("name")?.as_str()?;
                let id = name.strip_prefix("models/").unwrap_or(name);
                if id.trim().is_empty() {
                    return None;
                }
                Some(normalize_openai_compatible_record(
                    id,
                    record,
                    CapabilitySource::ProviderApi,
                ))
            })
            .collect());
    }
    Ok(normalize_openai_compatible_list(
        &data,
        CapabilitySource::ProviderApi,
    ))
}

async fn fetch_lmstudio_models(
    http: &reqwest::Client,
    config: &CatalogRequest,
) -> Result<Vec<ResolvedModelInfo>, CatalogError> {
    let root = server_root(&config.base_url);
    let candidates = [
        format!("{root}/api/v1/models"),
        format!("{root}/api/v0/models"),
        format!("{root}/v1/models"),
    ];
    let mut last_error =
        CatalogError::InvalidResponse("no LM Studio endpoint answered".to_string());
    for url in candidates {
        match get_json(
            http,
            &url,
            "lmstudio",
            config.api_key.as_deref(),
            Duration::from_secs(10),
        )
        .await
        {
            Ok(data) => {
                return Ok(normalize_lmstudio_list(
                    &data,
                    CapabilitySource::ProviderApi,
                ));
            }
            Err(error) => {
                // Only fall back on a missing endpoint; auth/server failures
                // stay visible instead of disguising as another API failure.
                let missing = matches!(
                    &error,
                    CatalogError::Provider { status: 404, .. } | CatalogError::InvalidResponse(_)
                );
                last_error = error;
                if !missing {
                    break;
                }
            }
        }
    }
    Err(last_error)
}

/// Maximum Ollama models enriched per catalog fetch. `/api/show` needs one
/// round trip per model; beyond this cap the remainder stay Unknown rather
/// than stalling resolution.
const OLLAMA_SHOW_ENRICHMENT_CAP: usize = 32;

async fn fetch_ollama_models(
    http: &reqwest::Client,
    config: &CatalogRequest,
) -> Result<Vec<ResolvedModelInfo>, CatalogError> {
    let root = server_root(&config.base_url);
    let data = get_json(
        http,
        &format!("{root}/api/tags"),
        "ollama",
        config.api_key.as_deref(),
        Duration::from_secs(10),
    )
    .await?;
    let names: Vec<String> = data
        .get("models")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|record| record.get("name")?.as_str().map(str::to_string))
        .filter(|name| !name.trim().is_empty())
        .collect();

    // Enrich concurrently: `/api/tags` carries no capability metadata, so
    // each model's real `/api/show` record (with `capabilities`) is the
    // source of truth. Best-effort per model — a failed show keeps that
    // model Unknown instead of failing the whole catalog.
    let mut enriched: HashMap<String, ResolvedModelInfo> = HashMap::new();
    let mut pending = tokio::task::JoinSet::new();
    for name in names.iter().take(OLLAMA_SHOW_ENRICHMENT_CAP) {
        let http = http.clone();
        let url = format!("{root}/api/show");
        let name = name.clone();
        let key = config.api_key.clone();
        pending.spawn(async move {
            let response = bearer(
                http.post(&url).timeout(Duration::from_secs(5)),
                key.as_deref(),
            )
            .json(&serde_json::json!({ "name": name }))
            .send()
            .await
            .map_err(|error| CatalogError::Transport(error.to_string()))?;
            if !response.status().is_success() {
                return Err(CatalogError::Provider {
                    status: response.status().as_u16(),
                    message: "ollama show failed".to_string(),
                });
            }
            let data: serde_json::Value = response
                .json()
                .await
                .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
            Ok::<_, CatalogError>(normalize_ollama_show(
                &name,
                &data,
                CapabilitySource::ProviderApi,
            ))
        });
    }
    while let Some(joined) = pending.join_next().await {
        if let Ok(Ok(info)) = joined {
            enriched.insert(info.id.clone(), info);
        }
    }
    Ok(names
        .into_iter()
        .map(|name| {
            enriched.remove(&name).unwrap_or_else(|| {
                let mut unknown = ResolvedModelInfo::unknown(name);
                unknown.source = CapabilitySource::ProviderApi;
                unknown
            })
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Cache + service.
// ---------------------------------------------------------------------------

/// Default catalog time-to-live: fresh enough to track router changes,
/// long enough to avoid a `/models` round trip per turn.
pub const DEFAULT_CATALOG_TTL: Duration = Duration::from_secs(15 * 60);
/// How long a stale-metadata downgrade (from an explicit provider
/// capability error) suppresses the advertised capability.
pub const DEFAULT_OVERRIDE_TTL: Duration = Duration::from_secs(30 * 60);

/// Short identity for the credential in use. Anonymous callers share one
/// bucket; keyed callers hash the key so distinct accounts never share a
/// catalog entry — without the key ever entering logs.
fn auth_identity(api_key: Option<&str>) -> String {
    match api_key.map(str::trim).filter(|key| !key.is_empty()) {
        None => "anon".to_string(),
        Some(key) => {
            use sha2::Digest as _;
            let mut hasher = sha2::Sha256::new();
            hasher.update(key.as_bytes());
            let digest = hasher.finalize();
            format!("key:{:x}", digest).chars().take(20).collect()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    provider: String,
    base_url: String,
    auth: String,
    public_catalog: Option<PublicCatalog>,
}

impl CacheKey {
    fn of(config: &CatalogRequest) -> Self {
        Self {
            provider: config.provider.trim().to_ascii_lowercase(),
            base_url: trim_base(&config.base_url).to_ascii_lowercase(),
            auth: auth_identity(config.api_key.as_deref()),
            public_catalog: config.public_catalog,
        }
    }
}

#[derive(Debug)]
struct CachedCatalog {
    models: Vec<ResolvedModelInfo>,
    fetched_at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct OverrideKey {
    provider: String,
    base_url: String,
    model: String,
}

impl OverrideKey {
    fn of(config: &CatalogRequest) -> Self {
        Self {
            provider: config.provider.trim().to_ascii_lowercase(),
            base_url: trim_base(&config.base_url).to_ascii_lowercase(),
            model: config.model.trim().to_string(),
        }
    }
}

/// Native capability-resolution service.
///
/// ```text
/// Provider API / Public Model API
///           ↓ fetch model catalog
/// normalize provider metadata
///           ↓ ResolvedModelInfo
/// runtime ModelCapabilities (via ModelCapabilities::from_resolved)
/// ```
#[derive(Debug)]
pub struct ModelCatalogService {
    official: ProviderCatalog,
    openrouter: OpenRouterCatalog,
    modelsdev: ModelsDevCatalog,
    cache: Mutex<HashMap<CacheKey, CachedCatalog>>,
    overrides: Mutex<HashMap<OverrideKey, (HashSet<Modality>, Instant)>>,
    ttl: Duration,
    override_ttl: Duration,
}

impl Default for ModelCatalogService {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelCatalogService {
    pub fn new() -> Self {
        Self {
            official: ProviderCatalog::new(),
            openrouter: OpenRouterCatalog::new(),
            modelsdev: ModelsDevCatalog::new(),
            cache: Mutex::new(HashMap::new()),
            overrides: Mutex::new(HashMap::new()),
            ttl: DEFAULT_CATALOG_TTL,
            override_ttl: DEFAULT_OVERRIDE_TTL,
        }
    }

    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl.clamp(Duration::from_secs(5 * 60), Duration::from_secs(30 * 60));
        self
    }

    pub fn with_override_ttl(mut self, ttl: Duration) -> Self {
        self.override_ttl = ttl;
        self
    }

    /// Fetch (or reuse the cached) full catalog for this request identity.
    /// Failures are never cached.
    pub async fn fetch_models(
        &self,
        config: &CatalogRequest,
    ) -> Result<Vec<ResolvedModelInfo>, CatalogError> {
        if let Some(cached) = self.cached(config) {
            return Ok(cached);
        }
        // 1. Selected provider's official API.
        let official = self.official.fetch_models(config).await;
        let models = match official {
            Ok(models) => models,
            Err(official_error) => {
                // 2. Explicit public metadata API configured for the provider.
                if let Some(public) = config.public_catalog {
                    let fallback = match public {
                        PublicCatalog::OpenRouter => self.openrouter.fetch_models(config).await,
                        PublicCatalog::ModelsDev => self.modelsdev.fetch_models(config).await,
                    };
                    match fallback {
                        Ok(models) => models,
                        Err(_) => return Err(official_error),
                    }
                } else {
                    return Err(official_error);
                }
            }
        };
        self.store(config, models.clone());
        Ok(models)
    }

    /// Resolve one model to its normalized capability record.
    ///
    /// - Exact catalog entry found → its metadata (with any temporary
    ///   stale-metadata downgrades applied). Router/auto ids resolve only
    ///   from their own catalog entry — the models behind a router are
    ///   never inspected.
    /// - Entry missing → [`ResolvedModelInfo::unknown`] (never an error:
    ///   the runtime degrades to the text/tool fallback).
    /// - Catalog fetch failed → [`CatalogError`] (not cached; the caller
    ///   decides between failing open to Unknown or surfacing the error).
    pub async fn resolve_model(
        &self,
        config: &CatalogRequest,
    ) -> Result<ResolvedModelInfo, CatalogError> {
        let models = self.fetch_models(config).await?;
        let wanted = config.model.trim();
        let mut info = models
            .into_iter()
            .find(|model| model.id == wanted)
            .unwrap_or_else(|| ResolvedModelInfo::unknown(wanted));
        for modality in self.active_overrides(config) {
            info = info.with_modality_unsupported(modality);
        }
        Ok(info)
    }

    /// [`resolve_model`](Self::resolve_model), failing open to Unknown when
    /// the catalog itself is unreachable. The runtime path uses this so a
    /// down `/models` endpoint degrades to the text/tool fallback instead
    /// of failing the turn.
    pub async fn resolve_model_or_unknown(&self, config: &CatalogRequest) -> ResolvedModelInfo {
        match self.resolve_model(config).await {
            Ok(info) => info,
            Err(error) => {
                tracing::warn!(
                    provider = %config.provider,
                    model = %config.model,
                    %error,
                    "model catalog unreachable; resolving capabilities as unknown"
                );
                ResolvedModelInfo::unknown(config.model.trim())
            }
        }
    }

    /// Synchronous cache-only lookup for contexts that cannot await (the
    /// sync provider factory). Returns `None` on any miss or expiry — the
    /// caller then uses the conservative default and the next async turn
    /// populates the cache.
    pub fn try_resolve_cached(&self, config: &CatalogRequest) -> Option<ResolvedModelInfo> {
        let models = self.cached(config)?;
        let wanted = config.model.trim();
        let mut info = models
            .into_iter()
            .find(|model| model.id == wanted)
            .unwrap_or_else(|| ResolvedModelInfo::unknown(wanted));
        for modality in self.active_overrides(config) {
            info = info.with_modality_unsupported(modality);
        }
        Some(info)
    }

    /// Record an explicit provider capability rejection: the advertised
    /// capability is temporarily suppressed and the cached catalog entry is
    /// dropped so the next resolution reloads provider metadata.
    pub fn note_capability_error(&self, config: &CatalogRequest, modality: Modality) {
        if let Ok(mut overrides) = self.overrides.lock() {
            overrides
                .entry(OverrideKey::of(config))
                .and_modify(|(modalities, at)| {
                    modalities.insert(modality);
                    *at = Instant::now();
                })
                .or_insert_with(|| (HashSet::from([modality]), Instant::now()));
        }
        self.invalidate(config);
    }

    /// Drop the cached catalog for this request identity (provider, base
    /// URL, auth identity, or explicit refresh changed).
    pub fn invalidate(&self, config: &CatalogRequest) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.remove(&CacheKey::of(config));
        }
    }

    /// Drop every cached catalog.
    pub fn invalidate_all(&self) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.clear();
        }
    }

    /// Invalidate and immediately reload the catalog for this identity.
    pub async fn refresh(
        &self,
        config: &CatalogRequest,
    ) -> Result<Vec<ResolvedModelInfo>, CatalogError> {
        self.invalidate(config);
        self.fetch_models(config).await
    }

    fn cached(&self, config: &CatalogRequest) -> Option<Vec<ResolvedModelInfo>> {
        let cache = self.cache.lock().ok()?;
        let entry = cache.get(&CacheKey::of(config))?;
        if entry.fetched_at.elapsed() > self.ttl {
            return None;
        }
        Some(entry.models.clone())
    }

    fn store(&self, config: &CatalogRequest, models: Vec<ResolvedModelInfo>) {
        if let Ok(mut cache) = self.cache.lock() {
            // Opportunistically evict expired entries so distinct identities
            // cannot grow the map without bound.
            let ttl = self.ttl;
            cache.retain(|_, entry| entry.fetched_at.elapsed() <= ttl);
            cache.insert(
                CacheKey::of(config),
                CachedCatalog {
                    models,
                    fetched_at: Instant::now(),
                },
            );
        }
    }

    fn active_overrides(&self, config: &CatalogRequest) -> HashSet<Modality> {
        let Ok(overrides) = self.overrides.lock() else {
            return HashSet::new();
        };
        match overrides.get(&OverrideKey::of(config)) {
            Some((modalities, at)) if at.elapsed() <= self.override_ttl => modalities.clone(),
            _ => HashSet::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use model_core::ModelCapabilities;

    fn provider_api() -> CapabilitySource {
        CapabilitySource::ProviderApi
    }

    fn record(json: serde_json::Value) -> serde_json::Value {
        json
    }

    #[test]
    fn api_image_modality_enables_image_only() {
        let info = normalize_openai_compatible_record(
            "router/free",
            &record(serde_json::json!({
                "architecture": {
                    "input_modalities": ["text", "image"],
                    "output_modalities": ["text"]
                },
                "supported_parameters": ["tools", "tool_choice", "parallel_tool_calls", "response_format", "reasoning"]
            })),
            provider_api(),
        );
        assert_eq!(info.image_input(), CapabilitySupport::Supported);
        assert_eq!(info.audio_input(), CapabilitySupport::Unsupported);
        assert_eq!(info.video_input(), CapabilitySupport::Unsupported);
        assert_eq!(info.pdf_input(), CapabilitySupport::Unsupported);
        assert_eq!(info.tool_calls, CapabilitySupport::Supported);
        assert_eq!(info.parallel_tool_calls, CapabilitySupport::Supported);
        assert_eq!(info.structured_output, CapabilitySupport::Supported);
        assert_eq!(info.reasoning, CapabilitySupport::Supported);
        let caps = ModelCapabilities::from_resolved(&info);
        assert!(caps.image_input && caps.image_tool_results);
        assert!(!caps.audio_input && !caps.video_input && !caps.pdf_input);
        assert!(caps.tool_calls && caps.parallel_tool_calls);
    }

    #[test]
    fn api_text_only_disables_all_media() {
        let info = normalize_openai_compatible_record(
            "some-text-model",
            &record(serde_json::json!({
                "architecture": { "input_modalities": ["text"] },
                "supported_parameters": ["tools"]
            })),
            provider_api(),
        );
        assert_eq!(info.image_input(), CapabilitySupport::Unsupported);
        assert_eq!(info.audio_input(), CapabilitySupport::Unsupported);
        assert_eq!(info.video_input(), CapabilitySupport::Unsupported);
        assert_eq!(info.pdf_input(), CapabilitySupport::Unsupported);
        let caps = ModelCapabilities::from_resolved(&info);
        assert!(!caps.image_input && !caps.image_tool_results);
        assert!(caps.tool_calls);
        assert!(!caps.parallel_tool_calls);
    }

    #[test]
    fn api_omitted_modalities_resolve_unknown_not_supported() {
        let info = normalize_openai_compatible_record(
            "some-model",
            &record(serde_json::json!({ "id": "some-model" })),
            provider_api(),
        );
        assert_eq!(info.image_input(), CapabilitySupport::Unknown);
        assert_eq!(info.audio_input(), CapabilitySupport::Unknown);
        assert_eq!(info.video_input(), CapabilitySupport::Unknown);
        assert_eq!(info.pdf_input(), CapabilitySupport::Unknown);
        assert_eq!(info.tool_calls, CapabilitySupport::Unknown);
        // Unknown never converts to Supported at the runtime boundary.
        let caps = ModelCapabilities::from_resolved(&info);
        assert!(!caps.image_input && !caps.image_tool_results);
        assert!(!caps.tool_calls);
    }

    #[test]
    fn api_audio_video_pdf_modalities_each_enable() {
        for (token, check) in [
            ("audio", Modality::Audio),
            ("video", Modality::Video),
            ("pdf", Modality::Pdf),
        ] {
            let info = normalize_openai_compatible_record(
                "m",
                &record(serde_json::json!({
                    "architecture": { "input_modalities": ["text", token] }
                })),
                provider_api(),
            );
            let support = match check {
                Modality::Audio => info.audio_input(),
                Modality::Video => info.video_input(),
                Modality::Pdf => info.pdf_input(),
                _ => unreachable!(),
            };
            assert_eq!(support, CapabilitySupport::Supported, "token {token}");
            assert_eq!(info.image_input(), CapabilitySupport::Unsupported);
        }
    }

    #[test]
    fn vision_named_model_without_metadata_must_not_enable_vision() {
        for name in [
            "super-vision-9000",
            "my-llava-finetune",
            "gpt-4o-vision-ultra",
            "qwen2.5-vl-7b",
        ] {
            let info = normalize_openai_compatible_record(
                name,
                &record(serde_json::json!({ "id": name })),
                provider_api(),
            );
            assert_eq!(
                info.image_input(),
                CapabilitySupport::Unknown,
                "model name {name} must not imply vision"
            );
            assert!(!ModelCapabilities::from_resolved(&info).image_tool_results);
        }
    }

    #[test]
    fn gpt_named_model_with_text_only_stays_text_only() {
        let info = normalize_openai_compatible_record(
            "gpt-text-only-turbo",
            &record(serde_json::json!({
                "architecture": { "input_modalities": ["text"] },
                "supported_parameters": []
            })),
            provider_api(),
        );
        assert_eq!(info.image_input(), CapabilitySupport::Unsupported);
        assert!(!ModelCapabilities::from_resolved(&info).image_input);
    }

    #[test]
    fn router_model_without_metadata_resolves_unknown() {
        for id in ["kilo-auto/free", "router/balanced", "auto/pick"] {
            let info = normalize_openai_compatible_record(
                id,
                &record(serde_json::json!({ "id": id })),
                provider_api(),
            );
            assert_eq!(
                info.image_input(),
                CapabilitySupport::Unknown,
                "router {id} must not inspect models behind it"
            );
            // Even when the router entry is missing from the list entirely.
            let list = normalize_openai_compatible_list(
                &serde_json::json!({ "data": [{ "id": "other-model" }] }),
                provider_api(),
            );
            assert!(list.iter().all(|model| model.id != id));
        }
    }

    #[test]
    fn lmstudio_vision_and_tool_metadata_normalize() {
        let info = normalize_lmstudio_record(
            "qwen-vl",
            &serde_json::json!({
                "key": "qwen-vl",
                "type": "vlm",
                "capabilities": { "vision": true, "trained_for_tool_use": true }
            }),
            provider_api(),
        );
        assert_eq!(info.image_input(), CapabilitySupport::Supported);
        assert_eq!(info.tool_calls, CapabilitySupport::Supported);

        let text = normalize_lmstudio_record(
            "llama",
            &serde_json::json!({
                "key": "llama",
                "type": "llm",
                "capabilities": { "vision": false, "trained_for_tool_use": false }
            }),
            provider_api(),
        );
        assert_eq!(text.image_input(), CapabilitySupport::Unsupported);
        assert_eq!(text.tool_calls, CapabilitySupport::Unsupported);

        // Legacy array shape.
        let legacy = normalize_lmstudio_record(
            "old",
            &serde_json::json!({ "key": "old", "capabilities": ["vision", "tool_use"] }),
            provider_api(),
        );
        assert_eq!(legacy.image_input(), CapabilitySupport::Supported);
        assert_eq!(legacy.tool_calls, CapabilitySupport::Supported);

        // No metadata at all stays Unknown — never inferred from the key.
        let bare = normalize_lmstudio_record(
            "llava-13b",
            &serde_json::json!({ "key": "llava-13b" }),
            provider_api(),
        );
        assert_eq!(bare.image_input(), CapabilitySupport::Unknown);
    }

    #[test]
    fn ollama_show_capabilities_normalize() {
        let vision = normalize_ollama_show(
            "llava:13b",
            &serde_json::json!({ "capabilities": ["completion", "vision"] }),
            provider_api(),
        );
        assert_eq!(vision.image_input(), CapabilitySupport::Supported);
        assert_eq!(vision.audio_input(), CapabilitySupport::Unsupported);

        let text = normalize_ollama_show(
            "llama3.1:8b",
            &serde_json::json!({ "capabilities": ["completion"] }),
            provider_api(),
        );
        assert_eq!(text.image_input(), CapabilitySupport::Unsupported);

        let unknown = normalize_ollama_show(
            "mystery",
            &serde_json::json!({ "model": "mystery" }),
            provider_api(),
        );
        assert_eq!(unknown.image_input(), CapabilitySupport::Unknown);
    }

    #[test]
    fn auth_identity_separates_anonymous_and_keyed_buckets() {
        assert_eq!(auth_identity(None), "anon");
        assert_eq!(auth_identity(Some("  ")), "anon");
        let first = auth_identity(Some("sk-one"));
        let second = auth_identity(Some("sk-two"));
        assert_ne!(first, second);
        assert_eq!(first, auth_identity(Some("sk-one")));
        assert!(
            !first.contains("sk-one"),
            "key must never leak into the identity"
        );
    }

    #[test]
    fn cache_key_changes_with_provider_base_url_and_key() {
        let base = CatalogRequest::new(
            "kilo",
            "https://api.kilo.ai/api/gateway",
            "kilo-auto/free",
            None,
        );
        let same = CatalogRequest::new(
            "kilo",
            "https://api.kilo.ai/api/gateway/",
            "kilo-auto/free",
            None,
        );
        assert_eq!(CacheKey::of(&base), CacheKey::of(&same));
        let other_provider = CatalogRequest::new(
            "openai",
            "https://api.kilo.ai/api/gateway",
            "kilo-auto/free",
            None,
        );
        assert_ne!(CacheKey::of(&base), CacheKey::of(&other_provider));
        let other_base =
            CatalogRequest::new("kilo", "https://api.kilo.ai/v2", "kilo-auto/free", None);
        assert_ne!(CacheKey::of(&base), CacheKey::of(&other_base));
        let keyed = CatalogRequest::new(
            "kilo",
            "https://api.kilo.ai/api/gateway",
            "kilo-auto/free",
            Some("sk-1".to_string()),
        );
        assert_ne!(CacheKey::of(&base), CacheKey::of(&keyed));
    }

    #[test]
    fn capability_error_note_downgrades_and_invalidates() {
        let service = ModelCatalogService::new();
        let config = CatalogRequest::new("kilo", "https://api.kilo.ai/api/gateway", "m", None);
        // Seed the cache directly: advertised image support.
        let advertised = ResolvedModelInfo {
            id: "m".to_string(),
            input_modalities: HashSet::from([Modality::Text, Modality::Image]),
            output_modalities: HashSet::from([Modality::Text]),
            modalities_known: true,
            tool_calls: CapabilitySupport::Supported,
            parallel_tool_calls: CapabilitySupport::Unknown,
            structured_output: CapabilitySupport::Unknown,
            reasoning: CapabilitySupport::Unknown,
            source: CapabilitySource::ProviderApi,
        };
        service.store(&config, vec![advertised]);
        assert_eq!(
            service.try_resolve_cached(&config).unwrap().image_input(),
            CapabilitySupport::Supported
        );
        // An explicit provider rejection suppresses the capability and drops
        // the cached catalog so the next async resolution reloads metadata.
        service.note_capability_error(&config, Modality::Image);
        assert!(service.try_resolve_cached(&config).is_none());
        service.store(
            &config,
            vec![ResolvedModelInfo {
                id: "m".to_string(),
                input_modalities: HashSet::from([Modality::Text, Modality::Image]),
                output_modalities: HashSet::from([Modality::Text]),
                modalities_known: true,
                tool_calls: CapabilitySupport::Supported,
                parallel_tool_calls: CapabilitySupport::Unknown,
                structured_output: CapabilitySupport::Unknown,
                reasoning: CapabilitySupport::Unknown,
                source: CapabilitySource::ProviderApi,
            }],
        );
        assert_eq!(
            service.try_resolve_cached(&config).unwrap().image_input(),
            CapabilitySupport::Unsupported,
            "stale advertised support must stay suppressed after reload"
        );
    }
}
