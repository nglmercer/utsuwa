//! Native host composition: storage, audit, approvals, secrets, sensors,
//! and the model gate. `main.rs` keeps CLI parsing, logging, platform
//! windows, and the top-level `start_host` flow; this module owns the
//! service-bundle construction details.

use std::sync::{Arc, Mutex};

use policy_core::ApprovalQueue;

/// Process-wide service bundle shared by the agent runtime, the task host,
/// and the IPC dispatcher. Built once at startup; every consumer clones
/// the `Arc`s it needs.
pub struct HostServices {
    pub storage: Option<Arc<Mutex<storage_core::Storage>>>,
    pub audit: Arc<audit_core::InMemorySink>,
    pub approvals: Arc<Mutex<ApprovalQueue>>,
    pub secrets: Arc<dyn secret_core::SecretStore>,
    pub sensors: Arc<app_host::runtime::SensorActivityHub>,
    pub model_gate: Arc<app_host::runtime::model_gate::ModelExecutionGate>,
}

/// Open every host service. A host without storage still runs — approvals
/// go in-memory and every launch re-prompts (fail-closed for authority,
/// open for availability).
pub fn open_host_services(dev_grant_workspace: bool) -> HostServices {
    // SQLite state: settings KV + persistent grants.
    let storage = match storage_core::Storage::open(&storage_core::default_db_path("utsuwa")) {
        Ok(store) => Some(Arc::new(Mutex::new(store))),
        Err(err) => {
            tracing::error!(%err, "failed to open state.db; running without storage");
            None
        }
    };
    // Shared audit sink: the permission queue and the agent runtime both
    // record here; the Activity panel reads it back over `activity.list`.
    let audit = Arc::new(audit_core::InMemorySink::new());
    let approvals = build_approvals(&storage, &audit);
    if dev_grant_workspace {
        install_dev_workspace_grant(&approvals);
    }
    let secrets = secret_core::system("utsuwa");
    // Host-owned sensor hub: privacy indicators stay authoritative even if
    // the agent runtime fails to initialize (degraded mode). The runtime
    // constructor attaches event publication on success; the degraded path
    // attaches separately so events are never duplicated.
    let sensors = Arc::new(app_host::runtime::SensorActivityHub::new());
    let model_gate = build_model_gate();
    HostServices {
        storage,
        audit,
        approvals,
        secrets,
        sensors,
        model_gate,
    }
}

/// The live permission kernel state: agent turns submit here, the dialog
/// resolves here, resumed turns read grants from here.
fn build_approvals(
    storage: &Option<Arc<Mutex<storage_core::Storage>>>,
    audit: &Arc<audit_core::InMemorySink>,
) -> Arc<Mutex<ApprovalQueue>> {
    Arc::new(Mutex::new(match storage {
        Some(store) => {
            let seed = match store.lock().expect("storage lock").load_grants() {
                Ok(rows) => rows.into_iter().map(|row| row.grant).collect(),
                Err(err) => {
                    tracing::error!(%err, "failed to load persistent grants; starting empty");
                    Vec::new()
                }
            };
            ApprovalQueue::new()
                .with_grants(seed)
                .on_persistent_grant(storage_core::persistent_grant_hook(Arc::clone(store)))
                .with_sink(Arc::clone(audit) as Arc<dyn audit_core::AuditSink>)
        }
        None => ApprovalQueue::new().with_sink(Arc::clone(audit) as Arc<dyn audit_core::AuditSink>),
    }))
}

/// Developer escape hatch (`--dev-grant-workspace`): read access to the
/// current working directory, refused for home and secret roots.
fn install_dev_workspace_grant(approvals: &Arc<Mutex<ApprovalQueue>>) {
    match std::env::current_dir().and_then(|path| path.canonicalize()) {
        Ok(workspace) => {
            let is_secret_root =
                policy_core::is_secret_path(&capability_core::Resource::Path(workspace.clone()));
            let is_home = std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .and_then(|home| std::path::PathBuf::from(home).canonicalize().ok())
                .is_some_and(|home| home == workspace);
            if !is_secret_root && !is_home {
                if let Ok(queue) = approvals.lock() {
                    if let Err(err) = queue.grant_direct(
                        capability_core::PrincipalKind::Agent,
                        capability_core::Capability::FilesystemRead,
                        capability_core::ResourceScope::new(vec![capability_core::Resource::Path(
                            workspace.clone(),
                        )]),
                        policy_core::GrantLifetime::Session,
                        format!("developer workspace grant for {}", workspace.display()),
                    ) {
                        tracing::error!(%err, "could not install developer workspace grant");
                    }
                }
            } else {
                tracing::warn!(path = %workspace.display(), "refusing developer grant for home or secret root");
            }
        }
        Err(err) => {
            tracing::warn!(%err, "cannot resolve current directory for developer grant")
        }
    }
}

/// One model gate for the process: interactive chat turns and background
/// task-agent turns serialize here (interactive first) instead of
/// contending for provider rate limits.
fn build_model_gate() -> Arc<app_host::runtime::model_gate::ModelExecutionGate> {
    Arc::new(app_host::runtime::model_gate::ModelExecutionGate::new())
}
