//! Policy engine: principal + capability request + context → decision.
//!
//! Architectural invariant: a decision is not authority. `Allow` only
//! authorizes minting a [`capability_core::CapabilityTicket`]; the broker
//! validates the ticket. `RequireUserApproval` routes to the frontend
//! permission UI and never auto-executes.

use capability_core::{Capability, CapabilityRequest, Principal, PrincipalKind, Resource};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// How long a grant lives once approved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GrantLifetime {
    Once,
    Task,
    Session,
    Persistent,
}

/// Outcome of policy evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorizationDecision {
    /// May mint a ticket with this TTL.
    Allow { ticket_ttl: Duration },
    Deny { reason: String },
    /// Block on explicit frontend approval first.
    RequireUserApproval { reason: String },
}

/// Ambient facts the policy may consider. Persistent grants live in SQLite
/// (`storage-core`); the engine takes them as input so storage stays out
/// of the decision function. The host seeds [`ApprovalQueue`] from storage
/// at boot and persists new `Persistent` approvals through its hook.
#[derive(Debug, Clone, Default)]
pub struct AuthorizationContext {
    /// Previously approved persistent/session grants to honor.
    pub grants: Vec<GrantedScope>,
}

/// A standing grant previously approved by the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantedScope {
    pub principal_kind: PrincipalKind,
    pub capability: Capability,
    pub scope: capability_core::ResourceScope,
    pub lifetime: GrantLifetime,
}

/// Capability risk class: reads/observation can be pre-approved, mutations
/// and control always need the user unless a standing grant exists.
/// External MCP tools count as control: third-party code the host did
/// not ship must never run on a read-only pre-approval.
fn is_mutation_or_control(cap: &Capability) -> bool {
    use Capability::*;
    matches!(
        cap,
        FilesystemWrite
            | FilesystemCreate
            | FilesystemDelete
            | FilesystemMove
            | ProcessSpawn
            | ProcessSignal
            | DesktopControl
            | ClipboardWrite
            | ApplicationLaunch
            | McpInvoke
            | PluginInvoke
    )
}

/// Default ticket TTLs: short for approvals, longer for harmless reads.
fn default_ttl(decision_allows_mutation: bool) -> Duration {
    if decision_allows_mutation {
        Duration::from_secs(120)
    } else {
        Duration::from_secs(300)
    }
}

/// Evaluate one request. Pure function of (principal, request, context) —
/// no I/O, no ambient authority, no panics. The span names the principal
/// kind and capability only: concrete resources may contain user paths.
pub fn authorize(
    principal: &Principal,
    request: &CapabilityRequest,
    context: &AuthorizationContext,
) -> AuthorizationDecision {
    let _span = tracing::debug_span!(
        "policy.authorize",
        kind = ?principal.kind(),
        capability = ?request.capability
    )
    .entered();
    // The WebView never holds direct OS authority: a Frontend principal
    // asking for a privileged capability is denied outright rather than
    // prompted (prompting would train users to bless the wrong layer).
    if *principal == Principal::Frontend {
        return AuthorizationDecision::Deny {
            reason: "frontend holds no direct OS authority; route through the agent".to_string(),
        };
    }

    // Unsigned native plugins are disabled by default (plan Phase 25).
    if matches!(principal, Principal::NativePlugin(_)) {
        return AuthorizationDecision::Deny {
            reason: "native plugins are disabled by default".to_string(),
        };
    }

    // Standing grant covering this exact capability + resource?
    let kind = principal.kind();
    for grant in &context.grants {
        if grant.principal_kind == kind
            && grant.capability == request.capability
            && grant.scope.allows(&request.resource)
        {
            return AuthorizationDecision::Allow {
                ticket_ttl: default_ttl(is_mutation_or_control(&request.capability)),
            };
        }
    }

    // The user themselves acting locally: allow, tickets still scope it.
    if *principal == Principal::User {
        return AuthorizationDecision::Allow {
            ticket_ttl: default_ttl(is_mutation_or_control(&request.capability)),
        };
    }

    // No standing grant matched. Mutations/control always need the human;
    // reads and observation do too when nothing grants them — the agent is
    // expected to request a project scope first ("request project read
    // scope → read code"), keeping the default-deny posture for every
    // non-user principal.
    AuthorizationDecision::RequireUserApproval {
        reason: format!(
            "{:?} has no grant for {:?} on {:?}",
            kind, request.capability, request.resource
        ),
    }
}

/// A permission request awaiting a human decision (Task 14). Published by
/// the agent runtime; rendered by the frontend permission dialog; resolved
/// back through [`ApprovalQueue::decide`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingRequest {
    pub id: String,
    pub principal: Principal,
    pub capability: Capability,
    pub resource: Resource,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QueueError {
    #[error("unknown permission request: {0}")]
    UnknownId(String),
    /// A `Persistent` approval could not be written to storage. The
    /// request stays pending so the user can retry; nothing was granted.
    #[error("persistent grant could not be stored: {0}")]
    Persist(String),
}

struct QueueInner {
    next_id: u64,
    pending: std::collections::HashMap<String, PendingRequest>,
    grants: Vec<GrantedScope>,
}

/// Thread-safe queue bridging the agent runtime, the permission dialog,
/// and the policy context. Approving inserts a real [`GrantedScope`], so a
/// resumed turn authorizes through the normal `authorize` path — approvals
/// never bypass policy, they extend it. Submissions and user decisions are
/// audit-logged when a sink is attached.
pub struct ApprovalQueue {
    inner: std::sync::Mutex<QueueInner>,
    sink: Option<std::sync::Arc<dyn audit_core::AuditSink>>,
    /// Called with every newly approved `Persistent` grant before it takes
    /// effect. The host installs a `storage-core` writer here; an error
    /// fails the decision and leaves the request pending.
    persist: Option<
        std::sync::Arc<dyn Fn(&GrantedScope) -> Result<(), String> + Send + Sync>,
    >,
}

impl Default for ApprovalQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl ApprovalQueue {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(QueueInner {
                next_id: 1,
                pending: std::collections::HashMap::new(),
                grants: Vec::new(),
            }),
            sink: None,
            persist: None,
        }
    }

    pub fn with_sink(mut self, sink: std::sync::Arc<dyn audit_core::AuditSink>) -> Self {
        self.sink = Some(sink);
        self
    }

    /// Seed in-memory grants loaded from storage at boot. Only
    /// `Persistent` entries are honored; anything else is a programming
    /// error from the loader and is rejected loudly.
    pub fn with_grants(self, grants: Vec<GrantedScope>) -> Self {
        for grant in &grants {
            assert_eq!(
                grant.lifetime,
                GrantLifetime::Persistent,
                "only Persistent grants may seed the approval queue"
            );
        }
        self.inner
            .lock()
            .expect("approval queue lock")
            .grants
            .extend(grants);
        self
    }

    /// Install the storage writer for new `Persistent` approvals.
    pub fn on_persistent_grant(
        mut self,
        hook: std::sync::Arc<dyn Fn(&GrantedScope) -> Result<(), String> + Send + Sync>,
    ) -> Self {
        self.persist = Some(hook);
        self
    }

    /// Snapshot of grants approved so far (seeded + decided).
    pub fn grants_snapshot(&self) -> Vec<GrantedScope> {
        self.inner.lock().expect("approval queue lock").grants.clone()
    }

    /// Publish a pending request. Returns its stable id for the dialog and
    /// the `permission.approve` / `permission.deny` replies.
    pub fn submit(
        &self,
        principal: Principal,
        capability: Capability,
        resource: Resource,
        reason: String,
    ) -> PendingRequest {
        let mut inner = self.inner.lock().expect("approval queue lock");
        let id = format!("perm-{}", inner.next_id);
        inner.next_id += 1;
        let request = PendingRequest {
            id: id.clone(),
            principal,
            capability,
            resource,
            reason,
        };
        inner.pending.insert(id, request.clone());
        if let Some(sink) = &self.sink {
            sink.record(audit_core::AuditRecord::now(
                request.principal.clone(),
                Some(request.capability.clone()),
                Some(request.resource.clone()),
                audit_core::AuditOutcome::ApprovalRequested,
                request.reason.clone(),
            ));
        }
        request
    }

    pub fn list(&self) -> Vec<PendingRequest> {
        let inner = self.inner.lock().expect("approval queue lock");
        let mut out: Vec<PendingRequest> = inner.pending.values().cloned().collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// Resolve a request. `Some(lifetime)` approves (records a grant scoped
    /// to the requested capability + resource); `None` denies. Returns true
    /// on approval. Either way the request leaves the queue — except when
    /// persisting a `Persistent` approval fails, in which case nothing is
    /// granted and the request stays pending for a retry.
    pub fn decide(
        &self,
        id: &str,
        lifetime: Option<GrantLifetime>,
    ) -> Result<bool, QueueError> {
        let _span = tracing::info_span!("permission.decide", request = %id).entered();
        // Phase 1: peek at the request without removing it, so a storage
        // failure below cannot lose a pending approval.
        let grant = {
            let inner = self.inner.lock().expect("approval queue lock");
            let request = inner
                .pending
                .get(id)
                .ok_or_else(|| QueueError::UnknownId(id.to_string()))?;
            lifetime.map(|lifetime| GrantedScope {
                principal_kind: request.principal.kind(),
                capability: request.capability.clone(),
                scope: capability_core::ResourceScope::new(vec![request.resource.clone()]),
                lifetime,
            })
        };
        // Phase 2: durable write first (outside the queue lock — the hook
        // does I/O). Only `Persistent` grants reach storage; without a hook
        // (tests, ephemeral hosts) they live in memory like the rest.
        if let Some(grant) = grant.as_ref().filter(|g| g.lifetime == GrantLifetime::Persistent) {
            if let Some(persist) = &self.persist {
                persist(grant).map_err(QueueError::Persist)?;
            }
        }
        // Phase 3: commit the decision.
        let mut inner = self.inner.lock().expect("approval queue lock");
        let request = inner
            .pending
            .remove(id)
            .ok_or_else(|| QueueError::UnknownId(id.to_string()))?;
        let approved = grant.is_some();
        if let Some(grant) = grant {
            inner.grants.push(grant);
        }
        if let Some(sink) = &self.sink {
            sink.record(audit_core::AuditRecord::now(
                request.principal.clone(),
                Some(request.capability.clone()),
                Some(request.resource.clone()),
                if approved {
                    audit_core::AuditOutcome::Approved
                } else {
                    audit_core::AuditOutcome::ApprovalDenied
                },
                format!("decided {id}"),
            ));
        }
        Ok(approved)
    }

    /// Snapshot of standing grants (seed + approvals) for agent turns.
    pub fn context(&self) -> AuthorizationContext {
        let inner = self.inner.lock().expect("approval queue lock");
        AuthorizationContext {
            grants: inner.grants.clone(),
        }
    }
}

/// Narrow helper used by brokers: does this resource look like a secret
/// path that should never be silently allowed? Defense in depth on top of
/// scope checks.
pub fn is_secret_path(resource: &Resource) -> bool {
    match resource {
        Resource::Path(p) => {
            let s = p.to_string_lossy();
            [".ssh", ".aws", ".gnupg", ".config", ".pki", "secrets"]
                .iter()
                .any(|seg| s.split('/').any(|c| c == *seg))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use capability_core::{AgentId, PluginId, ResourceScope, ToolId};
    use std::path::PathBuf;

    fn ctx() -> AuthorizationContext {
        AuthorizationContext { grants: vec![] }
    }

    fn read_req() -> CapabilityRequest {
        CapabilityRequest {
            principal: Principal::Agent(AgentId::new("a")),
            capability: Capability::FilesystemRead,
            resource: Resource::Path(PathBuf::from("/work/main.rs")),
        }
    }

    #[test]
    fn agent_read_without_grant_requires_approval() {
        let r = read_req();
        let d = authorize(&r.principal, &r, &ctx());
        assert!(matches!(d, AuthorizationDecision::RequireUserApproval { .. }));
    }

    #[test]
    fn agent_read_with_matching_grant_is_allowed() {
        let r = read_req();
        let granting = AuthorizationContext {
            grants: vec![GrantedScope {
                principal_kind: PrincipalKind::Agent,
                capability: Capability::FilesystemRead,
                scope: ResourceScope::new(vec![Resource::Path(PathBuf::from("/work"))]),
                lifetime: GrantLifetime::Session,
            }],
        };
        // read_req targets /work/main.rs — inside the granted tree.
        let d = authorize(&r.principal, &r, &granting);
        assert!(matches!(d, AuthorizationDecision::Allow { .. }));
    }

    #[test]
    fn agent_write_requires_approval() {
        let r = CapabilityRequest {
            capability: Capability::FilesystemWrite,
            ..read_req()
        };
        let d = authorize(&r.principal, &r, &ctx());
        assert!(matches!(d, AuthorizationDecision::RequireUserApproval { .. }));
    }

    #[test]
    fn frontend_privileged_request_is_denied_not_prompted() {
        let r = CapabilityRequest {
            principal: Principal::Frontend,
            ..read_req()
        };
        let d = authorize(&r.principal, &r, &ctx());
        assert!(matches!(d, AuthorizationDecision::Deny { .. }));
    }

    #[test]
    fn standing_grant_allows_matching_write() {
        let principal = Principal::BuiltinTool(ToolId::new("filesystem.patch"));
        let r = CapabilityRequest {
            principal: principal.clone(),
            capability: Capability::FilesystemWrite,
            resource: Resource::Path(PathBuf::from("/work/src/main.rs")),
        };
        let granting = AuthorizationContext {
            grants: vec![GrantedScope {
                principal_kind: PrincipalKind::BuiltinTool,
                capability: Capability::FilesystemWrite,
                scope: ResourceScope::new(vec![Resource::Path(PathBuf::from("/work"))]),
                lifetime: GrantLifetime::Task,
            }],
        };
        assert!(matches!(
            authorize(&principal, &r, &granting),
            AuthorizationDecision::Allow { .. }
        ));
        // Same grant does not cover a different tree.
        let elsewhere = CapabilityRequest {
            resource: Resource::Path(PathBuf::from("/etc/hosts")),
            ..r
        };
        assert!(matches!(
            authorize(&principal, &elsewhere, &granting),
            AuthorizationDecision::RequireUserApproval { .. }
        ));
    }

    #[test]
    fn native_plugin_is_denied_by_default() {
        let p = Principal::NativePlugin(PluginId::new("x"));
        let r = CapabilityRequest {
            principal: p.clone(),
            ..read_req()
        };
        assert!(matches!(
            authorize(&p, &r, &ctx()),
            AuthorizationDecision::Deny { .. }
        ));
    }

    #[test]
    fn approval_queue_submit_approve_deny() {
        let queue = ApprovalQueue::new();
        let req = queue.submit(
            Principal::Agent(AgentId::new("a")),
            Capability::FilesystemRead,
            Resource::Path(PathBuf::from("/work")),
            "read notes".to_string(),
        );
        assert_eq!(req.id, "perm-1");
        assert_eq!(queue.list().len(), 1);
        // Deny clears without granting.
        assert_eq!(queue.decide(&req.id, None), Ok(false));
        assert!(queue.list().is_empty());
        assert!(queue.context().grants.is_empty());

        let req = queue.submit(
            Principal::Agent(AgentId::new("a")),
            Capability::FilesystemWrite,
            Resource::Path(PathBuf::from("/work/f")),
            "patch".to_string(),
        );
        assert_eq!(queue.decide(&req.id, Some(GrantLifetime::Task)), Ok(true));
        let ctx = queue.context();
        assert_eq!(ctx.grants.len(), 1);
        assert_eq!(ctx.grants[0].lifetime, GrantLifetime::Task);
        assert_eq!(ctx.grants[0].principal_kind, PrincipalKind::Agent);
        // Unknown ids fail instead of granting.
        assert_eq!(
            queue.decide("perm-999", Some(GrantLifetime::Once)),
            Err(QueueError::UnknownId("perm-999".to_string()))
        );
    }

    #[test]
    fn persistent_approve_calls_hook_before_granting() {
        use std::sync::{Arc, Mutex};
        let seen = Arc::new(Mutex::new(Vec::new()));
        let probe = seen.clone();
        let queue = ApprovalQueue::new().on_persistent_grant(Arc::new(move |grant: &GrantedScope| {
            probe.lock().unwrap().push(grant.clone());
            Ok(())
        }));
        let req = queue.submit(
            Principal::Agent(AgentId::new("a")),
            Capability::FilesystemRead,
            Resource::Path(PathBuf::from("/work/f")),
            "read".to_string(),
        );
        assert_eq!(queue.decide(&req.id, Some(GrantLifetime::Persistent)), Ok(true));
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].lifetime, GrantLifetime::Persistent);
        assert_eq!(seen[0].capability, Capability::FilesystemRead);
        assert_eq!(queue.context().grants.len(), 1);
    }

    #[test]
    fn non_persistent_approve_skips_hook() {
        use std::sync::{Arc, Mutex};
        let calls = Arc::new(Mutex::new(0u32));
        let probe = calls.clone();
        let queue = ApprovalQueue::new().on_persistent_grant(Arc::new(move |_: &GrantedScope| {
            *probe.lock().unwrap() += 1;
            Ok(())
        }));
        let req = queue.submit(
            Principal::Agent(AgentId::new("a")),
            Capability::FilesystemRead,
            Resource::Path(PathBuf::from("/work/f")),
            "read".to_string(),
        );
        assert_eq!(queue.decide(&req.id, Some(GrantLifetime::Session)), Ok(true));
        assert_eq!(*calls.lock().unwrap(), 0);
        assert_eq!(queue.context().grants.len(), 1);
    }

    #[test]
    fn persist_failure_leaves_request_pending() {
        use std::sync::Arc;
        let queue = ApprovalQueue::new().on_persistent_grant(Arc::new(|_: &GrantedScope| {
            Err("disk full".to_string())
        }));
        let req = queue.submit(
            Principal::Agent(AgentId::new("a")),
            Capability::FilesystemRead,
            Resource::Path(PathBuf::from("/work/f")),
            "read".to_string(),
        );
        assert_eq!(
            queue.decide(&req.id, Some(GrantLifetime::Persistent)),
            Err(QueueError::Persist("disk full".to_string()))
        );
        // Nothing granted, request still pending for a retry.
        assert!(queue.context().grants.is_empty());
        assert_eq!(queue.list().len(), 1);
    }

    #[test]
    fn seeded_grants_must_be_persistent() {
        let persistent = GrantedScope {
            principal_kind: PrincipalKind::Agent,
            capability: Capability::FilesystemRead,
            scope: ResourceScope::new(vec![Resource::Path(PathBuf::from("/work"))]),
            lifetime: GrantLifetime::Persistent,
        };
        let queue = ApprovalQueue::new().with_grants(vec![persistent.clone()]);
        assert_eq!(queue.grants_snapshot(), vec![persistent]);
    }

    #[test]
    #[should_panic(expected = "only Persistent grants may seed")]
    fn seeded_session_grant_panics() {
        ApprovalQueue::new().with_grants(vec![GrantedScope {
            principal_kind: PrincipalKind::Agent,
            capability: Capability::FilesystemRead,
            scope: ResourceScope::new(vec![Resource::Path(PathBuf::from("/work"))]),
            lifetime: GrantLifetime::Session,
        }]);
    }

    #[test]
    fn secret_paths_are_detected() {
        assert!(is_secret_path(&Resource::Path(PathBuf::from(
            "/home/u/.ssh/id_ed25519"
        ))));
        assert!(!is_secret_path(&Resource::Path(PathBuf::from(
            "/work/main.rs"
        ))));
    }
}
