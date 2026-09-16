//! Provider-network IPC.
//!
//! The embedded WebView cannot directly call every public provider because
//! some documented APIs do not opt into browser CORS. Keep the network hop in
//! the common OpenAI-compatible Rust client and return the raw model catalog
//! to the frontend for the same parsing/classification used by web builds.

use super::Dispatcher;
use ipc_core::{ErrorCode, IpcErrorBody, IpcRequest};
use model_openai_compatible::OpenAICompatibleClient;
use serde_json::Value;

const MAX_PROVIDER_VALUE_LENGTH: usize = 4096;

impl Dispatcher {
    pub(crate) async fn fetch_provider_models(
        &self,
        request: &IpcRequest,
    ) -> Result<Value, IpcErrorBody> {
        let provider = request
            .params
            .get("provider")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| invalid_params("providers.fetch_models needs 'provider'"))?;
        validate_provider_value(provider, "provider")?;

        let base_url = request
            .params
            .get("base_url")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| invalid_params("providers.fetch_models needs 'base_url'"))?;
        let base_url = validate_provider_base_url(base_url)?;

        // A supplied key is an explicit per-request override. When omitted,
        // only Kilo resolves the native keychain value (without ever sending
        // it back to the WebView); every other provider stays anonymous so a
        // stored key is never attached to the wrong endpoint. An explicit
        // blank key means anonymous access.
        let api_key = if let Some(value) = request.params.get("api_key") {
            let value = value.as_str().ok_or_else(|| {
                invalid_params("providers.fetch_models 'api_key' must be a string")
            })?;
            normalize_api_key(value)
        } else if provider == "kilo" {
            // The secret store is synchronous OS IPC (on Linux it enters a
            // nested runtime), so it must never run on this async worker.
            // Read it on the blocking pool; a failed read falls back to an
            // anonymous catalog rather than failing the whole fetch.
            let stored = match self.secrets.clone() {
                None => None,
                Some(secrets) => tokio::task::spawn_blocking(move || {
                    secrets
                        .get(secret_core::ACCOUNT_MODEL_API_KEY)
                        .ok()
                        .flatten()
                })
                .await
                .unwrap_or_else(|err| {
                    tracing::warn!(%err, "secret store read failed; fetching anonymous catalog");
                    None
                }),
            };
            stored.and_then(|key| normalize_api_key(&key))
        } else {
            None
        };

        tracing::debug!(
            provider = %provider,
            base_url = %sanitize_provider_url(&base_url),
            authenticated = api_key.is_some(),
            "fetching provider model catalog through native HTTP"
        );

        let authenticated = api_key.is_some();
        let client = OpenAICompatibleClient::new(base_url, api_key, "");
        // Provider-specific catalog endpoints mirror the frontend's direct
        // fetch table (client-models.ts): same paths, same headers, same
        // envelope expectations. The frontend keeps parsing/classifying.
        let catalog = match provider {
            "anthropic" => client.fetch_anthropic_models().await,
            "ollama" => client.fetch_ollama_tags().await,
            "lmstudio" => client.fetch_lmstudio_models().await,
            "google" => client.fetch_google_models().await,
            "elevenlabs" => client.fetch_elevenlabs_models().await,
            "openai" | "deepseek" | "xai" | "kilo" | "openai-tts" => client.fetch_models().await,
            "openai-compatible" if looks_like_ollama(client.base_url()) => {
                client.fetch_ollama_tags().await
            }
            "openai-compatible" => client.fetch_models().await,
            _ => {
                return Err(invalid_params(format!(
                    "providers.fetch_models unknown provider '{provider}'"
                )))
            }
        }
        .map_err(|error| IpcErrorBody {
            code: ErrorCode::Internal,
            message: error.to_string(),
        })?;

        // Keep the key-use bit explicit because the frontend intentionally
        // does not hydrate keychain credentials into WebView memory. That
        // lets it expand from free-only to the full catalog after restart
        // without exposing the key itself.
        Ok(serde_json::json!({
            "catalog": catalog,
            "authenticated": authenticated,
        }))
    }
}

fn normalize_api_key(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// A default local Ollama reached through the generic OpenAI-compatible
/// provider: its model list lives at `/api/tags`. Mirrors the frontend's
/// `looksLikeOllama` (local-endpoints.ts); keep the two in sync.
fn looks_like_ollama(base_url: &str) -> bool {
    let Ok(url) = url::Url::parse(base_url) else {
        return false;
    };
    let host = url.host_str().unwrap_or_default();
    (host == "localhost" || host == "127.0.0.1") && url.port() == Some(11434)
}

fn validate_provider_value(value: &str, name: &str) -> Result<(), IpcErrorBody> {
    if value.len() > MAX_PROVIDER_VALUE_LENGTH {
        return Err(invalid_params(format!(
            "providers.fetch_models '{name}' is too long"
        )));
    }
    Ok(())
}

fn validate_provider_base_url(value: &str) -> Result<String, IpcErrorBody> {
    validate_provider_value(value, "base_url")?;
    let parsed = url::Url::parse(value).map_err(|_| {
        invalid_params("providers.fetch_models 'base_url' must be a valid http(s) URL")
    })?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(invalid_params(
            "providers.fetch_models 'base_url' must use http(s) without embedded credentials",
        ));
    }
    Ok(value.trim_end_matches('/').to_string())
}

fn sanitize_provider_url(value: &str) -> String {
    value.split(['?', '#']).next().unwrap_or(value).to_string()
}

fn invalid_params(message: impl Into<String>) -> IpcErrorBody {
    IpcErrorBody {
        code: ErrorCode::InvalidParams,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kilo_model_base_url_keeps_gateway_path() {
        assert_eq!(
            validate_provider_base_url("https://api.kilo.ai/api/gateway///").unwrap(),
            "https://api.kilo.ai/api/gateway"
        );
    }

    #[test]
    fn model_base_url_rejects_credentials_and_non_http_schemes() {
        assert!(validate_provider_base_url("https://user:pass@example.com/v1").is_err());
        assert!(validate_provider_base_url("file:///tmp/models").is_err());
    }

    #[test]
    fn blank_api_keys_are_anonymous() {
        assert_eq!(normalize_api_key(""), None);
        assert_eq!(normalize_api_key("   "), None);
        assert_eq!(
            normalize_api_key(" test-key "),
            Some("test-key".to_string())
        );
    }
}
