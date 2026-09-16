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
    /// MCP server connection state for settings UIs (never credentials).
    #[serde(rename = "mcp.status")]
    McpStatus,
    /// Connect one MCP server now and return its discovered tool names
    /// (settings "test" path; agent turns connect lazily otherwise).
    #[serde(rename = "mcp.connect")]
    McpConnect,
    /// Store (or, when empty, delete) one HTTP MCP server's Bearer [REDACTED]
    /// the OS secret store. Write-only: tokens are never returned.
    #[serde(rename = "mcp.set_server_token")]
    McpSetServerToken,
    #[serde(rename = "audio_capture.start")]
    AudioCaptureStart,
    #[serde(rename = "audio_capture.stop")]
    AudioCaptureStop,
    #[serde(rename = "audio_capture.cancel")]
    AudioCaptureCancel,
    /// Authoritative host sensor activity snapshots used by persistent
    /// human-facing indicators. Model-facing `camera.status`/`audio.status`
    /// tools are not required for UI visibility.
    #[serde(rename = "camera.activity.status")]
    CameraActivityStatus,
    #[serde(rename = "microphone.activity.status")]
    MicrophoneActivityStatus,
    /// Explicit user-facing screen sharing controls. These never imply
    /// DesktopControl; that capability remains independently authorized.
    #[serde(rename = "desktop.share_screen.start")]
    DesktopShareScreenStart,
    #[serde(rename = "desktop.share_screen.pause")]
    DesktopShareScreenPause,
    #[serde(rename = "desktop.share_screen.resume")]
    DesktopShareScreenResume,
    #[serde(rename = "desktop.share_screen.stop")]
    DesktopShareScreenStop,
    #[serde(rename = "desktop.share_screen.status")]
    DesktopShareScreenStatus,
    #[serde(rename = "desktop.control.enable")]
    DesktopControlEnable,
    #[serde(rename = "desktop.control.disable")]
    DesktopControlDisable,
    /// Global emergency stop: immediately disables pointer, keyboard, and
    /// semantic UI actions (observation stays active) and revokes standing
    /// DesktopControl grants. Operates independently from the model.
    #[serde(rename = "desktop.emergency_stop")]
    DesktopEmergencyStop,
    /// Clear a previously engaged emergency stop (user action only).
    #[serde(rename = "desktop.emergency_clear")]
    DesktopEmergencyClear,
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
    /// Queryable host state for the deterministic frontend/native
    /// handshake. A one-shot `app.ready` event must not be the only source
    /// of truth: the frontend calls this after registering listeners (and
    /// again after any missed event) to learn the host version, platform,
    /// desktop backend, and bootstrap capabilities.
    #[serde(rename = "host.runtime_state")]
    HostRuntimeState,
    /// Frontend-to-host readiness signal. Sent after the page has
    /// registered its native event listeners; the host marks the frontend
    /// ready and answers with the same payload as `host.runtime_state`.
    /// This replaces timing-dependent `app.ready` delivery.
    #[serde(rename = "host.frontend_ready")]
    HostFrontendReady,
    /// Debug-oriented frontend diagnostic report (`window.onerror`,
    /// `unhandledrejection`, `console.error`, bootstrap markers). Handled
    /// by logging a sanitized, truncated record on the host; never returns
    /// privileged data and performs no host action.
    #[serde(rename = "diagnostics.report")]
    DiagnosticsReport,
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
    fn handshake_and_diagnostics_methods_parse() {
        for (raw, method) in [
            (
                r#"{"id":"1","method":"host.runtime_state","params":{}}"#,
                IpcMethod::HostRuntimeState,
            ),
            (
                r#"{"id":"2","method":"host.frontend_ready","params":{}}"#,
                IpcMethod::HostFrontendReady,
            ),
            (
                r#"{"id":"3","method":"diagnostics.report","params":{"kind":"window.error"}}"#,
                IpcMethod::DiagnosticsReport,
            ),
        ] {
            assert_eq!(IpcRequest::parse(raw).unwrap().method, method);
        }
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

    #[test]
    fn sensor_activity_methods_are_typed_ipc_members() {
        for (method, expected) in [
            (IpcMethod::CameraActivityStatus, "camera.activity.status"),
            (
                IpcMethod::MicrophoneActivityStatus,
                "microphone.activity.status",
            ),
        ] {
            let request = IpcRequest::new(method, serde_json::json!({}));
            let raw = serde_json::to_string(&request).unwrap();
            assert!(raw.contains(expected), "{raw}");
            assert_eq!(IpcRequest::parse(&raw).unwrap(), request);
        }
    }
}
