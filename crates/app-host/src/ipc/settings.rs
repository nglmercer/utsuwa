//! Settings IPC, including the model-provider form.
use super::Dispatcher;
use ipc_core::{ErrorCode, IpcErrorBody, IpcRequest};
use serde_json::Value;

impl Dispatcher {
    /// Read a JSON setting from SQLite storage. Generic missing keys resolve
    /// to `{"value": null}`; typed settings expose their declared default.
    pub(crate) fn settings_get(&self, request: &IpcRequest) -> Result<Value, IpcErrorBody> {
        let storage = self.storage.as_ref().ok_or_else(|| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "storage is not attached".to_string(),
        })?;
        let key = request
            .params
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| IpcErrorBody {
                code: ErrorCode::InvalidParams,
                message: "settings.get needs a string 'key'".to_string(),
            })?;
        if key == "model.api_key" {
            // Credentials are write-only through the keychain-backed model
            // endpoint; exposing the legacy row would reintroduce a frontend
            // secret channel.
            return Ok(serde_json::json!({ "value": null }));
        }
        let storage = storage.lock().map_err(|_| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "storage lock failed".to_string(),
        })?;
        let value = storage.get_setting(key).map_err(|err| IpcErrorBody {
            code: ErrorCode::Internal,
            message: err.to_string(),
        })?;
        if key == crate::runtime::SETTING_AUTONOMOUS_FULL_ACCESS {
            // Missing or legacy-malformed rows use the durable setting's
            // declared default rather than leaking the generic null shape to
            // the access UI.
            return Ok(serde_json::json!({
                "value": value.and_then(|value| value.as_bool()).unwrap_or(false)
            }));
        }
        Ok(serde_json::json!({ "value": value }))
    }
    /// Write a JSON setting to SQLite storage. Model identity must go through
    /// the keychain-backed model endpoint; the generic settings path cannot
    /// write or expose model credentials.
    pub(crate) fn settings_set(&self, request: &IpcRequest) -> Result<Value, IpcErrorBody> {
        let storage = self.storage.as_ref().ok_or_else(|| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "storage is not attached".to_string(),
        })?;
        let key = request
            .params
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| IpcErrorBody {
                code: ErrorCode::InvalidParams,
                message: "settings.set needs a string 'key'".to_string(),
            })?;
        if key.is_empty() || key.len() > 256 {
            return Err(IpcErrorBody {
                code: ErrorCode::InvalidParams,
                message: "settings.set 'key' must be 1-256 chars".to_string(),
            });
        }
        if matches!(
            key,
            "model.provider" | "model.base_url" | "model.name" | "model.api_key"
        ) {
            return Err(IpcErrorBody {
                code: ErrorCode::InvalidParams,
                message: "model settings must use settings.set_model_provider".to_string(),
            });
        }
        let value = request.params.get("value").cloned().unwrap_or(Value::Null);
        let autonomous_full_access = if key == crate::runtime::SETTING_AUTONOMOUS_FULL_ACCESS {
            Some(value.as_bool().ok_or_else(|| IpcErrorBody {
                code: ErrorCode::InvalidParams,
                message: format!("settings.set '{key}' requires a boolean value"),
            })?)
        } else {
            None
        };
        if key == crate::runtime::SETTING_TOOL_PROFILE {
            let valid = value
                .as_str()
                .map(|profile| {
                    matches!(
                        profile.trim().to_ascii_lowercase().as_str(),
                        "simple" | "full"
                    )
                })
                .unwrap_or(false);
            if !valid {
                return Err(IpcErrorBody {
                    code: ErrorCode::InvalidParams,
                    message: format!("settings.set '{key}' requires the string 'simple' or 'full'"),
                });
            }
        }
        {
            let storage = storage.lock().map_err(|_| IpcErrorBody {
                code: ErrorCode::Internal,
                message: "storage lock failed".to_string(),
            })?;
            storage
                .set_setting(key, &value)
                .map_err(|err| IpcErrorBody {
                    code: ErrorCode::Internal,
                    message: err.to_string(),
                })?;
        }
        // Keep the live authorizer in sync with the durable row. This makes
        // both enabling and disabling effective without a runtime restart;
        // enabling also resumes a suspended Agent turn through its explicit
        // autonomous-mode path.
        if let Some(enabled) = autonomous_full_access {
            if let Some(agent) = self.agent.as_ref() {
                agent.set_autonomous_full_access(enabled);
            }
        }
        Ok(serde_json::json!({ "ok": true }))
    }
    /// Read the non-secret model identity and whether the native secret store
    /// contains its key. The key itself is never returned to the WebView.
    pub(crate) fn settings_get_model_provider(&self) -> Result<Value, IpcErrorBody> {
        let storage = self.storage.as_ref().ok_or_else(|| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "storage is not attached".to_string(),
        })?;
        let storage = storage.lock().map_err(|_| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "storage lock failed".to_string(),
        })?;
        let read_string = |key: &str| -> Result<Option<String>, IpcErrorBody> {
            storage
                .get_setting(key)
                .map_err(|err| IpcErrorBody {
                    code: ErrorCode::Internal,
                    message: err.to_string(),
                })
                .map(|value| value.and_then(|value| value.as_str().map(str::to_string)))
        };
        let provider = read_string("model.provider")?.unwrap_or_default();
        let base_url = read_string("model.base_url")?.unwrap_or_default();
        let model = read_string("model.name")?.unwrap_or_default();
        drop(storage);
        let has_api_key = self
            .secrets
            .as_ref()
            .map(|secrets| {
                secrets
                    .get(secret_core::ACCOUNT_MODEL_API_KEY)
                    .ok()
                    .flatten()
                    .is_some_and(|key| !key.is_empty())
            })
            .unwrap_or(false);
        Ok(serde_json::json!({
            "provider": provider,
            "base_url": base_url,
            "model": model,
            "has_api_key": has_api_key,
        }))
    }
    /// Atomically synchronize the model identity/configuration with native
    /// storage. The API key takes the parallel OS-keychain path and is never
    /// written to SQLite or returned to the WebView.
    pub(crate) fn settings_set_model_provider(
        &self,
        request: &IpcRequest,
    ) -> Result<Value, IpcErrorBody> {
        let storage = self.storage.as_ref().ok_or_else(|| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "storage is not attached".to_string(),
        })?;
        let provider = request
            .params
            .get("provider")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| IpcErrorBody {
                code: ErrorCode::InvalidParams,
                message: "settings.set_model_provider needs 'provider'".to_string(),
            })?;
        let base_url = request
            .params
            .get("base_url")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| IpcErrorBody {
                code: ErrorCode::InvalidParams,
                message: "settings.set_model_provider needs 'base_url'".to_string(),
            })?;
        let model = request
            .params
            .get("model")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| IpcErrorBody {
                code: ErrorCode::InvalidParams,
                message: "settings.set_model_provider needs 'model'".to_string(),
            })?;
        let base_url = validate_model_base_url(base_url)?;
        for (name, value) in [
            ("provider", provider),
            ("base_url", base_url.as_str()),
            ("model", model),
        ] {
            if value.len() > 4096 {
                return Err(IpcErrorBody {
                    code: ErrorCode::InvalidParams,
                    message: format!("settings.set_model_provider '{name}' is too long"),
                });
            }
        }

        let requested_key = request
            .params
            .get("api_key")
            .map(|key_value| {
                key_value.as_str().ok_or_else(|| IpcErrorBody {
                    code: ErrorCode::InvalidParams,
                    message: "settings.set_model_provider 'api_key' must be a string".to_string(),
                })
            })
            .transpose()?;

        // Update the key first. If SQLite fails, restore the old key so the
        // native model configuration remains coherent.
        let old_key = self.secrets.as_ref().and_then(|secrets| {
            secrets
                .get(secret_core::ACCOUNT_MODEL_API_KEY)
                .ok()
                .flatten()
        });
        if let Some(key) = requested_key {
            let secrets = self.secrets.as_ref().ok_or_else(|| IpcErrorBody {
                code: ErrorCode::Internal,
                message: "secret store is not attached".to_string(),
            })?;
            if key.is_empty() {
                secrets
                    .delete(secret_core::ACCOUNT_MODEL_API_KEY)
                    .map_err(|err| IpcErrorBody {
                        code: ErrorCode::Internal,
                        message: format!("could not clear model API key: {err}"),
                    })?;
            } else {
                secrets
                    .set(secret_core::ACCOUNT_MODEL_API_KEY, key)
                    .map_err(|err| IpcErrorBody {
                        code: ErrorCode::Internal,
                        message: format!("could not store model API key: {err}"),
                    })?;
            }
        }
        let storage_result = {
            let storage = storage.lock().map_err(|_| IpcErrorBody {
                code: ErrorCode::Internal,
                message: "storage lock failed".to_string(),
            })?;
            storage
                .set_setting(
                    "model.provider",
                    &Value::String(provider.trim().to_string()),
                )
                .and_then(|_| {
                    storage.set_setting("model.base_url", &Value::String(base_url.clone()))
                })
                .and_then(|_| {
                    storage.set_setting("model.name", &Value::String(model.trim().to_string()))
                })
                // Remove the old plaintext migration row after the key has
                // reached the native secret store.
                .and_then(|_| storage.delete_setting("model.api_key").map(|_| ()))
        };
        if let Err(err) = storage_result {
            if let (Some(secrets), Some(previous)) = (self.secrets.as_ref(), old_key.as_deref()) {
                let _ = secrets.set(secret_core::ACCOUNT_MODEL_API_KEY, previous);
            } else if requested_key.is_some() {
                if let Some(secrets) = self.secrets.as_ref() {
                    let _ = secrets.delete(secret_core::ACCOUNT_MODEL_API_KEY);
                }
            }
            return Err(IpcErrorBody {
                code: ErrorCode::Internal,
                message: err.to_string(),
            });
        }
        Ok(serde_json::json!({ "ok": true, "provider": provider, "model": model }))
    }
}
fn validate_model_base_url(raw: &str) -> Result<String, IpcErrorBody> {
    let value = raw.trim();
    let parsed = url::Url::parse(value).map_err(|_| IpcErrorBody {
        code: ErrorCode::InvalidParams,
        message: "model base_url must be a valid http(s) URL".to_string(),
    })?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(IpcErrorBody {
            code: ErrorCode::InvalidParams,
            message: "model base_url must use http(s) without embedded credentials".to_string(),
        });
    }
    Ok(value.to_string())
}
