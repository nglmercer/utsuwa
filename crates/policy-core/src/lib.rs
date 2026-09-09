//! Policy engine: principal + capability request + context → decision.
//!
//! Architectural invariant: a decision is not authority. `Allow` only
//! authorizes minting a [`capability_core::CapabilityTicket`]; the broker
//! validates the ticket. `RequireUserApproval` routes to the frontend
//! permission UI and never auto-executes.

use capability_core::{Capability, CapabilityRequest, Principal, PrincipalKind, Resource};
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime};

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
    Allow {
        ticket_ttl: Duration,
    },
    Deny {
        reason: String,
    },
    /// Block on explicit frontend approval first.
    RequireUserApproval {
        reason: String,
    },
}

/// Ambient facts the policy may consider. Persistent grants live in SQLite
/// (`storage-core`); the engine takes them as input so storage stays out
/// of the decision function. The host seeds [`ApprovalQueue`] from storage
/// at boot and persists new `Persistent` approvals through its hook.
#[derive(Debug, Clone, Default)]
pub struct AuthorizationContext {
    /// Previously approved persistent/session grants to honor.
    pub grants: Vec<GrantedScope>,
    /// The task currently being authorized. Task and once grants are bound
    /// to this identity and never become ambient authority.
    pub task_id: Option<String>,
    /// The model turn currently being authorized. Once grants are also
    /// bound to this identity and are consumed atomically before execution.
    pub turn_id: Option<String>,
}

/// A standing grant previously approved by the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantedScope {
    /// Stable identity for audit/debugging and future revocation APIs.
    #[serde(default)]
    pub id: String,
    pub principal_kind: PrincipalKind,
    pub capability: Capability,
    pub scope: capability_core::ResourceScope,
    pub lifetime: GrantLifetime,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub turn_id: Option<String>,
    #[serde(default = "unix_millis")]
    pub created_at: u64,
    #[serde(default)]
    pub expires_at: Option<u64>,
    #[serde(default)]
    pub uses_remaining: Option<u32>,
}

impl GrantedScope {
    /// Create a grant with the narrowest lifetime metadata for the approval
    /// that produced it. Task/once grants carry the current task and turn;
    /// session/persistent grants intentionally do not.
    pub fn new(
        principal_kind: PrincipalKind,
        capability: Capability,
        scope: capability_core::ResourceScope,
        lifetime: GrantLifetime,
        task_id: Option<String>,
        turn_id: Option<String>,
    ) -> Self {
        let bound = matches!(lifetime, GrantLifetime::Once | GrantLifetime::Task);
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            principal_kind,
            capability,
            scope,
            lifetime,
            task_id: bound.then_some(task_id).flatten(),
            turn_id: bound.then_some(turn_id).flatten(),
            created_at: unix_millis(),
            expires_at: None,
            uses_remaining: (lifetime == GrantLifetime::Once).then_some(1),
        }
    }

    /// Whether this grant is valid for the current authorization context.
    pub fn is_active(&self, context: &AuthorizationContext) -> bool {
        if self
            .expires_at
            .is_some_and(|expires| expires <= unix_millis())
        {
            return false;
        }
        if self.lifetime == GrantLifetime::Once && self.uses_remaining.unwrap_or(0) == 0 {
            return false;
        }
        if let Some(task_id) = &self.task_id {
            if context.task_id.as_ref() != Some(task_id) {
                return false;
            }
        }
        if let Some(turn_id) = &self.turn_id {
            if context.turn_id.as_ref() != Some(turn_id) {
                return false;
            }
        }
        match self.lifetime {
            GrantLifetime::Task => self.task_id.is_some() && context.task_id.is_some(),
            // A host task binds Once grants to a turn. The unbound form is
            // retained for the small public/context API, where a queue can
            // still provide a one-use grant without task metadata.
            GrantLifetime::Once => self.turn_id.is_none() || context.turn_id.is_some(),
            GrantLifetime::Session | GrantLifetime::Persistent => true,
        }
    }
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
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

fn is_dangerous_process(resource: &Resource) -> bool {
    let Resource::Process { executable, .. } = resource else {
        return false;
    };
    let Some(name) = executable.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    // Windows appends `.exe`; interpreters commonly append a version
    // (`python3.12`, `ruby3.3`, `perl5.38`). All of those forms remain a
    // fresh-approval surface.
    let stem = name.strip_suffix(".exe").unwrap_or(&name);
    let versioned = |prefix: &str| {
        stem.strip_prefix(prefix).is_some_and(|suffix| {
            suffix.is_empty() || suffix.chars().all(|ch| ch.is_ascii_digit() || ch == '.')
        })
    };
    matches!(
        stem,
        "sh" | "bash" | "zsh" | "fish" | "node" | "nodejs" | "pwsh" | "cmd"
    ) || versioned("python")
        || versioned("ruby")
        || versioned("perl")
        || stem.starts_with("powershell")
}

fn requires_fresh_approval(capability: &Capability, resource: &Resource) -> bool {
    is_secret_path(resource)
        || (*capability == Capability::ProcessSpawn && is_dangerous_process(resource))
}

/// Default ticket TTLs: short for approvals, longer for harmless reads.
fn default_ttl(decision_allows_mutation: bool) -> Duration {
    if decision_allows_mutation {
        Duration::from_secs(120)
    } else {
        Duration::from_secs(300)
    }
}

/// Ticket lifetime for an already-authorized capability request. The host's
/// autonomous Agent authorizer uses the same bounded lifetimes as ordinary
/// policy decisions; autonomous mode never creates an unbounded ticket.
pub fn ticket_ttl_for(capability: &Capability) -> Duration {
    default_ttl(is_mutation_or_control(capability))
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
    // The caller-supplied principal and the request identity must agree.
    // AgentRuntime constructs both values itself, but keeping this invariant
    // in the policy boundary prevents a confused deputy if another native
    // caller ever supplies mismatched values.
    if principal != &request.principal {
        return AuthorizationDecision::Deny {
            reason: "capability request principal does not match its caller".to_string(),
        };
    }

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
    let requires_fresh = requires_fresh_approval(&request.capability, &request.resource);
    for grant in &context.grants {
        if grant.principal_kind == kind
            && grant.capability == request.capability
            && grant.scope.allows(&request.resource)
            && grant.is_active(context)
            // A sensitive approval is represented by a bound/one-use grant,
            // so the operation can resume exactly once without turning a
            // secret path or interpreter into standing authority.
            && (!requires_fresh || grant.lifetime == GrantLifetime::Once)
        {
            return AuthorizationDecision::Allow {
                ticket_ttl: ticket_ttl_for(&request.capability),
            };
        }
    }

    // Secret paths are never silently allowed: no standing grant covers
    // them, however broad. The user can still approve each access in the
    // dialog — explicit, audited, and never inherited from a home-folder
    // style grant.
    if is_secret_path(&request.resource) {
        return AuthorizationDecision::RequireUserApproval {
            reason: format!(
                "{:?} on a secret path needs fresh approval every time: {:?}",
                request.capability, request.resource
            ),
        };
    }

    // Script interpreters and shells turn otherwise narrow argv approvals
    // into a general code-execution surface. They always need a fresh user
    // decision, even when an older executable-only grant exists.
    if request.capability == Capability::ProcessSpawn && is_dangerous_process(&request.resource) {
        return AuthorizationDecision::RequireUserApproval {
            reason: format!(
                "process.spawn for a shell or script interpreter needs fresh approval: {:?}",
                request.resource
            ),
        };
    }

    // The user themselves acting locally: allow, tickets still scope it.
    if *principal == Principal::User {
        return AuthorizationDecision::Allow {
            ticket_ttl: ticket_ttl_for(&request.capability),
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
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub turn_id: Option<String>,
    /// Sensitive path/interpreter requests can only be approved for one use;
    /// the frontend hides broader lifetime choices for these requests.
    #[serde(default)]
    pub requires_once: bool,
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
    persist: Option<std::sync::Arc<dyn Fn(&GrantedScope) -> Result<(), String> + Send + Sync>>,
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
        self.inner
            .lock()
            .expect("approval queue lock")
            .grants
            .clone()
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
        self.submit_for_task(principal, capability, resource, reason, None, None)
    }

    /// Publish an approval request bound to a live agent task/turn. The
    /// binding is copied into the resulting grant, so a later turn cannot
    /// reuse an approval that belonged to an abandoned task.
    pub fn submit_for_task(
        &self,
        principal: Principal,
        capability: Capability,
        resource: Resource,
        reason: String,
        task_id: Option<String>,
        turn_id: Option<String>,
    ) -> PendingRequest {
        let mut inner = self.inner.lock().expect("approval queue lock");
        let id = format!("perm-{}", inner.next_id);
        inner.next_id += 1;
        let requires_once = requires_fresh_approval(&capability, &resource);
        let request = PendingRequest {
            id: id.clone(),
            principal,
            capability,
            resource,
            reason,
            task_id,
            turn_id,
            requires_once,
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

    /// Withdraw a pending request created for the trusted Agent runtime when
    /// autonomous mode is enabled. This does not create a grant: the resumed
    /// turn must still pass the live autonomous authorizer and receive a
    /// normal invocation-bound capability ticket.
    ///
    /// The principal check is deliberate. A caller cannot use this helper to
    /// silently remove or auto-approve a request belonging to the frontend,
    /// a plugin, or another native principal.
    pub fn withdraw_agent(&self, id: &str) -> Option<PendingRequest> {
        let request = {
            let mut inner = self.inner.lock().expect("approval queue lock");
            let is_agent = inner
                .pending
                .get(id)
                .is_some_and(|request| matches!(request.principal, Principal::Agent(_)));
            if !is_agent {
                return None;
            }
            inner.pending.remove(id)
        }?;

        if let Some(sink) = &self.sink {
            sink.record(audit_core::AuditRecord::now(
                request.principal.clone(),
                Some(request.capability.clone()),
                Some(request.resource.clone()),
                audit_core::AuditOutcome::Authorized,
                "authorization_mode=autonomous_full_access; pending Agent request resumed"
                    .to_string(),
            ));
        }
        Some(request)
    }

    /// Resolve a request. `Some(lifetime)` approves (records a grant scoped
    /// to the requested capability + resource); `None` denies. Returns true
    /// on approval. Either way the request leaves the queue — except when
    /// persisting a `Persistent` approval fails, in which case nothing is
    /// granted and the request stays pending for a retry.
    pub fn decide(&self, id: &str, lifetime: Option<GrantLifetime>) -> Result<bool, QueueError> {
        let _span = tracing::info_span!("permission.decide", request = %id).entered();
        // Phase 1: peek at the request without removing it, so a storage
        // failure below cannot lose a pending approval.
        let grant = {
            let inner = self.inner.lock().expect("approval queue lock");
            let request = inner
                .pending
                .get(id)
                .ok_or_else(|| QueueError::UnknownId(id.to_string()))?;
            lifetime.map(|lifetime| {
                let lifetime = if request.requires_once {
                    GrantLifetime::Once
                } else {
                    lifetime
                };
                GrantedScope::new(
                    request.principal.kind(),
                    request.capability.clone(),
                    capability_core::ResourceScope::new(vec![request.resource.clone()]),
                    lifetime,
                    request.task_id.clone(),
                    request.turn_id.clone(),
                )
            })
        };
        // Phase 2: durable write first (outside the queue lock — the hook
        // does I/O). Only `Persistent` grants reach storage; without a hook
        // (tests, ephemeral hosts) they live in memory like the rest.
        if let Some(grant) = grant
            .as_ref()
            .filter(|g| g.lifetime == GrantLifetime::Persistent)
        {
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

    /// Record a grant directly, without a pending request. This is the
    /// explicit user path (settings UI): the caller already holds user
    /// intent, so no dialog is involved — but persistence is still
    /// fail-closed like [`ApprovalQueue::decide`], and the decision is
    /// audit-logged either way.
    pub fn grant_direct(
        &self,
        principal_kind: PrincipalKind,
        capability: Capability,
        scope: capability_core::ResourceScope,
        lifetime: GrantLifetime,
        reason: String,
    ) -> Result<GrantedScope, QueueError> {
        let grant = GrantedScope::new(
            principal_kind,
            capability.clone(),
            scope.clone(),
            lifetime,
            None,
            None,
        );
        if lifetime == GrantLifetime::Persistent {
            if let Some(persist) = &self.persist {
                persist(&grant).map_err(QueueError::Persist)?;
            }
        }
        {
            let mut inner = self.inner.lock().expect("approval queue lock");
            inner.grants.push(grant.clone());
        }
        if let Some(sink) = &self.sink {
            sink.record(audit_core::AuditRecord::now(
                Principal::Agent(capability_core::AgentId::new("user-grant")),
                Some(capability),
                scope.resources.first().cloned(),
                audit_core::AuditOutcome::Approved,
                reason,
            ));
        }
        Ok(grant)
    }

    /// Drop every in-memory grant matching a capability + scope. Returns
    /// how many were removed. Storage rows are the caller's job (they
    /// need row ids from `load_grants`); call this alongside so a
    /// revoked grant stops authorizing immediately, even before restart.
    pub fn revoke_where(
        &self,
        capability: &Capability,
        scope: &capability_core::ResourceScope,
    ) -> usize {
        let mut inner = self.inner.lock().expect("approval queue lock");
        let before = inner.grants.len();
        inner
            .grants
            .retain(|g| !(g.capability == *capability && g.scope == *scope));
        before - inner.grants.len()
    }

    /// Snapshot of standing grants (seed + approvals) for agent turns.
    pub fn context(&self) -> AuthorizationContext {
        self.context_for(None, None)
    }

    /// Snapshot grants together with the task/turn identity being resumed.
    pub fn context_for(
        &self,
        task_id: Option<String>,
        turn_id: Option<String>,
    ) -> AuthorizationContext {
        let inner = self.inner.lock().expect("approval queue lock");
        AuthorizationContext {
            grants: inner.grants.clone(),
            task_id,
            turn_id,
        }
    }

    /// Evaluate a request against a live task/turn snapshot.
    pub fn authorize_for(
        &self,
        principal: &Principal,
        request: &CapabilityRequest,
        task_id: Option<String>,
        turn_id: Option<String>,
    ) -> AuthorizationDecision {
        let context = self.context_for(task_id, turn_id);
        authorize(principal, request, &context)
    }

    /// Atomically consume a grant immediately before a privileged tool is
    /// invoked. Once grants disappear here, a retry or a second call cannot
    /// execute through the same approval.
    pub fn consume_for(
        &self,
        principal: &Principal,
        request: &CapabilityRequest,
        task_id: Option<&str>,
        turn_id: Option<&str>,
    ) -> bool {
        if *principal == Principal::User {
            return true;
        }
        let context = AuthorizationContext {
            grants: Vec::new(),
            task_id: task_id.map(str::to_string),
            turn_id: turn_id.map(str::to_string),
        };
        let mut inner = self.inner.lock().expect("approval queue lock");
        let Some(index) = inner.grants.iter().position(|grant| {
            grant.principal_kind == principal.kind()
                && grant.capability == request.capability
                && grant.scope.allows(&request.resource)
                && grant.is_active(&context)
        }) else {
            return false;
        };
        if inner.grants[index].lifetime == GrantLifetime::Once {
            inner.grants.remove(index);
        }
        true
    }

    /// Drop task and once grants when a task reaches a terminal state.
    pub fn end_task(&self, task_id: &str) {
        let mut inner = self.inner.lock().expect("approval queue lock");
        inner.grants.retain(|grant| {
            !matches!(grant.lifetime, GrantLifetime::Task | GrantLifetime::Once)
                || grant.task_id.as_deref() != Some(task_id)
        });
        inner
            .pending
            .retain(|_, request| request.task_id.as_deref() != Some(task_id));
    }
}

/// Narrow helper used by brokers: does this resource look like a secret
/// path that should never be silently allowed? Defense in depth on top of
/// scope checks.
pub fn is_secret_path(resource: &Resource) -> bool {
    match resource {
        Resource::Path(p) => {
            use std::path::Component;
            p.components().any(|component| {
                let Component::Normal(name) = component else {
                    return false;
                };
                let name = name.to_string_lossy();
                matches!(
                    name.to_ascii_lowercase().as_str(),
                    ".ssh" | ".aws" | ".gnupg" | ".credentials" | ".config" | ".pki" | "secrets"
                )
            })
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
        AuthorizationContext {
            grants: vec![],
            ..AuthorizationContext::default()
        }
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
        assert!(matches!(
            d,
            AuthorizationDecision::RequireUserApproval { .. }
        ));
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
                ..GrantedScope::new(
                    PrincipalKind::Agent,
                    Capability::FilesystemRead,
                    ResourceScope::new(vec![Resource::Path(PathBuf::from("/work"))]),
                    GrantLifetime::Session,
                    Some("task-1".to_string()),
                    Some("turn-1".to_string()),
                )
            }],
            task_id: Some("task-1".to_string()),
            turn_id: Some("turn-1".to_string()),
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
        assert!(matches!(
            d,
            AuthorizationDecision::RequireUserApproval { .. }
        ));
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
                ..GrantedScope::new(
                    PrincipalKind::BuiltinTool,
                    Capability::FilesystemWrite,
                    ResourceScope::new(vec![Resource::Path(PathBuf::from("/work"))]),
                    GrantLifetime::Task,
                    Some("task-1".to_string()),
                    Some("turn-1".to_string()),
                )
            }],
            task_id: Some("task-1".to_string()),
            turn_id: Some("turn-1".to_string()),
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
    fn dangerous_interpreters_always_need_fresh_approval() {
        let principal = Principal::Agent(AgentId::new("a"));
        let resource = Resource::Process {
            executable: PathBuf::from("/usr/bin/python3.12"),
            args: vec!["script.py".to_string()],
            cwd: PathBuf::from("/work"),
            env: vec![],
        };
        let request = CapabilityRequest {
            principal: principal.clone(),
            capability: Capability::ProcessSpawn,
            resource: resource.clone(),
        };
        let grant = GrantedScope::new(
            PrincipalKind::Agent,
            Capability::ProcessSpawn,
            ResourceScope::new(vec![resource]),
            GrantLifetime::Persistent,
            None,
            None,
        );
        let decision = authorize(
            &principal,
            &request,
            &AuthorizationContext {
                grants: vec![grant],
                ..AuthorizationContext::default()
            },
        );
        assert!(matches!(
            decision,
            AuthorizationDecision::RequireUserApproval { .. }
        ));
    }

    #[test]
    fn sensitive_approvals_resume_once_even_if_broader_lifetime_was_requested() {
        let queue = ApprovalQueue::new();
        let principal = Principal::Agent(AgentId::new("a"));
        let task_id = Some("task-sensitive".to_string());
        let turn_id = Some("turn-sensitive".to_string());
        let resource = Resource::Path(PathBuf::from("/work/.ssh/id_rsa"));
        let request = CapabilityRequest {
            principal: principal.clone(),
            capability: Capability::FilesystemRead,
            resource: resource.clone(),
        };
        let pending = queue.submit_for_task(
            principal.clone(),
            request.capability.clone(),
            resource,
            "read key".to_string(),
            task_id.clone(),
            turn_id.clone(),
        );
        assert!(pending.requires_once);
        assert_eq!(
            queue.decide(&pending.id, Some(GrantLifetime::Session)),
            Ok(true)
        );
        assert_eq!(queue.grants_snapshot()[0].lifetime, GrantLifetime::Once);
        assert!(matches!(
            queue.authorize_for(&principal, &request, task_id, turn_id),
            AuthorizationDecision::Allow { .. }
        ));
        assert!(queue.consume_for(
            &principal,
            &request,
            Some("task-sensitive"),
            Some("turn-sensitive")
        ));
        assert!(matches!(
            queue.authorize_for(
                &principal,
                &request,
                Some("task-sensitive".to_string()),
                Some("turn-sensitive".to_string())
            ),
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
    fn withdraw_agent_only_removes_agent_requests_without_granting() {
        let queue = ApprovalQueue::new();
        let agent = queue.submit(
            Principal::Agent(AgentId::new("a")),
            Capability::FilesystemWrite,
            Resource::Path(PathBuf::from("/work/file")),
            "write".to_string(),
        );
        let frontend = queue.submit(
            Principal::Frontend,
            Capability::FilesystemWrite,
            Resource::Path(PathBuf::from("/work/frontend-file")),
            "frontend write".to_string(),
        );

        assert!(queue.withdraw_agent(&agent.id).is_some());
        assert!(queue.withdraw_agent(&frontend.id).is_none());
        assert_eq!(queue.list().len(), 1);
        assert!(queue.grants_snapshot().is_empty());
    }

    #[test]
    fn grant_lifetimes_are_bound_and_consumed() {
        let queue = ApprovalQueue::new();
        let principal = Principal::Agent(AgentId::new("a"));
        let request = CapabilityRequest {
            principal: principal.clone(),
            capability: Capability::FilesystemWrite,
            resource: Resource::Path(PathBuf::from("/work/file.txt")),
        };

        let once = queue.submit_for_task(
            principal.clone(),
            request.capability.clone(),
            request.resource.clone(),
            "write once".to_string(),
            Some("task-1".to_string()),
            Some("turn-1".to_string()),
        );
        assert_eq!(queue.decide(&once.id, Some(GrantLifetime::Once)), Ok(true));
        assert!(matches!(
            queue.authorize_for(
                &principal,
                &request,
                Some("task-1".to_string()),
                Some("turn-1".to_string())
            ),
            AuthorizationDecision::Allow { .. }
        ));
        assert!(queue.consume_for(&principal, &request, Some("task-1"), Some("turn-1")));
        assert!(!queue.consume_for(&principal, &request, Some("task-1"), Some("turn-1")));
        assert!(matches!(
            queue.authorize_for(
                &principal,
                &request,
                Some("task-1".to_string()),
                Some("turn-1".to_string())
            ),
            AuthorizationDecision::RequireUserApproval { .. }
        ));

        let task = queue.submit_for_task(
            principal.clone(),
            request.capability.clone(),
            request.resource.clone(),
            "write during task".to_string(),
            Some("task-2".to_string()),
            Some("turn-2".to_string()),
        );
        assert_eq!(queue.decide(&task.id, Some(GrantLifetime::Task)), Ok(true));
        assert!(matches!(
            queue.authorize_for(
                &principal,
                &request,
                Some("task-2".to_string()),
                Some("turn-2".to_string())
            ),
            AuthorizationDecision::Allow { .. }
        ));
        assert!(matches!(
            queue.authorize_for(
                &principal,
                &request,
                Some("other-task".to_string()),
                Some("turn-2".to_string())
            ),
            AuthorizationDecision::RequireUserApproval { .. }
        ));
        queue.end_task("task-2");
        assert!(matches!(
            queue.authorize_for(
                &principal,
                &request,
                Some("task-2".to_string()),
                Some("turn-2".to_string())
            ),
            AuthorizationDecision::RequireUserApproval { .. }
        ));

        let session = queue.submit(
            principal.clone(),
            request.capability.clone(),
            request.resource.clone(),
            "write for session".to_string(),
        );
        assert_eq!(
            queue.decide(&session.id, Some(GrantLifetime::Session)),
            Ok(true)
        );
        assert!(matches!(
            queue.authorize_for(&principal, &request, Some("new-task".to_string()), None),
            AuthorizationDecision::Allow { .. }
        ));
    }

    #[test]
    fn persistent_approve_calls_hook_before_granting() {
        use std::sync::{Arc, Mutex};
        let seen = Arc::new(Mutex::new(Vec::new()));
        let probe = seen.clone();
        let queue =
            ApprovalQueue::new().on_persistent_grant(Arc::new(move |grant: &GrantedScope| {
                probe.lock().unwrap().push(grant.clone());
                Ok(())
            }));
        let req = queue.submit(
            Principal::Agent(AgentId::new("a")),
            Capability::FilesystemRead,
            Resource::Path(PathBuf::from("/work/f")),
            "read".to_string(),
        );
        assert_eq!(
            queue.decide(&req.id, Some(GrantLifetime::Persistent)),
            Ok(true)
        );
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
        assert_eq!(
            queue.decide(&req.id, Some(GrantLifetime::Session)),
            Ok(true)
        );
        assert_eq!(*calls.lock().unwrap(), 0);
        assert_eq!(queue.context().grants.len(), 1);
    }

    #[test]
    fn persist_failure_leaves_request_pending() {
        use std::sync::Arc;
        let queue = ApprovalQueue::new()
            .on_persistent_grant(Arc::new(|_: &GrantedScope| Err("disk full".to_string())));
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
            ..GrantedScope::new(
                PrincipalKind::Agent,
                Capability::FilesystemRead,
                ResourceScope::new(vec![Resource::Path(PathBuf::from("/work"))]),
                GrantLifetime::Persistent,
                None,
                None,
            )
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
            ..GrantedScope::new(
                PrincipalKind::Agent,
                Capability::FilesystemRead,
                ResourceScope::new(vec![Resource::Path(PathBuf::from("/work"))]),
                GrantLifetime::Session,
                None,
                None,
            )
        }]);
    }

    #[test]
    fn secret_paths_are_detected() {
        assert!(is_secret_path(&Resource::Path(PathBuf::from(
            "/home/u/.ssh/id_ed25519"
        ))));
        for directory in [
            ".aws",
            ".gnupg",
            ".credentials",
            ".config",
            ".pki",
            "secrets",
        ] {
            assert!(is_secret_path(&Resource::Path(
                PathBuf::from("/home/u").join(directory).join("file")
            )));
        }
        assert!(!is_secret_path(&Resource::Path(PathBuf::from(
            "/work/main.rs"
        ))));
    }

    #[cfg(windows)]
    #[test]
    fn windows_secret_directories_use_path_components() {
        assert!(is_secret_path(&Resource::Path(PathBuf::from(
            r"C:\Users\Bob\.ssh\id_ed25519"
        ))));
        assert!(is_secret_path(&Resource::Path(PathBuf::from(
            r"C:\Users\Bob\.AWS\credentials"
        ))));
        assert!(!is_secret_path(&Resource::Path(PathBuf::from(
            r"C:\Users\Bob\notes.txt"
        ))));
    }

    fn grant_home() -> GrantedScope {
        GrantedScope::new(
            PrincipalKind::Agent,
            Capability::FilesystemRead,
            ResourceScope::new(vec![Resource::Path(PathBuf::from("/home/u"))]),
            GrantLifetime::Persistent,
            None,
            None,
        )
    }

    #[test]
    fn broad_grant_covers_ordinary_files_but_never_secrets() {
        let ctx = AuthorizationContext {
            grants: vec![grant_home()],
            ..AuthorizationContext::default()
        };
        let ordinary = CapabilityRequest {
            principal: Principal::Agent(AgentId::new("a")),
            capability: Capability::FilesystemRead,
            resource: Resource::Path(PathBuf::from("/home/u/docs/note.txt")),
        };
        assert!(matches!(
            authorize(&ordinary.principal, &ordinary, &ctx),
            AuthorizationDecision::Allow { .. }
        ));
        let secret = CapabilityRequest {
            resource: Resource::Path(PathBuf::from("/home/u/.ssh/id_ed25519")),
            ..ordinary
        };
        assert!(matches!(
            authorize(&secret.principal, &secret, &ctx),
            AuthorizationDecision::RequireUserApproval { .. }
        ));
    }

    #[test]
    fn grant_direct_persists_and_revoke_drops() {
        use std::sync::{Arc, Mutex};
        let stored: Arc<Mutex<Vec<GrantedScope>>> = Arc::new(Mutex::new(Vec::new()));
        let hook_store = Arc::clone(&stored);
        let queue =
            ApprovalQueue::new().on_persistent_grant(Arc::new(move |grant: &GrantedScope| {
                hook_store.lock().expect("hook lock").push(grant.clone());
                Ok(())
            }));
        let grant = queue
            .grant_direct(
                PrincipalKind::Agent,
                Capability::FilesystemRead,
                ResourceScope::new(vec![Resource::Path(PathBuf::from("/home/u"))]),
                GrantLifetime::Persistent,
                "user broad-read grant".to_string(),
            )
            .unwrap();
        assert_eq!(stored.lock().expect("hook lock").len(), 1);
        assert_eq!(queue.grants_snapshot(), vec![grant.clone()]);
        // Session grants skip storage but still take effect in memory.
        queue
            .grant_direct(
                PrincipalKind::Agent,
                Capability::FilesystemRead,
                ResourceScope::new(vec![Resource::Path(PathBuf::from("/tmp"))]),
                GrantLifetime::Session,
                "session grant".to_string(),
            )
            .unwrap();
        assert_eq!(stored.lock().expect("hook lock").len(), 1);
        assert_eq!(queue.grants_snapshot().len(), 2);
        // Revoke drops exactly the matching scope.
        assert_eq!(
            queue.revoke_where(
                &Capability::FilesystemRead,
                &ResourceScope::new(vec![Resource::Path(PathBuf::from("/home/u"))]),
            ),
            1
        );
        assert_eq!(queue.grants_snapshot().len(), 1);
        assert_eq!(
            queue.revoke_where(
                &Capability::FilesystemRead,
                &ResourceScope::new(vec![Resource::Path(PathBuf::from("/nowhere"))]),
            ),
            0
        );
    }
}
