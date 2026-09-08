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
//! `agent.turn_failed`, `agent.turn_cancelled`, `permission.requested`.

use agent_core::{Agent, AgentLimits};
use audit_core::AuditSink;
use capability_core::AgentId;
use ipc_core::HostEvent;
use mcp_runtime::{McpManager, McpServerConfig};
use model_core::{ModelMessage, ModelProvider};
use model_openai_compatible::OpenAICompatibleClient;
use policy_core::ApprovalQueue;
use std::sync::{Arc, Mutex};
use storage_core::Storage;
use tool_core::ToolRegistry;
use tool_process::{ProcessLimits, ProcessManager};
use tracing::Instrument as _;

/// Setting keys the provider factory reads. Values are JSON strings.
pub const SETTING_BASE_URL: &str = "model.base_url";
pub const SETTING_API_KEY: &str = "model.api_key";
pub const SETTING_MODEL_NAME: &str = "model.name";
/// Settings key holding the MCP server set (JSON array of server configs).
pub const SETTING_MCP_SERVERS: &str = "mcp.servers";
/// Settings key holding the WASM plugin directory (JSON string path).
/// Discovered every turn so installs take effect without a restart;
/// only `Enabled` plugins register tools, and every call stays behind
/// policy + tickets.
pub const SETTING_PLUGIN_DIR: &str = "plugin.dir";

/// Transcript cap: oldest messages are dropped past this bound so a long
/// session cannot grow memory (or model context) without limit.
const MAX_TRANSCRIPT_MESSAGES: usize = 100;

/// Callback the runtime uses to reach the frontend (reply queue + wake).
pub type EmitFn = Arc<dyn Fn(HostEvent) + Send + Sync>;

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

/// A turn paused for the user, kept until the request is resolved.
struct Suspended {
    transcript: Vec<model_core::ModelMessage>,
    request_id: String,
}

struct State {
    generation: u64,
    transcript: Vec<ModelMessage>,
    suspended: Option<Suspended>,
    running: Option<tokio::task::JoinHandle<()>>,
}

/// Host-owned agent loop. Construct with [`AgentRuntime::start`], which
/// returns an `Arc` because worker tasks and the dispatcher share it.
pub struct AgentRuntime {
    agent_id: AgentId,
    approvals: Arc<Mutex<ApprovalQueue>>,
    audit: Option<Arc<dyn AuditSink>>,
    emit: EmitFn,
    provider_factory:
        Arc<dyn Fn() -> Result<Arc<dyn ModelProvider>, RuntimeError> + Send + Sync>,
    processes: Arc<ProcessManager>,
    mcp: Arc<McpManager>,
    plugins: Arc<plugin_wasm::PluginRuntime>,
    memory: Mutex<Arc<memory::MemoryStore>>,
    desktop: Mutex<Arc<dyn tool_desktop::DesktopBackend>>,
    storage: Option<Arc<Mutex<Storage>>>,
    state: Mutex<State>,
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
        Self::start_with_factory(
            approvals,
            storage.clone(),
            audit,
            emit,
            settings_provider_factory(storage),
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
        let executor = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("utsuwa-agent")
            .enable_all()
            .build()
            .map_err(|e| RuntimeError::Executor(e.to_string()))?;
        Ok(Arc::new(Self {
            agent_id: AgentId::new(uuid::Uuid::new_v4().to_string()),
            approvals,
            audit,
            emit,
            provider_factory,
            processes: ProcessManager::new(ProcessLimits::default()),
            mcp: Arc::new(McpManager::new()),
            plugins: Arc::new(
                plugin_wasm::PluginRuntime::new().map_err(|e| RuntimeError::Tools(e.to_string()))?,
            ),
            memory: Mutex::new(Arc::new(
                memory::MemoryStore::open_in_memory()
                    .map_err(|e| RuntimeError::Tools(e.to_string()))?,
            )),
            desktop: Mutex::new(tool_desktop::backend()),
            storage,
            state: Mutex::new(State {
                generation: 0,
                transcript: Vec::new(),
                suspended: None,
                running: None,
            }),
            executor,
        }))
    }

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, State>, RuntimeError> {
        self.state
            .lock()
            .map_err(|_| RuntimeError::Executor("runtime state lock failed".to_string()))
    }

    fn is_current(&self, generation: u64) -> bool {
        self.lock_state()
            .map(|state| state.generation == generation)
            .unwrap_or(false)
    }

    fn emit_if_current(&self, generation: u64, event: &str, data: serde_json::Value) {
        if self.is_current(generation) {
            (self.emit)(HostEvent {
                event: event.to_string(),
                data,
            });
        }
    }

    /// The MCP server manager: configure servers here (or via the
    /// `mcp.servers` settings key, which syncs every turn) and their
    /// tools join the next turn's registry through host policy.
    pub fn mcp_manager(&self) -> &Arc<McpManager> {
        &self.mcp
    }

    /// Synchronous MCP status snapshot (blocks on the worker executor).
    pub fn mcp_status_blocking(&self) -> Vec<mcp_runtime::McpServerStatus> {
        self.executor.block_on(self.mcp.status())
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
        self.memory.lock().map(|s| Arc::clone(&s)).unwrap_or_else(|_| {
            Arc::new(
                memory::MemoryStore::open_in_memory()
                    .expect("in-memory memory store always opens"),
            )
        })
    }

    /// Install the desktop backend (boot only, or tests with a fake).
    /// The turn registers `desktop.*` tools only while the backend
    /// reports availability — with the stub backend the model never
    /// sees actions that cannot run.
    pub fn set_desktop_backend(&self, backend: Arc<dyn tool_desktop::DesktopBackend>) {
        if let Ok(mut slot) = self.desktop.lock() {
            *slot = backend;
        }
    }

    /// Sync the MCP server set from settings and register every enabled
    /// server's tools into the turn registry. Best-effort per server: a
    /// down server logs and skips, never fails the turn.
    async fn attach_mcp_tools(&self, registry: &mut ToolRegistry) {
        if let Some(storage) = &self.storage {
            let configs: Option<Vec<McpServerConfig>> = storage
                .lock()
                .ok()
                .and_then(|store| store.get_setting(SETTING_MCP_SERVERS).ok())
                .flatten()
                .and_then(|value| {
                    serde_json::from_value(value).map_err(|e| {
                        tracing::warn!(%e, "mcp.servers setting is not a server array; ignoring");
                    }).ok()
                });
            if let Some(configs) = configs {
                if let Err(e) = self.mcp.sync_configs(configs).await {
                    tracing::warn!(%e, "mcp settings sync failed");
                }
            }
        }
        for id in self.mcp.server_ids().await {
            if let Err(e) = self.mcp.register_into(&id, registry).await {
                tracing::warn!(server = %id, error = %e, "mcp server unavailable this turn");
            }
        }
    }

    /// Discover the configured plugin directory and register every
    /// enabled plugin's tools into the turn registry. Best-effort like
    /// MCP: a broken plugin logs and skips, never fails the turn.
    /// Enabling (user action via `plugin.enable`) only loads code —
    /// calls still need a policy ticket per invocation.
    fn attach_plugin_tools(&self, registry: &mut ToolRegistry) {
        if let Some(storage) = &self.storage {
            let dir: Option<String> = storage
                .lock()
                .ok()
                .and_then(|store| store.get_setting(SETTING_PLUGIN_DIR).ok())
                .flatten()
                .and_then(|value| {
                    serde_json::from_value(value)
                        .map_err(|e| {
                            tracing::warn!(%e, "plugin.dir setting is not a path string; ignoring");
                        })
                        .ok()
                });
            if let Some(dir) = dir {
                if let Err(e) = self.plugins.discover_dir(std::path::Path::new(&dir)) {
                    tracing::warn!(dir = %dir, error = %e, "plugin discovery failed");
                }
            }
        }
        match self.plugins.register_enabled(registry) {
            Ok(added) => {
                if !added.is_empty() {
                    tracing::debug!(tools = ?added, "plugin tools registered for turn");
                }
            }
            Err(e) => tracing::warn!(error = %e, "plugin registration failed"),
        }
    }

    /// Register the memory tools from the current store. Best-effort:
    /// a poisoned slot logs and skips, never fails the turn.
    fn attach_memory_tools(&self, registry: &mut ToolRegistry) {
        let store = match self.memory.lock() {
            Ok(store) => Arc::clone(&store),
            Err(_) => {
                tracing::warn!("memory store lock failed; skipping memory tools this turn");
                return;
            }
        };
        for tool in [
            Arc::new(memory::tools::RememberTool { store: store.clone() })
                as Arc<dyn tool_core::Tool>,
            Arc::new(memory::tools::RecallTool { store: store.clone() }),
            Arc::new(memory::tools::ForgetTool { store }),
        ] {
            if let Err(e) = registry.register(tool) {
                tracing::warn!(error = %e, "memory tool registration failed");
            }
        }
    }

    /// Register `desktop.*` tools while a real backend is present. With
    /// the stub backend nothing registers: every desktop call stays
    /// behind policy + tickets, and unavailable actions stay invisible.
    fn attach_desktop_tools(&self, registry: &mut ToolRegistry) {
        let backend = match self.desktop.lock() {
            Ok(backend) => Arc::clone(&backend),
            Err(_) => {
                tracing::warn!("desktop backend lock failed; skipping desktop tools this turn");
                return;
            }
        };
        if !backend.is_available() {
            return;
        }
        use tool_desktop::tools::*;
        for tool in [
            Arc::new(ListWindowsTool { backend: backend.clone() }) as Arc<dyn tool_core::Tool>,
            Arc::new(AccessibilityTreeTool { backend: backend.clone() }),
            Arc::new(InvokeElementTool { backend: backend.clone() }),
            Arc::new(SetValueTool { backend: backend.clone() }),
            Arc::new(ScreenshotTool { backend: backend.clone() }),
            Arc::new(ClickTool { backend: backend.clone() }),
            Arc::new(TypeTextTool { backend }),
        ] {
            if let Err(e) = registry.register(tool) {
                tracing::warn!(error = %e, "desktop tool registration failed");
            }
        }
    }

    fn build_agent(&self) -> Result<Agent, RuntimeError> {
        let provider = (self.provider_factory)()?;
        let mut agent = Agent::new(provider)
            .with_agent_id(self.agent_id.clone())
            .with_limits(AgentLimits::default());
        if let Some(sink) = &self.audit {
            agent = agent.with_audit_sink(sink.clone());
        }
        Ok(agent)
    }

    /// Queue a user message and run the turn in the background. Supersedes
    /// any in-flight or suspended turn (their late events are dropped).
    pub fn send_message(self: &Arc<Self>, text: String) -> Result<(), RuntimeError> {
        let transcript = {
            let mut state = self.lock_state()?;
            state.generation += 1;
            if let Some(handle) = state.running.take() {
                handle.abort();
            }
            state.suspended = None;
            state.transcript.push(ModelMessage::user(text));
            while state.transcript.len() > MAX_TRANSCRIPT_MESSAGES {
                state.transcript.remove(0);
            }
            state.transcript.clone()
        };
        let generation = self.lock_state()?.generation;
        self.spawn_turn(transcript, generation, None)
    }

    /// Resume a suspended turn after its permission request was resolved.
    /// Approving re-runs the turn so the call executes under the new
    /// grant; denying notes the refusal so the model works around it.
    /// Resolving an unknown or already-superseded request is a no-op.
    pub fn notify_decided(self: &Arc<Self>, request_id: &str, approved: bool) {
        let (transcript, generation) = match self.lock_state() {
            Ok(mut state) => {
                let Some(suspended) = state.suspended.take() else {
                    return;
                };
                if suspended.request_id != request_id {
                    state.suspended = Some(suspended);
                    return;
                }
                state.generation += 1;
                if let Some(handle) = state.running.take() {
                    handle.abort();
                }
                state.transcript = suspended.transcript.clone();
                (suspended.transcript, state.generation)
            }
            Err(_) => return,
        };
        let note = if approved {
            format!(
                "The user approved permission request {request_id}. Continue the task; \
                 re-issue the tool call if it is still needed."
            )
        } else {
            format!(
                "The user denied permission request {request_id}. Do not retry that \
                 exact call; work around it or explain what you need."
            )
        };
        // Resume failures are terminal for this turn: the failure event
        // already tells the frontend what happened.
        let _ = self.spawn_turn(transcript, generation, Some(note));
    }

    /// Abort the in-flight turn and drop any suspended one. Late worker
    /// events are suppressed by the generation bump.
    pub fn cancel(self: &Arc<Self>) {
        let generation = match self.lock_state() {
            Ok(mut state) => {
                state.generation += 1;
                if let Some(handle) = state.running.take() {
                    handle.abort();
                }
                state.suspended = None;
                state.generation
            }
            Err(_) => return,
        };
        // The cancelling generation is current by construction.
        (self.emit)(HostEvent {
            event: "agent.turn_cancelled".to_string(),
            data: serde_json::json!({}),
        });
        let _ = generation;
    }

    fn spawn_turn(
        self: &Arc<Self>,
        transcript: Vec<ModelMessage>,
        generation: u64,
        resume_note: Option<String>,
    ) -> Result<(), RuntimeError> {
        let this = Arc::clone(self);
        let executor_handle = this.executor.handle().clone();
        let handle = executor_handle.spawn(async move {
            this.run_turn(transcript, generation, resume_note).await;
        });
        self.lock_state()?.running = Some(handle);
        Ok(())
    }

    async fn run_turn(
        &self,
        transcript: Vec<ModelMessage>,
        generation: u64,
        resume_note: Option<String>,
    ) {
        let span = tracing::info_span!("host.turn", generation, resumed = resume_note.is_some());
        self.run_turn_inner(transcript, generation, resume_note)
            .instrument(span)
            .await
    }

    async fn run_turn_inner(
        &self,
        mut transcript: Vec<ModelMessage>,
        generation: u64,
        resume_note: Option<String>,
    ) {
        if let Some(note) = resume_note {
            transcript.push(ModelMessage::user(note));
        }
        let agent = match self.build_agent() {
            Ok(agent) => agent,
            Err(err) => {
                self.emit_if_current(
                    generation,
                    "agent.turn_failed",
                    serde_json::json!({ "error": err.to_string() }),
                );
                return;
            }
        };
        let mut registry = match default_registry(&self.processes) {
            Ok(registry) => registry,
            Err(err) => {
                self.emit_if_current(
                    generation,
                    "agent.turn_failed",
                    serde_json::json!({ "error": err.to_string() }),
                );
                return;
            }
        };
        self.attach_mcp_tools(&mut registry).await;
        self.attach_plugin_tools(&mut registry);
        self.attach_memory_tools(&mut registry);
        self.attach_desktop_tools(&mut registry);
        let policy = match self.approvals.lock() {
            Ok(queue) => queue.context(),
            Err(_) => {
                self.emit_if_current(
                    generation,
                    "agent.turn_failed",
                    serde_json::json!({ "error": "approval queue lock failed" }),
                );
                return;
            }
        };
        match agent.turn_with_tools(transcript, &registry, &policy).await {
            Err(err) => {
                self.emit_if_current(
                    generation,
                    "agent.turn_failed",
                    serde_json::json!({ "error": err.to_string() }),
                );
            }
            Ok(outcome) => {
                if self.is_current(generation) {
                    if let Ok(mut state) = self.state.lock() {
                        state.transcript = outcome.messages.clone();
                    }
                }
                match outcome.pending_approval {
                    Some(pending) => {
                        let request = match self.approvals.lock() {
                            Ok(queue) => queue.submit(
                                agent.principal(),
                                pending.capability.clone(),
                                pending.resource.clone(),
                                pending.reason.clone(),
                            ),
                            Err(_) => {
                                self.emit_if_current(
                                    generation,
                                    "agent.turn_failed",
                                    serde_json::json!({ "error": "approval queue lock failed" }),
                                );
                                return;
                            }
                        };
                        if let Ok(mut state) = self.state.lock() {
                            if state.generation == generation {
                                state.suspended = Some(Suspended {
                                    transcript: outcome.messages.clone(),
                                    request_id: request.id.clone(),
                                });
                            }
                        }
                        match serde_json::to_value(&request) {
                            Ok(data) => self.emit_if_current(
                                generation,
                                "permission.requested",
                                data,
                            ),
                            Err(err) => tracing::warn!(%err, "cannot serialize permission request"),
                        }
                        self.emit_if_current(
                            generation,
                            "agent.turn_suspended",
                            serde_json::json!({
                                "text": outcome.text,
                                "request_id": request.id,
                            }),
                        );
                    }
                    None => {
                        let executed: Vec<serde_json::Value> = outcome
                            .executed
                            .iter()
                            .map(|step| {
                                serde_json::json!({
                                    "id": step.id,
                                    "name": step.name,
                                    "output": step.output.content,
                                })
                            })
                            .collect();
                        self.emit_if_current(
                            generation,
                            "agent.turn_done",
                            serde_json::json!({
                                "text": outcome.text,
                                "executed": executed,
                                "truncated": outcome.truncated,
                            }),
                        );
                    }
                }
            }
        }
    }
}

/// Tools the agent may call, all behind policy + tickets: the plan's
/// first tool set (read/search/patch) plus structured process execution
/// (spawn/status/kill, no shell).
fn default_registry(processes: &Arc<ProcessManager>) -> Result<ToolRegistry, RuntimeError> {
    use tool_filesystem::{FilesystemLimits, SearchLimits};
    let mut registry = ToolRegistry::new();
    let fs = FilesystemLimits::default();
    let search = SearchLimits::default();
    let tools: Vec<Arc<dyn tool_core::Tool>> = vec![
        Arc::new(tool_filesystem::ListTool { limits: fs.clone() }),
        Arc::new(tool_filesystem::StatTool),
        Arc::new(tool_filesystem::ReadTool { limits: fs.clone() }),
        Arc::new(tool_filesystem::ReadRangeTool { limits: fs.clone() }),
        Arc::new(tool_filesystem::SearchTextTool { limits: search.clone() }),
        Arc::new(tool_filesystem::GlobTool { limits: search }),
        Arc::new(tool_filesystem::PatchTool { limits: fs }),
        Arc::new(tool_process::SpawnTool {
            manager: Arc::clone(processes),
            limits: ProcessLimits::default(),
        }),
        Arc::new(tool_process::StatusTool {
            manager: Arc::clone(processes),
        }),
        Arc::new(tool_process::KillTool {
            manager: Arc::clone(processes),
        }),
    ];
    for tool in tools {
        registry
            .register(tool)
            .map_err(|e| RuntimeError::Tools(e.to_string()))?;
    }
    Ok(registry)
}

/// Provider factory reading `model.*` settings from storage. The API key
/// lives in the OS keychain (plan Phase 33); it is never logged and never
/// forwarded except to the configured provider base URL.
fn settings_provider_factory(
    storage: Option<Arc<Mutex<Storage>>>,
) -> Arc<dyn Fn() -> Result<Arc<dyn ModelProvider>, RuntimeError> + Send + Sync> {
    provider_factory_with_secrets(storage, secret_core::system("utsuwa"))
}

fn provider_factory_with_secrets(
    storage: Option<Arc<Mutex<Storage>>>,
    secrets: Arc<dyn secret_core::SecretStore>,
) -> Arc<dyn Fn() -> Result<Arc<dyn ModelProvider>, RuntimeError> + Send + Sync> {
    Arc::new(move || {
        let storage = storage.as_ref().ok_or(RuntimeError::ModelNotConfigured)?;
        let storage = storage
            .lock()
            .map_err(|_| RuntimeError::Settings("storage lock failed".to_string()))?;
        let get = |key: &str| -> Result<Option<String>, RuntimeError> {
            storage
                .get_setting(key)
                .map_err(|e| RuntimeError::Settings(e.to_string()))
                .map(|v| v.and_then(|v| v.as_str().map(str::to_string)))
        };
        let base_url = get(SETTING_BASE_URL)?
            .filter(|s| !s.is_empty())
            .ok_or(RuntimeError::ModelNotConfigured)?;
        let name = get(SETTING_MODEL_NAME)?
            .filter(|s| !s.is_empty())
            .ok_or(RuntimeError::ModelNotConfigured)?;
        let api_key = resolve_api_key(&storage, secrets.as_ref())?;
        Ok(Arc::new(OpenAICompatibleClient::new(base_url, api_key, name))
            as Arc<dyn ModelProvider>)
    })
}

/// API key resolution order: OS keychain first; then a one-time migration
/// of the legacy plaintext `model.api_key` settings value into the
/// keychain (the settings row is deleted afterwards). The key is never
/// logged, never exposed to the model context, and never forwarded except
/// to the configured provider.
fn resolve_api_key(
    storage: &Storage,
    secrets: &dyn secret_core::SecretStore,
) -> Result<Option<String>, RuntimeError> {
    match secrets.get(secret_core::ACCOUNT_MODEL_API_KEY) {
        Ok(Some(key)) if !key.is_empty() => return Ok(Some(key)),
        Ok(_) => {}
        Err(e) => tracing::warn!(%e, "secret store unreadable; checking legacy settings"),
    }
    let legacy = storage
        .get_setting(SETTING_API_KEY)
        .map_err(|e| RuntimeError::Settings(e.to_string()))?
        .and_then(|v| v.as_str().map(str::to_string))
        .filter(|s| !s.is_empty());
    if let Some(key) = legacy {
        match secrets.set(secret_core::ACCOUNT_MODEL_API_KEY, &key) {
            Ok(()) => {
                // Best-effort plaintext removal; a failure here only logs.
                if let Err(e) = storage.delete_setting(SETTING_API_KEY) {
                    tracing::warn!(%e, "migrated api key but could not delete the settings row");
                } else {
                    tracing::info!("migrated model.api_key from settings to the OS keychain");
                }
            }
            Err(e) => {
                tracing::warn!(%e, "cannot store api key in keychain; using settings value");
            }
        }
        return Ok(Some(key));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use model_core::{FinishReason, ModelError, ModelRequest, ModelStreamEvent, ToolCall};
    use std::time::{Duration, Instant};

    /// Scripted multi-turn provider: pops one scripted turn per call.
    struct QueueProvider {
        turns: Mutex<Vec<Vec<ModelStreamEvent>>>,
    }

    impl QueueProvider {
        fn new(turns: Vec<Vec<ModelStreamEvent>>) -> Arc<Self> {
            Arc::new(Self {
                turns: Mutex::new(turns.into_iter().rev().collect()),
            })
        }
    }

    #[async_trait::async_trait]
    impl ModelProvider for QueueProvider {
        async fn stream(
            &self,
            _request: ModelRequest,
        ) -> Result<model_core::ModelStream, ModelError> {
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

    fn harness_with(provider: Arc<QueueProvider>, grants: Vec<policy_core::GrantedScope>) -> Harness {
        let approvals = Arc::new(Mutex::new(
            ApprovalQueue::new().with_grants(grants),
        ));
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

    fn temp_project(name: &str) -> (std::path::PathBuf, String) {
        let dir = std::env::temp_dir().join(format!(
            "utsuwa-runtime-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("notes.txt");
        std::fs::write(&file, "project notes alpha").unwrap();
        (dir, file.to_string_lossy().to_string())
    }

    #[test]
    fn plain_text_turn_emits_done() {
        let harness = harness(QueueProvider::new(vec![text_turn("hello there")]));
        harness.runtime.send_message("hi".to_string()).unwrap();
        let done = wait_for(&harness, "agent.turn_done");
        assert_eq!(done.data["text"], "hello there");
        assert!(harness.approvals.lock().unwrap().list().is_empty());
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

        harness.runtime.send_message("read my notes".to_string()).unwrap();
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
    fn agent_spawns_and_polls_process_to_output() {
        let echo = tool_process::resolve_executable("echo").unwrap();
        let grant = policy_core::GrantedScope {
            principal_kind: capability_core::PrincipalKind::Agent,
            capability: capability_core::Capability::ProcessSpawn,
            scope: capability_core::ResourceScope::new(vec![
                capability_core::Resource::Executable(echo),
            ]),
            // Persistent: the only lifetime the queue seeds from storage.
            // No persist hook is attached here, so it stays in memory.
            lifetime: policy_core::GrantLifetime::Persistent,
        };
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
                let exited = results.iter().any(|(_, v)| v.get("state") == Some(&serde_json::Value::from("exited")));
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
            Arc::new(|| Ok(Arc::new(Driver { calls: Mutex::new(0) }) as Arc<dyn ModelProvider>)),
        )
        .unwrap();
        let harness = Harness {
            runtime,
            approvals,
            events,
        };

        harness.runtime.send_message("run the build".to_string()).unwrap();
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

        harness.runtime.send_message("remember this".to_string()).unwrap();
        let done = wait_for(&harness, "agent.turn_done");
        let executed = done.data["executed"].as_array().unwrap();
        assert_eq!(executed.len(), 1, "{executed:?}");
        assert_eq!(executed[0]["name"], "memory.remember");
        assert!(executed[0]["output"]["id"].as_i64().unwrap() > 0);
        // Pure notebook tools need no approval dance.
        assert!(harness.approvals.lock().unwrap().list().is_empty());
        // The fact survives in the runtime's store and recalls by keyword.
        let entries = harness
            .runtime
            .memory_store()
            .recall("concise", 5)
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "concise answers preferred");
        assert_eq!(entries[0].importance, 6);
    }

    struct FakeDesktop;

    #[async_trait::async_trait]
    impl tool_desktop::DesktopBackend for FakeDesktop {
        async fn list_windows(&self) -> Result<Vec<tool_desktop::WindowInfo>, tool_desktop::DesktopError> {
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
        async fn invoke_element(&self, _window_id: &str, _element_id: &str) -> Result<(), tool_desktop::DesktopError> {
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
            Err(tool_desktop::DesktopError::ActionFailed("no display".to_string()))
        }
        async fn click(
            &self,
            _window_id: Option<&str>,
            _at: tool_desktop::Point,
        ) -> Result<(), tool_desktop::DesktopError> {
            Ok(())
        }
        async fn type_text(&self, _window_id: Option<&str>, _text: &str) -> Result<(), tool_desktop::DesktopError> {
            Ok(())
        }
    }

    #[test]
    fn desktop_tools_join_the_turn_behind_policy() {
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
        let grant = policy_core::GrantedScope {
            principal_kind: capability_core::PrincipalKind::Agent,
            capability: capability_core::Capability::DesktopControl,
            scope: capability_core::ResourceScope::new(vec![
                capability_core::Resource::Window("w1".to_string()),
            ]),
            lifetime: policy_core::GrantLifetime::Persistent,
        };
        let provider = QueueProvider::new(vec![invoke_turn, text_turn("pressed")]);
        let harness = harness_with(provider, vec![grant]);

        // With the stub backend the model never sees desktop tools; with a
        // backend installed they join the turn like any other tool source.
        harness.runtime.set_desktop_backend(Arc::new(FakeDesktop));
        harness.runtime.send_message("press save".to_string()).unwrap();
        let done = wait_for(&harness, "agent.turn_done");
        let executed = done.data["executed"].as_array().unwrap();
        assert_eq!(executed.len(), 1, "{executed:?}");
        assert_eq!(executed[0]["name"], "desktop.invoke_element");
        assert_eq!(executed[0]["output"]["ok"], true);
    }

    #[test]
    fn deny_resumes_with_refusal_note() {
        let (_dir, path) = temp_project("deny");
        let provider = QueueProvider::new(vec![read_turn(&path), text_turn("understood")]);
        let harness = harness(provider);

        harness.runtime.send_message("read my notes".to_string()).unwrap();
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
        let script_source =
            include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../mcp-runtime/tests/fake_server.py"));
        let dir = std::env::temp_dir().join(format!(
            "utsuwa-runtime-mcp-test-{}",
            std::process::id()
        ));
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
                crate::agent_runtime::SETTING_MCP_SERVERS,
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
        let status = harness.runtime.mcp_status_blocking();
        assert_eq!(status.len(), 1);
        assert!(status[0].connected);
        assert_eq!(status[0].tools, 3);
    }

    #[test]
    fn mcp_tool_call_needs_approval_without_grant() {
        use mcp_runtime::{McpServerConfig, McpTransport, TrustLevel};
        use std::collections::HashMap;

        let script_source =
            include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../mcp-runtime/tests/fake_server.py"));
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
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(harness.runtime.mcp_manager().configure(config))
            .unwrap();

        harness.runtime.send_message("use the mcp tool".to_string()).unwrap();
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

        let dir = std::env::temp_dir().join(format!(
            "utsuwa-runtime-secrets-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let storage = Arc::new(Mutex::new(
            storage_core::Storage::open(&dir.join("state.db")).unwrap(),
        ));
        {
            let store = storage.lock().unwrap();
            store
                .set_setting(SETTING_BASE_URL, &serde_json::json!("http://localhost:11434/v1"))
                .unwrap();
            store.set_setting(SETTING_MODEL_NAME, &serde_json::json!("m")).unwrap();
            store.set_setting(SETTING_API_KEY, &serde_json::json!("sk-legacy")).unwrap();
        }
        let secrets: Arc<dyn SecretStore> = Arc::new(secret_core::MemoryStore::default());
        let factory = provider_factory_with_secrets(Some(Arc::clone(&storage)), Arc::clone(&secrets));

        // First build migrates the plaintext row into the secret store.
        factory().unwrap();
        assert_eq!(
            secrets.get(ACCOUNT_MODEL_API_KEY).unwrap(),
            Some("sk-legacy".to_string())
        );
        assert_eq!(storage.lock().unwrap().get_setting(SETTING_API_KEY).unwrap(), None);

        // Second build reads from the secret store, not settings.
        factory().unwrap();
        assert_eq!(
            secrets.get(ACCOUNT_MODEL_API_KEY).unwrap(),
            Some("sk-legacy".to_string())
        );
    }

    #[test]
    fn provider_builds_without_any_key() {
        let dir = std::env::temp_dir().join(format!(
            "utsuwa-runtime-nokey-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let storage = Arc::new(Mutex::new(
            storage_core::Storage::open(&dir.join("state.db")).unwrap(),
        ));
        {
            let store = storage.lock().unwrap();
            store
                .set_setting(SETTING_BASE_URL, &serde_json::json!("http://localhost:11434/v1"))
                .unwrap();
            store.set_setting(SETTING_MODEL_NAME, &serde_json::json!("m")).unwrap();
        }
        let secrets: Arc<dyn secret_core::SecretStore> =
            Arc::new(secret_core::MemoryStore::default());
        let factory = provider_factory_with_secrets(Some(storage), secrets);
        // Ollama-style keyless providers still construct.
        factory().unwrap();
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
}
