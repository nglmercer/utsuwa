//! Principal identity, capabilities, resource scopes, capability tickets.
//!
//! Architectural invariant: no privileged broker API may be called without a
//! [`Principal`]. Identity is typed — never a bare string in trusted code.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(id: impl Into<String>) -> Self {
                Self(id.into())
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0)
            }
        }
    };
}

id_type!(AgentId);
id_type!(ToolId);
id_type!(PluginId);
id_type!(ServerId);

/// Typed invocation / ticket identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InvocationId(pub uuid::Uuid);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TicketId(pub uuid::Uuid);

impl InvocationId {
    pub fn fresh() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

impl TicketId {
    pub fn fresh() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

/// Who is asking. Every privileged request carries one of these.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Principal {
    User,
    Frontend,
    Agent(AgentId),
    BuiltinTool(ToolId),
    WasmPlugin(PluginId),
    McpServer(ServerId),
    NativePlugin(PluginId),
}

/// Principal with identity stripped: grants apply to a class, not a name,
/// so a fresh agent id cannot dodge a deny — and cannot inherit an allow
/// minted for a different class either.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PrincipalKind {
    User,
    Frontend,
    Agent,
    BuiltinTool,
    WasmPlugin,
    McpServer,
    NativePlugin,
}

impl Principal {
    pub fn kind(&self) -> PrincipalKind {
        match self {
            Principal::User => PrincipalKind::User,
            Principal::Frontend => PrincipalKind::Frontend,
            Principal::Agent(_) => PrincipalKind::Agent,
            Principal::BuiltinTool(_) => PrincipalKind::BuiltinTool,
            Principal::WasmPlugin(_) => PrincipalKind::WasmPlugin,
            Principal::McpServer(_) => PrincipalKind::McpServer,
            Principal::NativePlugin(_) => PrincipalKind::NativePlugin,
        }
    }
}

/// Explicit authority. Structured enums, not strings.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Capability {
    FilesystemRead,
    FilesystemWrite,
    FilesystemCreate,
    FilesystemDelete,
    FilesystemMove,
    ProcessSpawn,
    ProcessSignal,
    NetworkConnect,
    DesktopObserve,
    DesktopControl,
    ScreenCapture,
    ClipboardRead,
    ClipboardWrite,
    ApplicationLaunch,
}

/// Concrete resource a capability acts on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Resource {
    Path(PathBuf),
    HostPort { host: String, port: u16 },
    Executable(PathBuf),
    Application(String),
    Window(String),
}

/// A granted scope: the set of resources a capability may touch.
/// Path scopes match by canonical-prefix; other resources match exactly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceScope {
    pub resources: Vec<Resource>,
}

impl ResourceScope {
    pub fn new(resources: Vec<Resource>) -> Self {
        Self { resources }
    }

    /// Scope check without filesystem access: compares lexical
    /// normalization (caller canonicalizes before calling). Never accepts a
    /// scope escape — a resource outside every granted root fails closed.
    pub fn allows(&self, resource: &Resource) -> bool {
        self.resources
            .iter()
            .any(|scope| scope_covers(scope, resource))
    }
}

fn normalized(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        use std::path::Component::*;
        match comp {
            CurDir => {}
            ParentDir => {
                out.pop();
            }
            c => out.push(c.as_os_str()),
        }
    }
    out
}

fn path_covers(scope: &Path, resource: &Path) -> bool {
    let scope = normalized(scope);
    let resource = normalized(resource);
    resource.starts_with(&scope)
}

fn scope_covers(scope: &Resource, resource: &Resource) -> bool {
    match (scope, resource) {
        (Resource::Path(s), Resource::Path(r)) => path_covers(s, r),
        (Resource::Executable(s), Resource::Executable(r)) => {
            path_covers(s, r) || normalized(s).file_name() == normalized(r).file_name()
        }
        (Resource::HostPort { host: sh, port: sp }, Resource::HostPort { host: rh, port: rp }) => {
            sh.eq_ignore_ascii_case(rh) && sp == rp
        }
        (Resource::Application(s), Resource::Application(r)) => s == r,
        (Resource::Window(s), Resource::Window(r)) => s == r,
        _ => false,
    }
}

/// A privileged request: who wants which capability on what resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityRequest {
    pub principal: Principal,
    pub capability: Capability,
    pub resource: Resource,
}

/// A policy decision is not OS authority: an allowed request mints a
/// short-lived, scoped, invocation-bound ticket, and only the ticket opens
/// the broker.
#[derive(Debug, Clone)]
pub struct CapabilityTicket {
    pub id: TicketId,
    pub principal: Principal,
    pub capability: Capability,
    pub scope: ResourceScope,
    pub invocation_id: InvocationId,
    pub expires_at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TicketError {
    #[error("ticket expired")]
    Expired,
    #[error("ticket does not cover the requested capability")]
    CapabilityMismatch,
    #[error("ticket scope does not cover the requested resource")]
    ScopeMismatch,
    #[error("ticket is bound to a different invocation")]
    InvocationMismatch,
    #[error("ticket is bound to a different principal")]
    PrincipalMismatch,
}

impl CapabilityTicket {
    pub fn mint(
        principal: Principal,
        capability: Capability,
        scope: ResourceScope,
        invocation_id: InvocationId,
        ttl: Duration,
    ) -> Self {
        Self {
            id: TicketId::fresh(),
            principal,
            capability,
            scope,
            invocation_id,
            expires_at: Instant::now() + ttl,
        }
    }

    /// Validate a ticket against a live request. Fails closed on every
    /// mismatch; no panics on untrusted input.
    pub fn check(
        &self,
        principal: &Principal,
        request: &CapabilityRequest,
        invocation_id: &InvocationId,
    ) -> Result<(), TicketError> {
        if Instant::now() > self.expires_at {
            return Err(TicketError::Expired);
        }
        if &self.principal != principal || &request.principal != principal {
            return Err(TicketError::PrincipalMismatch);
        }
        if self.capability != request.capability {
            return Err(TicketError::CapabilityMismatch);
        }
        if &self.invocation_id != invocation_id {
            return Err(TicketError::InvocationMismatch);
        }
        if !self.scope.allows(&request.resource) {
            return Err(TicketError::ScopeMismatch);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(principal: Principal, capability: Capability, resource: Resource) -> CapabilityRequest {
        CapabilityRequest {
            principal,
            capability,
            resource,
        }
    }

    #[test]
    fn path_scope_covers_children_but_not_siblings_or_escapes() {
        let scope = ResourceScope::new(vec![Resource::Path(PathBuf::from("/home/u/Projects"))]);
        assert!(scope.allows(&Resource::Path(PathBuf::from("/home/u/Projects/app/main.rs"))));
        assert!(!scope.allows(&Resource::Path(PathBuf::from("/home/u/Other/x"))));
        // `..` that escapes the root fails closed.
        assert!(!scope.allows(&Resource::Path(PathBuf::from(
            "/home/u/Projects/../.ssh/id_ed25519"
        ))));
    }

    #[test]
    fn host_port_match_is_exact_and_case_insensitive_on_host() {
        let scope = ResourceScope::new(vec![Resource::HostPort {
            host: "api.github.com".to_string(),
            port: 443,
        }]);
        assert!(scope.allows(&Resource::HostPort {
            host: "API.GitHub.COM".to_string(),
            port: 443,
        }));
        assert!(!scope.allows(&Resource::HostPort {
            host: "api.github.com".to_string(),
            port: 80,
        }));
        assert!(!scope.allows(&Resource::HostPort {
            host: "evil.example".to_string(),
            port: 443,
        }));
    }

    #[test]
    fn valid_ticket_passes_and_wrong_resource_fails() {
        let principal = Principal::Agent(AgentId::new("a1"));
        let inv = InvocationId::fresh();
        let ticket = CapabilityTicket::mint(
            principal.clone(),
            Capability::FilesystemRead,
            ResourceScope::new(vec![Resource::Path(PathBuf::from("/home/u/Projects"))]),
            inv.clone(),
            Duration::from_secs(60),
        );
        let ok = req(
            principal.clone(),
            Capability::FilesystemRead,
            Resource::Path(PathBuf::from("/home/u/Projects/app/main.rs")),
        );
        assert!(ticket.check(&principal, &ok, &inv).is_ok());

        let outside = req(
            principal.clone(),
            Capability::FilesystemRead,
            Resource::Path(PathBuf::from("/etc/passwd")),
        );
        assert_eq!(
            ticket.check(&principal, &outside, &inv),
            Err(TicketError::ScopeMismatch)
        );
    }

    #[test]
    fn ticket_rejects_wrong_capability_invocation_and_principal() {
        let principal = Principal::BuiltinTool(ToolId::new("filesystem.read"));
        let inv = InvocationId::fresh();
        let ticket = CapabilityTicket::mint(
            principal.clone(),
            Capability::FilesystemRead,
            ResourceScope::new(vec![Resource::Path(PathBuf::from("/work"))]),
            inv.clone(),
            Duration::from_secs(60),
        );
        let r = |cap| {
            req(
                principal.clone(),
                cap,
                Resource::Path(PathBuf::from("/work/f")),
            )
        };
        assert_eq!(
            ticket.check(&principal, &r(Capability::FilesystemWrite), &inv),
            Err(TicketError::CapabilityMismatch)
        );
        assert_eq!(
            ticket.check(&principal, &r(Capability::FilesystemRead), &InvocationId::fresh()),
            Err(TicketError::InvocationMismatch)
        );
        let other = Principal::Agent(AgentId::new("x"));
        assert_eq!(
            ticket.check(&other, &r(Capability::FilesystemRead), &inv),
            Err(TicketError::PrincipalMismatch)
        );
    }

    #[test]
    fn zero_ttl_ticket_is_expired() {
        let principal = Principal::User;
        let inv = InvocationId::fresh();
        let ticket = CapabilityTicket::mint(
            principal.clone(),
            Capability::ScreenCapture,
            ResourceScope::new(vec![]),
            inv.clone(),
            Duration::from_secs(0),
        );
        // Empty scope + immediate expiry: both fail; expiry is checked first.
        let r = req(principal.clone(), Capability::ScreenCapture, Resource::Window("w".into()));
        assert_eq!(
            ticket.check(&principal, &r, &inv),
            Err(TicketError::Expired)
        );
    }
}
