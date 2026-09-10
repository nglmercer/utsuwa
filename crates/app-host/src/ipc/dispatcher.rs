//! Typed IPC dispatcher (plan Task 6).
//!
//! Parses untrusted frontend strings into [`IpcRequest`] and answers the
//! methods the host currently implements. Everything else fails with a
//! typed error — unknown methods are never silently ignored, and raw OS
//! operations are unrepresentable (see `ipc-core`: they are not members of
//! [`IpcMethod`], so they cannot even parse).

use super::plugins::PluginOp;
use crate::runtime::AgentRuntime;
use ipc_core::{HostEvent, IpcErrorBody, IpcErrorResponse, IpcMethod, IpcRequest, IpcResponse};
use policy_core::ApprovalQueue;
use serde_json::Value;
use std::sync::{Arc, Mutex};

/// Host state the dispatcher may report. The agent runtime and storage
/// attachments are optional so headless/unit configurations keep working;
/// methods needing a missing attachment fail with a typed error.
pub struct Dispatcher {
    pub app_version: String,
    pub(crate) audio_capture: Option<Arc<crate::audio::AudioCaptureManager>>,
    pub(crate) media_registry: Arc<crate::audio::MediaRegistry>,
    pub(crate) approvals: Option<Arc<Mutex<ApprovalQueue>>>,
    pub(crate) agent: Option<Arc<AgentRuntime>>,
    pub(crate) storage: Option<Arc<Mutex<storage_core::Storage>>>,
    pub(crate) audit: Option<Arc<audit_core::InMemorySink>>,
    pub(crate) secrets: Option<Arc<dyn secret_core::SecretStore>>,
}

impl Clone for Dispatcher {
    fn clone(&self) -> Self {
        Self {
            app_version: self.app_version.clone(),
            audio_capture: self.audio_capture.clone(),
            media_registry: self.media_registry.clone(),
            approvals: self.approvals.clone(),
            agent: self.agent.clone(),
            storage: self.storage.clone(),
            audit: self.audit.clone(),
            secrets: self.secrets.clone(),
        }
    }
}

impl Dispatcher {
    pub fn new(app_version: impl Into<String>) -> Self {
        Self {
            app_version: app_version.into(),
            audio_capture: None,
            media_registry: Arc::new(crate::audio::MediaRegistry::new()),
            approvals: None,
            agent: None,
            storage: None,
            audit: None,
            secrets: None,
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

    /// Attach the same host secret store used by the model provider. The
    /// frontend never receives the key back through IPC.
    pub fn with_secret_store(mut self, secrets: Arc<dyn secret_core::SecretStore>) -> Self {
        self.secrets = Some(secrets);
        self
    }

    /// Attach native CPAL capture. The media registry is created with the
    /// dispatcher and remains available to the custom companion scheme.
    pub fn with_audio_capture(
        mut self,
        audio_capture: Arc<crate::audio::AudioCaptureManager>,
    ) -> Self {
        self.audio_capture = Some(audio_capture);
        self
    }

    pub fn media_registry(&self) -> Arc<crate::audio::MediaRegistry> {
        self.media_registry.clone()
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

    /// Dispatch an IPC request and deliver its reply asynchronously.
    ///
    /// Model catalog requests perform network I/O through the native
    /// OpenAI-compatible client because the embedded WebView cannot call
    /// providers that do not enable CORS. Keeping that work off the UI/IPC
    /// callback thread also prevents a slow provider from freezing the app.
    pub fn handle_message_with_callback<F>(&self, raw: &str, callback: F)
    where
        F: FnOnce(String) + Send + 'static,
    {
        let body = raw_body(raw);
        let request = match IpcRequest::parse(&body) {
            Ok(request) => request,
            Err(err) => {
                tracing::warn!(%err, "dropping unparsable ipc message (no id to reply to)");
                return;
            }
        };

        if request.method == IpcMethod::ProvidersFetchModels {
            let dispatcher = self.clone();
            std::thread::spawn(move || {
                let result = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| IpcErrorBody {
                        code: ipc_core::ErrorCode::Internal,
                        message: format!("could not start native model fetch: {error}"),
                    })
                    .and_then(|runtime| {
                        runtime.block_on(dispatcher.fetch_provider_models(&request))
                    });
                callback(dispatcher.reply_script_for(&request.id, result));
            });
        } else {
            callback(self.reply_script(&request));
        }
    }

    fn reply_script(&self, request: &IpcRequest) -> String {
        self.reply_script_for(&request.id, self.dispatch(request))
    }

    fn reply_script_for(&self, id: &str, result: Result<Value, IpcErrorBody>) -> String {
        match result {
            Ok(result) => {
                let response = IpcResponse::ok(id, result);
                match serde_json::to_string(&response) {
                    Ok(json) => resolve_script(id, true, &json_payload(&json, true)),
                    Err(err) => {
                        tracing::error!(%err, "failed to encode ipc response");
                        internal_error_script(id)
                    }
                }
            }
            Err(error) => {
                let response = IpcErrorResponse {
                    id: id.to_string(),
                    error,
                };
                match serde_json::to_string(&response) {
                    Ok(json) => resolve_script(id, false, &json_payload(&json, false)),
                    Err(err) => {
                        tracing::error!(%err, "failed to encode ipc error");
                        internal_error_script(id)
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
            IpcMethod::SettingsGetModelProvider => self.settings_get_model_provider(),
            IpcMethod::SettingsSetModelProvider => self.settings_set_model_provider(request),
            IpcMethod::ProvidersFetchModels => Err(IpcErrorBody {
                code: ipc_core::ErrorCode::Internal,
                message: "providers.fetch_models must be dispatched asynchronously".to_string(),
            }),
            IpcMethod::AudioCaptureStart => self.audio_capture_start(request),
            IpcMethod::AudioCaptureStop => self.audio_capture_stop(),
            IpcMethod::AudioCaptureCancel => self.audio_capture_cancel(),
            IpcMethod::ActivityList => self.activity_list(request),
            IpcMethod::PluginList => self.plugin_list(),
            IpcMethod::PluginEnable => self.plugin_manage(request, PluginOp::Enable),
            IpcMethod::PluginDisable => self.plugin_manage(request, PluginOp::Disable),
            IpcMethod::PluginUpdate => self.plugin_manage(request, PluginOp::Update),
            IpcMethod::PluginRemove => self.plugin_manage(request, PluginOp::Remove),
        }
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
        let pending = queue.lock().unwrap().submit(
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
        let dir = std::env::temp_dir().join(format!("utsuwa-grant-test-{}", std::process::id()));
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
        assert_eq!(storage.lock().unwrap().load_grants().unwrap().len(), 1);

        // The grant authorizes agent reads underneath (and reports home).
        let script = dispatcher
            .handle_message(r#"{"id":"31","method":"permission.grants","params":{}}"#)
            .unwrap();
        assert!(script.contains("__resolve(\"31\", true"), "{script}");
        assert!(
            script.contains(&dir.to_string_lossy().to_string()),
            "{script}"
        );
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
        assert!(storage.lock().unwrap().load_grants().unwrap().is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn broad_read_grant_rejects_anything_but_reads() {
        let queue = Arc::new(Mutex::new(ApprovalQueue::new()));
        let dispatcher = dispatcher().with_approvals(queue);
        for (id, params) in [
            (
                "40",
                r#"{"capability":"FilesystemWrite","lifetime":"persistent"}"#,
            ),
            (
                "41",
                r#"{"capability":"FilesystemRead","path":"/no/such/dir/utsuwa","lifetime":"persistent"}"#,
            ),
            (
                "42",
                r#"{"capability":"FilesystemRead","path":"relative/path","lifetime":"persistent"}"#,
            ),
            ("43", r#"{"capability":"FilesystemRead","lifetime":"once"}"#),
        ] {
            let script = dispatcher
                .handle_message(&format!(
                    r#"{{"id":"{id}","method":"permission.grant","params":{params}}}"#
                ))
                .unwrap();
            assert!(
                script.contains(&format!("__resolve(\"{id}\", false")),
                "{script}"
            );
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
        let emit: crate::runtime::EmitFn = Arc::new(|_| {});
        AgentRuntime::start_with_factory(
            Arc::new(Mutex::new(ApprovalQueue::new())),
            None,
            None,
            emit,
            Arc::new(|| Err(crate::runtime::RuntimeError::ModelNotConfigured)),
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

        // The autonomous setting is native-persisted and type-checked rather
        // than being a frontend-only flag.
        let script = dispatcher
            .handle_message(&format!(
                r#"{{"id":"33","method":"settings.get","params":{{"key":"{}"}}}}"#,
                crate::runtime::SETTING_AUTONOMOUS_FULL_ACCESS
            ))
            .unwrap();
        assert!(script.contains("\"value\":false"), "{script}");
        let script = dispatcher
            .handle_message(&format!(
                r#"{{"id":"34","method":"settings.set","params":{{"key":"{}","value":true}}}}"#,
                crate::runtime::SETTING_AUTONOMOUS_FULL_ACCESS
            ))
            .unwrap();
        assert!(script.contains("\"ok\":true"), "{script}");
        let script = dispatcher
            .handle_message(&format!(
                r#"{{"id":"35","method":"settings.get","params":{{"key":"{}"}}}}"#,
                crate::runtime::SETTING_AUTONOMOUS_FULL_ACCESS
            ))
            .unwrap();
        assert!(script.contains("\"value\":true"), "{script}");
        let script = dispatcher
            .handle_message(&format!(
                r#"{{"id":"36","method":"settings.set","params":{{"key":"{}","value":"yes"}}}}"#,
                crate::runtime::SETTING_AUTONOMOUS_FULL_ACCESS
            ))
            .unwrap();
        assert!(script.contains("requires a boolean value"), "{script}");

        let script = dispatcher
            .handle_message(&format!(
                r#"{{"id":"37","method":"settings.set","params":{{"key":"{}","value":"simple"}}}}"#,
                crate::runtime::SETTING_TOOL_PROFILE
            ))
            .unwrap();
        assert!(script.contains("\"ok\":true"), "{script}");
        let script = dispatcher
            .handle_message(&format!(
                r#"{{"id":"38","method":"settings.set","params":{{"key":"{}","value":"tiny"}}}}"#,
                crate::runtime::SETTING_TOOL_PROFILE
            ))
            .unwrap();
        assert!(
            script.contains("requires the string 'simple' or 'full'"),
            "{script}"
        );
    }

    #[test]
    fn autonomous_setting_updates_the_live_agent_authorizer() {
        let storage = temp_storage("autonomous-live");
        let approvals = Arc::new(Mutex::new(ApprovalQueue::new()));
        let emit: crate::runtime::EmitFn = Arc::new(|_| {});
        let agent = AgentRuntime::start_with_factory(
            Arc::clone(&approvals),
            Some(Arc::clone(&storage)),
            None,
            emit,
            Arc::new(|| Err(crate::runtime::RuntimeError::ModelNotConfigured)),
        )
        .unwrap();
        assert!(!agent.autonomous_full_access_enabled());

        let dispatcher = dispatcher()
            .with_storage(storage)
            .with_approvals(approvals)
            .with_agent(Arc::clone(&agent));
        let script = dispatcher
            .handle_message(&format!(
                r#"{{"id":"60","method":"settings.set","params":{{"key":"{}","value":true}}}}"#,
                crate::runtime::SETTING_AUTONOMOUS_FULL_ACCESS
            ))
            .unwrap();
        assert!(script.contains("\"ok\":true"), "{script}");
        assert!(agent.autonomous_full_access_enabled());

        let script = dispatcher
            .handle_message(&format!(
                r#"{{"id":"61","method":"settings.set","params":{{"key":"{}","value":false}}}}"#,
                crate::runtime::SETTING_AUTONOMOUS_FULL_ACCESS
            ))
            .unwrap();
        assert!(script.contains("\"ok\":true"), "{script}");
        assert!(!agent.autonomous_full_access_enabled());
    }

    #[test]
    fn autonomous_setting_survives_runtime_restart() {
        let storage = temp_storage("autonomous-restart");
        storage
            .lock()
            .unwrap()
            .set_setting(
                crate::runtime::SETTING_AUTONOMOUS_FULL_ACCESS,
                &serde_json::json!(true),
            )
            .unwrap();

        let start = |storage: Arc<Mutex<storage_core::Storage>>| {
            AgentRuntime::start_with_factory(
                Arc::new(Mutex::new(ApprovalQueue::new())),
                Some(storage),
                None,
                Arc::new(|_| {}),
                Arc::new(|| Err(crate::runtime::RuntimeError::ModelNotConfigured)),
            )
            .unwrap()
        };

        let first = start(Arc::clone(&storage));
        assert!(first.autonomous_full_access_enabled());
        drop(first);

        storage
            .lock()
            .unwrap()
            .set_setting(
                crate::runtime::SETTING_AUTONOMOUS_FULL_ACCESS,
                &serde_json::json!(false),
            )
            .unwrap();
        let second = start(Arc::clone(&storage));
        assert!(!second.autonomous_full_access_enabled());
    }

    #[test]
    fn model_settings_use_native_storage_and_keychain_only() {
        use secret_core::SecretStore;

        let storage = temp_storage("model-settings");
        let secrets: Arc<dyn SecretStore> = Arc::new(secret_core::MemoryStore::default());
        let dispatcher = dispatcher()
            .with_storage(Arc::clone(&storage))
            .with_secret_store(Arc::clone(&secrets));
        let script = dispatcher
            .handle_message(
                r#"{"id":"35","method":"settings.set_model_provider","params":{"provider":"openai","base_url":"https://api.openai.com/v1/","model":"gpt-test","api_key":"sk-test-secret"}}"#,
            )
            .unwrap();
        assert!(script.contains("__resolve(\"35\", true"), "{script}");
        assert!(
            !script.contains("sk-test-secret"),
            "secret returned over IPC: {script}"
        );

        let store = storage.lock().unwrap();
        assert_eq!(
            store.get_setting("model.provider").unwrap(),
            Some(serde_json::json!("openai"))
        );
        assert_eq!(
            store.get_setting("model.base_url").unwrap(),
            Some(serde_json::json!("https://api.openai.com/v1/"))
        );
        assert_eq!(
            store.get_setting("model.name").unwrap(),
            Some(serde_json::json!("gpt-test"))
        );
        assert_eq!(store.get_setting("model.api_key").unwrap(), None);
        drop(store);
        assert_eq!(
            secrets.get(secret_core::ACCOUNT_MODEL_API_KEY).unwrap(),
            Some("sk-test-secret".to_string())
        );

        let script = dispatcher
            .handle_message(r#"{"id":"36","method":"settings.get_model_provider","params":{}}"#)
            .unwrap();
        assert!(script.contains("\"provider\":\"openai\""), "{script}");
        assert!(script.contains("\"has_api_key\":true"), "{script}");
        assert!(
            !script.contains("sk-test-secret"),
            "secret returned over IPC: {script}"
        );

        let script = dispatcher
            .handle_message(
                r#"{"id":"38","method":"settings.set_model_provider","params":{"provider":"custom","base_url":"file:///etc/passwd","model":"m"}}"#,
            )
            .unwrap();
        assert!(script.contains("__resolve(\"38\", false"), "{script}");

        let script = dispatcher
            .handle_message(
                r#"{"id":"37","method":"settings.set","params":{"key":"model.api_key","value":"bad"}}"#,
            )
            .unwrap();
        assert!(script.contains("__resolve(\"37\", false"), "{script}");
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
        agent.plugin_manager().discover_dir(&root).unwrap();
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
            .handle_message(
                r#"{"id":"40","method":"agent.send_message","params":{"text":"hello"}}"#,
            )
            .unwrap();
        assert!(script.contains("__resolve(\"40\", true"), "{script}");
        assert!(script.contains("\"accepted\":true"), "{script}");

        for (id, params) in [("41", "{}"), ("42", r#"{"text":"  "}"#)] {
            let script = dispatcher
                .handle_message(&format!(
                    r#"{{"id":"{id}","method":"agent.send_message","params":{params}}}"#
                ))
                .unwrap();
            assert!(
                script.contains(&format!("__resolve(\"{id}\", false")),
                "{script}"
            );
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
