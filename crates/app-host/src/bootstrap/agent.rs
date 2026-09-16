//! Agent runtime construction for the native host.

use std::sync::Arc;

use super::services::HostServices;
use app_host::runtime::AgentRuntime;
use app_host::runtime::EmitFn;

/// Start the host-owned agent loop. `None` on failure (degraded mode: the
/// host still serves IPC, but `agent.*` methods fail and sensor events
/// attach directly to the emit path instead).
pub fn start_agent_runtime(services: &HostServices, emit: &EmitFn) -> Option<Arc<AgentRuntime>> {
    match AgentRuntime::start_with_secrets_and_sensors(
        Arc::clone(&services.approvals),
        services.storage.clone(),
        Some(Arc::clone(&services.audit) as Arc<dyn audit_core::AuditSink>),
        Arc::clone(emit),
        Arc::clone(&services.secrets),
        Arc::clone(&services.sensors),
    ) {
        Ok(runtime) => Some(runtime),
        Err(err) => {
            tracing::error!(%err, "agent runtime unavailable; agent.* methods will fail");
            services.sensors.attach_event_publisher(emit);
            None
        }
    }
}

/// Finish agent wiring once the task host exists: shared secrets, the
/// process model gate, durable memory, and the one SQLite authority for
/// model-facing `tasks.*` tools.
pub fn configure_runtime(
    runtime: &Arc<AgentRuntime>,
    services: &HostServices,
    tasks: &Option<Arc<task_host::TaskHost>>,
) {
    // MCP Bearer [REDACTED] resolve from the same keychain-backed store.
    runtime.set_secret_store(Arc::clone(&services.secrets));
    runtime.set_model_gate(Arc::clone(&services.model_gate));
    // One SQLite authority: model-facing `tasks.*` tools reuse the
    // TaskHost's store instead of reopening tasks.db on every call.
    if let Some(tasks) = tasks {
        runtime.set_task_store(tasks.store().as_ref().clone());
    }
    // Durable memory beside state.db; an unopenable file falls
    // back to the runtime's isolated in-memory store (logged).
    let memory_path = storage_core::default_state_dir("utsuwa").join("memory.db");
    match memory::MemoryStore::open(&memory_path) {
        Ok(store) => runtime.set_memory_store(Arc::new(store)),
        Err(err) => {
            tracing::error!(%err, "failed to open memory.db; using in-memory memory")
        }
    }
}
