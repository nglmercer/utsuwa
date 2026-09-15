//! Live agent runtime (Task 18): model turns run in the host binary.
//!
//! Flow: `agent.send_message` → worker task runs
//! [`agent_core::Agent::turn_with_tools`] against the filesystem tool
//! registry and the live [`ApprovalQueue`] grants. A turn that needs the
//! user stops with a [`policy_core::PendingApproval`]: the runtime
//! publishes it to the queue (the permission dialog resolves it),
//! emits `permission.requested`, and suspends the transcript. Resolving
//! the request — approve or deny — resumes the turn with an outcome note,
//! so the model re-issues (or drops) the call against the updated policy.
//! Approvals never bypass policy; they extend it, exactly like the
//! manual dialog flow.
//!
//! Host events emitted: `agent.turn_done`, `agent.turn_suspended`,
//! `agent.turn_failed`, `agent.turn_cancelled`, `permission.requested`,
//! `permission.dismissed`.
//!
//! The runtime is a thin facade: session lifecycle, turn execution,
//! authorization, prompts, and provider configuration live in the
//! child modules. Tool composition lives in `crate::tooling`.

pub mod authorization;
pub mod events;
pub mod prompts;
pub mod providers;
pub mod session;
pub mod turn;

#[cfg(test)]
pub(crate) use self::authorization::QueueAuthorizer;
#[cfg(test)]
pub(crate) use self::prompts::compose_host_system_prompt_for_context;
pub use self::prompts::host_environment_context;
#[cfg(test)]
pub(crate) use self::prompts::host_os_label;
#[cfg(test)]
pub(crate) use self::prompts::{compose_host_system_prompt, compose_host_system_prompt_for};
#[cfg(test)]
pub(crate) use self::providers::configured_tool_profile;
pub use self::providers::{
    normalize_provider_base_url, tool_profile_for_provider, ToolProfile, SETTING_API_KEY,
    SETTING_AUTONOMOUS_FULL_ACCESS, SETTING_BASE_URL, SETTING_BROWSER_CDP_ENDPOINT,
    SETTING_MCP_SERVERS, SETTING_MODEL_NAME, SETTING_MODEL_VISION, SETTING_PLUGIN_DIR,
    SETTING_PROVIDER, SETTING_TOOL_PROFILE,
};
pub(crate) use self::providers::{
    provider_factory_with_secrets, read_autonomous_full_access, read_cdp_endpoint,
    read_mcp_configs, read_plugin_dir,
};
pub use self::session::AgentRequest;
pub(crate) use self::session::State;
pub(crate) use file_target::{ConversationFileContext, FileRef};
pub(crate) use host_core::HostEnvironment;
pub(crate) use tool_process::{ProcessLimits, ProcessManager};

use crate::tooling::{ProcessToolPack, SystemToolPack};
use audit_core::AuditSink;
use capability_core::AgentId;
use ipc_core::HostEvent;
use mcp_runtime::McpManager;
use model_core::ModelProvider;
use policy_core::ApprovalQueue;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Instant;
use storage_core::Storage;
use tool_sdk::ToolCatalog;

/// Callback the runtime uses to reach the frontend (reply queue + wake).
pub type EmitFn = Arc<dyn Fn(HostEvent) + Send + Sync>;

/// Host-owned privacy state shared by native capture, model-facing tools,
/// IPC snapshots, and frontend event publication. It deliberately outlives
/// `AgentRuntime`: a model/executor failure must never hide an active sensor.
#[derive(Clone)]
pub struct SensorActivityHub {
    camera: tool_camera::CameraState,
    microphone: tool_audio::AudioState,
}

impl SensorActivityHub {
    pub fn new() -> Self {
        Self {
            camera: tool_camera::CameraState::new(),
            microphone: tool_audio::AudioState::new(),
        }
    }

    pub fn camera(&self) -> tool_camera::CameraState {
        self.camera.clone()
    }

    pub fn microphone(&self) -> tool_audio::AudioState {
        self.microphone.clone()
    }

    pub fn camera_status(&self) -> tool_camera::CameraActivityState {
        self.camera.activity_state()
    }

    pub fn microphone_status(&self) -> tool_audio::MicrophoneActivityState {
        self.microphone.activity_state()
    }

    /// Attach host-owned event publication. The callbacks are synchronous,
    /// tiny, and invoked after state is updated, so the GTK thread is never
    /// asked to perform capture or other blocking work.
    pub fn attach_event_publisher(&self, emit: &EmitFn) {
        let emit_camera = Arc::clone(emit);
        self.camera.add_activity_listener(move |state| {
            if let Ok(data) = serde_json::to_value(state) {
                emit_camera(HostEvent {
                    event: "camera.activity.changed".to_string(),
                    data,
                });
            }
        });
        let emit_microphone = Arc::clone(emit);
        self.microphone.add_activity_listener(move |state| {
            if let Ok(data) = serde_json::to_value(state) {
                emit_microphone(HostEvent {
                    event: "microphone.activity.changed".to_string(),
                    data,
                });
            }
        });
    }
}

impl Default for SensorActivityHub {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("agent runtime unavailable: {0}")]
    Executor(String),
    #[error("model is not configured (set model.base_url and model.name in settings)")]
    ModelNotConfigured,
    #[error("model settings are unreadable: {0}")]
    Settings(String),
    #[error("tool registry failed: {0}")]
    Tools(String),
}

/// User-visible state for the native Share Screen control. Capture and
/// control are intentionally separate fields: starting a screen share never
/// flips `control_enabled`, and control tools remain governed by their own
/// DesktopControl capability tickets.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ScreenShareStatus {
    pub available: bool,
    pub backend: String,
    pub sharing: bool,
    pub paused: bool,
    pub control_enabled: bool,
    pub emergency_stopped: bool,
    pub session_id: Option<String>,
    pub target: Option<tool_desktop::CaptureTarget>,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub displays: Vec<tool_desktop::DisplayInfo>,
    pub windows: Vec<tool_desktop::WindowInfo>,
}

struct ScreenShareState {
    backend: Arc<dyn tool_desktop::DesktopBackend>,
    session_id: tool_desktop::CaptureSessionId,
    target: tool_desktop::CaptureTarget,
    paused: bool,
    control_enabled: bool,
    started_at: chrono::DateTime<chrono::Utc>,
}

/// Host-owned agent loop. Construct with [`AgentRuntime::start`], which
/// returns an `Arc` because worker tasks and the dispatcher share it.
pub struct AgentRuntime {
    agent_id: AgentId,
    approvals: Arc<Mutex<ApprovalQueue>>,
    audit: Option<Arc<dyn AuditSink>>,
    emit: EmitFn,
    provider_factory: Arc<dyn Fn() -> Result<Arc<dyn ModelProvider>, RuntimeError> + Send + Sync>,
    processes: Arc<ProcessManager>,
    mcp: Arc<McpManager>,
    plugins: Arc<plugin_wasm::PluginRuntime>,
    browser: Mutex<Option<(String, Arc<dyn tool_browser::BrowserBackend>)>>,
    camera_backend: Arc<dyn camera_capture::CameraBackend>,
    mcp_config: Mutex<Option<Vec<mcp_runtime::McpServerConfig>>>,
    plugin_dir: Mutex<Option<Option<String>>>,
    memory: Mutex<Arc<memory::MemoryStore>>,
    desktop: Mutex<tool_desktop::plugin::DesktopPlugin>,
    artifacts: Arc<artifact_core::InMemoryArtifactStore>,
    computer_sessions: tool_desktop::ComputerSessionManager,
    sensors: Arc<SensorActivityHub>,
    screen_share: Mutex<Option<ScreenShareState>>,
    storage: Option<Arc<Mutex<Storage>>>,
    autonomous_full_access: Arc<AtomicBool>,
    file_context: Arc<Mutex<ConversationFileContext>>,
    state: Arc<Mutex<State>>,
    executor: tokio::runtime::Runtime,
}

impl AgentRuntime {
    /// Start the runtime with the provider read from storage settings.
    pub fn start(
        approvals: Arc<Mutex<ApprovalQueue>>,
        storage: Option<Arc<Mutex<Storage>>>,
        audit: Option<Arc<dyn AuditSink>>,
        emit: EmitFn,
    ) -> Result<Arc<Self>, RuntimeError> {
        Self::start_with_secrets(
            approvals,
            storage.clone(),
            audit,
            emit,
            secret_core::system("utsuwa"),
        )
    }
    /// Start with the host's shared secret store so settings writes and
    /// provider reads use the same keychain/memory fallback instance.
    pub fn start_with_secrets(
        approvals: Arc<Mutex<ApprovalQueue>>,
        storage: Option<Arc<Mutex<Storage>>>,
        audit: Option<Arc<dyn AuditSink>>,
        emit: EmitFn,
        secrets: Arc<dyn secret_core::SecretStore>,
    ) -> Result<Arc<Self>, RuntimeError> {
        Self::start_with_factory(
            approvals,
            storage.clone(),
            audit,
            emit,
            provider_factory_with_secrets(storage, secrets),
        )
    }

    /// Start with the host-owned sensor hub. Native app composition uses this
    /// constructor so privacy indicators remain authoritative in degraded
    /// agent mode; headless callers can keep using the legacy constructor,
    /// which creates an isolated hub.
    pub fn start_with_secrets_and_sensors(
        approvals: Arc<Mutex<ApprovalQueue>>,
        storage: Option<Arc<Mutex<Storage>>>,
        audit: Option<Arc<dyn AuditSink>>,
        emit: EmitFn,
        secrets: Arc<dyn secret_core::SecretStore>,
        sensors: Arc<SensorActivityHub>,
    ) -> Result<Arc<Self>, RuntimeError> {
        Self::start_with_factory_and_sensors(
            approvals,
            storage.clone(),
            audit,
            emit,
            provider_factory_with_secrets(storage, secrets),
            sensors,
        )
    }
    /// Start with an explicit provider factory (tests inject stubs).
    pub fn start_with_factory(
        approvals: Arc<Mutex<ApprovalQueue>>,
        storage: Option<Arc<Mutex<Storage>>>,
        audit: Option<Arc<dyn AuditSink>>,
        emit: EmitFn,
        provider_factory: Arc<
            dyn Fn() -> Result<Arc<dyn ModelProvider>, RuntimeError> + Send + Sync,
        >,
    ) -> Result<Arc<Self>, RuntimeError> {
        Self::start_with_factory_and_sensors(
            approvals,
            storage,
            audit,
            emit,
            provider_factory,
            Arc::new(SensorActivityHub::new()),
        )
    }

    /// Start with explicit provider and host services. The executor is
    /// intentionally private; callers submit async work through its handle
    /// rather than entering it synchronously with `block_on`.
    pub fn start_with_factory_and_sensors(
        approvals: Arc<Mutex<ApprovalQueue>>,
        storage: Option<Arc<Mutex<Storage>>>,
        audit: Option<Arc<dyn AuditSink>>,
        emit: EmitFn,
        provider_factory: Arc<
            dyn Fn() -> Result<Arc<dyn ModelProvider>, RuntimeError> + Send + Sync,
        >,
        sensors: Arc<SensorActivityHub>,
    ) -> Result<Arc<Self>, RuntimeError> {
        let startup = Instant::now();
        let autonomous_full_access = Arc::new(AtomicBool::new(
            read_autonomous_full_access(storage.as_ref()).unwrap_or(false),
        ));
        let executor = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("utsuwa-agent")
            .enable_all()
            .build()
            .map_err(|e| RuntimeError::Executor(e.to_string()))?;
        let runtime = Arc::new(Self {
            agent_id: AgentId::new(uuid::Uuid::new_v4().to_string()),
            approvals,
            audit,
            emit,
            provider_factory,
            processes: ProcessManager::new(ProcessLimits::default()),
            mcp: Arc::new(McpManager::new()),
            plugins: Arc::new(
                plugin_wasm::PluginRuntime::new()
                    .map_err(|e| RuntimeError::Tools(e.to_string()))?,
            ),
            browser: Mutex::new(None),
            camera_backend: Self::camera_backend(),
            mcp_config: Mutex::new(None),
            plugin_dir: Mutex::new(None),
            memory: Mutex::new(Arc::new(
                memory::MemoryStore::open_in_memory()
                    .map_err(|e| RuntimeError::Tools(e.to_string()))?,
            )),
            desktop: Mutex::new(Self::desktop_plugin()),
            artifacts: Arc::new(artifact_core::InMemoryArtifactStore::new()),
            computer_sessions: tool_desktop::ComputerSessionManager::new(),
            sensors,
            screen_share: Mutex::new(None),
            storage,
            autonomous_full_access,
            file_context: Arc::new(Mutex::new(ConversationFileContext::default())),
            state: Arc::new(Mutex::new(State {
                generation: 0,
                transcript: Vec::new(),
                suspended: None,
                running: None,
                task_id: None,
                turn_id: None,
                system_prompt: None,
                replay_cache: None,
            })),
            executor,
        });
        // Host-owned privacy publication: synchronous listeners on the hub
        // forward activity changes to the frontend without depending on the
        // model executor's health.
        runtime.sensors.attach_event_publisher(&runtime.emit);
        tracing::debug!(
            elapsed_ms = startup.elapsed().as_millis() as u64,
            "host.runtime.ready"
        );
        Ok(runtime)
    }
    /// Whether the explicit native setting is currently enabled in the live
    /// runtime. The persisted value is refreshed when a turn is constructed;
    /// this atomic mirror lets a settings change take effect for the next
    /// authorization request without restarting the host.
    pub fn autonomous_full_access_enabled(&self) -> bool {
        self.autonomous_full_access.load(Ordering::SeqCst)
    }
    /// Snapshot the active conversational file identity. The returned
    /// reference is only an identifier; callers must resolve and re-authorize
    /// it before touching the filesystem.
    pub fn active_file_ref(&self) -> Option<FileRef> {
        self.file_context
            .lock()
            .ok()
            .and_then(|context| context.active_file.clone())
    }
    /// Update the live mode after the native settings row has been written.
    /// Enabling it also wakes the one suspended Agent turn, if any, without
    /// creating a broad grant or changing the behavior of other principals.
    pub fn set_autonomous_full_access(self: &Arc<Self>, enabled: bool) {
        self.autonomous_full_access.store(enabled, Ordering::SeqCst);
        if enabled {
            self.resume_suspended_for_autonomous_access();
        }
    }
    /// Desktop plugins installed on this host. Linux prefers the native
    /// ScreenCast/RemoteDesktop portal when a Wayland session is present;
    /// its backend keeps Xwayland as an explicit legacy fallback. X11 is
    /// still registered independently for X11-only sessions. Windows UI
    /// Automation and macOS AX use the same registry and policy
    /// boundary.
    fn desktop_plugin() -> tool_desktop::plugin::DesktopPlugin {
        use tool_desktop::plugin::{DesktopPlugin, DesktopPluginRegistry};
        let discovery_started = Instant::now();
        let mut registry = DesktopPluginRegistry::new();
        #[cfg(target_os = "linux")]
        let atspi_service = desktop_linux_atspi::AtspiService::new();
        #[cfg(target_os = "linux")]
        if let Some(portal) = desktop_linux_wayland::plugin_with_atspi(atspi_service.clone()) {
            registry.register(portal);
        }
        // Native AT-SPI semantics stay available as their own plugin for
        // sessions without a portal. Both registrations share the same
        // cached service and one background availability probe.
        #[cfg(target_os = "linux")]
        registry.register(desktop_linux_atspi::plugin_with_service(atspi_service));
        #[cfg(target_os = "linux")]
        if let Some(linux) = desktop_linux::plugin() {
            registry.register(linux);
        }
        #[cfg(target_os = "windows")]
        if let Some(windows) = desktop_windows::plugin() {
            registry.register(windows);
        }
        #[cfg(target_os = "macos")]
        if let Some(macos) = desktop_macos::plugin() {
            registry.register(macos);
        }
        let selected = registry.select().unwrap_or_else(DesktopPlugin::stub);
        tracing::debug!(
            backend = %selected.manifest.id,
            elapsed_ms = discovery_started.elapsed().as_millis() as u64,
            "desktop backend discovery complete"
        );
        selected
    }
    /// The MCP server manager: configure servers here (or via the
    /// `mcp.servers` settings key, which syncs every turn) and their
    /// tools join the next turn's registry through host policy.
    pub fn mcp_manager(&self) -> &Arc<McpManager> {
        &self.mcp
    }
    /// Async MCP status snapshot. Keeping this async prevents callers from
    /// re-entering the agent executor with a nested `block_on`.
    pub async fn mcp_status(&self) -> Vec<mcp_runtime::McpServerStatus> {
        self.mcp.status().await
    }
    /// The WASM plugin manager: lifecycle calls here (`plugin.enable` /
    /// `plugin.disable` / …) take effect on the next turn's registry,
    /// which is rebuilt per turn through [`PluginRuntime::register_enabled`].
    pub fn plugin_manager(&self) -> &Arc<plugin_wasm::PluginRuntime> {
        &self.plugins
    }
    /// Install the durable memory store (boot only; last call wins). Each
    /// turn's registry serves `memory.remember` / `memory.recall` /
    /// `memory.forget` from the current store. Without this call the
    /// runtime uses an isolated in-memory store (tests, headless runs).
    pub fn set_memory_store(&self, store: Arc<memory::MemoryStore>) {
        if let Ok(mut slot) = self.memory.lock() {
            *slot = store;
        }
    }
    /// The current memory store (tests and diagnostics).
    pub fn memory_store(&self) -> Arc<memory::MemoryStore> {
        self.memory
            .lock()
            .map(|s| Arc::clone(&s))
            .unwrap_or_else(|_| {
                Arc::new(
                    memory::MemoryStore::open_in_memory()
                        .expect("in-memory memory store always opens"),
                )
            })
    }
    /// Camera backend for this host: real OS capture where a camera stack
    /// exists, honest unavailability elsewhere.
    fn camera_backend() -> Arc<dyn camera_capture::CameraBackend> {
        #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
        {
            Arc::new(camera_capture::NokhwaBackend)
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
        {
            Arc::new(camera_capture::StubBackend)
        }
    }

    /// Install the desktop plugin (boot only, or tests with a fake).
    /// The turn registers `desktop.*` tools only for the active
    /// plugin's declared capabilities — with the stub plugin the model
    /// never sees actions that cannot run.
    pub fn set_desktop_plugin(&self, plugin: tool_desktop::plugin::DesktopPlugin) {
        if let Ok(mut slot) = self.desktop.lock() {
            *slot = plugin;
        }
    }

    /// Shared expiring media store used by desktop tools and provider
    /// adapters for this host process. It intentionally does not expose a
    /// filesystem path or persist captures.
    pub fn artifact_store(&self) -> Arc<dyn artifact_core::ArtifactStore> {
        self.artifacts.clone()
    }

    pub fn computer_sessions(&self) -> tool_desktop::ComputerSessionManager {
        self.computer_sessions.clone()
    }

    /// Shared authoritative microphone activity handle. The native frontend
    /// capture manager and model-facing `audio.*` tools use this same state,
    /// so one IPC indicator covers both capture paths.
    pub fn audio_activity_state(&self) -> tool_audio::AudioState {
        self.sensors.microphone()
    }

    pub fn camera_activity_status(&self) -> tool_camera::CameraActivityState {
        self.sensors.camera_status()
    }

    pub fn microphone_activity_status(&self) -> tool_audio::MicrophoneActivityState {
        self.sensors.microphone_status()
    }

    /// Schedule host-owned async work on the runtime without exposing a
    /// synchronous bridge back into Tokio.
    #[allow(dead_code)]
    pub(crate) fn spawn_host_task<F>(&self, task: F) -> tokio::task::JoinHandle<()>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        self.executor.handle().spawn(task)
    }

    /// Handle to the agent executor for async callers and tests. Async code
    /// must `await` on this handle instead of entering the runtime with
    /// `block_on`.
    pub fn executor_handle(&self) -> tokio::runtime::Handle {
        self.executor.handle().clone()
    }

    /// Enter the agent executor from synchronous host code (GTK/IPC threads).
    /// Fails closed with `runtime_unavailable` instead of panicking when the
    /// caller is already running on a Tokio worker — that path must use the
    /// async variant of the operation.
    fn block_on_host<F>(&self, future: F) -> Result<F::Output, RuntimeError>
    where
        F: std::future::Future,
    {
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(RuntimeError::Tools(
                "runtime_unavailable: synchronous host operation cannot block a Tokio worker; use the async variant".to_string(),
            ));
        }
        Ok(self.executor.block_on(future))
    }

    /// Cached CDP backend for this host. The endpoint is host configuration
    /// (loopback-only, sanitized); the handle is rebuilt only when the
    /// configured endpoint changes, and availability probes are cached with
    /// a short TTL so per-turn snapshots stay cheap.
    fn browser_backend(&self) -> Arc<dyn tool_browser::BrowserBackend> {
        let endpoint = read_cdp_endpoint(self.storage.as_ref());
        let mut slot = self.browser.lock().unwrap_or_else(|poison| {
            let mut guard = poison.into_inner();
            *guard = None;
            guard
        });
        if let Some((cached_endpoint, backend)) = slot.as_ref() {
            if *cached_endpoint == endpoint {
                return Arc::clone(backend);
            }
        }
        let backend: Arc<dyn tool_browser::BrowserBackend> =
            Arc::new(tool_browser::CachedCdpBackend::new(endpoint.clone()));
        *slot = Some((endpoint, Arc::clone(&backend)));
        backend
    }

    fn active_desktop_plugin(&self) -> Result<tool_desktop::plugin::DesktopPlugin, RuntimeError> {
        self.desktop
            .lock()
            .map(|plugin| plugin.clone())
            .map_err(|_| RuntimeError::Tools("desktop plugin lock failed".to_string()))
    }

    fn record_screen_share_audit(
        &self,
        capability: capability_core::Capability,
        outcome: audit_core::AuditOutcome,
        target: &tool_desktop::CaptureTarget,
        detail: &str,
    ) {
        if let Some(sink) = &self.audit {
            sink.record(audit_core::AuditRecord::now(
                capability_core::Principal::User,
                Some(capability),
                Some(target.resource()),
                outcome,
                detail,
            ));
        }
    }

    fn emit_screen_share_changed(&self, status: &ScreenShareStatus) {
        if let Ok(data) = serde_json::to_value(status) {
            (self.emit)(HostEvent {
                event: "desktop.share_screen.changed".to_string(),
                data,
            });
        }
    }

    /// Start a user-initiated screen sharing session. This is the native UI
    /// entry point; model-facing capture tools still need their own
    /// ScreenCapture ticket when they request frames.
    pub fn screen_share_start(
        &self,
        config: tool_desktop::CaptureConfig,
    ) -> Result<ScreenShareStatus, RuntimeError> {
        {
            let state = self
                .screen_share
                .lock()
                .map_err(|_| RuntimeError::Tools("screen-share lock failed".to_string()))?;
            if state.is_some() {
                return Err(RuntimeError::Tools(
                    "a screen-sharing session is already active".to_string(),
                ));
            }
        }
        let plugin = self.active_desktop_plugin()?;
        if !plugin.is_available() {
            return Err(RuntimeError::Tools(
                "no available desktop backend can share the screen".to_string(),
            ));
        }
        let target = config.target.clone();
        let backend = plugin.backend.clone();
        let session = self
            .block_on_host(self.computer_sessions.start_capture(
                backend.clone(),
                config,
                self.artifacts.clone(),
            ))?
            .map_err(|error| RuntimeError::Tools(error.to_string()))?;
        if let Err(error) = self
            .block_on_host(backend.set_control_enabled(false))?
            .map_err(|error| RuntimeError::Tools(error.to_string()))
        {
            let _ = self.block_on_host(
                self.computer_sessions
                    .stop_capture(&tool_desktop::CaptureSessionId(session.id.0.clone())),
            );
            return Err(RuntimeError::Tools(format!(
                "could not initialize the control gate: {error}"
            )));
        }
        let started_at = session.started_at;
        let state = ScreenShareState {
            backend,
            session_id: tool_desktop::CaptureSessionId(session.id.0.clone()),
            target: target.clone(),
            paused: false,
            control_enabled: false,
            started_at,
        };
        self.screen_share
            .lock()
            .map_err(|_| RuntimeError::Tools("screen-share lock failed".to_string()))?
            .replace(state);
        // Approvals for session-derived authority (screen capture,
        // desktop observation/control) granted from here on are bound to
        // this share and die with it; unrelated grants stay unbound.
        if let Ok(queue) = self.approvals.lock() {
            queue.set_active_sharing_session(Some(session.id.0.clone()));
        }
        self.record_screen_share_audit(
            capability_core::Capability::ScreenCapture,
            audit_core::AuditOutcome::Authorized,
            &target,
            "user started screen sharing",
        );
        let status = self.screen_share_status();
        self.emit_screen_share_changed(&status);
        Ok(status)
    }

    pub fn screen_share_stop(&self) -> Result<ScreenShareStatus, RuntimeError> {
        let (session_id, target, backend) = {
            let state = self
                .screen_share
                .lock()
                .map_err(|_| RuntimeError::Tools("screen-share lock failed".to_string()))?;
            let Some(state) = state.as_ref() else {
                let status = self.screen_share_status();
                self.emit_screen_share_changed(&status);
                return Ok(status);
            };
            (
                state.session_id.clone(),
                state.target.clone(),
                state.backend.clone(),
            )
        };
        self.block_on_host(self.computer_sessions.stop_capture(&session_id))?
            .map_err(|error| RuntimeError::Tools(error.to_string()))?;
        // Deterministic teardown: capture stops above; here the session's
        // derived authority is revoked (capture/observe/control grants
        // bound to this share) while unrelated grants survive.
        let revoked = self
            .approvals
            .lock()
            .map(|queue| queue.revoke_sharing_session(&session_id.0))
            .unwrap_or(0);
        if let Some(sink) = &self.audit {
            sink.record(audit_core::AuditRecord::now(
                capability_core::Principal::User,
                Some(capability_core::Capability::ScreenCapture),
                Some(target.resource()),
                audit_core::AuditOutcome::Blocked,
                format!(
                    "stopped screen sharing: revoked {revoked} session-bound grant(s), capture artifacts deleted, control disabled"
                ),
            ));
        }
        let restore_result = self
            .block_on_host(backend.set_control_enabled(true))
            .map(|inner| inner.map_err(|error| RuntimeError::Tools(error.to_string())))
            .and_then(|inner| inner);
        self.screen_share
            .lock()
            .map_err(|_| RuntimeError::Tools("screen-share lock failed".to_string()))?
            .take();
        if let Err(error) = restore_result {
            return Err(RuntimeError::Tools(format!(
                "screen sharing stopped, but restoring desktop control failed: {error}"
            )));
        }
        {
            self.record_screen_share_audit(
                capability_core::Capability::ScreenCapture,
                audit_core::AuditOutcome::Executed,
                &target,
                "user stopped screen sharing",
            );
        }
        let status = self.screen_share_status();
        self.emit_screen_share_changed(&status);
        Ok(status)
    }

    pub fn screen_share_pause(&self) -> Result<ScreenShareStatus, RuntimeError> {
        let session_id = self
            .screen_share
            .lock()
            .map_err(|_| RuntimeError::Tools("screen-share lock failed".to_string()))?
            .as_ref()
            .ok_or_else(|| RuntimeError::Tools("no active screen-sharing session".to_string()))?
            .session_id
            .clone();
        self.block_on_host(self.computer_sessions.set_paused(&session_id, true))?
            .map_err(|error| RuntimeError::Tools(error.to_string()))?;
        if let Ok(mut state) = self.screen_share.lock() {
            if let Some(state) = state.as_mut() {
                state.paused = true;
            }
        }
        let status = self.screen_share_status();
        self.emit_screen_share_changed(&status);
        Ok(status)
    }

    pub fn screen_share_resume(&self) -> Result<ScreenShareStatus, RuntimeError> {
        let session_id = self
            .screen_share
            .lock()
            .map_err(|_| RuntimeError::Tools("screen-share lock failed".to_string()))?
            .as_ref()
            .ok_or_else(|| RuntimeError::Tools("no active screen-sharing session".to_string()))?
            .session_id
            .clone();
        self.block_on_host(self.computer_sessions.set_paused(&session_id, false))?
            .map_err(|error| RuntimeError::Tools(error.to_string()))?;
        if let Ok(mut state) = self.screen_share.lock() {
            if let Some(state) = state.as_mut() {
                state.paused = false;
            }
        }
        let status = self.screen_share_status();
        self.emit_screen_share_changed(&status);
        Ok(status)
    }

    pub fn screen_share_set_control(
        &self,
        enabled: bool,
    ) -> Result<ScreenShareStatus, RuntimeError> {
        let (target, backend, session_id) = {
            let state = self
                .screen_share
                .lock()
                .map_err(|_| RuntimeError::Tools("screen-share lock failed".to_string()))?;
            let state = state.as_ref().ok_or_else(|| {
                RuntimeError::Tools("no active screen-sharing session".to_string())
            })?;
            (
                state.target.clone(),
                state.backend.clone(),
                state.session_id.clone(),
            )
        };
        self.block_on_host(backend.set_control_enabled(enabled))?
            .map_err(|error| RuntimeError::Tools(error.to_string()))?;
        self.block_on_host(
            self.computer_sessions
                .set_control_enabled(&session_id, enabled),
        )?
        .map_err(|error| RuntimeError::Tools(error.to_string()))?;
        self.screen_share
            .lock()
            .map_err(|_| RuntimeError::Tools("screen-share lock failed".to_string()))?
            .as_mut()
            .ok_or_else(|| RuntimeError::Tools("screen-sharing session ended".to_string()))?
            .control_enabled = enabled;
        self.record_screen_share_audit(
            capability_core::Capability::DesktopControl,
            audit_core::AuditOutcome::Authorized,
            &target,
            if enabled {
                "user enabled desktop control separately from screen sharing"
            } else {
                "user disabled desktop control"
            },
        );
        let status = self.screen_share_status();
        self.emit_screen_share_changed(&status);
        Ok(status)
    }

    /// Engage the global emergency stop: pointer, keyboard, and semantic
    /// UI actions stop immediately (observation stays active), standing
    /// DesktopControl grants are revoked, and per-session control flags
    /// clear. Independent from the model — only the user clears it.
    pub fn desktop_emergency_stop(&self) -> ScreenShareStatus {
        let audit_record = tool_desktop::desktop_emergency_stop();
        let revoked = self
            .approvals
            .lock()
            .map(|queue| queue.revoke_capability(&capability_core::Capability::DesktopControl))
            .unwrap_or(0);
        let sessions_revoked = self
            .block_on_host(self.computer_sessions.revoke_control())
            .unwrap_or(0);
        if let Some(sink) = &self.audit {
            sink.record(audit_record);
            sink.record(audit_core::AuditRecord::now(
                capability_core::Principal::User,
                Some(capability_core::Capability::DesktopControl),
                None,
                audit_core::AuditOutcome::Blocked,
                format!(
                    "emergency stop invalidated {revoked} standing grant(s) and {sessions_revoked} session control flag(s)"
                ),
            ));
        }
        let status = self.screen_share_status();
        self.emit_screen_share_changed(&status);
        status
    }

    /// Clear a previously engaged emergency stop (user action only — the
    /// model cannot call this; it is an IPC user control, not a tool).
    pub fn desktop_clear_emergency_stop(&self) -> ScreenShareStatus {
        let audit_record = tool_desktop::desktop_clear_emergency_stop();
        if let Some(sink) = &self.audit {
            sink.record(audit_record);
        }
        let status = self.screen_share_status();
        self.emit_screen_share_changed(&status);
        status
    }

    pub fn screen_share_status(&self) -> ScreenShareStatus {
        // Step traces (not debug: the frontend polls this): a missing
        // completion trace names the wedged backend call.
        tracing::trace!("screen_share.status.begin");
        let plugin = self
            .desktop
            .lock()
            .map(|plugin| plugin.clone())
            .unwrap_or_else(|_| tool_desktop::plugin::DesktopPlugin::stub());
        tracing::trace!("screen_share.status.plugin_locked");
        let displays = if plugin.is_available() {
            // Never block a Tokio worker for a status snapshot: agent-turn
            // callers use `screen_share_status_async` instead.
            if tokio::runtime::Handle::try_current().is_ok() {
                Vec::new()
            } else {
                tracing::trace!("screen_share.status.list_displays.begin");
                let displays = self
                    .block_on_host(plugin.backend.list_displays())
                    .ok()
                    .and_then(|inner| inner.ok())
                    .unwrap_or_default();
                tracing::trace!("screen_share.status.list_displays.ok");
                displays
            }
        } else {
            Vec::new()
        };
        let windows = if plugin.is_available() {
            if tokio::runtime::Handle::try_current().is_ok() {
                Vec::new()
            } else {
                tracing::trace!("screen_share.status.list_windows.begin");
                let windows = self
                    .block_on_host(plugin.backend.list_windows())
                    .ok()
                    .and_then(|inner| inner.ok())
                    .unwrap_or_default();
                tracing::trace!("screen_share.status.list_windows.ok");
                windows
            }
        } else {
            Vec::new()
        };
        let state = self.screen_share.lock().ok();
        let state = state.as_ref().and_then(|state| state.as_ref());
        ScreenShareStatus {
            available: plugin.is_available(),
            backend: plugin.manifest.id,
            sharing: state.is_some(),
            paused: state.is_some_and(|state| state.paused),
            control_enabled: state.is_some_and(|state| state.control_enabled)
                && !tool_desktop::emergency_stop_active(),
            emergency_stopped: tool_desktop::emergency_stop_active(),
            session_id: state.map(|state| state.session_id.0.clone()),
            target: state.map(|state| state.target.clone()),
            started_at: state.map(|state| state.started_at),
            displays,
            windows,
        }
    }

    /// Async status snapshot for callers already on the agent executor.
    /// Awaits backend listing instead of entering the runtime synchronously.
    pub async fn screen_share_status_async(&self) -> ScreenShareStatus {
        let plugin = self
            .desktop
            .lock()
            .map(|plugin| plugin.clone())
            .unwrap_or_else(|_| tool_desktop::plugin::DesktopPlugin::stub());
        let available = plugin.is_available();
        let (displays, windows) = if available {
            let displays = plugin.backend.list_displays().await.unwrap_or_default();
            let windows = plugin.backend.list_windows().await.unwrap_or_default();
            (displays, windows)
        } else {
            (Vec::new(), Vec::new())
        };
        let state = self.screen_share.lock().ok();
        let state = state.as_ref().and_then(|state| state.as_ref());
        ScreenShareStatus {
            available,
            backend: plugin.manifest.id,
            sharing: state.is_some(),
            paused: state.is_some_and(|state| state.paused),
            control_enabled: state.is_some_and(|state| state.control_enabled)
                && !tool_desktop::emergency_stop_active(),
            emergency_stopped: tool_desktop::emergency_stop_active(),
            session_id: state.map(|state| state.session_id.0.clone()),
            target: state.map(|state| state.target.clone()),
            started_at: state.map(|state| state.started_at),
            displays,
            windows,
        }
    }
    /// Build the per-turn [`ToolCatalog`]: static packs (system facts,
    /// process tools) plus best-effort sources (MCP, plugins, memory,
    /// desktop) and the required builtin filesystem surface. Snapshot
    /// once per turn; leaf crates own their tool construction, so this
    /// method only clones handles and reads settings into managers.
    async fn tool_catalog(&self, host_environment: &HostEnvironment) -> ToolCatalog {
        let turn_started = Instant::now();
        self.sync_mcp_cached().await;
        self.discover_plugins_cached();
        let memory_store = match self.memory.lock() {
            Ok(store) => Some(Arc::clone(&store)),
            Err(_) => {
                tracing::warn!("memory store lock failed; skipping memory tools this turn");
                None
            }
        };
        let desktop = match self.desktop.lock() {
            Ok(plugin) => Some(plugin.clone()),
            Err(_) => {
                tracing::warn!("desktop plugin lock failed; skipping desktop tools this turn");
                None
            }
        };
        let filesystem_desktop = host_environment
            .user_dirs
            .desktop
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        let processes = Arc::clone(&self.processes);
        let environment = host_environment.clone();
        let file_context = Some(Arc::clone(&self.file_context));

        let catalog = ToolCatalog::new()
            .with_pack(SystemToolPack::new(environment))
            .with_pack(ProcessToolPack::new(processes))
            .with_required_pack(tool_filesystem_host::HostFilesystemPack::new(
                host_environment.clone(),
                file_context,
            ))
            // Local-machine packs: each tool enforces its own capability
            // tickets; the profile only controls model visibility.
            .with_pack(tool_http::HttpToolPack::with_artifacts(
                self.artifacts.clone(),
            ))
            .with_pack(tool_archive::ArchiveToolPack::new())
            .with_pack(tool_git::GitToolPack)
            .with_pack(tool_notification::NotificationToolPack)
            // The CDP endpoint is host configuration (loopback-only,
            // sanitized), read fresh every turn; the pack hides control
            // tools while no browser answers there.
            .with_pack(tool_browser::BrowserToolPack::with_services(
                self.browser_backend(),
                self.artifacts.clone(),
            ))
            .with_pack(tool_camera::CameraToolPack {
                backend: Arc::clone(&self.camera_backend),
                artifacts: self.artifacts.clone(),
                state: self.sensors.camera(),
            })
            .with_pack(tool_audio::AudioToolPack {
                artifacts: self.artifacts.clone(),
                state: self.sensors.microphone(),
            })
            .with_pack(tool_media::MediaToolPack {
                artifacts: self.artifacts.clone(),
            })
            .with_pack(tool_system::SystemToolPack)
            .with_pack(tool_system::ClipboardToolPack)
            .with_pack(tool_system::ApplicationToolPack)
            .with_pack(tool_system::DocumentToolPack {
                artifacts: self.artifacts.clone(),
            })
            .with_source(mcp_runtime::McpToolSource::new(Arc::clone(&self.mcp)))
            .with_source(plugin_wasm::PluginToolSource::new(Arc::clone(
                &self.plugins,
            )));
        let catalog = match memory_store {
            Some(store) => catalog.with_pack(memory::tools::MemoryToolPack::new(store)),
            None => catalog,
        };
        let catalog = match desktop {
            Some(plugin) => catalog.with_pack(tool_desktop::DesktopToolPack::new_with_services(
                plugin,
                filesystem_desktop,
                self.artifacts.clone(),
                self.computer_sessions.clone(),
            )),
            None => catalog,
        };
        tracing::debug!(
            elapsed_ms = turn_started.elapsed().as_millis() as u64,
            "host.turn.tool_catalog"
        );
        catalog
    }

    /// Sync MCP servers only when the `mcp.servers` setting changed since
    /// the last turn. Unchanged configuration reuses the live manager, so
    /// repeated turns do not reconnect or respawn servers.
    async fn sync_mcp_cached(&self) {
        let configs = read_mcp_configs(self.storage.as_ref());
        let changed = match self.mcp_config.lock() {
            Ok(mut slot) => {
                if *slot == configs {
                    false
                } else {
                    *slot = configs.clone();
                    true
                }
            }
            Err(_) => true,
        };
        if !changed {
            return;
        }
        if let Some(configs) = configs {
            if let Err(error) = self.mcp.sync_configs(configs).await {
                tracing::warn!(%error, "mcp settings sync failed");
            }
        }
    }

    /// Rediscover the plugin directory only when the `plugin.dir` setting
    /// changed since the last turn.
    fn discover_plugins_cached(&self) {
        let dir = read_plugin_dir(self.storage.as_ref());
        let changed = match self.plugin_dir.lock() {
            Ok(mut slot) => {
                if *slot == dir {
                    false
                } else {
                    *slot = dir.clone();
                    true
                }
            }
            Err(_) => true,
        };
        if !changed {
            return;
        }
        if let Some(dir) = dir.flatten() {
            if let Err(error) = self.plugins.discover_dir(std::path::Path::new(&dir)) {
                tracing::warn!(dir = %dir, error = %error, "plugin discovery failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::ToolAuthorizer;
    use model_core::{
        FinishReason, ModelError, ModelMessage, ModelRequest, ModelStreamEvent, ToolCall,
    };
    use policy_core::{ApprovalQueue, AuthorizationDecision};
    use std::time::{Duration, Instant};
    use tool_sdk::ToolLoadContext;

    #[test]
    fn provider_base_url_normalization_is_provider_aware() {
        assert_eq!(
            normalize_provider_base_url("lmstudio", "http://localhost:1234"),
            "http://localhost:1234/v1"
        );
        assert_eq!(
            normalize_provider_base_url("lmstudio", "http://localhost:1234/v1/"),
            "http://localhost:1234/v1"
        );
        assert_eq!(
            normalize_provider_base_url("lmstudio", "http://localhost:1234/v1/chat/completions"),
            "http://localhost:1234/v1"
        );
        assert_eq!(
            normalize_provider_base_url("ollama", "http://localhost:11434/"),
            "http://localhost:11434/v1"
        );
        assert_eq!(
            normalize_provider_base_url("openai-compatible", "http://localhost:9000/custom"),
            "http://localhost:9000/custom"
        );
        assert_eq!(
            normalize_provider_base_url(
                "openai-compatible",
                "http://localhost:9000/custom/chat/completions"
            ),
            "http://localhost:9000/custom"
        );
        assert_eq!(
            normalize_provider_base_url("kilo", "https://api.kilo.ai/api/gateway"),
            "https://api.kilo.ai/api/gateway"
        );
    }

    fn test_authorizer(enabled: bool) -> QueueAuthorizer {
        QueueAuthorizer {
            approvals: Arc::new(Mutex::new(ApprovalQueue::new())),
            task_id: "task-test".to_string(),
            turn_id: "turn-test".to_string(),
            autonomous_full_access: Arc::new(AtomicBool::new(enabled)),
        }
    }

    #[test]
    fn autonomous_authorizer_allows_every_agent_capability_but_not_frontend() {
        let authorizer = test_authorizer(true);
        let agent = capability_core::Principal::Agent(AgentId::new("agent-test"));
        let requests = [
            (
                capability_core::Capability::FilesystemRead,
                capability_core::Resource::Path("/home/u/.ssh/id_ed25519".into()),
            ),
            (
                capability_core::Capability::FilesystemWrite,
                capability_core::Resource::Path("/home/u/.aws/credentials".into()),
            ),
            (
                capability_core::Capability::FilesystemCreate,
                capability_core::Resource::Path("/home/u/new.txt".into()),
            ),
            (
                capability_core::Capability::FilesystemDelete,
                capability_core::Resource::Path("/home/u/old.txt".into()),
            ),
            (
                capability_core::Capability::FilesystemMove,
                capability_core::Resource::Path("/home/u/moved.txt".into()),
            ),
            (
                capability_core::Capability::ProcessSpawn,
                capability_core::Resource::Process {
                    executable: "/bin/bash".into(),
                    args: vec!["-lc".to_string(), "echo hello".to_string()],
                    cwd: "/work".into(),
                    env: vec![],
                },
            ),
            (
                capability_core::Capability::ProcessSignal,
                capability_core::Resource::Process {
                    executable: "/bin/echo".into(),
                    args: vec![],
                    cwd: "/work".into(),
                    env: vec![],
                },
            ),
            (
                capability_core::Capability::NetworkConnect,
                capability_core::Resource::HostPort {
                    host: "example.test".to_string(),
                    port: 443,
                },
            ),
            (
                capability_core::Capability::DesktopObserve,
                capability_core::Resource::Window("window-1".to_string()),
            ),
            (
                capability_core::Capability::DesktopControl,
                capability_core::Resource::Window("window-1".to_string()),
            ),
            (
                capability_core::Capability::ScreenCapture,
                capability_core::Resource::Window("window-1".to_string()),
            ),
            (
                capability_core::Capability::ClipboardRead,
                capability_core::Resource::Clipboard,
            ),
            (
                capability_core::Capability::ClipboardWrite,
                capability_core::Resource::Clipboard,
            ),
            (
                capability_core::Capability::ApplicationLaunch,
                capability_core::Resource::Application("notes".to_string()),
            ),
            (
                capability_core::Capability::McpInvoke,
                capability_core::Resource::McpTool {
                    server: "server".to_string(),
                    tool: "tool".to_string(),
                },
            ),
            (
                capability_core::Capability::PluginInvoke,
                capability_core::Resource::PluginTool {
                    plugin: "plugin".to_string(),
                    tool: "tool".to_string(),
                },
            ),
        ];

        for (capability, resource) in requests {
            let request = capability_core::CapabilityRequest {
                principal: agent.clone(),
                capability,
                resource,
            };
            assert!(matches!(
                authorizer.authorize(&agent, &request),
                AuthorizationDecision::Allow { .. }
            ));
            assert!(authorizer.commit(&agent, &request));
        }
        assert!(authorizer.authorization_mode().is_some());
        assert!(authorizer
            .approvals
            .lock()
            .unwrap()
            .grants_snapshot()
            .is_empty());

        let frontend = capability_core::Principal::Frontend;
        let frontend_request = capability_core::CapabilityRequest {
            principal: frontend.clone(),
            capability: capability_core::Capability::FilesystemWrite,
            resource: capability_core::Resource::Path("/home/u/should-not-write".into()),
        };
        assert!(matches!(
            authorizer.authorize(&frontend, &frontend_request),
            AuthorizationDecision::Deny { .. }
        ));
        assert!(!authorizer.commit(&frontend, &frontend_request));

        for principal in [
            capability_core::Principal::NativePlugin(capability_core::PluginId::new("native")),
            capability_core::Principal::WasmPlugin(capability_core::PluginId::new("wasm")),
            capability_core::Principal::McpServer(capability_core::ServerId::new("mcp")),
        ] {
            let request = capability_core::CapabilityRequest {
                principal: principal.clone(),
                capability: capability_core::Capability::FilesystemWrite,
                resource: capability_core::Resource::Path("/home/u/plugin-file".into()),
            };
            assert!(!matches!(
                authorizer.authorize(&principal, &request),
                AuthorizationDecision::Allow { .. }
            ));
            assert!(!authorizer.commit(&principal, &request));
        }
    }

    #[test]
    fn autonomous_authorizer_preserves_secret_and_interpreter_prompts_when_off() {
        let authorizer = test_authorizer(false);
        let agent = capability_core::Principal::Agent(AgentId::new("agent-test"));
        let secret = capability_core::CapabilityRequest {
            principal: agent.clone(),
            capability: capability_core::Capability::FilesystemRead,
            resource: capability_core::Resource::Path("/home/u/.ssh/id_ed25519".into()),
        };
        assert!(matches!(
            authorizer.authorize(&agent, &secret),
            AuthorizationDecision::RequireUserApproval { .. }
        ));
        let shell = capability_core::CapabilityRequest {
            principal: agent.clone(),
            capability: capability_core::Capability::ProcessSpawn,
            resource: capability_core::Resource::Process {
                executable: "/bin/bash".into(),
                args: vec!["-lc".to_string(), "echo hello".to_string()],
                cwd: "/work".into(),
                env: vec![],
            },
        };
        assert!(matches!(
            authorizer.authorize(&agent, &shell),
            AuthorizationDecision::RequireUserApproval { .. }
        ));
    }

    #[test]
    fn native_host_context_uses_host_native_path_style() {
        let context = host_environment_context(false);
        assert!(context.contains("<host_environment>"));
        assert!(context.contains(&format!("Architecture: {}", std::env::consts::ARCH)));
        assert!(context.contains(&format!("OS: {}", host_os_label())));
        assert!(context.contains(&format!("Path style: {}", host_core::host_path_style())));
        assert!(context.contains("Available semantic user directories:"));
        assert!(context.contains(
            "Filesystem tools prefer file_ref or semantic directory ids with relative paths"
        ));
        assert!(context.contains(
            "For creating new files in Desktop/Documents/etc., use filesystem.create_user_file"
        ));
        assert!(context.contains("never first try filesystem.edit or filesystem.write"));
        assert!(context.contains("use filesystem.edit:"));
        assert!(context.contains("Current local date:"));
        assert!(context.contains("Current local datetime:"));
        assert!(context.contains("new_text_source may be current_date"));
        assert!(context.contains("use a placeholder such as 'Updated date'"));
        assert!(context.contains("A failed edit attempt does not mean editing is unsupported"));
        assert!(context.contains("Never repeat the exact same filesystem arguments"));
        assert!(context.contains(
            "Do not tell the user that file editing is unavailable unless the native tool actually returns an unavailable or denied result"
        ));
        assert!(context.contains("call system.time"));
        assert!(context.contains("empty target.relative_path"));
        assert!(context.contains("never invent a file_ref"));
        #[cfg(target_os = "linux")]
        {
            assert!(context.contains("OS: Linux"));
            assert!(context.contains("Path style: POSIX"));
            assert!(!context.contains("C:\\Users\\"));
        }
    }

    #[test]
    fn native_host_context_injects_the_active_file_reference() {
        let context = Arc::new(Mutex::new(ConversationFileContext::default()));
        context.lock().unwrap().record_success(
            FileRef::parse("file:desktop:note.txt").expect("test file ref is valid"),
        );
        let environment = HostEnvironment {
            os: "linux",
            architecture: "x86_64",
            home: Some(std::path::PathBuf::from("/tmp/home")),
            cwd: Some(std::path::PathBuf::from("/tmp/home")),
            user_dirs: host_core::UserDirectories {
                desktop: Some(std::path::PathBuf::from("/tmp/home/Escritorio")),
                ..Default::default()
            },
            xdg_config_source: None,
            path_style: "POSIX",
            path_separator: "/",
        };
        let prompt = compose_host_system_prompt_for_context(
            Some("continue"),
            false,
            &environment,
            Some(&context),
        );
        assert!(prompt.contains("<active_file>"));
        assert!(prompt.contains("file_ref: file:desktop:note.txt"));
        assert!(prompt.contains("Reuse this file_ref"));
        assert!(!prompt.contains("/tmp/home/Escritorio/note.txt"));
    }

    #[tokio::test]
    async fn system_time_returns_fresh_parseable_native_clock_facts() {
        // Resolved through the pack (the runtime's composition seam),
        // not by constructing the tool directly.
        use tool_sdk::ToolPack as _;
        let pack = SystemToolPack::new(HostEnvironment::snapshot());
        let tool = pack
            .tools(&ToolLoadContext::new(ToolProfile::Full))
            .into_iter()
            .find(|tool| tool.metadata().id.0 == "system.time")
            .expect("system pack must provide system.time");
        assert!(tool.required_capability(&serde_json::json!({})).is_none());
        let before = chrono::Utc::now().timestamp();
        let output = tool
            .invoke(
                tool_core::ToolContext::new(capability_core::Principal::Agent(AgentId::new(
                    "time-test",
                ))),
                serde_json::json!({}),
            )
            .await
            .unwrap();
        let after = chrono::Utc::now().timestamp();
        let content = &output.content;
        let local =
            chrono::DateTime::parse_from_rfc3339(content["local"].as_str().unwrap()).unwrap();
        let utc = chrono::DateTime::parse_from_rfc3339(content["utc"].as_str().unwrap()).unwrap();
        assert_eq!(content["date"], local.format("%Y-%m-%d").to_string());
        assert_eq!(content["time"], local.format("%H:%M:%S").to_string());
        assert_eq!(content["utc_offset"], local.format("%:z").to_string());
        assert_eq!(content["unix_timestamp"], utc.timestamp());
        assert!((before..=after).contains(&content["unix_timestamp"].as_i64().unwrap()));
        assert!(utc.timestamp() >= 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn english_and_spanish_desktop_requests_share_the_host_resolved_path() {
        let home =
            std::env::temp_dir().join(format!("utsuwa-prompt-language-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let desktop = home.join("Escritorio");
        std::fs::create_dir_all(&desktop).unwrap();
        let environment = HostEnvironment {
            os: "linux",
            architecture: std::env::consts::ARCH,
            home: Some(home.clone()),
            cwd: Some(home.clone()),
            user_dirs: host_core::UserDirectories {
                desktop: Some(desktop.clone()),
                ..Default::default()
            },
            xdg_config_source: None,
            path_style: "POSIX",
            path_separator: "/",
        };
        let english = compose_host_system_prompt_for(
            Some("Create hello.txt on my desktop."),
            false,
            &environment,
        );
        let spanish = compose_host_system_prompt_for(
            Some("Crea hello.txt en mi escritorio."),
            false,
            &environment,
        );
        let desktop_line = |prompt: &str| {
            prompt
                .lines()
                .find(|line| line.starts_with("Available semantic user directories:"))
                .expect("host context must report Desktop")
                .to_string()
        };
        assert_eq!(desktop_line(&english), desktop_line(&spanish));
        assert!(desktop_line(&english).contains("desktop"));
        assert!(!english.contains(&desktop.display().to_string()));
        assert!(!spanish.contains(&desktop.display().to_string()));
        assert!(english.contains("Never translate filesystem directory names"));
        assert!(spanish.contains("Never translate filesystem directory names"));
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn native_host_context_is_appended_without_replacing_character_prompt() {
        let prompt = compose_host_system_prompt(Some("character voice"), true);
        assert!(prompt.starts_with("character voice"));
        assert!(prompt.contains("<host_environment>"));
        assert!(prompt.contains("Autonomous Full Access is enabled"));
    }

    #[test]
    fn native_host_context_rebuilds_existing_runtime_block_without_duplication() {
        let first = compose_host_system_prompt(Some("character voice"), true);
        let rebuilt = compose_host_system_prompt(Some(&first), false);
        assert!(rebuilt.starts_with("character voice"));
        assert_eq!(rebuilt.matches("<host_environment>").count(), 1);
        assert!(rebuilt.contains("Autonomous Full Access is disabled"));
    }

    /// Scripted multi-turn provider: pops one scripted turn per call.
    struct QueueProvider {
        turns: Mutex<Vec<Vec<ModelStreamEvent>>>,
        seen: Mutex<Vec<ModelRequest>>,
    }

    impl QueueProvider {
        fn new(turns: Vec<Vec<ModelStreamEvent>>) -> Arc<Self> {
            Arc::new(Self {
                turns: Mutex::new(turns.into_iter().rev().collect()),
                seen: Mutex::new(Vec::new()),
            })
        }
    }

    /// Integration provider that makes the model consume the explicit host
    /// environment result before choosing a filesystem path. This exercises
    /// the complete native loop, including tool-call/result ids.
    struct EnvironmentThenWriteProvider {
        target: std::path::PathBuf,
        calls: Mutex<usize>,
        seen: Mutex<Vec<ModelRequest>>,
    }

    #[async_trait::async_trait]
    impl ModelProvider for EnvironmentThenWriteProvider {
        async fn stream(
            &self,
            request: ModelRequest,
        ) -> Result<model_core::ModelStream, ModelError> {
            self.seen.lock().unwrap().push(request.clone());
            let call = {
                let mut calls = self.calls.lock().unwrap();
                let current = *calls;
                *calls += 1;
                current
            };
            let events = match call {
                0 => vec![
                    ModelStreamEvent::ToolCall(ToolCall {
                        id: "environment-1".to_string(),
                        name: "system.environment".to_string(),
                        arguments: "{}".to_string(),
                    }),
                    ModelStreamEvent::Done {
                        finish_reason: FinishReason::ToolCalls,
                    },
                ],
                1 => {
                    let environment_result = request
                        .messages
                        .iter()
                        .find_map(|message| {
                            message
                                .tool_result
                                .as_ref()
                                .filter(|result| result.tool_call_id == "environment-1")
                        })
                        .expect("system.environment result must reach the next model request");
                    let environment: serde_json::Value =
                        serde_json::from_str(&environment_result.content)
                            .expect("environment result is JSON");
                    let cwd = std::env::current_dir().unwrap();
                    assert_eq!(environment["cwd"].as_str(), Some(cwd.to_str().unwrap()));
                    vec![
                        ModelStreamEvent::ToolCall(ToolCall {
                            id: "write-1".to_string(),
                            name: "filesystem.write".to_string(),
                            arguments: serde_json::json!({
                                "path": self.target.to_string_lossy(),
                                "content": "Hello from Utsuwa",
                            })
                            .to_string(),
                        }),
                        ModelStreamEvent::Done {
                            finish_reason: FinishReason::ToolCalls,
                        },
                    ]
                }
                _ => {
                    let write_result = request
                        .messages
                        .iter()
                        .find_map(|message| {
                            message
                                .tool_result
                                .as_ref()
                                .filter(|result| result.tool_call_id == "write-1")
                        })
                        .expect("filesystem.write result must reach the final model request");
                    assert!(!write_result.is_error);
                    vec![
                        ModelStreamEvent::TextDelta("created it".to_string()),
                        ModelStreamEvent::Done {
                            finish_reason: FinishReason::Stop,
                        },
                    ]
                }
            };
            Ok(Box::pin(futures_util::stream::iter(
                events.into_iter().map(Ok),
            )))
        }
    }

    #[async_trait::async_trait]
    impl ModelProvider for QueueProvider {
        async fn stream(
            &self,
            request: ModelRequest,
        ) -> Result<model_core::ModelStream, ModelError> {
            self.seen.lock().unwrap().push(request);
            let turn = self.turns.lock().unwrap().pop().unwrap_or_else(|| {
                vec![ModelStreamEvent::Done {
                    finish_reason: FinishReason::Stop,
                }]
            });
            Ok(Box::pin(futures_util::stream::iter(
                turn.into_iter().map(Ok),
            )))
        }
    }

    fn text_turn(text: &str) -> Vec<ModelStreamEvent> {
        vec![
            ModelStreamEvent::TextDelta(text.to_string()),
            ModelStreamEvent::Done {
                finish_reason: FinishReason::Stop,
            },
        ]
    }

    fn read_turn(path: &str) -> Vec<ModelStreamEvent> {
        vec![
            ModelStreamEvent::ToolCall(ToolCall {
                id: "r1".to_string(),
                name: "filesystem.read".to_string(),
                arguments: serde_json::json!({ "path": path }).to_string(),
            }),
            ModelStreamEvent::Done {
                finish_reason: FinishReason::ToolCalls,
            },
        ]
    }

    struct Harness {
        runtime: Arc<AgentRuntime>,
        approvals: Arc<Mutex<ApprovalQueue>>,
        events: Arc<Mutex<Vec<HostEvent>>>,
    }

    fn harness(provider: Arc<QueueProvider>) -> Harness {
        harness_with(provider, Vec::new())
    }

    fn harness_with(
        provider: Arc<QueueProvider>,
        grants: Vec<policy_core::GrantedScope>,
    ) -> Harness {
        let approvals = Arc::new(Mutex::new(ApprovalQueue::new().with_grants(grants)));
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let emit: EmitFn = Arc::new(move |event| {
            sink.lock().unwrap().push(event);
        });
        let runtime = AgentRuntime::start_with_factory(
            Arc::clone(&approvals),
            None,
            None,
            emit,
            Arc::new(move || Ok(Arc::clone(&provider) as Arc<dyn ModelProvider>)),
        )
        .unwrap();
        Harness {
            runtime,
            approvals,
            events,
        }
    }

    fn harness_with_storage(provider: Arc<QueueProvider>, storage: Arc<Mutex<Storage>>) -> Harness {
        let approvals = Arc::new(Mutex::new(ApprovalQueue::new()));
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let runtime = AgentRuntime::start_with_factory(
            Arc::clone(&approvals),
            Some(storage),
            None,
            Arc::new(move |event| {
                sink.lock().unwrap().push(event);
            }),
            Arc::new(move || Ok(Arc::clone(&provider) as Arc<dyn ModelProvider>)),
        )
        .unwrap();
        Harness {
            runtime,
            approvals,
            events,
        }
    }

    fn harness_with_storage_and_audit(
        provider: Arc<QueueProvider>,
        storage: Arc<Mutex<Storage>>,
    ) -> (Harness, Arc<audit_core::InMemorySink>) {
        let approvals = Arc::new(Mutex::new(ApprovalQueue::new()));
        let events = Arc::new(Mutex::new(Vec::new()));
        let audit = Arc::new(audit_core::InMemorySink::new());
        let sink = events.clone();
        let runtime = AgentRuntime::start_with_factory(
            Arc::clone(&approvals),
            Some(storage),
            Some(Arc::clone(&audit) as Arc<dyn audit_core::AuditSink>),
            Arc::new(move |event| {
                sink.lock().unwrap().push(event);
            }),
            Arc::new(move || Ok(Arc::clone(&provider) as Arc<dyn ModelProvider>)),
        )
        .unwrap();
        (
            Harness {
                runtime,
                approvals,
                events,
            },
            audit,
        )
    }

    fn wait_for(harness: &Harness, event: &str) -> HostEvent {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            if let Some(found) = harness
                .events
                .lock()
                .unwrap()
                .iter()
                .find(|e| e.event == event)
                .cloned()
            {
                return found;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for host event {event}");
    }

    fn wait_for_sensor_state(harness: &Harness, event: &str, active: bool) -> HostEvent {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            if let Some(found) = harness
                .events
                .lock()
                .unwrap()
                .iter()
                .find(|candidate| candidate.event == event && candidate.data["active"] == active)
                .cloned()
            {
                return found;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for {event} active={active}");
    }

    #[test]
    fn microphone_activity_publishes_authoritative_host_events() {
        let provider = QueueProvider::new(Vec::new());
        let harness = harness(Arc::clone(&provider));
        let audio = harness.runtime.audio_activity_state();

        audio.begin_external_session("native-mic".to_string(), "mic-0".to_string(), 123);
        let started = wait_for_sensor_state(&harness, "microphone.activity.changed", true);
        assert_eq!(started.data["session_count"], 1);
        assert_eq!(
            harness.runtime.microphone_activity_status().device,
            Some("mic-0".to_string())
        );

        audio.end_session("native-mic");
        let stopped = wait_for_sensor_state(&harness, "microphone.activity.changed", false);
        assert_eq!(stopped.data["session_count"], 0);
        assert!(!harness.runtime.microphone_activity_status().active);
    }

    fn temp_project(name: &str) -> (std::path::PathBuf, String) {
        let dir =
            std::env::temp_dir().join(format!("utsuwa-runtime-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("notes.txt");
        std::fs::write(&file, "project notes alpha").unwrap();
        (dir, file.to_string_lossy().to_string())
    }

    fn write_turn(path: &str, content: &str) -> Vec<ModelStreamEvent> {
        vec![
            ModelStreamEvent::ToolCall(ToolCall {
                id: "w1".to_string(),
                name: "filesystem.write".to_string(),
                arguments: serde_json::json!({
                    "path": path,
                    "content": content,
                })
                .to_string(),
            }),
            ModelStreamEvent::Done {
                finish_reason: FinishReason::ToolCalls,
            },
        ]
    }

    #[test]
    fn native_turn_preserves_history_system_prompt_without_explicit_prompt() {
        let provider = QueueProvider::new(vec![text_turn("ok")]);
        let harness = harness(Arc::clone(&provider));
        harness
            .runtime
            .send_request(AgentRequest {
                text: "hello".to_string(),
                history: vec![
                    ModelMessage::system("character voice"),
                    ModelMessage::user("earlier"),
                ],
                append_user_message: true,
                ..AgentRequest::default()
            })
            .unwrap();
        wait_for(&harness, "agent.turn_done");

        let seen = provider.seen.lock().unwrap();
        assert!(seen[0].messages[0].content.starts_with("character voice"));
        assert!(seen[0].messages[0].content.contains("<host_environment>"));
    }

    #[test]
    fn plain_text_turn_emits_done() {
        let provider = QueueProvider::new(vec![text_turn("hello there")]);
        let harness = harness(Arc::clone(&provider));
        harness.runtime.send_message("hi".to_string()).unwrap();
        let done = wait_for(&harness, "agent.turn_done");
        assert_eq!(done.data["text"], "hello there");
        assert!(harness.approvals.lock().unwrap().list().is_empty());
        let tools = &provider.seen.lock().unwrap()[0].tools;
        let expected = [
            "filesystem.list",
            "filesystem.stat",
            "filesystem.read",
            "filesystem.read_range",
            "filesystem.search_text",
            "filesystem.glob",
            "filesystem.patch",
            "filesystem.write",
            "process.spawn",
            "process.status",
            "process.kill",
            "filesystem.resolve_user_dir",
            "filesystem.write_user_file",
            "filesystem.create_user_file",
            "filesystem.edit_user_file",
            "filesystem.edit_file",
            "filesystem.edit",
            "filesystem.replace_user_file",
            "filesystem.append_user_file",
            "filesystem.append_file",
            "system.environment",
            "system.time",
            "desktop.status",
            "desktop.inspect",
        ];
        for tool_id in expected {
            assert!(
                tools.iter().any(|tool| tool.name == tool_id),
                "native request is missing {tool_id}"
            );
        }
    }

    #[test]
    fn local_model_profile_is_small_but_keeps_high_level_tools() {
        assert_eq!(
            tool_profile_for_provider(Some("lmstudio")),
            ToolProfile::Simple
        );
        assert_eq!(
            tool_profile_for_provider(Some("ollama")),
            ToolProfile::Simple
        );
        assert_eq!(
            tool_profile_for_provider(Some("LMStudio")),
            ToolProfile::Simple
        );
        assert_eq!(tool_profile_for_provider(Some("openai")), ToolProfile::Full);
        assert_eq!(tool_profile_for_provider(None), ToolProfile::Full);

        assert!(ToolProfile::Simple.allows_tool("system.environment"));
        assert!(ToolProfile::Simple.allows_tool("system.time"));
        assert!(ToolProfile::Simple.allows_tool("filesystem.create_user_file"));
        // Small models get exactly one edit interface; the overlapping
        // edit_user_file/edit_file tools stay available to Full.
        assert!(ToolProfile::Simple.allows_tool("filesystem.edit"));
        assert!(!ToolProfile::Simple.allows_tool("filesystem.edit_user_file"));
        assert!(!ToolProfile::Simple.allows_tool("filesystem.edit_file"));
        assert!(ToolProfile::Full.allows_tool("filesystem.edit_user_file"));
        assert!(ToolProfile::Full.allows_tool("filesystem.edit_file"));
        assert!(ToolProfile::Simple.allows_tool("filesystem.replace_user_file"));
        assert!(ToolProfile::Simple.allows_tool("filesystem.append_user_file"));
        assert!(ToolProfile::Simple.allows_tool("filesystem.append_file"));
        assert!(ToolProfile::Simple.allows_tool("filesystem.read"));
        assert!(ToolProfile::Simple.allows_tool("filesystem.write"));
        assert!(ToolProfile::Simple.allows_tool("desktop.status"));
        assert!(ToolProfile::Simple.allows_tool("desktop.inspect"));
        assert!(ToolProfile::Simple.allows_tool("desktop.screenshot"));
        assert!(ToolProfile::Simple.allows_tool("desktop.click"));
        assert!(ToolProfile::Simple.allows_tool("desktop.type_text"));
        assert!(!ToolProfile::Simple.allows_tool("filesystem.patch"));
        assert!(!ToolProfile::Simple.allows_tool("filesystem.write_user_file"));
        assert!(!ToolProfile::Simple.allows_tool("desktop.list_windows"));
        assert!(!ToolProfile::Simple.allows_tool("desktop.accessibility_tree"));
        assert!(!ToolProfile::Simple.allows_tool("desktop.invoke_element"));
        assert!(!ToolProfile::Simple.allows_tool("desktop.set_value"));
        assert!(ToolProfile::Full.allows_tool("filesystem.write_user_file"));
        assert!(ToolProfile::Full.allows_tool("system.time"));
        assert!(ToolProfile::Full.allows_tool("desktop.list_windows"));
        assert!(ToolProfile::Full.allows_tool("desktop.accessibility_tree"));
    }

    #[test]
    fn browser_cdp_endpoint_setting_is_loopback_only() {
        let storage_dir =
            std::env::temp_dir().join(format!("utsuwa-cdp-endpoint-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&storage_dir);
        let storage = Arc::new(Mutex::new(
            Storage::open(&storage_dir.join("state.db")).unwrap(),
        ));
        // Absent: the default.
        assert_eq!(
            read_cdp_endpoint(Some(&storage)),
            tool_browser::DEFAULT_CDP_ENDPOINT
        );
        // Loopback custom port: honored.
        storage
            .lock()
            .unwrap()
            .set_setting(
                SETTING_BROWSER_CDP_ENDPOINT,
                &serde_json::json!("http://127.0.0.1:9333"),
            )
            .unwrap();
        assert_eq!(read_cdp_endpoint(Some(&storage)), "http://127.0.0.1:9333");
        // Remote endpoint: fails closed to the default, never honored.
        storage
            .lock()
            .unwrap()
            .set_setting(
                SETTING_BROWSER_CDP_ENDPOINT,
                &serde_json::json!("http://192.168.1.10:9222"),
            )
            .unwrap();
        assert_eq!(
            read_cdp_endpoint(Some(&storage)),
            tool_browser::DEFAULT_CDP_ENDPOINT
        );
        let _ = std::fs::remove_dir_all(&storage_dir);
    }

    #[test]
    fn explicit_model_tool_profile_overrides_provider_inference() {
        let storage_dir =
            std::env::temp_dir().join(format!("utsuwa-tool-profile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&storage_dir);
        let storage = Arc::new(Mutex::new(
            Storage::open(&storage_dir.join("state.db")).unwrap(),
        ));
        storage
            .lock()
            .unwrap()
            .set_setting(SETTING_PROVIDER, &serde_json::json!("lmstudio"))
            .unwrap();
        assert_eq!(configured_tool_profile(Some(&storage)), ToolProfile::Simple);
        storage
            .lock()
            .unwrap()
            .set_setting(SETTING_TOOL_PROFILE, &serde_json::json!("full"))
            .unwrap();
        assert_eq!(configured_tool_profile(Some(&storage)), ToolProfile::Full);
        std::fs::remove_dir_all(&storage_dir).ok();
    }

    #[test]
    fn emergency_stop_revokes_control_grants_and_reports_state() {
        let grant = policy_core::GrantedScope::new(
            capability_core::PrincipalKind::Agent,
            capability_core::Capability::DesktopControl,
            capability_core::ResourceScope::new(vec![capability_core::Resource::Window(
                "w1".to_string(),
            )]),
            policy_core::GrantLifetime::Persistent,
            None,
            None,
        );
        let observe_grant = policy_core::GrantedScope::new(
            capability_core::PrincipalKind::Agent,
            capability_core::Capability::DesktopObserve,
            capability_core::ResourceScope::new(vec![capability_core::Resource::Window(
                "w1".to_string(),
            )]),
            policy_core::GrantLifetime::Persistent,
            None,
            None,
        );
        let provider = QueueProvider::new(vec![text_turn("ok")]);
        let harness = harness_with(provider, vec![grant, observe_grant]);
        // This test exercises policy revocation, not native window probing.
        // Keep it deterministic on headless hosts where a real portal backend
        // may block while enumerating windows a second time.
        harness
            .runtime
            .set_desktop_plugin(tool_desktop::plugin::DesktopPlugin::stub());

        let status = harness.runtime.desktop_emergency_stop();
        assert!(status.emergency_stopped);
        assert!(!status.control_enabled);
        // DesktopControl grants are gone; unrelated grants survive.
        let remaining = harness.approvals.lock().unwrap().grants_snapshot();
        assert!(
            remaining
                .iter()
                .all(|grant| grant.capability != capability_core::Capability::DesktopControl),
            "{remaining:?}"
        );
        assert!(
            remaining
                .iter()
                .any(|grant| grant.capability == capability_core::Capability::DesktopObserve),
            "{remaining:?}"
        );

        let status = harness.runtime.desktop_clear_emergency_stop();
        assert!(!status.emergency_stopped);
        assert!(!tool_desktop::emergency_stop_active());
    }

    #[test]
    fn live_registry_registers_the_new_local_machine_packs() {
        // Sync test: the runtime owns an internal executor, which cannot
        // be driven from inside another async context.
        let worker = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let provider = QueueProvider::new(vec![text_turn("ok")]);
        let harness = harness(provider);
        let registry = worker.block_on(async {
            let catalog = harness
                .runtime
                .tool_catalog(&HostEnvironment::snapshot())
                .await;
            catalog
                .snapshot(&ToolLoadContext::new(ToolProfile::Full))
                .await
                .expect("live catalog snapshot must succeed")
        });
        let ids: Vec<String> = registry
            .list()
            .into_iter()
            .map(|metadata| metadata.id.0)
            .collect();
        // Tools with no backend dependency are always advertised.
        for expected in [
            "http.get",
            "http.download",
            "archive.list",
            "archive.extract",
            "git.status",
            "git.push",
            "notification.show",
            "browser.status",
            "camera.list",
            "audio.list_devices",
            "audio.record",
            "media.metadata",
            "system.cpu",
            "system.processes",
            "clipboard.read",
            "application.launch",
            "document.extract_text",
            "image.resize",
        ] {
            assert!(ids.iter().any(|id| id == expected), "missing {expected}");
        }
        // Backend-dependent tools track live availability: present exactly
        // when their backend answers on this machine, so the test stays
        // honest with or without a browser, camera, or ffmpeg installed.
        let browser_available =
            tool_browser::CdpBackend::new(read_cdp_endpoint(harness.runtime.storage.as_ref()))
                .is_available();
        for control in ["browser.snapshot", "browser.cookies.delete"] {
            assert_eq!(
                ids.iter().any(|id| id == control),
                browser_available,
                "{control} must track CDP availability"
            );
        }
        use tool_browser::BrowserBackend as _;
        let camera_available = AgentRuntime::camera_backend().is_available();
        assert_eq!(
            ids.iter().any(|id| id == "camera.capture_photo"),
            camera_available,
            "camera.capture_photo must track camera availability"
        );
        assert_eq!(
            ids.iter().any(|id| id == "media.video_keyframes"),
            tool_media::ffmpeg_available(),
            "media.video_keyframes must track ffmpeg availability"
        );
        assert_eq!(
            ids.iter().any(|id| id == "pdf.page_image"),
            tool_system::pdftoppm_available(),
            "pdf.page_image must track pdftoppm availability"
        );
    }

    #[tokio::test]
    async fn simple_profile_filters_redundant_builtin_tools_from_model_registry() {
        // Same composition the runtime snapshots per turn: static packs
        // plus the required filesystem surface, filtered centrally.
        let processes = ProcessManager::new(ProcessLimits::default());
        let environment = HostEnvironment::snapshot();
        let catalog = ToolCatalog::new()
            .with_pack(SystemToolPack::new(environment.clone()))
            .with_pack(ProcessToolPack::new(processes))
            .with_required_pack(tool_filesystem_host::HostFilesystemPack::new(
                environment,
                None,
            ));
        let registry = catalog
            .snapshot(&ToolLoadContext::new(ToolProfile::Simple))
            .await
            .expect("builtin catalog snapshot must succeed");
        let ids: Vec<String> = registry
            .list()
            .into_iter()
            .map(|metadata| metadata.id.0)
            .collect();
        for expected in [
            "system.environment",
            "system.time",
            "filesystem.create_user_file",
            "filesystem.edit",
            "filesystem.replace_user_file",
            "filesystem.append_user_file",
            "filesystem.append_file",
            "filesystem.read",
            "filesystem.list",
            "filesystem.write",
            "process.spawn",
        ] {
            assert!(ids.iter().any(|id| id == expected), "missing {expected}");
        }
        for hidden in [
            "filesystem.stat",
            "filesystem.read_range",
            "filesystem.search_text",
            "filesystem.glob",
            "filesystem.patch",
            "filesystem.edit_user_file",
            "filesystem.edit_file",
            "filesystem.resolve_user_dir",
            "filesystem.write_user_file",
            "desktop.list_windows",
            "desktop.accessibility_tree",
            "desktop.invoke_element",
            "desktop.set_value",
        ] {
            assert!(!ids.iter().any(|id| id == hidden), "unexpected {hidden}");
        }
    }

    #[test]
    fn environment_tool_result_drives_a_native_write_and_preserves_ids() {
        let target = std::env::current_dir().unwrap().join(format!(
            ".utsuwa-agent-environment-test-{}.md",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&target);
        let storage_dir = std::env::temp_dir().join(format!(
            "utsuwa-runtime-environment-storage-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&storage_dir);
        let storage = Arc::new(Mutex::new(
            Storage::open(&storage_dir.join("state.db")).unwrap(),
        ));
        storage
            .lock()
            .unwrap()
            .set_setting(SETTING_AUTONOMOUS_FULL_ACCESS, &serde_json::json!(true))
            .unwrap();
        let provider = Arc::new(EnvironmentThenWriteProvider {
            target: target.clone(),
            calls: Mutex::new(0),
            seen: Mutex::new(Vec::new()),
        });
        let approvals = Arc::new(Mutex::new(ApprovalQueue::new()));
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let runtime = AgentRuntime::start_with_factory(
            Arc::clone(&approvals),
            Some(storage),
            None,
            Arc::new(move |event| sink.lock().unwrap().push(event)),
            Arc::new({
                let provider = Arc::clone(&provider);
                move || Ok(Arc::clone(&provider) as Arc<dyn ModelProvider>)
            }),
        )
        .unwrap();
        let harness = Harness {
            runtime,
            approvals,
            events,
        };
        harness
            .runtime
            .send_message("create it".to_string())
            .unwrap();
        let done = wait_for(&harness, "agent.turn_done");
        assert_eq!(done.data["text"], "created it");
        assert_eq!(done.data["tool_steps"].as_array().unwrap().len(), 2);
        assert_eq!(done.data["tool_steps"][0]["name"], "system.environment");
        assert_eq!(done.data["tool_steps"][0]["ok"], true);
        assert_eq!(done.data["tool_steps"][1]["name"], "filesystem.write");
        assert_eq!(done.data["tool_steps"][1]["ok"], true);
        assert_eq!(
            done.data["tool_steps"][1]["output"]["path"],
            target.to_string_lossy().as_ref()
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "Hello from Utsuwa"
        );
        assert!(harness
            .events
            .lock()
            .unwrap()
            .iter()
            .all(|event| event.event != "permission.requested"));
        let seen = provider.seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert_eq!(
            seen[1]
                .messages
                .last()
                .unwrap()
                .tool_result
                .as_ref()
                .unwrap()
                .tool_call_id,
            "environment-1"
        );
        assert_eq!(
            seen[2]
                .messages
                .last()
                .unwrap()
                .tool_result
                .as_ref()
                .unwrap()
                .tool_call_id,
            "write-1"
        );
        std::fs::remove_file(&target).ok();
        std::fs::remove_dir_all(&storage_dir).ok();
    }

    #[test]
    fn autonomous_mode_executes_filesystem_write_without_permission_request() {
        let (dir, _) = temp_project("autonomous-write");
        let target = dir.join("hello.md");
        let storage = Arc::new(Mutex::new(Storage::open(&dir.join("state.db")).unwrap()));
        storage
            .lock()
            .unwrap()
            .set_setting(SETTING_AUTONOMOUS_FULL_ACCESS, &serde_json::json!(true))
            .unwrap();

        let provider = QueueProvider::new(vec![
            write_turn(&target.to_string_lossy(), "Hello from Utsuwa"),
            text_turn("created it"),
        ]);
        let (harness, audit) = harness_with_storage_and_audit(Arc::clone(&provider), storage);
        assert!(harness.runtime.autonomous_full_access_enabled());
        harness
            .runtime
            .send_message("Create hello.md".to_string())
            .unwrap();

        let done = wait_for(&harness, "agent.turn_done");
        assert_eq!(done.data["text"], "created it");
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "Hello from Utsuwa"
        );
        assert!(harness
            .events
            .lock()
            .unwrap()
            .iter()
            .all(|event| event.event != "permission.requested"));
        assert!(harness.approvals.lock().unwrap().list().is_empty());
        let write_audit = audit
            .records()
            .into_iter()
            .find(|record| {
                record.outcome == audit_core::AuditOutcome::Executed
                    && record.capability == Some(capability_core::Capability::FilesystemWrite)
            })
            .expect("autonomous write should be audit-logged");
        assert!(write_audit
            .detail
            .contains("authorization_mode=autonomous_full_access"));
        assert!(write_audit.mutation.is_some());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn failed_autonomous_write_is_exposed_in_turn_done() {
        let (dir, _) = temp_project("autonomous-failed-write");
        let localized_desktop = dir.join("Escritorio");
        std::fs::create_dir_all(&localized_desktop).unwrap();
        let wrong_parent = dir.join("Desktop");
        let wrong_target = wrong_parent.join("hello.md");
        let storage = Arc::new(Mutex::new(Storage::open(&dir.join("state.db")).unwrap()));
        storage
            .lock()
            .unwrap()
            .set_setting(SETTING_AUTONOMOUS_FULL_ACCESS, &serde_json::json!(true))
            .unwrap();
        let provider = QueueProvider::new(vec![
            write_turn(&wrong_target.to_string_lossy(), "Hello from Utsuwa"),
            text_turn("I could not create it at that path"),
        ]);
        let harness = harness_with_storage(Arc::clone(&provider), storage);
        harness
            .runtime
            .send_message("create it".to_string())
            .unwrap();
        let done = wait_for(&harness, "agent.turn_done");
        assert_eq!(done.data["text"], "I could not create it at that path");
        assert!(done.data["executed"].as_array().unwrap().is_empty());
        let step = &done.data["tool_steps"][0];
        assert_eq!(step["name"], "filesystem.write");
        assert_eq!(step["status"], "failed");
        assert_eq!(step["ok"], false);
        assert!(step["error"].as_str().unwrap().contains(&format!(
            "parent directory does not exist: {}",
            wrong_parent.display()
        )));
        assert!(!wrong_parent.exists());
        assert!(!wrong_target.exists());
        assert!(harness
            .events
            .lock()
            .unwrap()
            .iter()
            .all(|event| event.event != "permission.requested"));
        assert!(localized_desktop.is_dir());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn autonomous_mode_runs_bash_without_interpreter_permission_request() {
        if !std::path::Path::new("/bin/bash").is_file() {
            return;
        }
        let (dir, _) = temp_project("autonomous-bash");
        let target = dir.join("hello.md");
        let storage = Arc::new(Mutex::new(Storage::open(&dir.join("state.db")).unwrap()));
        storage
            .lock()
            .unwrap()
            .set_setting(SETTING_AUTONOMOUS_FULL_ACCESS, &serde_json::json!(true))
            .unwrap();

        let command = format!("printf '%s' 'Hello from Utsuwa' > {}", target.display());
        let spawn_turn = vec![
            ModelStreamEvent::ToolCall(ToolCall {
                id: "p1".to_string(),
                name: "process.spawn".to_string(),
                arguments: serde_json::json!({
                    "executable": "/bin/bash",
                    "args": ["-lc", command],
                    "cwd": dir.to_string_lossy(),
                })
                .to_string(),
            }),
            ModelStreamEvent::Done {
                finish_reason: FinishReason::ToolCalls,
            },
        ];
        let provider = QueueProvider::new(vec![spawn_turn, text_turn("ran it")]);
        let harness = harness_with_storage(Arc::clone(&provider), storage);
        harness
            .runtime
            .send_message("Run the command".to_string())
            .unwrap();
        let done = wait_for(&harness, "agent.turn_done");
        assert_eq!(done.data["text"], "ran it");
        let start = Instant::now();
        while !target.exists() && start.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "Hello from Utsuwa"
        );
        assert!(harness
            .events
            .lock()
            .unwrap()
            .iter()
            .all(|event| event.event != "permission.requested"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn enabling_autonomous_mode_resumes_a_suspended_agent_turn() {
        let (dir, path) = temp_project("autonomous-resume");
        let provider = QueueProvider::new(vec![read_turn(&path), text_turn("read it")]);
        let harness = harness(Arc::clone(&provider));
        harness
            .runtime
            .send_message("Read my notes".to_string())
            .unwrap();
        let suspended = wait_for(&harness, "agent.turn_suspended");
        let request_id = suspended.data["request_id"].as_str().unwrap();
        assert_eq!(harness.approvals.lock().unwrap().list().len(), 1);

        // This is the live half of the native settings.set path. The runtime
        // withdraws only its Agent request and retries under the mode; no
        // standing grant is created.
        harness.runtime.set_autonomous_full_access(true);
        let done = wait_for(&harness, "agent.turn_done");
        assert_eq!(done.data["text"], "read it");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "project notes alpha"
        );
        assert!(harness.approvals.lock().unwrap().list().is_empty());
        assert!(harness.events.lock().unwrap().iter().any(|event| {
            event.event == "permission.dismissed" && event.data["id"] == request_id
        }));
        assert!(harness
            .approvals
            .lock()
            .unwrap()
            .grants_snapshot()
            .is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn approval_suspends_then_approve_resumes_and_executes() {
        let (_dir, path) = temp_project("approve");
        let provider = QueueProvider::new(vec![
            read_turn(&path),
            read_turn(&path),
            text_turn("done reading"),
        ]);
        let harness = harness(provider);

        harness
            .runtime
            .send_message("read my notes".to_string())
            .unwrap();
        let requested = wait_for(&harness, "permission.requested");
        assert_eq!(requested.data["capability"], "FilesystemRead");
        let suspended = wait_for(&harness, "agent.turn_suspended");
        let request_id = suspended.data["request_id"].as_str().unwrap().to_string();
        assert_eq!(requested.data["id"], request_id);

        // The read must not have executed: no grant existed.
        assert!(harness
            .events
            .lock()
            .unwrap()
            .iter()
            .all(|e| e.event != "agent.turn_done"));

        // Approve with a session grant, then resume: the re-issued call
        // executes under the grant and the turn completes.
        let pending = harness.approvals.lock().unwrap().list();
        assert_eq!(pending.len(), 1);
        harness
            .approvals
            .lock()
            .unwrap()
            .decide(&pending[0].id, Some(policy_core::GrantLifetime::Session))
            .unwrap();
        harness.runtime.notify_decided(&request_id, true);

        let done = wait_for(&harness, "agent.turn_done");
        assert_eq!(done.data["text"], "done reading");
        let executed = done.data["executed"].as_array().unwrap();
        assert_eq!(executed.len(), 1);
        assert_eq!(executed[0]["name"], "filesystem.read");
        assert!(executed[0]["output"]["content"]
            .as_str()
            .unwrap()
            .contains("project notes alpha"));
        assert!(harness.approvals.lock().unwrap().list().is_empty());
    }

    #[test]
    fn native_filesystem_read_round_trip_reaches_final_model_response() {
        struct ReadbackProvider {
            calls: Mutex<usize>,
            seen: Mutex<Vec<ModelRequest>>,
            path: String,
        }

        #[async_trait::async_trait]
        impl ModelProvider for ReadbackProvider {
            async fn stream(
                &self,
                request: ModelRequest,
            ) -> Result<model_core::ModelStream, ModelError> {
                let mut calls = self.calls.lock().unwrap();
                *calls += 1;
                let call_number = *calls;
                drop(calls);
                self.seen.lock().unwrap().push(request.clone());

                // The first call suspends for approval; after approval the
                // host intentionally re-runs the model so it can re-issue
                // the tool call, then receives a third call with the result.
                let turn = if call_number <= 2 {
                    read_turn(&self.path)
                } else {
                    let result = request
                        .messages
                        .iter()
                        .find_map(|message| message.tool_result.as_ref())
                        .expect("tool result must be attached before the final model call");
                    let output: serde_json::Value = serde_json::from_str(&result.content).unwrap();
                    let content = output["content"].as_str().unwrap_or_default();
                    text_turn(&format!("The file says: {content}"))
                };
                Ok(Box::pin(futures_util::stream::iter(
                    turn.into_iter().map(Ok),
                )))
            }
        }

        let (dir, path) = temp_project("native-round-trip");
        std::fs::write(&path, "hello from utsuwa").unwrap();
        let provider = Arc::new(ReadbackProvider {
            calls: Mutex::new(0),
            seen: Mutex::new(Vec::new()),
            path: path.clone(),
        });
        let approvals = Arc::new(Mutex::new(ApprovalQueue::new()));
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let runtime = AgentRuntime::start_with_factory(
            Arc::clone(&approvals),
            None,
            None,
            Arc::new(move |event| {
                sink.lock().unwrap().push(event);
            }),
            Arc::new({
                let provider = Arc::clone(&provider);
                move || Ok(Arc::clone(&provider) as Arc<dyn ModelProvider>)
            }),
        )
        .unwrap();
        let harness = Harness {
            runtime,
            approvals,
            events,
        };

        harness
            .runtime
            .send_message("read notes.txt".to_string())
            .unwrap();
        let requested = wait_for(&harness, "permission.requested");
        assert_eq!(requested.data["capability"], "FilesystemRead");
        let suspended = wait_for(&harness, "agent.turn_suspended");
        let request_id = suspended.data["request_id"].as_str().unwrap().to_string();
        let pending = harness.approvals.lock().unwrap().list()[0].clone();
        assert_eq!(pending.id, request_id);

        harness
            .approvals
            .lock()
            .unwrap()
            .decide(&request_id, Some(policy_core::GrantLifetime::Session))
            .unwrap();
        harness.runtime.notify_decided(&request_id, true);

        let done = wait_for(&harness, "agent.turn_done");
        assert_eq!(done.data["text"], "The file says: hello from utsuwa");
        assert_eq!(done.data["executed"].as_array().unwrap().len(), 1);
        assert_eq!(done.data["executed"][0]["name"], "filesystem.read");
        assert_eq!(
            done.data["executed"][0]["output"]["content"],
            "hello from utsuwa"
        );

        let seen = provider.seen.lock().unwrap();
        assert!(seen[0]
            .tools
            .iter()
            .any(|tool| tool.name == "filesystem.read"));
        // The approved turn re-issues the call once; the following model
        // request is the first one that can contain its result.
        let result = seen[2]
            .messages
            .iter()
            .find_map(|message| message.tool_result.as_ref())
            .expect("native tool result must be in the next model request");
        assert_eq!(result.tool_call_id, "r1");
        assert!(!result.is_error);
        assert!(result.content.contains("hello from utsuwa"));

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn agent_spawns_and_polls_process_to_output() {
        let echo = tool_process::resolve_executable("echo").unwrap();
        let grant = policy_core::GrantedScope::new(
            capability_core::PrincipalKind::Agent,
            capability_core::Capability::ProcessSpawn,
            capability_core::ResourceScope::new(vec![capability_core::Resource::Process {
                executable: echo,
                args: vec!["build-ok".to_string()],
                cwd: std::env::current_dir().unwrap().canonicalize().unwrap(),
                env: Vec::new(),
            }]),
            policy_core::GrantLifetime::Persistent,
            None,
            None,
        );
        // Driver provider: spawn echo, then poll its handle (parsed from
        // the spawn result visible in the request transcript), then stop.
        struct Driver {
            calls: Mutex<usize>,
        }
        #[async_trait::async_trait]
        impl ModelProvider for Driver {
            async fn stream(
                &self,
                request: ModelRequest,
            ) -> Result<model_core::ModelStream, ModelError> {
                let mut calls = self.calls.lock().unwrap();
                *calls += 1;
                let call_number = *calls;
                drop(calls);
                let results: Vec<(String, serde_json::Value)> = request
                    .messages
                    .iter()
                    .filter_map(|m| m.tool_result.as_ref())
                    .filter_map(|r| {
                        serde_json::from_str::<serde_json::Value>(&r.content)
                            .ok()
                            .map(|v| (r.tool_call_id.clone(), v))
                    })
                    .collect();
                let handle = results
                    .iter()
                    .find(|(id, _)| id == "c1")
                    .and_then(|(_, v)| v.get("handle")?.as_str())
                    .map(str::to_string);
                let exited = results
                    .iter()
                    .any(|(_, v)| v.get("state") == Some(&serde_json::Value::from("exited")));
                let turn = match (call_number, handle) {
                    (1, _) => vec![
                        ModelStreamEvent::ToolCall(ToolCall {
                            id: "c1".to_string(),
                            name: "process.spawn".to_string(),
                            arguments: serde_json::json!({
                                "executable": "echo",
                                "args": ["build-ok"],
                                "timeout_ms": 15_000,
                            })
                            .to_string(),
                        }),
                        ModelStreamEvent::Done {
                            finish_reason: FinishReason::ToolCalls,
                        },
                    ],
                    // Poll until the child reports exited (echo is fast;
                    // the loop bound caps pathological cases).
                    (_, Some(handle)) if !exited => vec![
                        ModelStreamEvent::ToolCall(ToolCall {
                            id: format!("poll-{call_number}"),
                            name: "process.status".to_string(),
                            arguments: serde_json::json!({ "handle": handle }).to_string(),
                        }),
                        ModelStreamEvent::Done {
                            finish_reason: FinishReason::ToolCalls,
                        },
                    ],
                    _ => text_turn("build finished"),
                };
                Ok(Box::pin(futures_util::stream::iter(
                    turn.into_iter().map(Ok),
                )))
            }
        }
        let approvals = Arc::new(Mutex::new(ApprovalQueue::new().with_grants(vec![grant])));
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let runtime = AgentRuntime::start_with_factory(
            Arc::clone(&approvals),
            None,
            None,
            Arc::new(move |event| {
                sink.lock().unwrap().push(event);
            }),
            Arc::new(|| {
                Ok(Arc::new(Driver {
                    calls: Mutex::new(0),
                }) as Arc<dyn ModelProvider>)
            }),
        )
        .unwrap();
        let harness = Harness {
            runtime,
            approvals,
            events,
        };

        harness
            .runtime
            .send_message("run the build".to_string())
            .unwrap();
        let done = wait_for(&harness, "agent.turn_done");
        let executed = done.data["executed"].as_array().unwrap();
        assert!(executed.len() >= 2, "{executed:?}");
        assert_eq!(executed[0]["name"], "process.spawn");
        let last = executed.last().unwrap();
        assert_eq!(last["name"], "process.status");
        assert_eq!(last["output"]["state"], "exited");
        assert_eq!(last["output"]["stdout"], "build-ok\n");
        // No approval was needed: the session grant covered the spawn.
        assert!(harness.approvals.lock().unwrap().list().is_empty());
    }

    #[test]
    fn memory_tools_roundtrip_without_approval() {
        let remember_turn = vec![
            ModelStreamEvent::ToolCall(ToolCall {
                id: "m1".to_string(),
                name: "memory.remember".to_string(),
                arguments: serde_json::json!({
                    "text": "concise answers preferred",
                    "tags": ["style"],
                    "importance": 6,
                })
                .to_string(),
            }),
            ModelStreamEvent::Done {
                finish_reason: FinishReason::ToolCalls,
            },
        ];
        let provider = QueueProvider::new(vec![remember_turn, text_turn("noted")]);
        let harness = harness(provider);

        harness
            .runtime
            .send_message("remember this".to_string())
            .unwrap();
        let done = wait_for(&harness, "agent.turn_done");
        let executed = done.data["executed"].as_array().unwrap();
        assert_eq!(executed.len(), 1, "{executed:?}");
        assert_eq!(executed[0]["name"], "memory.remember");
        assert!(executed[0]["output"]["id"].as_i64().unwrap() > 0);
        // Pure notebook tools need no approval dance.
        assert!(harness.approvals.lock().unwrap().list().is_empty());
        // The fact survives in the runtime's store and recalls by keyword.
        let entries = harness.runtime.memory_store().recall("concise", 5).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "concise answers preferred");
        assert_eq!(entries[0].importance, 6);
    }

    struct FakeDesktop;

    #[async_trait::async_trait]
    impl tool_desktop::DesktopBackend for FakeDesktop {
        async fn list_windows(
            &self,
        ) -> Result<Vec<tool_desktop::WindowInfo>, tool_desktop::DesktopError> {
            Ok(vec![tool_desktop::WindowInfo {
                id: "w1".to_string(),
                title: "Notes".to_string(),
                app: "notes".to_string(),
            }])
        }
        async fn accessibility_tree(
            &self,
            _window_id: &str,
        ) -> Result<Vec<tool_desktop::ElementNode>, tool_desktop::DesktopError> {
            Ok(vec![])
        }
        async fn invoke_element(
            &self,
            _window_id: &str,
            _element_id: &str,
        ) -> Result<(), tool_desktop::DesktopError> {
            Ok(())
        }
        async fn set_value(
            &self,
            _window_id: &str,
            _element_id: &str,
            _value: &str,
        ) -> Result<(), tool_desktop::DesktopError> {
            Ok(())
        }
        async fn screenshot(
            &self,
            _window_id: Option<&str>,
        ) -> Result<tool_desktop::Screenshot, tool_desktop::DesktopError> {
            Err(tool_desktop::DesktopError::ActionFailed(
                "no display".to_string(),
            ))
        }
        async fn click(
            &self,
            _window_id: Option<&str>,
            _at: tool_desktop::Point,
        ) -> Result<(), tool_desktop::DesktopError> {
            Ok(())
        }
        async fn type_text(
            &self,
            _window_id: Option<&str>,
            _text: &str,
        ) -> Result<(), tool_desktop::DesktopError> {
            Ok(())
        }
    }

    #[test]
    fn desktop_tools_join_the_turn_behind_policy() {
        let observe_turn = vec![
            ModelStreamEvent::ToolCall(ToolCall {
                id: "o1".to_string(),
                name: "desktop.accessibility_tree".to_string(),
                arguments: serde_json::json!({"window_id": "w1"}).to_string(),
            }),
            ModelStreamEvent::Done {
                finish_reason: FinishReason::ToolCalls,
            },
        ];
        let invoke_turn = vec![
            ModelStreamEvent::ToolCall(ToolCall {
                id: "d1".to_string(),
                name: "desktop.invoke_element".to_string(),
                arguments: serde_json::json!({
                    "window_id": "w1",
                    "element_id": "e1",
                })
                .to_string(),
            }),
            ModelStreamEvent::Done {
                finish_reason: FinishReason::ToolCalls,
            },
        ];
        let grant = policy_core::GrantedScope::new(
            capability_core::PrincipalKind::Agent,
            capability_core::Capability::DesktopControl,
            capability_core::ResourceScope::new(vec![capability_core::Resource::Window(
                "w1".to_string(),
            )]),
            policy_core::GrantLifetime::Persistent,
            None,
            None,
        );
        let observe_grant = policy_core::GrantedScope::new(
            capability_core::PrincipalKind::Agent,
            capability_core::Capability::DesktopObserve,
            capability_core::ResourceScope::new(vec![capability_core::Resource::Window(
                "w1".to_string(),
            )]),
            policy_core::GrantLifetime::Persistent,
            None,
            None,
        );
        let provider = QueueProvider::new(vec![observe_turn, invoke_turn, text_turn("pressed")]);
        let harness = harness_with(provider, vec![grant, observe_grant]);

        // With the stub backend the model never sees desktop tools; with a
        // backend installed they join the turn like any other tool source.
        harness
            .runtime
            .set_desktop_plugin(tool_desktop::plugin::DesktopPlugin::new(
                tool_desktop::plugin::DesktopPluginManifest {
                    id: "desktop.test-fake".to_string(),
                    name: "test fake".to_string(),
                    version: "0.1.0".to_string(),
                    platforms: vec![std::env::consts::OS.to_string()],
                    capabilities: tool_desktop::plugin::FULL_CAPABILITIES.to_vec(),
                    description: "test".to_string(),
                },
                Arc::new(FakeDesktop),
            ));
        harness
            .runtime
            .send_message("press save".to_string())
            .unwrap();
        let done = wait_for(&harness, "agent.turn_done");
        let executed = done.data["executed"].as_array().unwrap();
        assert_eq!(executed.len(), 2, "{executed:?}");
        assert_eq!(executed[0]["name"], "desktop.accessibility_tree");
        assert_eq!(executed[1]["name"], "desktop.invoke_element");
        assert_eq!(executed[1]["output"]["ok"], true);
    }

    #[test]
    fn deny_resumes_with_refusal_note() {
        let (_dir, path) = temp_project("deny");
        let provider = QueueProvider::new(vec![read_turn(&path), text_turn("understood")]);
        let harness = harness(provider);

        harness
            .runtime
            .send_message("read my notes".to_string())
            .unwrap();
        let suspended = wait_for(&harness, "agent.turn_suspended");
        let request_id = suspended.data["request_id"].as_str().unwrap().to_string();

        let pending = harness.approvals.lock().unwrap().list();
        assert_eq!(pending.len(), 1);
        harness
            .approvals
            .lock()
            .unwrap()
            .decide(&pending[0].id, None)
            .unwrap();
        harness.runtime.notify_decided(&request_id, false);

        // The model pivots on the refusal note instead of executing.
        let done = wait_for(&harness, "agent.turn_done");
        assert_eq!(done.data["text"], "understood");
        assert_eq!(done.data["executed"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn settings_mcp_servers_join_the_turn() {
        use mcp_runtime::{McpServerConfig, McpTransport, TrustLevel};
        use std::collections::HashMap;

        // The shared fake MCP server (same script the mcp-runtime
        // integration tests use).
        let script_source = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../mcp-runtime/tests/fake_server.py"
        ));
        let dir =
            std::env::temp_dir().join(format!("utsuwa-runtime-mcp-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake_server.py");
        std::fs::write(&script, script_source).unwrap();

        let db = dir.join("state.db");
        let storage = Arc::new(Mutex::new(storage_core::Storage::open(&db).unwrap()));
        let config = McpServerConfig {
            id: capability_core::ServerId::new("fake"),
            transport: McpTransport::Stdio {
                command: "python3".to_string(),
                args: vec![script.to_string_lossy().to_string()],
                env_allowlist: vec!["PATH".to_string()],
                extra_env: HashMap::new(),
            },
            enabled: true,
            trust: TrustLevel::Untrusted,
        };
        storage
            .lock()
            .unwrap()
            .set_setting(
                crate::runtime::SETTING_MCP_SERVERS,
                &serde_json::to_value(vec![config]).unwrap(),
            )
            .unwrap();

        let harness = {
            let approvals = Arc::new(Mutex::new(ApprovalQueue::new()));
            let events = Arc::new(Mutex::new(Vec::new()));
            let sink = events.clone();
            let provider = QueueProvider::new(vec![text_turn("mcp turn done")]);
            let runtime = AgentRuntime::start_with_factory(
                Arc::clone(&approvals),
                Some(Arc::clone(&storage)),
                None,
                Arc::new(move |event| {
                    sink.lock().unwrap().push(event);
                }),
                Arc::new(move || Ok(Arc::clone(&provider) as Arc<dyn ModelProvider>)),
            )
            .unwrap();
            Harness {
                runtime,
                approvals,
                events,
            }
        };

        harness.runtime.send_message("hi".to_string()).unwrap();
        let done = wait_for(&harness, "agent.turn_done");
        assert_eq!(done.data["text"], "mcp turn done");

        // The settings-driven server connected and registered its tools.
        let status = harness
            .runtime
            .executor_handle()
            .block_on(harness.runtime.mcp_status());
        assert_eq!(status.len(), 1);
        assert!(status[0].connected);
        assert_eq!(status[0].tools, 3);
    }

    #[test]
    fn mcp_tool_call_needs_approval_without_grant() {
        use mcp_runtime::{McpServerConfig, McpTransport, TrustLevel};
        use std::collections::HashMap;

        let script_source = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../mcp-runtime/tests/fake_server.py"
        ));
        let dir = std::env::temp_dir().join(format!(
            "utsuwa-runtime-mcp-approval-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake_server.py");
        std::fs::write(&script, script_source).unwrap();

        let approvals = Arc::new(Mutex::new(ApprovalQueue::new()));
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let provider = QueueProvider::new(vec![vec![
            ModelStreamEvent::ToolCall(ToolCall {
                id: "m1".to_string(),
                name: "mcp.fake.echo".to_string(),
                arguments: serde_json::json!({"message": "via-mcp"}).to_string(),
            }),
            ModelStreamEvent::Done {
                finish_reason: FinishReason::ToolCalls,
            },
        ]]);
        let runtime = AgentRuntime::start_with_factory(
            Arc::clone(&approvals),
            None,
            None,
            Arc::new(move |event| {
                sink.lock().unwrap().push(event);
            }),
            Arc::new(move || Ok(Arc::clone(&provider) as Arc<dyn ModelProvider>)),
        )
        .unwrap();
        let harness = Harness {
            runtime,
            approvals,
            events,
        };

        // Configure the server directly (no settings involved here).
        let config = McpServerConfig {
            id: capability_core::ServerId::new("fake"),
            transport: McpTransport::Stdio {
                command: "python3".to_string(),
                args: vec![script.to_string_lossy().to_string()],
                env_allowlist: vec!["PATH".to_string()],
                extra_env: HashMap::new(),
            },
            enabled: true,
            trust: TrustLevel::Untrusted,
        };
        harness
            .runtime
            .executor_handle()
            .block_on(harness.runtime.mcp_manager().configure(config))
            .unwrap();

        harness
            .runtime
            .send_message("use the mcp tool".to_string())
            .unwrap();
        let requested = wait_for(&harness, "permission.requested");
        assert_eq!(requested.data["capability"], "McpInvoke");
        assert_eq!(
            requested.data["resource"],
            serde_json::json!({"McpTool": {"server": "fake", "tool": "echo"}})
        );
        // Nothing executed: the turn suspended for the human.
        assert_eq!(harness.approvals.lock().unwrap().list().len(), 1);
        assert!(harness
            .events
            .lock()
            .unwrap()
            .iter()
            .all(|e| e.event != "agent.turn_done"));
    }

    #[test]
    fn legacy_api_key_migrates_to_the_secret_store() {
        use secret_core::{SecretStore, ACCOUNT_MODEL_API_KEY};

        let dir =
            std::env::temp_dir().join(format!("utsuwa-runtime-secrets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let storage = Arc::new(Mutex::new(
            storage_core::Storage::open(&dir.join("state.db")).unwrap(),
        ));
        {
            let store = storage.lock().unwrap();
            store
                .set_setting(
                    SETTING_BASE_URL,
                    &serde_json::json!("http://localhost:11434/v1"),
                )
                .unwrap();
            store
                .set_setting(SETTING_MODEL_NAME, &serde_json::json!("m"))
                .unwrap();
            store
                .set_setting(SETTING_API_KEY, &serde_json::json!("sk-legacy"))
                .unwrap();
        }
        let secrets: Arc<dyn SecretStore> = Arc::new(secret_core::MemoryStore::default());
        let factory =
            provider_factory_with_secrets(Some(Arc::clone(&storage)), Arc::clone(&secrets));

        // First build migrates the plaintext row into the secret store.
        factory().unwrap();
        assert_eq!(
            secrets.get(ACCOUNT_MODEL_API_KEY).unwrap(),
            Some("sk-legacy".to_string())
        );
        assert_eq!(
            storage
                .lock()
                .unwrap()
                .get_setting(SETTING_API_KEY)
                .unwrap(),
            None
        );

        // Second build reads from the secret store, not settings.
        factory().unwrap();
        assert_eq!(
            secrets.get(ACCOUNT_MODEL_API_KEY).unwrap(),
            Some("sk-legacy".to_string())
        );
    }

    #[test]
    fn provider_builds_without_any_key() {
        let dir = std::env::temp_dir().join(format!("utsuwa-runtime-nokey-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let storage = Arc::new(Mutex::new(
            storage_core::Storage::open(&dir.join("state.db")).unwrap(),
        ));
        {
            let store = storage.lock().unwrap();
            store
                .set_setting(
                    SETTING_BASE_URL,
                    &serde_json::json!("http://localhost:11434/v1"),
                )
                .unwrap();
            store
                .set_setting(SETTING_MODEL_NAME, &serde_json::json!("m"))
                .unwrap();
        }
        let secrets: Arc<dyn secret_core::SecretStore> =
            Arc::new(secret_core::MemoryStore::default());
        let factory = provider_factory_with_secrets(Some(storage), secrets);
        // Ollama-style keyless providers still construct.
        factory().unwrap();
    }

    #[test]
    fn kilo_provider_builds_without_an_api_key() {
        let dir =
            std::env::temp_dir().join(format!("utsuwa-runtime-kilo-nokey-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let storage = Arc::new(Mutex::new(
            storage_core::Storage::open(&dir.join("state.db")).unwrap(),
        ));
        {
            let store = storage.lock().unwrap();
            store
                .set_setting(SETTING_PROVIDER, &serde_json::json!("kilo"))
                .unwrap();
            store
                .set_setting(
                    SETTING_BASE_URL,
                    &serde_json::json!("https://api.kilo.ai/api/gateway"),
                )
                .unwrap();
            store
                .set_setting(SETTING_MODEL_NAME, &serde_json::json!("kilo-auto/free"))
                .unwrap();
        }
        let secrets: Arc<dyn secret_core::SecretStore> =
            Arc::new(secret_core::MemoryStore::default());
        let factory = provider_factory_with_secrets(Some(storage), secrets);

        // Kilo is routed through the generic OpenAI-compatible factory.
        factory().unwrap();
    }

    #[test]
    fn cloud_provider_without_key_is_not_configured() {
        let dir =
            std::env::temp_dir().join(format!("utsuwa-runtime-cloud-nokey-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let storage = Arc::new(Mutex::new(
            storage_core::Storage::open(&dir.join("state.db")).unwrap(),
        ));
        {
            let store = storage.lock().unwrap();
            store
                .set_setting(SETTING_PROVIDER, &serde_json::json!("openai"))
                .unwrap();
            store
                .set_setting(
                    SETTING_BASE_URL,
                    &serde_json::json!("https://api.openai.com/v1"),
                )
                .unwrap();
            store
                .set_setting(SETTING_MODEL_NAME, &serde_json::json!("gpt-test"))
                .unwrap();
        }
        let secrets: Arc<dyn secret_core::SecretStore> =
            Arc::new(secret_core::MemoryStore::default());
        let factory = provider_factory_with_secrets(Some(storage), secrets);
        assert!(matches!(factory(), Err(RuntimeError::ModelNotConfigured)));
    }

    #[test]
    fn provider_factory_resolves_vision_from_stored_settings() {
        fn factory_for(settings: &[(&str, serde_json::Value)]) -> model_core::ModelCapabilities {
            let dir = std::env::temp_dir().join(format!(
                "utsuwa-runtime-vision-{}-{}",
                std::process::id(),
                settings.len()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            let storage = Arc::new(Mutex::new(
                storage_core::Storage::open(&dir.join("state.db")).unwrap(),
            ));
            {
                let store = storage.lock().unwrap();
                for (key, value) in settings {
                    store.set_setting(key, value).unwrap();
                }
            }
            let secrets: Arc<dyn secret_core::SecretStore> =
                Arc::new(secret_core::MemoryStore::default());
            let factory = provider_factory_with_secrets(Some(storage), secrets);
            factory().unwrap().capabilities()
        }
        let local = |model: &str| {
            vec![
                (SETTING_PROVIDER, serde_json::json!("ollama")),
                (
                    SETTING_BASE_URL,
                    serde_json::json!("http://localhost:11434/v1"),
                ),
                (SETTING_MODEL_NAME, serde_json::json!(model)),
            ]
        };
        // No override: the provider/model-name heuristic decides.
        assert!(factory_for(&local("llava:13b")).image_tool_results);
        assert!(!factory_for(&local("llama3.1:8b")).image_tool_results);
        // Explicit stored override wins over the heuristic either way.
        let mut forced = local("llama3.1:8b");
        forced.push((SETTING_MODEL_VISION, serde_json::json!(true)));
        assert!(factory_for(&forced).image_tool_results);
        let mut denied = local("llava:13b");
        denied.push((SETTING_MODEL_VISION, serde_json::json!(false)));
        assert!(!factory_for(&denied).image_tool_results);
    }

    #[test]
    fn cancel_suppresses_late_worker_events() {
        struct HangingProvider;
        #[async_trait::async_trait]
        impl ModelProvider for HangingProvider {
            async fn stream(
                &self,
                _request: ModelRequest,
            ) -> Result<model_core::ModelStream, ModelError> {
                Ok(Box::pin(futures_util::stream::pending()))
            }
        }
        let approvals = Arc::new(Mutex::new(ApprovalQueue::new()));
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let runtime = AgentRuntime::start_with_factory(
            approvals,
            None,
            None,
            Arc::new(move |event| {
                sink.lock().unwrap().push(event);
            }),
            Arc::new(|| Ok(Arc::new(HangingProvider) as Arc<dyn ModelProvider>)),
        )
        .unwrap();

        runtime.send_message("hang".to_string()).unwrap();
        // Let the worker reach the hanging stream before cancelling.
        std::thread::sleep(Duration::from_millis(200));
        runtime.cancel();

        let start = Instant::now();
        let cancelled = loop {
            if let Some(found) = events
                .lock()
                .unwrap()
                .iter()
                .find(|e| e.event == "agent.turn_cancelled")
                .cloned()
            {
                break found;
            }
            assert!(start.elapsed() < Duration::from_secs(5), "no cancel event");
            std::thread::sleep(Duration::from_millis(10));
        };
        let _ = cancelled;
        // The aborted worker must never resolve the turn afterwards.
        std::thread::sleep(Duration::from_millis(300));
        assert!(events
            .lock()
            .unwrap()
            .iter()
            .all(|e| e.event != "agent.turn_done"));
    }

    #[test]
    fn status_and_catalog_paths_are_safe_inside_the_agent_executor() {
        // Regression test for the `Cannot start a runtime from within a
        // runtime` panic: every status/catalog path must be awaitable from
        // a task running on the agent executor itself, with no nested
        // `block_on` or runtime construction.
        let provider = QueueProvider::new(vec![text_turn("ok")]);
        let harness = harness(provider);
        let runtime = Arc::clone(&harness.runtime);
        // Hermetic: real portal/X11/AT-SPI backends may block on session
        // infrastructure, so pin the stub backend for this test. The goal
        // is executor safety, not backend I/O.
        runtime.set_desktop_plugin(tool_desktop::plugin::DesktopPlugin::stub());
        runtime.executor_handle().block_on(async {
            // MCP status (async end-to-end, no blocking wrapper).
            let _ = runtime.mcp_status().await;
            // Full per-turn catalog composition + snapshot.
            let catalog = runtime.tool_catalog(&HostEnvironment::snapshot()).await;
            catalog
                .snapshot(&ToolLoadContext::new(ToolProfile::Full))
                .await
                .expect("catalog snapshot inside executor must succeed");
            // Desktop/backend status snapshots (async variants).
            let _ = runtime.screen_share_status_async().await;
            // Sensor privacy snapshots (host-owned, no executor needed).
            let _ = runtime.camera_activity_status();
            let _ = runtime.microphone_activity_status();
            // Spawning further async work from inside is allowed.
            runtime
                .executor_handle()
                .spawn(async move {})
                .await
                .expect("spawn inside executor must succeed");
        });
        // The synchronous IPC status path still works from a plain thread.
        let status = runtime.screen_share_status();
        let _ = status.available;
    }

    #[test]
    fn sync_host_ops_fail_closed_inside_the_agent_executor() {
        // Synchronous bridges must return `runtime_unavailable` instead of
        // panicking when (mis)used from a Tokio worker.
        let provider = QueueProvider::new(vec![text_turn("ok")]);
        let harness = harness(provider);
        let runtime = Arc::clone(&harness.runtime);
        runtime.set_desktop_plugin(tool_desktop::plugin::DesktopPlugin::stub());
        let result = runtime.executor_handle().block_on(async {
            // block_on_host is private; exercise it indirectly through the
            // sync status guard: inside the executor the sync snapshot must
            // not panic (it returns empty listings, see async variant for
            // full data).
            let status = runtime.screen_share_status();
            let _ = status.available;
            // Direct check: entering the executor synchronously is refused.
            tokio::runtime::Handle::try_current().is_ok()
        });
        assert!(result, "test must run inside the agent executor");
    }

    /// Faithful stand-in for the Linux keyring 4 / zbus blocking backend:
    /// entering a nested runtime panics with `Cannot start a runtime from
    /// within a runtime` when (incorrectly) invoked on a Tokio async worker,
    /// and succeeds on a blocking-pool thread.
    struct NestedRuntimeSecretStore {
        inner: secret_core::MemoryStore,
    }

    impl secret_core::SecretStore for NestedRuntimeSecretStore {
        fn get(&self, account: &str) -> Result<Option<String>, secret_core::SecretError> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("nested runtime builds off async workers");
            runtime.block_on(async { self.inner.get(account) })
        }

        fn set(&self, account: &str, secret: &str) -> Result<(), secret_core::SecretError> {
            self.inner.set(account, secret)
        }

        fn delete(&self, account: &str) -> Result<(), secret_core::SecretError> {
            self.inner.delete(account)
        }
    }

    #[test]
    fn secret_store_reads_run_off_the_async_worker() {
        // Regression test for the turn that died before provider init: the
        // provider factory reads the API key through the synchronous secret
        // store, and on Linux that call panics when made on an async worker.
        // The factory must therefore run on the blocking pool: the turn
        // completes instead of losing its worker with no terminal event.
        let provider = QueueProvider::new(vec![text_turn("key read off-worker")]);
        let secrets: Arc<dyn secret_core::SecretStore> = Arc::new(NestedRuntimeSecretStore {
            inner: secret_core::MemoryStore::default(),
        });
        let approvals = Arc::new(Mutex::new(ApprovalQueue::new()));
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let runtime = AgentRuntime::start_with_factory(
            Arc::clone(&approvals),
            None,
            None,
            Arc::new(move |event| {
                sink.lock().unwrap().push(event);
            }),
            Arc::new(move || {
                // What `resolve_api_key` does with the keychain on every turn.
                let _ = secrets.get(secret_core::ACCOUNT_MODEL_API_KEY);
                Ok(Arc::clone(&provider) as Arc<dyn ModelProvider>)
            }),
        )
        .unwrap();
        let harness = Harness {
            runtime,
            approvals,
            events,
        };
        harness.runtime.send_message("hi".to_string()).unwrap();
        let done = wait_for(&harness, "agent.turn_done");
        assert_eq!(done.data["text"], "key read off-worker");
        assert!(harness.runtime.lock_state().unwrap().running.is_none());
    }

    #[test]
    fn panicking_provider_factory_emits_exactly_one_turn_failed() {
        let approvals = Arc::new(Mutex::new(ApprovalQueue::new()));
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let runtime = AgentRuntime::start_with_factory(
            Arc::clone(&approvals),
            None,
            None,
            Arc::new(move |event| {
                sink.lock().unwrap().push(event);
            }),
            Arc::new(|| -> Result<Arc<dyn ModelProvider>, RuntimeError> {
                panic!("boom in provider factory")
            }),
        )
        .unwrap();
        let harness = Harness {
            runtime,
            approvals,
            events,
        };
        harness.runtime.send_message("hi".to_string()).unwrap();
        let failed = wait_for(&harness, "agent.turn_failed");
        assert!(
            failed.data["error"]
                .as_str()
                .unwrap()
                .contains("provider task failed"),
            "unexpected error: {}",
            failed.data["error"]
        );
        // Exactly one terminal event: no duplicate, no done/suspended/cancelled.
        std::thread::sleep(Duration::from_millis(300));
        {
            let events = harness.events.lock().unwrap();
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.event == "agent.turn_failed")
                    .count(),
                1
            );
            assert!(events.iter().all(|event| !matches!(
                event.event.as_str(),
                "agent.turn_done" | "agent.turn_suspended" | "agent.turn_cancelled"
            )));
        }
        assert!(harness.runtime.lock_state().unwrap().running.is_none());
    }

    struct PanicProvider;

    #[async_trait::async_trait]
    impl ModelProvider for PanicProvider {
        async fn stream(
            &self,
            _request: ModelRequest,
        ) -> Result<model_core::ModelStream, ModelError> {
            panic!("boom in provider stream")
        }
    }

    #[test]
    fn panicking_provider_stream_emits_exactly_one_turn_failed() {
        // A panic anywhere else in the turn (model, tool, wiring) is caught
        // by panic supervision and still resolves the turn — the UI must
        // never spin forever.
        let approvals = Arc::new(Mutex::new(ApprovalQueue::new()));
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let runtime = AgentRuntime::start_with_factory(
            Arc::clone(&approvals),
            None,
            None,
            Arc::new(move |event| {
                sink.lock().unwrap().push(event);
            }),
            Arc::new(|| Ok(Arc::new(PanicProvider) as Arc<dyn ModelProvider>)),
        )
        .unwrap();
        let harness = Harness {
            runtime,
            approvals,
            events,
        };
        harness.runtime.send_message("hi".to_string()).unwrap();
        let failed = wait_for(&harness, "agent.turn_failed");
        assert!(
            failed.data["error"]
                .as_str()
                .unwrap()
                .contains("agent worker panicked"),
            "unexpected error: {}",
            failed.data["error"]
        );
        std::thread::sleep(Duration::from_millis(300));
        {
            let events = harness.events.lock().unwrap();
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.event == "agent.turn_failed")
                    .count(),
                1
            );
            assert!(events.iter().all(|event| !matches!(
                event.event.as_str(),
                "agent.turn_done" | "agent.turn_suspended" | "agent.turn_cancelled"
            )));
        }
        assert!(harness.runtime.lock_state().unwrap().running.is_none());
    }

    #[test]
    fn completed_turn_clears_running_and_makes_cancel_silent() {
        let provider = QueueProvider::new(vec![text_turn("done")]);
        let finished = harness(provider);
        finished.runtime.send_message("hi".to_string()).unwrap();
        wait_for(&finished, "agent.turn_done");
        // No stale handle survives a normal completed turn.
        assert!(finished.runtime.lock_state().unwrap().running.is_none());
        // Cancelling an idle runtime reports nothing: no worker, no event.
        finished.runtime.cancel();
        std::thread::sleep(Duration::from_millis(300));
        assert!(finished
            .events
            .lock()
            .unwrap()
            .iter()
            .all(|event| event.event != "agent.turn_cancelled"));

        // Same for a runtime that never ran a turn at all. (Only agent
        // events are asserted: sensor activity is process-global, so a
        // parallel test's microphone/camera session can land here too.)
        let provider = QueueProvider::new(vec![text_turn("unused")]);
        let idle = harness(provider);
        idle.runtime.cancel();
        std::thread::sleep(Duration::from_millis(100));
        assert!(idle
            .events
            .lock()
            .unwrap()
            .iter()
            .all(|event| !event.event.starts_with("agent.")));
    }

    #[test]
    fn suspended_turn_clears_running_and_still_resumes() {
        let (dir, path) = temp_project("suspend-running");
        let provider = QueueProvider::new(vec![
            read_turn(&path),
            read_turn(&path),
            text_turn("done reading"),
        ]);
        let harness = harness(provider);
        harness
            .runtime
            .send_message("read my notes".to_string())
            .unwrap();
        let suspended = wait_for(&harness, "agent.turn_suspended");
        let request_id = suspended.data["request_id"].as_str().unwrap().to_string();
        // A parked turn is not a running turn.
        assert!(harness.runtime.lock_state().unwrap().running.is_none());

        let pending = harness.approvals.lock().unwrap().list();
        assert_eq!(pending.len(), 1);
        harness
            .approvals
            .lock()
            .unwrap()
            .decide(&pending[0].id, Some(policy_core::GrantLifetime::Session))
            .unwrap();
        harness.runtime.notify_decided(&request_id, true);
        let done = wait_for(&harness, "agent.turn_done");
        assert_eq!(done.data["text"], "done reading");
        assert!(harness.runtime.lock_state().unwrap().running.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cancel_while_suspended_still_reports() {
        // Suspension counts as active work (unlike an idle runtime), so
        // cancelling it still emits `agent.turn_cancelled`.
        let (dir, path) = temp_project("cancel-suspended");
        let provider = QueueProvider::new(vec![read_turn(&path)]);
        let harness = harness(provider);
        harness
            .runtime
            .send_message("read my notes".to_string())
            .unwrap();
        wait_for(&harness, "agent.turn_suspended");
        harness.runtime.cancel();
        wait_for(&harness, "agent.turn_cancelled");
        std::fs::remove_dir_all(&dir).ok();
    }
}
