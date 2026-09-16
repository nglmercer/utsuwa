//! Durable task authority construction for the native host: the
//! focused tool registry, the `TaskHost` (with its agent backend), and the
//! background tick loop plus worker pool.

use std::sync::Arc;

use super::services::HostServices;
use app_host::runtime::EmitFn;
use tool_sdk::ToolPack as _;

/// Background scheduling cadence: the single tick loop wakes this often
/// (plus on-demand wakes from mutating IPC calls).
pub const TASK_TICK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Focused registry for task steps: read-only system facts plus
/// notifications. Privileged tools arrive through the task-review
/// approval-ticket flow — never through this registry.
pub fn build_task_registry() -> Arc<tool_core::ToolRegistry> {
    let mut registry = tool_core::ToolRegistry::new();
    let load_ctx = tool_sdk::ToolLoadContext::default();
    let system_pack =
        app_host::tooling::SystemToolPack::new(host_core::HostEnvironment::snapshot());
    for tool in system_pack
        .tools(&load_ctx)
        .into_iter()
        .chain(tool_notification::NotificationToolPack.tools(&load_ctx))
    {
        if let Err(err) = registry.register(tool) {
            tracing::warn!(%err, "task registry: skipping duplicate tool");
        }
    }
    Arc::new(registry)
}

/// Open the durable task authority (tasks.db beside state.db): Rust owns
/// WHAT/WHEN/whether-it-succeeded; the renderer only produces receipts.
/// Agent steps run bounded turns on the configured provider via
/// `TaskAgentBackend`. `None` when tasks.db cannot open (`task.*`
/// methods fail, everything else keeps running).
pub fn build_task_host(services: &HostServices, emit: &EmitFn) -> Option<Arc<task_host::TaskHost>> {
    let tasks_path = storage_core::default_state_dir("utsuwa").join("tasks.db");
    let registry = build_task_registry();
    let host_services = Arc::new(task_host::HostServices::new(Arc::clone(&registry)));
    let providers = app_host::runtime::providers::provider_factory_with_secrets(
        services.storage.clone(),
        Arc::clone(&services.secrets),
    );
    // Cloned (not moved) so the task agent backend snapshots the same
    // standing grants for its step-scoped authorizer.
    let task_approvals = Arc::clone(&services.approvals);
    let agent_backend = Arc::new(app_host::runtime::task_agent::TaskAgentBackend::new(
        providers,
        registry,
        task_approvals,
        Arc::clone(emit),
        Arc::clone(&host_services),
    ));
    agent_backend.set_model_gate(Arc::clone(&services.model_gate));
    match task_host::TaskHost::open_with_services(
        &tasks_path,
        host_services,
        Arc::clone(emit),
        Some(agent_backend),
    ) {
        Ok(host) => Some(Arc::new(host)),
        Err(err) => {
            tracing::error!(%err, "failed to open tasks.db; task.* methods will fail");
            None
        }
    }
}

/// Background task tick (the single scheduling authority) plus the worker
/// pool that executes dispatched tasks off the tick path. Without the
/// agent executor (degraded mode) neither runs and mutating `task.*` IPC
/// calls fall back to one inline tick each.
pub fn install_task_workers(
    runtime: &Arc<app_host::runtime::AgentRuntime>,
    tasks: Arc<task_host::TaskHost>,
) {
    runtime.executor_handle().spawn(task_host::run_tick_loop(
        Arc::clone(&tasks),
        TASK_TICK_INTERVAL,
    ));
    runtime.executor_handle().spawn(task_host::run_worker_pool(
        tasks,
        task_host::WorkerConfig::default(),
    ));
}
