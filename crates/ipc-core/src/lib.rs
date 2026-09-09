//! Typed JSON-RPC-inspired IPC envelopes for the native host.
//!
//! Architectural invariant: the WebView is unprivileged. Only the
//! application-level methods in [`IpcMethod`] are exposed to JavaScript.
//! Raw OS operations (`fs.*`, `process.*`, `shell.*`) are never IPC methods.

use serde::{Deserialize, Serialize};

/// Every method the frontend is allowed to call. Any method outside this
/// enum is rejected at parse time — there is no stringly-typed fallback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IpcMethod {
    #[serde(rename = "app.version")]
    AppVersion,
    #[serde(rename = "app.ready")]
    AppReady,
    #[serde(rename = "agent.send_message")]
    AgentSendMessage,
    #[serde(rename = "agent.cancel")]
    AgentCancel,
    #[serde(rename = "permission.approve")]
    PermissionApprove,
    #[serde(rename = "permission.deny")]
    PermissionDeny,
    #[serde(rename = "permission.list")]
    PermissionList,
    /// Mint a standing read grant directly (explicit user action in
    /// settings). Restricted to read-only capabilities — mutations always
    /// need the per-request dialog.
    #[serde(rename = "permission.grant")]
    PermissionGrant,
    /// Drop standing grants matching a capability + scope, in memory and
    /// in storage.
    #[serde(rename = "permission.revoke")]
    PermissionRevoke,
    /// Standing grants plus the resolved home directory (for settings UI).
    #[serde(rename = "permission.grants")]
    PermissionGrants,
    #[serde(rename = "settings.get")]
    SettingsGet,
    #[serde(rename = "settings.set")]
    SettingsSet,
    #[serde(rename = "settings.get_model_provider")]
    SettingsGetModelProvider,
    #[serde(rename = "settings.set_model_provider")]
    SettingsSetModelProvider,
    /// Fetch an OpenAI-compatible model catalog through the native HTTP
    /// client. This is needed when a provider does not enable WebView CORS.
    #[serde(rename = "providers.fetch_models")]
    ProvidersFetchModels,
    #[serde(rename = "activity.list")]
    ActivityList,
    #[serde(rename = "plugin.list")]
    PluginList,
    #[serde(rename = "plugin.enable")]
    PluginEnable,
    #[serde(rename = "plugin.disable")]
    PluginDisable,
    #[serde(rename = "plugin.update")]
    PluginUpdate,
    #[serde(rename = "plugin.remove")]
    PluginRemove,
}

/// Request envelope: frontend → host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IpcRequest {
    pub id: String,
    pub method: IpcMethod,
    #[serde(default)]
    pub params: serde_json::Value,
}

/// Success envelope: host → frontend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IpcResponse {
    pub id: String,
    pub result: serde_json::Value,
}

/// Machine-readable error codes. `permission_denied` is the only way a
/// denied privileged operation surfaces to the frontend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    #[serde(rename = "permission_denied")]
    PermissionDenied,
    #[serde(rename = "method_not_found")]
    MethodNotFound,
    #[serde(rename = "invalid_params")]
    InvalidParams,
    #[serde(rename = "cancelled")]
    Cancelled,
    #[serde(rename = "internal")]
    Internal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IpcErrorBody {
    pub code: ErrorCode,
    pub message: String,
}

/// Error envelope: host → frontend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IpcErrorResponse {
    pub id: String,
    pub error: IpcErrorBody,
}

/// Push event envelope: host → frontend (streaming text, tool state,
/// permission prompts, …).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostEvent {
    pub event: String,
    #[serde(default)]
    pub data: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum IpcParseError {
    #[error("invalid JSON-RPC envelope: {0}")]
    InvalidEnvelope(String),
}

impl IpcRequest {
    /// Create a request with a fresh id.
    pub fn new(method: IpcMethod, params: serde_json::Value) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            method,
            params,
        }
    }

    /// Parse and validate an untrusted frontend string. Unknown methods fail
    /// here because [`IpcMethod`] has no catch-all variant.
    pub fn parse(raw: &str) -> Result<Self, IpcParseError> {
        serde_json::from_str(raw).map_err(|e| IpcParseError::InvalidEnvelope(e.to_string()))
    }
}

impl IpcResponse {
    pub fn ok(id: impl Into<String>, result: serde_json::Value) -> Self {
        Self {
            id: id.into(),
            result,
        }
    }
}

impl IpcErrorResponse {
    pub fn err(id: impl Into<String>, code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            error: IpcErrorBody {
                code,
                message: message.into(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips_through_json() {
        let req = IpcRequest::new(
            IpcMethod::AgentSendMessage,
            serde_json::json!({"text": "hello"}),
        );
        let raw = serde_json::to_string(&req).unwrap();
        let back = IpcRequest::parse(&raw).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn unknown_method_is_rejected() {
        let raw = r#"{"id":"1","method":"fs.read","params":{}}"#;
        assert!(IpcRequest::parse(raw).is_err());
    }

    #[test]
    fn raw_shell_is_rejected() {
        let raw = r#"{"id":"1","method":"shell.exec","params":{"cmd":"rm -rf /"}}"#;
        assert!(IpcRequest::parse(raw).is_err());
    }

    #[test]
    fn error_envelope_uses_permission_denied_code() {
        let err = IpcErrorResponse::err("1", ErrorCode::PermissionDenied, "denied by policy");
        let raw = serde_json::to_string(&err).unwrap();
        assert!(raw.contains("permission_denied"));
        let back: IpcErrorResponse = serde_json::from_str(&raw).unwrap();
        assert_eq!(back.error.code, ErrorCode::PermissionDenied);
    }

    #[test]
    fn event_envelope_shape() {
        let ev = HostEvent {
            event: "agent.text_delta".to_string(),
            data: serde_json::json!({"delta": "hi"}),
        };
        let raw = serde_json::to_string(&ev).unwrap();
        assert!(raw.contains("agent.text_delta"));
    }
}
