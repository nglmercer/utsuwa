//! Typed IPC dispatcher (plan Task 6).
//!
//! Parses untrusted frontend strings into [`IpcRequest`] and answers the
//! methods the host currently implements. Everything else fails with a
//! typed error — unknown methods are never silently ignored, and raw OS
//! operations are unrepresentable (see `ipc-core`: they are not members of
//! [`IpcMethod`], so they cannot even parse).

use crate::agent_runtime::AgentRuntime;
use ipc_core::{ErrorCode, HostEvent, IpcErrorBody, IpcErrorResponse, IpcMethod, IpcRequest, IpcResponse};
use policy_core::{ApprovalQueue, GrantLifetime, QueueError};
use serde_json::Value;
use std::sync::{Arc, Mutex};

/// Host state the dispatcher may report. The agent runtime and storage
/// attachments are optional so headless/unit configurations keep working;
/// methods needing a missing attachment fail with a typed error.
pub struct Dispatcher {
    pub app_version: String,
    approvals: Option<Arc<Mutex<ApprovalQueue>>>,
    agent: Option<Arc<AgentRuntime>>,
    storage: Option<Arc<Mutex<storage_core::Storage>>>,
}

impl Clone for Dispatcher {
    fn clone(&self) -> Self {
        Self {
            app_version: self.app_version.clone(),
            approvals: self.approvals.clone(),
            agent: self.agent.clone(),
            storage: self.storage.clone(),
        }
    }
}

impl Dispatcher {
    pub fn new(app_version: impl Into<String>) -> Self {
        Self {
            app_version: app_version.into(),
            approvals: None,
            agent: None,
            storage: None,
        }
    }

    /// Attach the approval queue so the permission dialog's replies
    /// (`permission.approve` / `permission.deny`) resolve real requests.
    pub fn with_approvals(mut self, approvals: Arc<Mutex<ApprovalQueue>>) -> Self {
        self.approvals = Some(approvals);
        self
    }

    /// Attach the live agent runtime (`agent.send_message` / `agent.cancel`).
    pub fn with_agent(mut self, agent: Arc<AgentRuntime>) -> Self {
        self.agent = Some(agent);
        self
    }

    /// Attach SQLite storage (`settings.get` / `settings.set`).
    pub fn with_storage(mut self, storage: Arc<Mutex<storage_core::Storage>>) -> Self {
        self.storage = Some(storage);
        self
    }

    /// Parse one raw frontend message. Returns the script payloads the host
    /// should evaluate in the WebView (`__resolve` calls).
    pub fn handle_message(&self, raw: &str) -> Option<String> {
        let body = raw_body(raw);
        match IpcRequest::parse(&body) {
            Ok(request) => Some(self.reply_script(&request)),
            Err(err) => {
                tracing::warn!(%err, "dropping unparsable ipc message (no id to reply to)");
                None
            }
        }
    }

    fn reply_script(&self, request: &IpcRequest) -> String {
        match self.dispatch(request) {
            Ok(result) => {
                let response = IpcResponse::ok(request.id.clone(), result);
                match serde_json::to_string(&response) {
                    Ok(json) => resolve_script(&request.id, true, &json_payload(&json, true)),
                    Err(err) => {
                        tracing::error!(%err, "failed to encode ipc response");
                        internal_error_script(&request.id)
                    }
                }
            }
            Err(error) => {
                let response = IpcErrorResponse {
                    id: request.id.clone(),
                    error,
                };
                match serde_json::to_string(&response) {
                    Ok(json) => resolve_script(&request.id, false, &json_payload(&json, false)),
                    Err(err) => {
                        tracing::error!(%err, "failed to encode ipc error");
                        internal_error_script(&request.id)
                    }
                }
            }
        }
    }

    fn dispatch(&self, request: &IpcRequest) -> Result<Value, IpcErrorBody> {
        match request.method {
            IpcMethod::AppVersion => Ok(serde_json::json!({
                "version": self.app_version,
                "host": "utsuwa-native",
            })),
            IpcMethod::AppReady => Ok(serde_json::json!({ "ok": true })),
            IpcMethod::PermissionApprove => self.decide(request, true),
            IpcMethod::PermissionDeny => self.decide(request, false),
            IpcMethod::PermissionList => self.list_pending(),
            IpcMethod::AgentSendMessage => self.agent_send(request),
            IpcMethod::AgentCancel => self.agent_cancel(),
            IpcMethod::SettingsGet => self.settings_get(request),
            IpcMethod::SettingsSet => self.settings_set(request),
        }
    }

    /// Resolve one permission reply from the dialog. Approving records a
    /// grant scoped to the requested capability + resource, so the resumed
    /// turn authorizes through normal policy — never a bypass.
    fn decide(&self, request: &IpcRequest, approve: bool) -> Result<Value, IpcErrorBody> {
        let queue = self.approvals.as_ref().ok_or_else(|| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "approval queue is not attached".to_string(),
        })?;
        let id = request.params.get("id").and_then(|v| v.as_str()).ok_or_else(|| {
            IpcErrorBody {
                code: ErrorCode::InvalidParams,
                message: "permission reply needs a string 'id'".to_string(),
            }
        })?;
        let lifetime = if approve {
            Some(parse_lifetime(
                request.params.get("lifetime").and_then(|v| v.as_str()),
            )?)
        } else {
            None
        };
        let queue = queue.lock().map_err(|_| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "approval queue lock failed".to_string(),
        })?;
        let id_owned = id.to_string();
        let outcome = queue.decide(id, lifetime);
        // Release the queue lock before waking the runtime: the resumed
        // worker re-locks the queue for its policy context.
        drop(queue);
        match outcome {
            Ok(granted) => {
                // Wake a suspended turn, if this decision resolves it.
                // Approve resumes under the new grant; deny resumes with
                // a refusal note so the model works around it.
                if let Some(agent) = self.agent.as_ref() {
                    agent.notify_decided(&id_owned, granted);
                }
                Ok(serde_json::json!({ "ok": true, "granted": granted }))
            }
            Err(QueueError::UnknownId(_)) => Err(IpcErrorBody {
                code: ErrorCode::InvalidParams,
                message: "unknown permission request".to_string(),
            }),
            // Storage failed: the request is still pending, so the user
            // can retry. Internal, not a params problem.
            Err(QueueError::Persist(detail)) => Err(IpcErrorBody {
                code: ErrorCode::Internal,
                message: format!("persistent approval could not be stored: {detail}"),
            }),
        }
    }

    /// Snapshot of queued requests so a (re)mounted dialog can sync
    /// without racing boot-time push events.
    fn list_pending(&self) -> Result<Value, IpcErrorBody> {
        let queue = self.approvals.as_ref().ok_or_else(|| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "approval queue is not attached".to_string(),
        })?;
        let queue = queue.lock().map_err(|_| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "approval queue lock failed".to_string(),
        })?;
        serde_json::to_value(queue.list()).map_err(|err| IpcErrorBody {
            code: ErrorCode::Internal,
            message: err.to_string(),
        })
    }

    /// Start (or supersede) an agent turn. Returns immediately; the text,
    /// tool results, and approval prompts arrive as host events
    /// (`agent.turn_done`, `permission.requested`, …).
    fn agent_send(&self, request: &IpcRequest) -> Result<Value, IpcErrorBody> {
        let agent = self.agent.as_ref().ok_or_else(|| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "agent runtime is not attached".to_string(),
        })?;
        let text = request.params.get("text").and_then(|v| v.as_str()).ok_or_else(|| {
            IpcErrorBody {
                code: ErrorCode::InvalidParams,
                message: "agent.send_message needs a string 'text'".to_string(),
            }
        })?;
        let text = text.trim();
        if text.is_empty() {
            return Err(IpcErrorBody {
                code: ErrorCode::InvalidParams,
                message: "agent.send_message needs a non-empty 'text'".to_string(),
            });
        }
        if text.len() > 32 * 1024 {
            return Err(IpcErrorBody {
                code: ErrorCode::InvalidParams,
                message: "agent.send_message 'text' exceeds 32 KiB".to_string(),
            });
        }
        agent.send_message(text.to_string()).map_err(|err| IpcErrorBody {
            code: ErrorCode::Internal,
            message: err.to_string(),
        })?;
        Ok(serde_json::json!({ "ok": true, "accepted": true }))
    }

    fn agent_cancel(&self) -> Result<Value, IpcErrorBody> {
        let agent = self.agent.as_ref().ok_or_else(|| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "agent runtime is not attached".to_string(),
        })?;
        agent.cancel();
        Ok(serde_json::json!({ "ok": true }))
    }

    /// Read a JSON setting from SQLite storage. Missing keys resolve to
    /// `{"value": null}` rather than erroring.
    fn settings_get(&self, request: &IpcRequest) -> Result<Value, IpcErrorBody> {
        let storage = self.storage.as_ref().ok_or_else(|| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "storage is not attached".to_string(),
        })?;
        let key = request.params.get("key").and_then(|v| v.as_str()).ok_or_else(|| {
            IpcErrorBody {
                code: ErrorCode::InvalidParams,
                message: "settings.get needs a string 'key'".to_string(),
            }
        })?;
        let storage = storage.lock().map_err(|_| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "storage lock failed".to_string(),
        })?;
        let value = storage.get_setting(key).map_err(|err| IpcErrorBody {
            code: ErrorCode::Internal,
            message: err.to_string(),
        })?;
        Ok(serde_json::json!({ "value": value }))
    }

    /// Write a JSON setting to SQLite storage. Model credentials use
    /// `model.base_url` / `model.api_key` / `model.name` (OS keychain
    /// arrives with plan Phase 33; the key never leaves this host except
    /// to the configured provider).
    fn settings_set(&self, request: &IpcRequest) -> Result<Value, IpcErrorBody> {
        let storage = self.storage.as_ref().ok_or_else(|| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "storage is not attached".to_string(),
        })?;
        let key = request.params.get("key").and_then(|v| v.as_str()).ok_or_else(|| {
            IpcErrorBody {
                code: ErrorCode::InvalidParams,
                message: "settings.set needs a string 'key'".to_string(),
            }
        })?;
        if key.is_empty() || key.len() > 256 {
            return Err(IpcErrorBody {
                code: ErrorCode::InvalidParams,
                message: "settings.set 'key' must be 1-256 chars".to_string(),
            });
        }
        let value = request.params.get("value").cloned().unwrap_or(Value::Null);
        let storage = storage.lock().map_err(|_| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "storage lock failed".to_string(),
        })?;
        storage.set_setting(key, &value).map_err(|err| IpcErrorBody {
            code: ErrorCode::Internal,
            message: err.to_string(),
        })?;
        Ok(serde_json::json!({ "ok": true }))
    }
}

/// wry delivers IPC bodies either raw or wrapped by platform quirks; accept
/// a JSON string that itself contains the envelope.
fn raw_body(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.starts_with('"') {
        serde_json::from_str::<String>(trimmed).unwrap_or_else(|_| trimmed.to_string())
    } else {
        trimmed.to_string()
    }
}

/// Extract the `result` / `error` member of an encoded envelope so the
/// bridge receives only the payload (the id travels as `__resolve` arg).
fn json_payload(envelope_json: &str, ok: bool) -> String {
    let key = if ok { "result" } else { "error" };
    serde_json::from_str::<Value>(envelope_json)
        .ok()
        .and_then(|v| v.get(key).cloned())
        .map(|v| v.to_string())
        .unwrap_or_else(|| "null".to_string())
}

fn resolve_script(id: &str, ok: bool, payload_json: &str) -> String {
    format!(
        "window.utsuwa && window.utsuwa.__resolve({}, {}, {})",
        serde_json::to_string(id).unwrap_or_else(|_| "\"\"".to_string()),
        ok,
        payload_json
    )
}

fn internal_error_script(id: &str) -> String {
    resolve_script(
        id,
        false,
        r#"{"code":"internal","message":"failed to encode response"}"#,
    )
}

/// Dialog lifetime strings → [`GrantLifetime`]. Defaults to `Once` so a
/// dialog that omits duration grants the least authority.
fn parse_lifetime(raw: Option<&str>) -> Result<GrantLifetime, IpcErrorBody> {
    match raw {
        None | Some("once") => Ok(GrantLifetime::Once),
        Some("task") => Ok(GrantLifetime::Task),
        Some("session") => Ok(GrantLifetime::Session),
        Some("persistent") => Ok(GrantLifetime::Persistent),
        Some(other) => Err(IpcErrorBody {
            code: ErrorCode::InvalidParams,
            message: format!("unknown grant lifetime: '{other}'"),
        }),
    }
}

/// Script that pushes a [`HostEvent`] to the page bridge.
pub fn emit_script(event: &HostEvent) -> String {
    let data = event.data.to_string();
    format!(
        "window.utsuwa && window.utsuwa.__emit({}, {})",
        serde_json::to_string(&event.event).unwrap_or_else(|_| "\"\"".to_string()),
        data
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dispatcher() -> Dispatcher {
        Dispatcher::new("0.1.0")
    }

    #[test]
    fn version_request_resolves_with_version() {
        let script = dispatcher()
            .handle_message(r#"{"id":"1","method":"app.version","params":{}}"#)
            .unwrap();
        assert!(script.contains("__resolve(\"1\", true"), "{script}");
        assert!(script.contains("0.1.0"), "{script}");
    }

    #[test]
    fn ready_request_resolves_ok() {
        let script = dispatcher()
            .handle_message(r#"{"id":"2","method":"app.ready","params":{}}"#)
            .unwrap();
        assert!(script.contains("__resolve(\"2\", true"), "{script}");
    }

    #[test]
    fn agent_send_without_runtime_rejects_with_typed_error() {
        let script = dispatcher()
            .handle_message(r#"{"id":"3","method":"agent.send_message","params":{"text":"hi"}}"#)
            .unwrap();
        assert!(script.contains("__resolve(\"3\", false"), "{script}");
        assert!(script.contains("agent runtime is not attached"), "{script}");
    }

    #[test]
    fn raw_shell_never_parses_and_gets_no_reply() {
        assert!(dispatcher()
            .handle_message(r#"{"id":"4","method":"shell.exec","params":{}}"#)
            .is_none());
        assert!(dispatcher().handle_message("not json at all").is_none());
    }

    #[test]
    fn approve_records_grant_and_deny_clears() {
        use capability_core::{Capability, Resource};
        let queue = Arc::new(Mutex::new(ApprovalQueue::new()));
        let pending = queue
            .lock()
            .unwrap()
            .submit(
                capability_core::Principal::User,
                Capability::FilesystemRead,
                Resource::Path("/work".into()),
                "test".to_string(),
            );
        let dispatcher = dispatcher().with_approvals(queue.clone());
        let script = dispatcher
            .handle_message(&format!(
                r#"{{"id":"10","method":"permission.approve","params":{{"id":"{}","lifetime":"session"}}}}"#,
                pending.id
            ))
            .unwrap();
        assert!(script.contains("__resolve(\"10\", true"), "{script}");
        assert!(script.contains("\"granted\":true"), "{script}");
        assert!(queue.lock().unwrap().list().is_empty());
        assert_eq!(queue.lock().unwrap().context().grants.len(), 1);

        let script = dispatcher
            .handle_message(r#"{"id":"11","method":"permission.deny","params":{"id":"perm-999"}}"#)
            .unwrap();
        assert!(script.contains("__resolve(\"11\", false"), "{script}");
        assert!(script.contains("unknown permission request"), "{script}");
    }

    #[test]
    fn approve_without_id_or_lifetime_is_rejected() {
        let queue = Arc::new(Mutex::new(ApprovalQueue::new()));
        let dispatcher = dispatcher().with_approvals(queue);
        let script = dispatcher
            .handle_message(r#"{"id":"12","method":"permission.approve","params":{}}"#)
            .unwrap();
        assert!(script.contains("__resolve(\"12\", false"), "{script}");
        let script = dispatcher
            .handle_message(
                r#"{"id":"13","method":"permission.deny","params":{"id":"x","lifetime":"forever"}}"#,
            )
            .unwrap();
        // Deny ignores lifetime; unknown id still errors.
        assert!(script.contains("__resolve(\"13\", false"), "{script}");
    }

    #[test]
    fn list_returns_queued_requests() {
        use capability_core::{Capability, Principal, Resource};
        let queue = Arc::new(Mutex::new(ApprovalQueue::new()));
        queue.lock().unwrap().submit(
            Principal::User,
            Capability::FilesystemRead,
            Resource::Path("/work".into()),
            "r".to_string(),
        );
        let dispatcher = dispatcher().with_approvals(queue);
        let script = dispatcher
            .handle_message(r#"{"id":"20","method":"permission.list","params":{}}"#)
            .unwrap();
        assert!(script.contains("__resolve(\"20\", true"), "{script}");
        assert!(script.contains("perm-1"), "{script}");
        assert!(script.contains("FilesystemRead"), "{script}");
    }

    fn temp_storage(name: &str) -> Arc<Mutex<storage_core::Storage>> {
        let dir = std::env::temp_dir().join(format!(
            "utsuwa-dispatcher-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        Arc::new(Mutex::new(
            storage_core::Storage::open(&dir.join("state.db")).unwrap(),
        ))
    }

    fn stub_runtime() -> Arc<AgentRuntime> {
        let emit: crate::agent_runtime::EmitFn = Arc::new(|_| {});
        AgentRuntime::start_with_factory(
            Arc::new(Mutex::new(ApprovalQueue::new())),
            None,
            emit,
            Arc::new(|| {
                Err(crate::agent_runtime::RuntimeError::ModelNotConfigured)
            }),
        )
        .unwrap()
    }

    #[test]
    fn settings_round_trip_through_dispatcher() {
        let dispatcher = dispatcher().with_storage(temp_storage("roundtrip"));
        let script = dispatcher
            .handle_message(r#"{"id":"30","method":"settings.get","params":{"key":"theme"}}"#)
            .unwrap();
        assert!(script.contains("__resolve(\"30\", true"), "{script}");
        assert!(script.contains("\"value\":null"), "{script}");

        let script = dispatcher
            .handle_message(
                r#"{"id":"31","method":"settings.set","params":{"key":"theme","value":"dark"}}"#,
            )
            .unwrap();
        assert!(script.contains("\"ok\":true"), "{script}");

        let script = dispatcher
            .handle_message(r#"{"id":"32","method":"settings.get","params":{"key":"theme"}}"#)
            .unwrap();
        assert!(script.contains("\"value\":\"dark\""), "{script}");
    }

    #[test]
    fn settings_without_storage_or_key_rejects() {
        let script = dispatcher()
            .handle_message(r#"{"id":"33","method":"settings.get","params":{"key":"k"}}"#)
            .unwrap();
        assert!(script.contains("storage is not attached"), "{script}");

        let script = dispatcher()
            .with_storage(temp_storage("nokey"))
            .handle_message(r#"{"id":"34","method":"settings.get","params":{}}"#)
            .unwrap();
        assert!(script.contains("needs a string 'key'"), "{script}");
    }

    #[test]
    fn agent_send_validates_text() {
        let dispatcher = dispatcher().with_agent(stub_runtime());
        let script = dispatcher
            .handle_message(r#"{"id":"40","method":"agent.send_message","params":{"text":"hello"}}"#)
            .unwrap();
        assert!(script.contains("__resolve(\"40\", true"), "{script}");
        assert!(script.contains("\"accepted\":true"), "{script}");

        for (id, params) in [("41", "{}"), ("42", r#"{"text":"  "}"#)] {
            let script = dispatcher
                .handle_message(&format!(
                    r#"{{"id":"{id}","method":"agent.send_message","params":{params}}}"#
                ))
                .unwrap();
            assert!(script.contains(&format!("__resolve(\"{id}\", false")), "{script}");
        }

        let script = dispatcher
            .handle_message(r#"{"id":"43","method":"agent.cancel","params":{}}"#)
            .unwrap();
        assert!(script.contains("__resolve(\"43\", true"), "{script}");
    }

    #[test]
    fn emit_script_shape() {
        let script = emit_script(&HostEvent {
            event: "agent.text_delta".to_string(),
            data: serde_json::json!({"delta": "hi"}),
        });
        assert!(script.contains("__emit(\"agent.text_delta\""), "{script}");
    }
}
