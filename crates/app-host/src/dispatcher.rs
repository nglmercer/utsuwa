//! Typed IPC dispatcher (plan Task 6).
//!
//! Parses untrusted frontend strings into [`IpcRequest`] and answers the
//! methods the host currently implements. Everything else fails with a
//! typed error — unknown methods are never silently ignored, and raw OS
//! operations are unrepresentable (see `ipc-core`: they are not members of
//! [`IpcMethod`], so they cannot even parse).

use crate::agent_runtime::AgentRuntime;
use capability_core::{Capability, PrincipalKind, Resource, ResourceScope};
use ipc_core::{ErrorCode, HostEvent, IpcErrorBody, IpcErrorResponse, IpcMethod, IpcRequest, IpcResponse};
use policy_core::{ApprovalQueue, GrantLifetime, QueueError};
use serde_json::Value;
use std::sync::{Arc, Mutex};

/// A plugin lifecycle operation behind `plugin.*` IPC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PluginOp {
    Enable,
    Disable,
    Update,
    Remove,
}

/// Host state the dispatcher may report. The agent runtime and storage
/// attachments are optional so headless/unit configurations keep working;
/// methods needing a missing attachment fail with a typed error.
pub struct Dispatcher {
    pub app_version: String,
    approvals: Option<Arc<Mutex<ApprovalQueue>>>,
    agent: Option<Arc<AgentRuntime>>,
    storage: Option<Arc<Mutex<storage_core::Storage>>>,
    audit: Option<Arc<audit_core::InMemorySink>>,
}

impl Clone for Dispatcher {
    fn clone(&self) -> Self {
        Self {
            app_version: self.app_version.clone(),
            approvals: self.approvals.clone(),
            agent: self.agent.clone(),
            storage: self.storage.clone(),
            audit: self.audit.clone(),
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
            audit: None,
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

    /// Attach the shared audit sink (`activity.list`).
    pub fn with_audit(mut self, audit: Arc<audit_core::InMemorySink>) -> Self {
        self.audit = Some(audit);
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
            IpcMethod::PermissionGrant => self.permission_grant(request),
            IpcMethod::PermissionRevoke => self.permission_revoke(request),
            IpcMethod::PermissionGrants => self.permission_grants(),
            IpcMethod::AgentSendMessage => self.agent_send(request),
            IpcMethod::AgentCancel => self.agent_cancel(),
            IpcMethod::SettingsGet => self.settings_get(request),
            IpcMethod::SettingsSet => self.settings_set(request),
            IpcMethod::ActivityList => self.activity_list(request),
            IpcMethod::PluginList => self.plugin_list(),
            IpcMethod::PluginEnable => self.plugin_manage(request, PluginOp::Enable),
            IpcMethod::PluginDisable => self.plugin_manage(request, PluginOp::Disable),
            IpcMethod::PluginUpdate => self.plugin_manage(request, PluginOp::Update),
            IpcMethod::PluginRemove => self.plugin_manage(request, PluginOp::Remove),
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

    /// The user's home directory, resolved from the environment. Used as
    /// the default broad-read scope when settings omit a path.
    fn home_dir() -> Result<std::path::PathBuf, IpcErrorBody> {
        std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .map(std::path::PathBuf::from)
            .map_err(|_| IpcErrorBody {
                code: ErrorCode::Internal,
                message: "cannot determine the home directory; pass an explicit path".to_string(),
            })
    }

    /// Resolve the grant/revoke target: an explicit absolute path, or the
    /// home directory. The target must exist (canonicalized, so `..` and
    /// symlinks resolve before any check) and must not itself be a secret
    /// path — key directories keep per-file approval even from their owner.
    fn grant_root(params: &Value) -> Result<std::path::PathBuf, IpcErrorBody> {
        let raw = params.get("path").and_then(|v| v.as_str());
        let base = match raw {
            Some(p) => std::path::PathBuf::from(p),
            None => Self::home_dir()?,
        };
        if !base.is_absolute() {
            return Err(IpcErrorBody {
                code: ErrorCode::InvalidParams,
                message: "grant path must be absolute".to_string(),
            });
        }
        let canonical = base.canonicalize().map_err(|_| IpcErrorBody {
            code: ErrorCode::InvalidParams,
            message: format!("grant path does not exist: {}", base.display()),
        })?;
        if policy_core::is_secret_path(&Resource::Path(canonical.clone())) {
            return Err(IpcErrorBody {
                code: ErrorCode::InvalidParams,
                message: "secret paths keep per-file approval and cannot be granted in bulk".to_string(),
            });
        }
        Ok(canonical)
    }

    /// Mint a standing grant from explicit user action (settings UI).
    /// Restricted to `FilesystemRead`: reads can be pre-approved, but
    /// mutations and control always go through the per-request dialog —
    /// there is deliberately no bulk path for them.
    fn permission_grant(&self, request: &IpcRequest) -> Result<Value, IpcErrorBody> {
        if request.params.get("capability").and_then(|v| v.as_str()) != Some("FilesystemRead") {
            return Err(IpcErrorBody {
                code: ErrorCode::InvalidParams,
                message: "permission.grant only issues FilesystemRead grants".to_string(),
            });
        }
        // Standing grants outlive the request by definition: only
        // session and persistent lifetimes make sense here. Omitted
        // lifetimes default to session (vanishes on restart); the
        // settings toggle passes persistent explicitly.
        let lifetime = match request.params.get("lifetime").and_then(|v| v.as_str()) {
            None => GrantLifetime::Session,
            Some(_) => {
                let parsed =
                    parse_lifetime(request.params.get("lifetime").and_then(|v| v.as_str()))?;
                if !matches!(parsed, GrantLifetime::Session | GrantLifetime::Persistent) {
                    return Err(IpcErrorBody {
                        code: ErrorCode::InvalidParams,
                        message: "grant lifetime must be 'session' or 'persistent'".to_string(),
                    });
                }
                parsed
            }
        };
        let root = Self::grant_root(&request.params)?;
        let scope = ResourceScope::new(vec![Resource::Path(root.clone())]);
        let queue = self.approvals.as_ref().ok_or_else(|| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "approval queue is not attached".to_string(),
        })?;
        let queue = queue.lock().map_err(|_| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "approval queue lock failed".to_string(),
        })?;
        queue
            .grant_direct(
                PrincipalKind::Agent,
                Capability::FilesystemRead,
                scope,
                lifetime,
                format!("user broad-read grant for {}", root.display()),
            )
            .map_err(|e| match e {
                QueueError::Persist(detail) => IpcErrorBody {
                    code: ErrorCode::Internal,
                    message: format!("grant could not be stored: {detail}"),
                },
                other => IpcErrorBody {
                    code: ErrorCode::Internal,
                    message: other.to_string(),
                },
            })?;
        Ok(serde_json::json!({ "ok": true, "path": root.to_string_lossy() }))
    }

    /// Drop standing grants for a capability + scope, in memory and in
    /// storage, so revocation takes effect immediately and survives restart.
    fn permission_revoke(&self, request: &IpcRequest) -> Result<Value, IpcErrorBody> {
        if request.params.get("capability").and_then(|v| v.as_str()) != Some("FilesystemRead") {
            return Err(IpcErrorBody {
                code: ErrorCode::InvalidParams,
                message: "permission.revoke only revokes FilesystemRead grants".to_string(),
            });
        }
        let root = Self::grant_root(&request.params)?;
        let scope = ResourceScope::new(vec![Resource::Path(root.clone())]);
        let queue = self.approvals.as_ref().ok_or_else(|| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "approval queue is not attached".to_string(),
        })?;
        let queue = queue.lock().map_err(|_| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "approval queue lock failed".to_string(),
        })?;
        let mut removed = queue.revoke_where(&Capability::FilesystemRead, &scope);
        drop(queue);
        if let Some(storage) = self.storage.as_ref() {
            let storage = storage.lock().map_err(|_| IpcErrorBody {
                code: ErrorCode::Internal,
                message: "storage lock failed".to_string(),
            })?;
            let rows = storage.load_grants().map_err(|e| IpcErrorBody {
                code: ErrorCode::Internal,
                message: format!("could not load grants: {e}"),
            })?;
            for row in rows {
                if row.grant.capability == Capability::FilesystemRead && row.grant.scope == scope {
                    storage.delete_grant(row.id).map_err(|e| IpcErrorBody {
                        code: ErrorCode::Internal,
                        message: format!("could not delete grant: {e}"),
                    })?;
                    removed += 1;
                }
            }
        }
        Ok(serde_json::json!({ "ok": true, "removed": removed }))
    }

    /// Standing grants plus the resolved home directory, so settings UI
    /// can render the broad-read toggle without guessing paths.
    fn permission_grants(&self) -> Result<Value, IpcErrorBody> {
        let queue = self.approvals.as_ref().ok_or_else(|| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "approval queue is not attached".to_string(),
        })?;
        let queue = queue.lock().map_err(|_| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "approval queue lock failed".to_string(),
        })?;
        let grants = serde_json::to_value(queue.grants_snapshot()).map_err(|err| IpcErrorBody {
            code: ErrorCode::Internal,
            message: err.to_string(),
        })?;
        drop(queue);
        let home = Self::home_dir().ok().map(|p| p.to_string_lossy().into_owned());
        Ok(serde_json::json!({ "grants": grants, "home": home }))
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

    /// Every known WASM plugin with trust + lifecycle state, for the
    /// Plugins panel (plan Phase 24/25). Needs the agent runtime for its
    /// plugin manager; without one the panel reports unavailability.
    fn plugin_list(&self) -> Result<Value, IpcErrorBody> {
        let agent = self.agent.as_ref().ok_or_else(|| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "agent runtime is not attached".to_string(),
        })?;
        serde_json::to_value(agent.plugin_manager().infos()).map_err(|err| IpcErrorBody {
            code: ErrorCode::Internal,
            message: err.to_string(),
        })
    }

    /// One lifecycle transition by plugin id: enable / disable / update /
    /// remove (plan Phase 24). Activation never grants authority — it only
    /// loads code and registers tools behind policy + tickets.
    fn plugin_manage(&self, request: &IpcRequest, op: PluginOp) -> Result<Value, IpcErrorBody> {
        let agent = self.agent.as_ref().ok_or_else(|| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "agent runtime is not attached".to_string(),
        })?;
        let id = request.params.get("id").and_then(|v| v.as_str()).ok_or_else(|| {
            IpcErrorBody {
                code: ErrorCode::InvalidParams,
                message: "plugin lifecycle methods need a string 'id'".to_string(),
            }
        })?;
        if id.is_empty() || id.len() > 128 {
            return Err(IpcErrorBody {
                code: ErrorCode::InvalidParams,
                message: "plugin 'id' must be 1-128 chars".to_string(),
            });
        }
        let manager = agent.plugin_manager();
        let outcome = match op {
            PluginOp::Enable => manager.enable(id),
            PluginOp::Disable => manager.disable(id),
            PluginOp::Update => manager.update(id),
            PluginOp::Remove => manager.remove(id),
        };
        outcome.map_err(|err| IpcErrorBody {
            // Unknown ids are a caller error; engine/lifecycle failures
            // are host-side.
            code: match err {
                plugin_wasm::WasmError::Unknown(_) => ErrorCode::InvalidParams,
                _ => ErrorCode::Internal,
            },
            message: err.to_string(),
        })?;
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

    /// Recent audit records, newest first, for the Activity panel
    /// (plan Phase 35: time, tool, status, resource, principal, duration).
    /// `params.limit` clamps to 1..=200 (default 50). Details are already
    /// redacted at record time.
    fn activity_list(&self, request: &IpcRequest) -> Result<Value, IpcErrorBody> {
        let audit = self.audit.as_ref().ok_or_else(|| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "audit sink is not attached".to_string(),
        })?;
        let limit = request
            .params
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(50)
            .clamp(1, 200) as usize;
        let mut records = audit.records();
        records.reverse();
        records.truncate(limit);
        serde_json::to_value(&records).map_err(|err| IpcErrorBody {
            code: ErrorCode::Internal,
            message: err.to_string(),
        })
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

    #[test]
    fn broad_read_grant_revoke_roundtrip() {
        use capability_core::{Capability, Principal, Resource};
        let dir = std::env::temp_dir().join(format!(
            "utsuwa-grant-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let storage = temp_storage("grants");
        let queue = Arc::new(Mutex::new(
            ApprovalQueue::new()
                .on_persistent_grant(storage_core::persistent_grant_hook(storage.clone())),
        ));
        let dispatcher = dispatcher()
            .with_approvals(queue.clone())
            .with_storage(storage.clone());

        // Grant persistent read on the temp dir.
        let script = dispatcher
            .handle_message(&format!(
                r#"{{"id":"30","method":"permission.grant","params":{{"capability":"FilesystemRead","path":"{}","lifetime":"persistent"}}}}"#,
                dir.to_string_lossy()
            ))
            .unwrap();
        assert!(script.contains("__resolve(\"30\", true"), "{script}");
        assert_eq!(queue.lock().unwrap().context().grants.len(), 1);
        assert_eq!(
            storage
                .lock()
                .unwrap()
                .load_grants()
                .unwrap()
                .len(),
            1
        );

        // The grant authorizes agent reads underneath (and reports home).
        let script = dispatcher
            .handle_message(r#"{"id":"31","method":"permission.grants","params":{}}"#)
            .unwrap();
        assert!(script.contains("__resolve(\"31\", true"), "{script}");
        assert!(script.contains(&dir.to_string_lossy().to_string()), "{script}");
        let ctx = queue.lock().unwrap().context();
        let req = capability_core::CapabilityRequest {
            principal: Principal::Agent(capability_core::AgentId::new("a")),
            capability: Capability::FilesystemRead,
            resource: Resource::Path(dir.join("note.txt")),
        };
        assert!(matches!(
            policy_core::authorize(&req.principal, &req, &ctx),
            policy_core::AuthorizationDecision::Allow { .. }
        ));

        // Revoke drops it from memory and storage alike.
        let script = dispatcher
            .handle_message(&format!(
                r#"{{"id":"32","method":"permission.revoke","params":{{"capability":"FilesystemRead","path":"{}"}}}}"#,
                dir.to_string_lossy()
            ))
            .unwrap();
        assert!(script.contains("__resolve(\"32\", true"), "{script}");
        assert!(script.contains("\"removed\":2"), "{script}");
        assert!(queue.lock().unwrap().context().grants.is_empty());
        assert!(storage
            .lock()
            .unwrap()
            .load_grants()
            .unwrap()
            .is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn broad_read_grant_rejects_anything_but_reads() {
        let queue = Arc::new(Mutex::new(ApprovalQueue::new()));
        let dispatcher = dispatcher().with_approvals(queue);
        for (id, params) in [
            ("40", r#"{"capability":"FilesystemWrite","lifetime":"persistent"}"#),
            ("41", r#"{"capability":"FilesystemRead","path":"/no/such/dir/utsuwa","lifetime":"persistent"}"#),
            ("42", r#"{"capability":"FilesystemRead","path":"relative/path","lifetime":"persistent"}"#),
            ("43", r#"{"capability":"FilesystemRead","lifetime":"once"}"#),
        ] {
            let script = dispatcher
                .handle_message(&format!(
                    r#"{{"id":"{id}","method":"permission.grant","params":{params}}}"#
                ))
                .unwrap();
            assert!(script.contains(&format!("__resolve(\"{id}\", false")), "{script}");
        }
        // Secret roots are refused even from their owner.
        let ssh = std::env::temp_dir().join(format!("utsuwa-grant-ssh-{}", std::process::id()));
        std::fs::create_dir_all(ssh.join(".ssh")).unwrap();
        let script = dispatcher
            .handle_message(&format!(
                r#"{{"id":"44","method":"permission.grant","params":{{"capability":"FilesystemRead","path":"{}","lifetime":"persistent"}}}}"#,
                ssh.join(".ssh").to_string_lossy()
            ))
            .unwrap();
        assert!(script.contains("__resolve(\"44\", false"), "{script}");
        std::fs::remove_dir_all(&ssh).ok();
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

    /// Minimal echo guest: returns its arguments unchanged.
    const ECHO_WAT: &str = r#"(module
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 4096))
  (func (export "alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (global.get $heap) (local.set $ptr)
    (global.get $heap) (local.get $len) (i32.add) (global.set $heap)
    (local.get $ptr))
  (func (export "invoke") (param $ptr i32) (param $len i32) (result i64)
    (local $out i32)
    (local.get $len) (call 0) (local.set $out)
    (memory.copy (local.get $out) (local.get $ptr) (local.get $len))
    (i64.or
      (i64.shl (i64.extend_i32_u (local.get $out)) (i64.const 32))
      (i64.extend_i32_u (local.get $len)))))"#;

    fn plugin_root(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "utsuwa-dispatcher-plugin-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("echo");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.toml"),
            "[plugin]\nid = \"echo\"\nname = \"Echo\"\nversion = \"0.1.0\"\napi = 1\n\n\
             [runtime]\ntype = \"wasm\"\n\n\
             [[tools]]\nname = \"run\"\ndescription = \"echo\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("plugin.wasm"), wat::parse_str(ECHO_WAT).unwrap()).unwrap();
        root
    }

    #[test]
    fn plugin_lifecycle_through_ipc() {
        let root = plugin_root("lifecycle");
        let agent = stub_runtime();
        agent
            .plugin_manager()
            .discover_dir(&root)
            .unwrap();
        agent.plugin_manager().load("echo").unwrap();
        let dispatcher = dispatcher().with_agent(agent);

        // Loaded, not yet serving.
        let script = dispatcher
            .handle_message(r#"{"id":"50","method":"plugin.list","params":{}}"#)
            .unwrap();
        assert!(script.contains("__resolve(\"50\", true"), "{script}");
        assert!(script.contains("\"state\":\"loaded\""), "{script}");

        // Enable serves it; list shows the state change.
        let script = dispatcher
            .handle_message(r#"{"id":"51","method":"plugin.enable","params":{"id":"echo"}}"#)
            .unwrap();
        assert!(script.contains("\"ok\":true"), "{script}");
        let script = dispatcher
            .handle_message(r#"{"id":"52","method":"plugin.list","params":{}}"#)
            .unwrap();
        assert!(script.contains("\"state\":\"enabled\""), "{script}");

        // Disable unserves; unknown ids and missing ids are caller errors.
        let script = dispatcher
            .handle_message(r#"{"id":"53","method":"plugin.disable","params":{"id":"echo"}}"#)
            .unwrap();
        assert!(script.contains("\"ok\":true"), "{script}");
        let script = dispatcher
            .handle_message(r#"{"id":"54","method":"plugin.enable","params":{"id":"nope"}}"#)
            .unwrap();
        assert!(script.contains("__resolve(\"54\", false"), "{script}");
        assert!(script.contains("unknown plugin"), "{script}");
        let script = dispatcher
            .handle_message(r#"{"id":"55","method":"plugin.update","params":{}}"#)
            .unwrap();
        assert!(script.contains("__resolve(\"55\", false"), "{script}");
        assert!(script.contains("need a string 'id'"), "{script}");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn plugin_methods_without_runtime_reject() {
        let script = dispatcher()
            .handle_message(r#"{"id":"56","method":"plugin.list","params":{}}"#)
            .unwrap();
        assert!(script.contains("agent runtime is not attached"), "{script}");
        let script = dispatcher()
            .handle_message(r#"{"id":"57","method":"plugin.remove","params":{"id":"echo"}}"#)
            .unwrap();
        assert!(script.contains("agent runtime is not attached"), "{script}");
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
    fn activity_lists_newest_first_with_limit() {
        use audit_core::{AuditOutcome, AuditRecord, AuditSink};
        let sink = Arc::new(audit_core::InMemorySink::new());
        for i in 0..5 {
            sink.record(AuditRecord::now(
                capability_core::Principal::User,
                None,
                None,
                AuditOutcome::Executed,
                format!("step-{i}"),
            ));
        }
        let audited = dispatcher().with_audit(sink);

        let script = audited
            .handle_message(r#"{"id":"50","method":"activity.list","params":{"limit":2}}"#)
            .unwrap();
        assert!(script.contains("__resolve(\"50\", true"), "{script}");
        // Newest first: step-4 before step-3, and step-0 is cut off.
        let step4 = script.find("step-4").unwrap();
        let step3 = script.find("step-3").unwrap();
        assert!(step4 < step3);
        assert!(!script.contains("step-0"));

        // Missing sink is a typed error, not a panic.
        let script = dispatcher()
            .handle_message(r#"{"id":"51","method":"activity.list","params":{}}"#)
            .unwrap();
        assert!(script.contains("audit sink is not attached"), "{script}");
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
