//! Typed IPC dispatcher (plan Task 6).
//!
//! Parses untrusted frontend strings into [`IpcRequest`] and answers the
//! methods the host currently implements. Everything else fails with a
//! typed error — unknown methods are never silently ignored, and raw OS
//! operations are unrepresentable (see `ipc-core`: they are not members of
//! [`IpcMethod`], so they cannot even parse).

use super::plugins::PluginOp;
#[cfg(test)]
use crate::runtime::sync_factory;
use crate::runtime::AgentRuntime;
use ipc_core::{HostEvent, IpcErrorBody, IpcErrorResponse, IpcMethod, IpcRequest, IpcResponse};
use policy_core::ApprovalQueue;
use serde_json::Value;
use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    Arc, Mutex,
};

/// Version of the `window.utsuwa` bridge protocol spoken by this host.
/// Must stay in sync with `BRIDGE_VERSION` in `src/bridge.js` and
/// `EXPECTED_BRIDGE_PROTOCOL` in the frontend handshake module. Bump on any
/// incompatible bridge change so a stale/partial bridge is detected instead
/// of being mistaken for a healthy host.
pub const BRIDGE_PROTOCOL_VERSION: u32 = 1;

/// Host state the dispatcher may report. The agent runtime and storage
/// attachments are optional so headless/unit configurations keep working;
/// methods needing a missing attachment fail with a typed error.
pub struct Dispatcher {
    pub app_version: String,
    pub(crate) audio_capture: Option<Arc<crate::audio::AudioCaptureManager>>,
    pub(crate) media_registry: Arc<crate::audio::MediaRegistry>,
    pub(crate) approvals: Option<Arc<Mutex<ApprovalQueue>>>,
    pub(crate) agent: Option<Arc<AgentRuntime>>,
    pub(crate) sensors: Option<Arc<crate::runtime::SensorActivityHub>>,
    pub(crate) storage: Option<Arc<Mutex<storage_core::Storage>>>,
    pub(crate) audit: Option<Arc<audit_core::InMemorySink>>,
    pub(crate) secrets: Option<Arc<dyn secret_core::SecretStore>>,
    /// Set when the frontend completes the deterministic handshake
    /// (`host.frontend_ready`) after registering its event listeners.
    pub(crate) frontend_ready: Arc<AtomicBool>,
}

impl Clone for Dispatcher {
    fn clone(&self) -> Self {
        Self {
            app_version: self.app_version.clone(),
            audio_capture: self.audio_capture.clone(),
            media_registry: self.media_registry.clone(),
            approvals: self.approvals.clone(),
            agent: self.agent.clone(),
            sensors: self.sensors.clone(),
            storage: self.storage.clone(),
            audit: self.audit.clone(),
            secrets: self.secrets.clone(),
            frontend_ready: Arc::clone(&self.frontend_ready),
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
            sensors: None,
            storage: None,
            audit: None,
            secrets: None,
            frontend_ready: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Whether the frontend has completed the deterministic handshake.
    /// Shared across dispatcher clones (the flag lives behind an `Arc`).
    pub fn is_frontend_ready(&self) -> bool {
        self.frontend_ready.load(Ordering::SeqCst)
    }

    /// Queryable host state for bootstrap. The frontend calls
    /// `host.runtime_state` / `host.frontend_ready` so a missed one-shot
    /// `app.ready` event can never strand it.
    pub fn runtime_state(&self) -> Value {
        serde_json::json!({
            "bridgeProtocol": BRIDGE_PROTOCOL_VERSION,
            "hostVersion": self.app_version,
            "ready": true,
            "platform": std::env::consts::OS,
            "desktopBackend": desktop_backend(),
            "frontendReady": self.is_frontend_ready(),
            "capabilities": {
                "agent": self.agent.is_some(),
                "audioCapture": self.audio_capture.is_some(),
                "storage": self.storage.is_some(),
            },
        })
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

    /// Attach the host-owned sensor hub. Privacy indicators read from here
    /// first so they stay authoritative even when the agent runtime failed
    /// to initialize (degraded mode).
    pub fn with_sensors(mut self, sensors: Arc<crate::runtime::SensorActivityHub>) -> Self {
        self.sensors = Some(sensors);
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
        // Method name only (never params: they may carry secrets). Proves
        // which frontend calls reach the host and when; the matching
        // `webview.ipc.reply` proves the dispatch completed (a request
        // without a reply means the handler wedged the calling thread).
        tracing::debug!(method = ?request.method, id = %request.id, "webview.ipc.request");

        if request.method == IpcMethod::ProvidersFetchModels {
            let dispatcher = self.clone();
            let reply_id = request.id.clone();
            let method = request.method.clone();
            // Run off the UI/IPC callback thread on a dedicated worker so a
            // slow provider cannot freeze the app. The worker is a plain OS
            // thread outside every Tokio runtime, so entering an executor
            // here can never nest inside a Tokio worker.
            std::thread::spawn(move || {
                let started = std::time::Instant::now();
                let owned = request;
                let result = dispatcher.block_on_async(move |dispatcher| async move {
                    dispatcher.fetch_provider_models(&owned).await
                });
                tracing::debug!(
                    method = ?method,
                    id = %reply_id,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "webview.ipc.reply"
                );
                callback(dispatcher.reply_script_for(&reply_id, result));
            });
        } else if runs_off_ui_thread(&request.method) {
            let dispatcher = self.clone();
            let owned = request;
            // Blocking OS services (AT-SPI/D-Bus, X11, portals) must never
            // run on the UI/IPC callback thread: a same-process round-trip
            // would deadlock the main loop. The worker dispatches
            // synchronously WITHOUT entering a runtime first, so the nested
            // `block_on_host` calls inside stay legal (they refuse when a
            // Tokio context is already entered).
            let spawned = std::thread::Builder::new()
                .name("utsuwa-ipc-worker".to_string())
                .spawn(move || {
                    let started = std::time::Instant::now();
                    let script = dispatcher.reply_script(&owned);
                    tracing::debug!(
                        method = ?owned.method,
                        id = %owned.id,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "webview.ipc.reply"
                    );
                    callback(script);
                });
            if spawned.is_err() {
                tracing::warn!("dropping ipc reply: worker thread failed to spawn");
            }
        } else {
            let started = std::time::Instant::now();
            let script = self.reply_script(&request);
            tracing::debug!(
                method = ?request.method,
                id = %request.id,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "webview.ipc.reply"
            );
            callback(script);
        }
    }

    /// Drive one async dispatcher future from a synchronous IPC worker
    /// thread. Prefers the agent executor handle when attached (no extra
    /// runtime); otherwise builds a throwaway current-thread runtime for
    /// this thread only. Must only be called from non-Tokio threads — IPC
    /// workers satisfy that by construction.
    fn block_on_async<F, Fut>(&self, run: F) -> Result<Value, IpcErrorBody>
    where
        F: FnOnce(Self) -> Fut,
        Fut: std::future::Future<Output = Result<Value, IpcErrorBody>>,
    {
        debug_assert!(
            tokio::runtime::Handle::try_current().is_err(),
            "async IPC work must not block a Tokio worker"
        );
        let dispatcher = self.clone();
        if let Some(agent) = &self.agent {
            agent.executor_handle().block_on(run(dispatcher))
        } else {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| IpcErrorBody {
                    code: ipc_core::ErrorCode::Internal,
                    message: format!("could not start native model fetch: {error}"),
                })?;
            runtime.block_on(run(dispatcher))
        }
    }

    /// Async dispatch entry point for hosts that already run on an executor.
    /// Awaits async methods directly instead of bridging through `block_on`,
    /// so agent-worker callers can reuse this without nesting runtimes.
    pub async fn dispatch_async(&self, request: &IpcRequest) -> Result<Value, IpcErrorBody> {
        match request.method {
            IpcMethod::ProvidersFetchModels => self.fetch_provider_models(request).await,
            _ => self.dispatch(request),
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
            IpcMethod::HostRuntimeState => Ok(self.runtime_state()),
            IpcMethod::HostFrontendReady => {
                let first = !self.frontend_ready.swap(true, Ordering::SeqCst);
                tracing::debug!(
                    first_handshake = first,
                    "host.frontend.ready: frontend registered listeners"
                );
                Ok(self.runtime_state())
            }
            IpcMethod::DiagnosticsReport => self.diagnostics_report(request),
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
            IpcMethod::CameraActivityStatus => self.camera_activity_status(),
            IpcMethod::MicrophoneActivityStatus => self.microphone_activity_status(),
            IpcMethod::DesktopShareScreenStart => self.desktop_share_screen_start(request),
            IpcMethod::DesktopShareScreenPause => self.desktop_share_screen_pause(),
            IpcMethod::DesktopShareScreenResume => self.desktop_share_screen_resume(),
            IpcMethod::DesktopShareScreenStop => self.desktop_share_screen_stop(),
            IpcMethod::DesktopShareScreenStatus => self.desktop_share_screen_status(),
            IpcMethod::DesktopControlEnable => self.desktop_control_set(true),
            IpcMethod::DesktopControlDisable => self.desktop_control_set(false),
            IpcMethod::DesktopEmergencyStop => self.desktop_emergency_stop(),
            IpcMethod::DesktopEmergencyClear => self.desktop_emergency_clear(),
            IpcMethod::ActivityList => self.activity_list(request),
            IpcMethod::PluginList => self.plugin_list(),
            IpcMethod::PluginEnable => self.plugin_manage(request, PluginOp::Enable),
            IpcMethod::PluginDisable => self.plugin_manage(request, PluginOp::Disable),
            IpcMethod::PluginUpdate => self.plugin_manage(request, PluginOp::Update),
            IpcMethod::PluginRemove => self.plugin_manage(request, PluginOp::Remove),
        }
    }

    /// Debug-oriented frontend diagnostic report. Only known string fields
    /// are read, each truncated, so a compromised page cannot flood the
    /// host log with unbounded payloads or exfiltrate anything beyond its
    /// own error text. This method performs no host action and returns no
    /// privileged data.
    fn diagnostics_report(&self, request: &IpcRequest) -> Result<Value, IpcErrorBody> {
        let params = &request.params;
        let kind = params
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let kind = truncate(kind, 64);
        let message = params
            .get("message")
            .and_then(Value::as_str)
            .map(|m| truncate(m, 2000))
            .unwrap_or_default();
        let url = params
            .get("url")
            .and_then(Value::as_str)
            .map(|u| truncate(u, 500))
            .unwrap_or_default();
        let line = params.get("line").and_then(Value::as_u64).unwrap_or(0);
        let stack = params
            .get("stack")
            .and_then(Value::as_str)
            .map(|s| truncate(s, 2000))
            .unwrap_or_default();
        // Flood guard: a page logging in a hot loop degrades to debug
        // after the burst budget; the IPC reply is unaffected.
        let loud = diagnostics_priority();
        match kind.as_ref() {
            "window.error" | "unhandledrejection" | "console.error" if loud => {
                tracing::warn!(
                    kind = %kind,
                    message = %message,
                    url = %url,
                    line,
                    stack = %stack,
                    "webview diagnostic"
                );
            }
            _ => {
                tracing::debug!(
                    kind = %kind,
                    message = %message,
                    url = %url,
                    line,
                    throttled = !loud,
                    "webview diagnostic"
                );
            }
        }
        Ok(serde_json::json!({ "ok": true }))
    }
}

/// Lock-free flood guard for frontend diagnostics: allows `DIAG_BURST`
/// warn-level reports per rolling window; the rest degrade to debug.
const DIAG_BURST: u32 = 30;
const DIAG_WINDOW_SECS: u64 = 60;

static DIAG_WINDOW_START: AtomicU64 = AtomicU64::new(0);
static DIAG_COUNT: AtomicU32 = AtomicU32::new(0);

/// True when this report is within the warn-level burst budget.
fn diagnostics_priority() -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if now.saturating_sub(DIAG_WINDOW_START.load(Ordering::Relaxed)) >= DIAG_WINDOW_SECS {
        // New window: best-effort reset (a lost race just shifts it).
        DIAG_WINDOW_START.store(now, Ordering::Relaxed);
        DIAG_COUNT.store(1, Ordering::Relaxed);
        return true;
    }
    DIAG_COUNT.fetch_add(1, Ordering::Relaxed) < DIAG_BURST
}

/// Methods that may block on OS services (AT-SPI/D-Bus round-trips, X11,
/// portal approvals) and therefore always dispatch on a worker thread,
/// never on the UI/IPC callback thread. Everything else is in-memory or
/// local-file fast and stays inline.
fn runs_off_ui_thread(method: &IpcMethod) -> bool {
    matches!(
        method,
        IpcMethod::DesktopShareScreenStart
            | IpcMethod::DesktopShareScreenPause
            | IpcMethod::DesktopShareScreenResume
            | IpcMethod::DesktopShareScreenStop
            | IpcMethod::DesktopShareScreenStatus
            | IpcMethod::DesktopControlEnable
            | IpcMethod::DesktopControlDisable
            | IpcMethod::DesktopEmergencyStop
            | IpcMethod::DesktopEmergencyClear
    )
}

/// Desktop WebView backend driving this host process. Reported in
/// `host.runtime_state` so diagnostics can name the platform path.
fn desktop_backend() -> &'static str {
    if cfg!(target_os = "linux") {
        "gtk"
    } else if cfg!(target_os = "windows") {
        "webview2"
    } else if cfg!(target_os = "macos") {
        "wkwebview"
    } else {
        "unknown"
    }
}

/// Truncate untrusted frontend text at a char boundary for log safety.
fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        text.chars().take(max_chars).collect()
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
///
/// Lossless by construction: when the bridge already exists the event goes
/// through `__emit` (which buffers until the frontend drains it after
/// registering listeners); when the bridge initialisation script has not
/// run yet — e.g. `app.ready` evaluated before document start — the event
/// is stashed in `window.__utsuwaEarlyEvents`, which the bridge drains on
/// load. Either way the event is never silently dropped.
pub fn emit_script(event: &HostEvent) -> String {
    let data = event.data.to_string();
    let name = serde_json::to_string(&event.event).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        "(function(){{var e={name},d={data};var b=window.utsuwa;\
         if(b&&b.__emit){{b.__emit(e,d);}}else{{\
         (window.__utsuwaEarlyEvents=window.__utsuwaEarlyEvents||[]).push({{event:e,data:d}});}}}})();"
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
    fn sensor_status_served_from_host_hub_without_agent_runtime() {
        // Degraded mode: privacy indicators stay authoritative from the
        // host-owned hub even when AgentRuntime failed to initialize.
        let sensors = Arc::new(crate::runtime::SensorActivityHub::new());
        let dispatcher = dispatcher().with_sensors(sensors);
        let script = dispatcher
            .handle_message(r#"{"id":"20","method":"camera.activity.status","params":{}}"#)
            .unwrap();
        assert!(script.contains("__resolve(\"20\", true"), "{script}");
        let script = dispatcher
            .handle_message(r#"{"id":"21","method":"microphone.activity.status","params":{}}"#)
            .unwrap();
        assert!(script.contains("__resolve(\"21\", true"), "{script}");
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
            sync_factory(|| Err(crate::runtime::RuntimeError::ModelNotConfigured)),
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
            script.contains("requires one of minimal, simple, standard"),
            "{script}"
        );
        for (id, profile) in [
            ("39", "minimal"),
            ("40", "standard"),
            ("41", "developer"),
            ("42", "computeruse"),
            ("43", "full"),
        ] {
            let script = dispatcher
                .handle_message(&format!(
                    r#"{{"id":"{id}","method":"settings.set","params":{{"key":"{}","value":"{profile}"}}}}"#,
                    crate::runtime::SETTING_TOOL_PROFILE
                ))
                .unwrap();
            assert!(script.contains("\"ok\":true"), "{profile}: {script}");
        }
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
            sync_factory(|| Err(crate::runtime::RuntimeError::ModelNotConfigured)),
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
                sync_factory(|| Err(crate::runtime::RuntimeError::ModelNotConfigured)),
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
    fn model_vision_override_roundtrips_and_clears() {
        let storage = temp_storage("model-vision");
        let secrets: Arc<dyn secret_core::SecretStore> =
            Arc::new(secret_core::MemoryStore::default());
        let dispatcher = dispatcher()
            .with_storage(Arc::clone(&storage))
            .with_secret_store(Arc::clone(&secrets));
        let script = dispatcher
            .handle_message(
                r#"{"id":"40","method":"settings.set_model_provider","params":{"provider":"ollama","base_url":"http://localhost:11434/v1","model":"llava:13b","vision":true}}"#,
            )
            .unwrap();
        assert!(script.contains("__resolve(\"40\", true"), "{script}");
        assert_eq!(
            storage.lock().unwrap().get_setting("model.vision").unwrap(),
            Some(serde_json::json!(true))
        );
        let script = dispatcher
            .handle_message(r#"{"id":"41","method":"settings.get_model_provider","params":{}}"#)
            .unwrap();
        assert!(script.contains("\"vision\":true"), "{script}");
        // A sync without `vision` clears the stale override so the
        // provider/model-name heuristic applies to the new model.
        let script = dispatcher
            .handle_message(
                r#"{"id":"42","method":"settings.set_model_provider","params":{"provider":"ollama","base_url":"http://localhost:11434/v1","model":"llama3.1:8b"}}"#,
            )
            .unwrap();
        assert!(script.contains("__resolve(\"42\", true"), "{script}");
        assert_eq!(
            storage.lock().unwrap().get_setting("model.vision").unwrap(),
            None
        );
        // Non-boolean vision is rejected, not coerced.
        let script = dispatcher
            .handle_message(
                r#"{"id":"43","method":"settings.set_model_provider","params":{"provider":"ollama","base_url":"http://localhost:11434/v1","model":"llava:13b","vision":"yes"}}"#,
            )
            .unwrap();
        assert!(script.contains("__resolve(\"43\", false"), "{script}");
        // The generic endpoint cannot write the vision override either.
        let script = dispatcher
            .handle_message(
                r#"{"id":"44","method":"settings.set","params":{"key":"model.vision","value":true}}"#,
            )
            .unwrap();
        assert!(script.contains("__resolve(\"44\", false"), "{script}");
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
        // Live bridge path …
        assert!(script.contains("b.__emit(e,d)"), "{script}");
        assert!(script.contains("\"agent.text_delta\""), "{script}");
        assert!(script.contains("\"delta\":\"hi\""), "{script}");
        // … plus the pre-bridge stash so early events are never dropped.
        assert!(script.contains("__utsuwaEarlyEvents"), "{script}");
    }

    #[test]
    fn runtime_state_reports_version_platform_and_capabilities() {
        let dispatcher = dispatcher().with_agent(stub_runtime());
        let script = dispatcher
            .handle_message(r#"{"id":"70","method":"host.runtime_state","params":{}}"#)
            .unwrap();
        assert!(script.contains("__resolve(\"70\", true"), "{script}");
        assert!(
            script.contains(&format!("\"bridgeProtocol\":{BRIDGE_PROTOCOL_VERSION}")),
            "{script}"
        );
        assert!(script.contains("\"hostVersion\":\"0.1.0\""), "{script}");
        assert!(script.contains("\"ready\":true"), "{script}");
        assert!(
            script.contains(&format!("\"platform\":\"{}\"", std::env::consts::OS)),
            "{script}"
        );
        assert!(script.contains("\"desktopBackend\":"), "{script}");
        assert!(script.contains("\"agent\":true"), "{script}");
        assert!(script.contains("\"audioCapture\":false"), "{script}");
        assert!(script.contains("\"storage\":false"), "{script}");
    }

    #[test]
    fn frontend_ready_handshake_marks_ready_and_returns_state() {
        let dispatcher = dispatcher();
        assert!(!dispatcher.is_frontend_ready());
        let script = dispatcher
            .handle_message(r#"{"id":"71","method":"host.frontend_ready","params":{}}"#)
            .unwrap();
        assert!(script.contains("__resolve(\"71\", true"), "{script}");
        assert!(script.contains("\"ready\":true"), "{script}");
        assert!(dispatcher.is_frontend_ready());
        // Clones share the flag: the IPC-handler clone and the emit clone
        // observe the same handshake.
        assert!(dispatcher.clone().is_frontend_ready());
        // Repeat handshakes (reload/navigation) stay idempotent.
        let script = dispatcher
            .handle_message(r#"{"id":"72","method":"host.frontend_ready","params":{}}"#)
            .unwrap();
        assert!(script.contains("__resolve(\"72\", true"), "{script}");
    }

    #[test]
    fn diagnostics_report_is_accepted_without_side_effects() {
        let script = dispatcher()
            .handle_message(
                r#"{"id":"73","method":"diagnostics.report","params":{"kind":"window.error","message":"boom","url":"companion://app/app","line":12,"stack":"at f"}}"#,
            )
            .unwrap();
        assert!(script.contains("__resolve(\"73\", true"), "{script}");
        assert!(script.contains("\"ok\":true"), "{script}");
        // Unknown kinds and missing fields degrade, never reject.
        let script = dispatcher()
            .handle_message(r#"{"id":"74","method":"diagnostics.report","params":{}}"#)
            .unwrap();
        assert!(script.contains("__resolve(\"74\", true"), "{script}");
    }
}
