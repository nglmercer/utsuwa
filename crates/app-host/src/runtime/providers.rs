//! Model provider configuration: settings keys, profiles, factory.
//!
//! Capabilities are discovered from the provider's real model/catalog API
//! via [`model_catalog::ModelCatalogService`] — never from model-name
//! substrings or provider-id assumptions. The stored `model.vision` value
//! is only a debug override (`--vision on|off`); when absent, the
//! API-discovered capability decides.
use super::RuntimeError;
use model_catalog::{CatalogRequest, ModelCatalogService};
use model_core::{CapabilitySource, ModelCapabilities, ModelProvider, ResolvedModelInfo};
use model_openai_compatible::{AnthropicClient, CapabilityContext, OpenAICompatibleClient};
use std::sync::{Arc, Mutex};
use storage_core::Storage;

pub const SETTING_PROVIDER: &str = "model.provider";
pub const SETTING_BASE_URL: &str = "model.base_url";
pub const SETTING_API_KEY: &str = "model.api_key";
pub const SETTING_MODEL_NAME: &str = "model.name";
/// Debug override for whether the configured model accepts images. Written
/// by `settings.set_model_provider` when the caller sends `vision`
/// (`--vision on|off`); when absent, the provider API's advertised
/// capability decides (`--vision auto`). An explicit override always wins
/// over discovery.
pub const SETTING_MODEL_VISION: &str = "model.vision";

/// Async provider constructor. Resolution is async because capability
/// discovery fetches the provider's `/models` catalog over HTTP; blocking
/// settings/keychain reads hop to the blocking pool inside.
pub type ProviderFactory = Arc<
    dyn Fn()
            -> futures_util::future::BoxFuture<'static, Result<Arc<dyn ModelProvider>, RuntimeError>>
        + Send
        + Sync,
>;

/// Wrap a synchronous constructor (tests inject stub providers) as a
/// [`ProviderFactory`]. Construction runs inside the future so a stub
/// panic surfaces through the factory future like any real provider
/// failure instead of unwinding through the factory call site.
pub fn sync_factory(
    build: impl Fn() -> Result<Arc<dyn ModelProvider>, RuntimeError> + Send + Sync + 'static,
) -> ProviderFactory {
    let build = Arc::new(build);
    Arc::new(move || {
        let build = Arc::clone(&build);
        Box::pin(async move { build() })
            as futures_util::future::BoxFuture<
                'static,
                Result<Arc<dyn ModelProvider>, RuntimeError>,
            >
    })
}
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
) -> ProviderFactory {
    // The catalog service is shared across turns so `/models` responses are
    // cached per provider + base URL + auth identity (5–30 minute TTL).
    let catalog = Arc::new(ModelCatalogService::new());
    Arc::new(move || {
        let storage = storage.clone();
        let secrets = Arc::clone(&secrets);
        let catalog = Arc::clone(&catalog);
        Box::pin(async move {
            // The settings/keychain reads perform blocking work (SQLite plus
            // synchronous OS-keychain IPC, which enters a nested Tokio
            // runtime on Linux), so they run on the blocking pool while the
            // catalog HTTP below stays on this async worker.
            let config = tokio::task::spawn_blocking(move || {
                read_provider_config(storage.as_ref(), secrets.as_ref())
            })
            .await
            .map_err(|err| RuntimeError::Executor(format!("provider task failed: {err}")))??;
            // Fail fast before any catalog network when a key is required.
            if provider_requires_api_key(&config.provider) && config.api_key.is_none() {
                return Err(RuntimeError::ModelNotConfigured);
            }
            let resolved = resolve_provider_capabilities(&config, &catalog).await;
            tracing::debug!(
                provider = %config.provider,
                model = %config.name,
                normalized_base_url = %sanitize_provider_url_for_log(&config.base_url),
                capabilities_source = %resolved.info.source,
                image_input = %resolved.info.image_input(),
                vision_override = ?config.vision,
                "native agent provider selected"
            );
            let provider_id = config.provider.clone();
            ProviderRegistry::default_registry().create(&provider_id, config, &resolved, &catalog)
        })
            as futures_util::future::BoxFuture<
                'static,
                Result<Arc<dyn ModelProvider>, RuntimeError>,
            >
    })
}

/// Read the stored model identity. Blocking-safe (SQLite + sync keychain
/// IPC): callers run this on the blocking pool, never on an async worker.
fn read_provider_config(
    storage: Option<&Arc<Mutex<Storage>>>,
    secrets: &dyn secret_core::SecretStore,
) -> Result<ProviderConfig, RuntimeError> {
    let storage = storage.ok_or(RuntimeError::ModelNotConfigured)?;
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
    let api_key = resolve_api_key(&storage, secrets)?;
    let vision = storage
        .get_setting(SETTING_MODEL_VISION)
        .ok()
        .flatten()
        .and_then(|value| value.as_bool());
    Ok(ProviderConfig {
        provider,
        base_url,
        name,
        api_key,
        vision,
    })
}

/// Resolved, secret-free-except-key parameters for one provider instance.
/// Settings/secret reading stays in [`read_provider_config`]; factories
/// only construct clients.
pub struct ProviderConfig {
    pub provider: String,
    pub base_url: String,
    pub name: String,
    pub api_key: Option<String>,
    /// Debug vision override from [`SETTING_MODEL_VISION`]. `None` (`auto`)
    /// means "use the API-discovered capability"; `Some(true)` forces
    /// image sending and `Some(false)` force-disables it.
    pub vision: Option<bool>,
}

/// API-discovered capabilities for one configured model, plus the catalog
/// identity the adapter needs for stale-metadata recovery.
pub struct ProviderCapabilities {
    pub info: ResolvedModelInfo,
    pub capabilities: ModelCapabilities,
    pub catalog_request: CatalogRequest,
}

/// Resolve capabilities for the selected model before creating its client:
///
/// ```text
/// selected model
///     ↓ ModelCatalogService.resolve_model()
/// ResolvedModelInfo
///     ↓ ModelCapabilities::from_resolved
/// ModelCapabilities (+ debug override) → ModelProvider
/// ```
///
/// Only explicit `Supported` enables a capability. `Unknown` (catalog
/// silent or unreachable) maps to the safe text/tool fallback.
pub async fn resolve_provider_capabilities(
    config: &ProviderConfig,
    catalog: &ModelCatalogService,
) -> ProviderCapabilities {
    let catalog_request = CatalogRequest::new(
        config.provider.clone(),
        config.base_url.clone(),
        config.name.clone(),
        config.api_key.clone(),
    );
    let mut info = catalog.resolve_model_or_unknown(&catalog_request).await;
    let mut capabilities = ModelCapabilities::from_resolved(&info);
    match config.vision {
        Some(true) => {
            capabilities.image_input = true;
            capabilities.image_tool_results = true;
            info.source = CapabilitySource::DebugOverride;
        }
        Some(false) => {
            capabilities.image_input = false;
            capabilities.image_tool_results = false;
        }
        None => {}
    }
    ProviderCapabilities {
        info,
        capabilities,
        catalog_request,
    }
}

/// Constructs one provider family from a resolved [`ProviderConfig`]
/// plus its API-discovered [`ProviderCapabilities`]. Adding a provider
/// means adding a factory + one registry line — never touching the
/// settings/caching code above.
pub trait ModelProviderFactory: Send + Sync {
    fn id(&self) -> &'static str;

    /// Whether this factory handles `provider_id`. The OpenAI-compatible
    /// factory answers true as the legacy fallback (older databases may
    /// lack a provider id entirely).
    fn supports(&self, provider_id: &str) -> bool {
        self.id() == provider_id
    }

    fn create(
        &self,
        config: ProviderConfig,
        resolved: &ProviderCapabilities,
        catalog: &Arc<ModelCatalogService>,
    ) -> Result<Arc<dyn ModelProvider>, RuntimeError>;
}

fn capability_context(
    resolved: &ProviderCapabilities,
    catalog: &Arc<ModelCatalogService>,
) -> CapabilityContext {
    CapabilityContext {
        catalog: Arc::clone(catalog),
        request: model_catalog::CatalogRequest::new(
            resolved.catalog_request.provider.clone(),
            resolved.catalog_request.base_url.clone(),
            resolved.catalog_request.model.clone(),
            resolved.catalog_request.api_key.clone(),
        ),
    }
}

/// Anthropic Messages API client.
pub struct AnthropicProviderFactory;

impl ModelProviderFactory for AnthropicProviderFactory {
    fn id(&self) -> &'static str {
        "anthropic"
    }

    fn create(
        &self,
        config: ProviderConfig,
        resolved: &ProviderCapabilities,
        catalog: &Arc<ModelCatalogService>,
    ) -> Result<Arc<dyn ModelProvider>, RuntimeError> {
        let api_key = config.api_key.ok_or(RuntimeError::ModelNotConfigured)?;
        Ok(Arc::new(
            AnthropicClient::new(config.base_url, api_key, config.name)
                .with_capabilities(resolved.capabilities)
                .with_capability_context(capability_context(resolved, catalog)),
        ) as Arc<dyn ModelProvider>)
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

    fn create(
        &self,
        config: ProviderConfig,
        resolved: &ProviderCapabilities,
        catalog: &Arc<ModelCatalogService>,
    ) -> Result<Arc<dyn ModelProvider>, RuntimeError> {
        tracing::debug!(
            provider = %config.provider,
            model = %config.name,
            capabilities_source = %resolved.info.source,
            image_input = %resolved.info.image_input(),
            explicit_override = config.vision.is_some(),
            "openai-compatible capabilities resolved"
        );
        Ok(Arc::new(
            OpenAICompatibleClient::new(config.base_url, config.api_key, config.name)
                .with_capabilities(resolved.capabilities)
                .with_capability_context(capability_context(resolved, catalog)),
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
        resolved: &ProviderCapabilities,
        catalog: &Arc<ModelCatalogService>,
    ) -> Result<Arc<dyn ModelProvider>, RuntimeError> {
        for factory in &self.factories {
            if factory.supports(provider_id) {
                if provider_requires_api_key(provider_id) && config.api_key.is_none() {
                    return Err(RuntimeError::ModelNotConfigured);
                }
                return factory.create(config, resolved, catalog);
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

    /// Serve one canned `/models` payload, returning the base URL to point
    /// the catalog at. The socket stays open for the catalog's single fetch.
    async fn mock_models_server(
        payload: serde_json::Value,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            let mut chunk = [0u8; 4096];
            while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                let n = socket.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                head.extend_from_slice(&chunk[..n]);
            }
            let body = payload.to_string();
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        (base_url, handle)
    }

    #[tokio::test]
    async fn auto_uses_api_discovered_capabilities() {
        let catalog = ModelCatalogService::new();
        // Advertised image input -> images enabled, regardless of the name.
        let (base_url, server) = mock_models_server(serde_json::json!({
            "data": [{
                "id": "plain-name-7b",
                "architecture": { "input_modalities": ["text", "image"] },
                "supported_parameters": ["tools"],
            }],
        }))
        .await;
        let discovered = ProviderConfig {
            base_url,
            ..config("openai-compatible", "plain-name-7b", None)
        };
        let resolved = resolve_provider_capabilities(&discovered, &catalog).await;
        server.await.unwrap();
        assert_eq!(
            resolved.info.image_input(),
            model_core::CapabilitySupport::Supported
        );
        assert!(resolved.capabilities.image_tool_results);

        // Text-only advertisement -> images disabled, even for a vision-y name.
        let (base_url, server) = mock_models_server(serde_json::json!({
            "data": [{
                "id": "gpt-4o-vision-ultra",
                "architecture": { "input_modalities": ["text"] },
                "supported_parameters": ["tools"],
            }],
        }))
        .await;
        let discovered = ProviderConfig {
            base_url,
            ..config("openai-compatible", "gpt-4o-vision-ultra", None)
        };
        let resolved = resolve_provider_capabilities(&discovered, &catalog).await;
        server.await.unwrap();
        assert_eq!(
            resolved.info.image_input(),
            model_core::CapabilitySupport::Unsupported
        );
        assert!(!resolved.capabilities.image_tool_results);

        // Silent catalog entry -> Unknown -> conservative fallback.
        let (base_url, server) = mock_models_server(serde_json::json!({
            "data": [{ "id": "llava-finetune-13b" }],
        }))
        .await;
        let discovered = ProviderConfig {
            base_url,
            ..config("openai-compatible", "llava-finetune-13b", None)
        };
        let resolved = resolve_provider_capabilities(&discovered, &catalog).await;
        server.await.unwrap();
        assert_eq!(
            resolved.info.image_input(),
            model_core::CapabilitySupport::Unknown
        );
        assert!(!resolved.capabilities.image_tool_results);
    }

    #[tokio::test]
    async fn unreachable_catalog_fails_open_to_conservative_unknown() {
        let catalog = ModelCatalogService::new();
        // Nothing listens on the discard port: instant connection refused.
        let discovered = ProviderConfig {
            base_url: "http://127.0.0.1:9/".to_string(),
            ..config("openai-compatible", "llava:13b", None)
        };
        let resolved = resolve_provider_capabilities(&discovered, &catalog).await;
        assert_eq!(
            resolved.info.image_input(),
            model_core::CapabilitySupport::Unknown
        );
        assert!(!resolved.capabilities.image_tool_results);
    }

    #[tokio::test]
    async fn factory_applies_explicit_vision_override_over_discovery() {
        let catalog = Arc::new(ModelCatalogService::new());
        let factory = OpenAiCompatibleProviderFactory;
        let registry = ProviderRegistry::default_registry();
        // Closed port: discovery always yields Unknown here.
        let closed = "http://127.0.0.1:9/".to_string();

        // Explicit true forces images on (debug override).
        let forced = ProviderConfig {
            base_url: closed.clone(),
            ..config("openai-compatible", "text-model", Some(true))
        };
        let resolved = resolve_provider_capabilities(&forced, &catalog).await;
        let client = registry
            .create(&forced.provider.clone(), forced, &resolved, &catalog)
            .unwrap();
        assert!(client.capabilities().image_tool_results);

        // Explicit false forces images off even when advertised.
        let (base_url, server) = mock_models_server(serde_json::json!({
            "data": [{
                "id": "vlm-9b",
                "architecture": { "input_modalities": ["text", "image"] },
                "supported_parameters": ["tools"],
            }],
        }))
        .await;
        let denied = ProviderConfig {
            base_url,
            ..config("openai-compatible", "vlm-9b", Some(false))
        };
        let resolved = resolve_provider_capabilities(&denied, &catalog).await;
        server.await.unwrap();
        assert_eq!(
            resolved.info.image_input(),
            model_core::CapabilitySupport::Supported,
            "discovery still reports the advertisement"
        );
        assert!(
            !resolved.capabilities.image_tool_results,
            "but the override disables sending"
        );
        let client = registry
            .create(&denied.provider.clone(), denied, &resolved, &catalog)
            .unwrap();
        assert!(!client.capabilities().image_tool_results);

        // Auto with silent entry stays conservative for the factory too.
        let silent = ProviderConfig {
            base_url: closed,
            ..config("ollama", "llama3.1:8b", None)
        };
        let resolved = resolve_provider_capabilities(&silent, &catalog).await;
        let client = factory
            .create(
                ProviderConfig {
                    provider: silent.provider.clone(),
                    base_url: silent.base_url.clone(),
                    name: silent.name.clone(),
                    api_key: None,
                    vision: None,
                },
                &resolved,
                &catalog,
            )
            .unwrap();
        assert!(!client.capabilities().image_tool_results);
    }
}
