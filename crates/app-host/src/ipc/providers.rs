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
        // use the native keychain value without ever sending it back to the
        // WebView. An explicit blank key means anonymous access.
        let api_key = if let Some(value) = request.params.get("api_key") {
            let value = value.as_str().ok_or_else(|| {
                invalid_params("providers.fetch_models 'api_key' must be a string")
            })?;
            normalize_api_key(value)
        } else {
            self.secrets
                .as_ref()
                .and_then(|secrets| {
                    secrets
                        .get(secret_core::ACCOUNT_MODEL_API_KEY)
                        .ok()
                        .flatten()
                })
                .and_then(|key| normalize_api_key(&key))
        };

        tracing::debug!(
            provider = %provider,
            base_url = %sanitize_provider_url(&base_url),
            authenticated = api_key.is_some(),
            "fetching provider model catalog through native HTTP"
        );

        let authenticated = api_key.is_some();
        let catalog = OpenAICompatibleClient::new(base_url, api_key, "")
            .fetch_models()
            .await
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
