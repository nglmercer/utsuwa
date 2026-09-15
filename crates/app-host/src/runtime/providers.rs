//! Model provider configuration: settings keys, profiles, factory.
use super::RuntimeError;
use model_core::ModelProvider;
use model_openai_compatible::{AnthropicClient, OpenAICompatibleClient};
use std::sync::{Arc, Mutex};
use storage_core::Storage;

pub const SETTING_PROVIDER: &str = "model.provider";
pub const SETTING_BASE_URL: &str = "model.base_url";
pub const SETTING_API_KEY: &str = "model.api_key";
pub const SETTING_MODEL_NAME: &str = "model.name";
/// Explicit operator override for whether the configured model accepts
/// images. Written by `settings.set_model_provider` when the frontend
/// sends `vision`; when absent, [`model_supports_vision`] infers from the
/// provider id + model name instead. Explicit configuration always wins.
pub const SETTING_MODEL_VISION: &str = "model.vision";
/// Settings key holding the MCP server set (JSON array of server configs).
pub const SETTING_MCP_SERVERS: &str = "mcp.servers";
/// Settings key holding the WASM plugin directory (JSON string path).
/// Discovered every turn so installs take effect without a restart;
/// only `Enabled` plugins register tools, and every call stays behind
/// policy + tickets.
pub const SETTING_PLUGIN_DIR: &str = "plugin.dir";
/// Native persistent setting for the explicit Agent-only autonomous mode.
/// Missing or invalid values are treated as `false`.
pub const SETTING_AUTONOMOUS_FULL_ACCESS: &str = "agent.autonomous_full_access";
/// Optional native model-facing tool profile. When absent, local providers
/// use the small-model profile and other providers use the complete profile.
pub const SETTING_TOOL_PROFILE: &str = "agent.tool_profile";
/// Optional browser CDP endpoint (JSON string, e.g.
/// `"http://localhost:9333"`). Loopback only: remote or non-HTTP values
/// fail closed to the default and warn. Read per turn so changes take
/// effect without a restart. Never model-selectable.
pub const SETTING_BROWSER_CDP_ENDPOINT: &str = "browser.cdp_endpoint";

/// Resolve the host-configured CDP endpoint to a safe loopback value.
/// Non-loopback or malformed configuration warns and falls back to the
/// default instead of handing a remote debugger to the agent.
pub(crate) fn read_cdp_endpoint(storage: Option<&Arc<Mutex<Storage>>>) -> String {
    let configured: Option<String> = storage
        .and_then(|storage| storage.lock().ok())
        .and_then(|storage| storage.get_setting(SETTING_BROWSER_CDP_ENDPOINT).ok())
        .flatten()
        .and_then(|value| value.as_str().map(str::to_string));
    let endpoint = tool_browser::sanitize_cdp_endpoint(configured.as_deref());
    if let Some(configured) = configured {
        if endpoint != configured.trim().trim_end_matches('/').to_ascii_lowercase() {
            tracing::warn!(
                configured = %configured,
                resolved = %endpoint,
                "browser.cdp_endpoint is not a loopback http endpoint; using the safe default"
            );
        }
    }
    endpoint
}

pub(crate) fn read_mcp_configs(
    storage: Option<&Arc<Mutex<Storage>>>,
) -> Option<Vec<mcp_runtime::McpServerConfig>> {
    let storage = storage?;
    storage
        .lock()
        .ok()
        .and_then(|store| store.get_setting(SETTING_MCP_SERVERS).ok())
        .flatten()
        .and_then(|value| {
            serde_json::from_value(value)
                .map_err(|error| {
                    tracing::warn!(%error, "mcp.servers setting is not a server array; ignoring");
                })
                .ok()
        })
}

pub(crate) fn read_plugin_dir(storage: Option<&Arc<Mutex<Storage>>>) -> Option<Option<String>> {
    let storage = storage?;
    let dir: Option<String> = storage
        .lock()
        .ok()
        .and_then(|store| store.get_setting(SETTING_PLUGIN_DIR).ok())
        .flatten()
        .and_then(|value| {
            serde_json::from_value(value)
                .map_err(|error| {
                    tracing::warn!(%error, "plugin.dir setting is not a path string; ignoring");
                })
                .ok()
        });
    Some(dir)
}

pub use tool_sdk::ToolProfile;

pub fn tool_profile_for_provider(provider: Option<&str>) -> ToolProfile {
    match provider
        .map(str::trim)
        .filter(|provider| !provider.is_empty())
    {
        Some(provider)
            if provider.eq_ignore_ascii_case("lmstudio")
                || provider.eq_ignore_ascii_case("ollama") =>
        {
            ToolProfile::Simple
        }
        _ => ToolProfile::Full,
    }
}

pub(crate) fn configured_tool_profile(storage: Option<&Arc<Mutex<Storage>>>) -> ToolProfile {
    let Some(storage) = storage else {
        return ToolProfile::Full;
    };
    let Ok(storage) = storage.lock() else {
        tracing::warn!("tool profile storage lock failed; using full profile");
        return ToolProfile::Full;
    };
    if let Ok(Some(value)) = storage.get_setting(SETTING_TOOL_PROFILE) {
        if let Some(profile) = value.as_str() {
            match profile.trim().to_ascii_lowercase().as_str() {
                "simple" => return ToolProfile::Simple,
                "full" => return ToolProfile::Full,
                "minimal" => return ToolProfile::Minimal,
                "standard" => return ToolProfile::Standard,
                "developer" => return ToolProfile::Developer,
                "computeruse" | "computer-use" | "computer_use" => return ToolProfile::ComputerUse,
                _ => tracing::warn!(
                    profile = %profile,
                    "unknown agent.tool_profile value; inferring from provider"
                ),
            }
        }
    }
    let provider = storage
        .get_setting(SETTING_PROVIDER)
        .ok()
        .flatten()
        .and_then(|value| value.as_str().map(str::to_owned));
    tool_profile_for_provider(provider.as_deref())
}

/// Read the native persistent mode setting. A missing or malformed value is
/// the safe default: autonomous access is disabled.
pub(crate) fn read_autonomous_full_access(storage: Option<&Arc<Mutex<Storage>>>) -> Option<bool> {
    let storage = storage?;
    let storage = storage.lock().ok()?;
    let value = storage.get_setting(SETTING_AUTONOMOUS_FULL_ACCESS).ok()?;
    Some(value.and_then(|value| value.as_bool()).unwrap_or(false))
}

pub(crate) fn provider_factory_with_secrets(
    storage: Option<Arc<Mutex<Storage>>>,
    secrets: Arc<dyn secret_core::SecretStore>,
) -> Arc<dyn Fn() -> Result<Arc<dyn ModelProvider>, RuntimeError> + Send + Sync> {
    Arc::new(move || {
        let storage = storage.as_ref().ok_or(RuntimeError::ModelNotConfigured)?;
        let storage = storage
            .lock()
            .map_err(|_| RuntimeError::Settings("storage lock failed".to_string()))?;
        let get = |key: &str| -> Result<Option<String>, RuntimeError> {
            storage
                .get_setting(key)
                .map_err(|e| RuntimeError::Settings(e.to_string()))
                .map(|v| v.and_then(|v| v.as_str().map(str::to_string)))
        };
        let raw_base_url = get(SETTING_BASE_URL)?
            .filter(|s| !s.is_empty())
            .ok_or(RuntimeError::ModelNotConfigured)?;
        let name = get(SETTING_MODEL_NAME)?
            .filter(|s| !s.is_empty())
            .ok_or(RuntimeError::ModelNotConfigured)?;
        // Older databases may lack the provider id, so retain compatibility
        // by treating them as generic OpenAI-compatible endpoints.
        let provider = get(SETTING_PROVIDER)?.unwrap_or_else(|| "openai-compatible".to_string());
        // Frontend synchronization normalizes this already, but older native
        // databases can contain a bare LM Studio/Ollama host or a pasted full
        // endpoint. Normalize at the provider boundary as a defense in depth.
        let base_url = if provider == "anthropic" {
            raw_base_url.trim_end_matches('/').to_string()
        } else {
            normalize_provider_base_url(&provider, &raw_base_url)
        };
        let api_key = resolve_api_key(&storage, secrets.as_ref())?;
        let vision = storage
            .get_setting(SETTING_MODEL_VISION)
            .ok()
            .flatten()
            .and_then(|value| value.as_bool());
        tracing::debug!(
            provider = %provider,
            model = %name,
            normalized_base_url = %sanitize_provider_url_for_log(&base_url),
            vision_override = ?vision,
            "native agent provider selected"
        );
        ProviderRegistry::default_registry().create(
            &provider,
            ProviderConfig {
                provider: provider.clone(),
                base_url,
                name,
                api_key,
                vision,
            },
        )
    })
}

/// Resolved, secret-free-except-key parameters for one provider instance.
/// Settings/secret reading stays in [`provider_factory_with_secrets`];
/// factories only construct clients.
pub struct ProviderConfig {
    pub provider: String,
    pub base_url: String,
    pub name: String,
    pub api_key: Option<String>,
    /// Explicit vision override from [`SETTING_MODEL_VISION`]. `None`
    /// means "infer from provider + model name".
    pub vision: Option<bool>,
}

/// Infer whether a provider/model pair accepts image input. This mirrors
/// the frontend gate (`canShowImages` in
/// `src/lib/services/providers/vision.ts`, plus the `supportsVision`
/// flags in `registry.ts`): a flagged cloud provider or a local provider,
/// AND a model id that looks vision-capable. Keep the two in sync — a
/// mismatch means the UI offers vision the host then degrades to metadata
/// text, or vice versa.
///
/// An explicit [`SETTING_MODEL_VISION`] value always wins over this
/// heuristic; it exists so gateways and new model ids work without a
/// code change.
pub fn model_supports_vision(provider_id: &str, model_name: &str) -> bool {
    let provider = provider_id.trim().to_ascii_lowercase();
    let provider_has_vision =
        matches!(provider.as_str(), "openai" | "anthropic" | "google" | "xai");
    let is_local = matches!(provider.as_str(), "ollama" | "lmstudio");
    if !provider_has_vision && !is_local {
        return false;
    }
    model_name_looks_vision_capable(model_name)
}

/// Substrings that strongly imply a model accepts images. Lowercase;
/// mirrors `VISION_MODEL_HINTS` in `src/lib/services/providers/vision.ts`.
const VISION_MODEL_HINTS: &[&str] = &[
    "vision",
    "-vl",
    "vl-",
    "llava",
    "bakllava",
    "moondream",
    "minicpm-v",
    "llama3.2-vision",
    "llama-3.2-vision",
    "qwen2-vl",
    "qwen2.5-vl",
    "gemma3",
    "pixtral",
    "internvl",
    "gpt-4o",
    "gpt-4.1",
    "gpt-4-turbo",
    "gpt-4-vision",
    "gpt-5",
    "o3",
    "o4",
    "claude-3",
    "claude-4",
    "claude-opus",
    "claude-sonnet",
    "claude-haiku",
    "gemini",
    "grok-2-vision",
    "grok-4",
    "llama4",
    "llama-4",
    "mistral-small-3",
    "phi-3.5-vision",
    "phi-4-multimodal",
];

/// Names that match a hint but are actually text-only. Mirrors
/// `TEXT_ONLY_MODELS` in `src/lib/services/providers/vision.ts`.
const TEXT_ONLY_MODELS: &[&str] = &["gemma3:1b", "gemma-3-1b", "gemma3:270m"];

fn model_name_looks_vision_capable(model_name: &str) -> bool {
    let name = model_name.to_ascii_lowercase();
    if name.is_empty() {
        return false;
    }
    if TEXT_ONLY_MODELS.iter().any(|denied| name.contains(denied)) {
        return false;
    }
    VISION_MODEL_HINTS.iter().any(|hint| name.contains(hint))
}

/// Constructs one provider family from a resolved [`ProviderConfig`].
/// Adding a provider means adding a factory + one registry line —
/// never touching the settings/caching code above.
pub trait ModelProviderFactory: Send + Sync {
    fn id(&self) -> &'static str;

    /// Whether this factory handles `provider_id`. The OpenAI-compatible
    /// factory answers true as the legacy fallback (older databases may
    /// lack a provider id entirely).
    fn supports(&self, provider_id: &str) -> bool {
        self.id() == provider_id
    }

    fn create(&self, config: ProviderConfig) -> Result<Arc<dyn ModelProvider>, RuntimeError>;
}

/// Anthropic Messages API client.
pub struct AnthropicProviderFactory;

impl ModelProviderFactory for AnthropicProviderFactory {
    fn id(&self) -> &'static str {
        "anthropic"
    }

    fn create(&self, config: ProviderConfig) -> Result<Arc<dyn ModelProvider>, RuntimeError> {
        let api_key = config.api_key.ok_or(RuntimeError::ModelNotConfigured)?;
        Ok(
            Arc::new(AnthropicClient::new(config.base_url, api_key, config.name))
                as Arc<dyn ModelProvider>,
        )
    }
}

/// Generic OpenAI-compatible chat-completions client (OpenAI, LM Studio,
/// Ollama, and arbitrary gateways). Also the fallback for unknown or
/// missing provider ids.
pub struct OpenAiCompatibleProviderFactory;

impl ModelProviderFactory for OpenAiCompatibleProviderFactory {
    fn id(&self) -> &'static str {
        "openai-compatible"
    }

    fn supports(&self, _provider_id: &str) -> bool {
        true
    }

    fn create(&self, config: ProviderConfig) -> Result<Arc<dyn ModelProvider>, RuntimeError> {
        // Without image tool results, screenshots degrade to metadata text
        // and a vision-capable model never sees pixels. Explicit operator
        // configuration wins; otherwise infer from provider + model name.
        let vision = config
            .vision
            .unwrap_or_else(|| model_supports_vision(&config.provider, &config.name));
        tracing::debug!(
            provider = %config.provider,
            model = %config.name,
            vision,
            explicit = config.vision.is_some(),
            "openai-compatible vision capability resolved"
        );
        let mut capabilities = model_core::ModelCapabilities::default();
        if vision {
            capabilities = capabilities.with_image_tool_results(true);
        }
        Ok(Arc::new(
            OpenAICompatibleClient::new(config.base_url, config.api_key, config.name)
                .with_capabilities(capabilities),
        ) as Arc<dyn ModelProvider>)
    }
}

/// Ordered factory set; first `supports` match wins. The fallback factory
/// must be registered last.
pub struct ProviderRegistry {
    factories: Vec<Arc<dyn ModelProviderFactory>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self {
            factories: Vec::new(),
        }
    }

    pub fn register<F: ModelProviderFactory + 'static>(mut self, factory: F) -> Self {
        self.factories.push(Arc::new(factory));
        self
    }

    pub fn default_registry() -> Self {
        Self::new()
            .register(AnthropicProviderFactory)
            .register(OpenAiCompatibleProviderFactory)
    }

    pub fn create(
        &self,
        provider_id: &str,
        config: ProviderConfig,
    ) -> Result<Arc<dyn ModelProvider>, RuntimeError> {
        for factory in &self.factories {
            if factory.supports(provider_id) {
                if provider_requires_api_key(provider_id) && config.api_key.is_none() {
                    return Err(RuntimeError::ModelNotConfigured);
                }
                return factory.create(config);
            }
        }
        Err(RuntimeError::ModelNotConfigured)
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Normalize only endpoint semantics known by the native provider factory.
/// LM Studio and Ollama expose their OpenAI-compatible chat API below `/v1`;
/// other providers gain `/v1` only when the configured URL is a bare host
/// (`https://api.example.com` → `https://api.example.com/v1`) — otherwise a
/// model list lands on `{host}/models` and chat on `{host}/chat/completions`,
/// where gateways answer with HTML landing pages instead of the API.
/// Arbitrary OpenAI-compatible gateways with a configured sub-path keep it
/// intact, and Anthropic stays untouched (header-versioned at the root).
pub fn normalize_provider_base_url(provider: &str, base_url: &str) -> String {
    let mut normalized = base_url.trim().trim_end_matches('/').to_string();
    const CHAT_SUFFIX: &str = "/chat/completions";
    if normalized.to_ascii_lowercase().ends_with(CHAT_SUFFIX) {
        normalized.truncate(normalized.len() - CHAT_SUFFIX.len());
    }

    let needs_v1 = matches!(provider, "lmstudio" | "ollama")
        && !normalized.to_ascii_lowercase().ends_with("/v1")
        || provider != "anthropic" && is_bare_host_url(&normalized);
    if needs_v1 {
        normalized.push_str("/v1");
    }
    normalized
}

/// True when the URL carries no path beyond the root, so the OpenAI `/v1`
/// convention can be assumed without overriding an operator's sub-path.
/// Unparsable values are left alone rather than guessed.
fn is_bare_host_url(url: &str) -> bool {
    url::Url::parse(url)
        .map(|parsed| parsed.path() == "/" || parsed.path().is_empty())
        .unwrap_or(false)
}

fn sanitize_provider_url_for_log(base_url: &str) -> String {
    base_url
        .split(['?', '#'])
        .next()
        .unwrap_or(base_url)
        .to_string()
}

fn provider_requires_api_key(provider: &str) -> bool {
    matches!(
        provider,
        "openai" | "google" | "deepseek" | "xai" | "groq" | "mistral"
    )
}

/// API key resolution order: OS keychain first; then a one-time migration
/// of the legacy plaintext `model.api_key` settings value into the
/// keychain (the settings row is deleted afterwards). The key is never
/// logged, never exposed to the model context, and never forwarded except
/// to the configured provider.
fn resolve_api_key(
    storage: &Storage,
    secrets: &dyn secret_core::SecretStore,
) -> Result<Option<String>, RuntimeError> {
    match secrets.get(secret_core::ACCOUNT_MODEL_API_KEY) {
        Ok(Some(key)) if !key.is_empty() => return Ok(Some(key)),
        Ok(_) => {}
        Err(e) => tracing::warn!(%e, "secret store unreadable; checking legacy settings"),
    }
    let legacy = storage
        .get_setting(SETTING_API_KEY)
        .map_err(|e| RuntimeError::Settings(e.to_string()))?
        .and_then(|v| v.as_str().map(str::to_string))
        .filter(|s| !s.is_empty());
    if let Some(key) = legacy {
        match secrets.set(secret_core::ACCOUNT_MODEL_API_KEY, &key) {
            Ok(()) => {
                storage
                    .delete_setting(SETTING_API_KEY)
                    .map_err(|e| RuntimeError::Settings(format!(
                        "migrated model API key but could not delete its plaintext settings row: {e}"
                    )))?;
                tracing::info!("migrated model.api_key from settings to the OS keychain");
            }
            Err(e) => {
                return Err(RuntimeError::Settings(format!(
                    "cannot move the legacy model API key into native secret storage: {e}"
                )));
            }
        }
        return Ok(Some(key));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(provider: &str, name: &str, vision: Option<bool>) -> ProviderConfig {
        ProviderConfig {
            provider: provider.to_string(),
            base_url: "https://api.example.com/v1".to_string(),
            name: name.to_string(),
            api_key: None,
            vision,
        }
    }

    #[test]
    fn vision_heuristic_matches_cloud_and_local_models() {
        // Flagged cloud provider + vision model name.
        assert!(model_supports_vision("openai", "gpt-4o"));
        assert!(model_supports_vision("openai", "GPT-5-mini"));
        assert!(model_supports_vision("anthropic", "claude-sonnet-4-5"));
        assert!(model_supports_vision("google", "gemini-2.5-flash"));
        // Local providers depend on the installed model.
        assert!(model_supports_vision("ollama", "llava:13b"));
        assert!(model_supports_vision("ollama", "qwen2.5-vl:7b"));
        assert!(model_supports_vision("lmstudio", "gemma3:4b"));
        // Text-only models stay text-only everywhere.
        assert!(!model_supports_vision("openai", "gpt-3.5-turbo"));
        assert!(!model_supports_vision("ollama", "llama3.1:8b"));
        assert!(!model_supports_vision("ollama", "gemma3:1b"));
        assert!(!model_supports_vision("ollama", ""));
        // Unflagged providers never infer vision (explicit opt-in only).
        assert!(!model_supports_vision("deepseek", "deepseek-vl2"));
        assert!(!model_supports_vision("kilo", "gpt-4o"));
        assert!(!model_supports_vision("openai-compatible", "llava"));
    }

    #[test]
    fn factory_applies_explicit_vision_override_over_heuristic() {
        let factory = OpenAiCompatibleProviderFactory;
        // Heuristic on: gateway model id the heuristic does not know.
        let inferred = factory.create(config("openai", "gpt-4o", None)).unwrap();
        assert!(inferred.capabilities().image_tool_results);
        assert!(inferred.capabilities().image_input);
        // Explicit true wins over a negative heuristic (unknown gateway id).
        let forced = factory
            .create(config("openai-compatible", "my-custom-vlm-9b", Some(true)))
            .unwrap();
        assert!(forced.capabilities().image_tool_results);
        // Explicit false wins over a positive heuristic.
        let denied = factory
            .create(config("openai", "gpt-4o", Some(false)))
            .unwrap();
        assert!(!denied.capabilities().image_tool_results);
        assert!(!denied.capabilities().image_input);
        // Default stays conservative for text models.
        let text = factory
            .create(config("ollama", "llama3.1:8b", None))
            .unwrap();
        assert!(!text.capabilities().image_tool_results);
    }
}
