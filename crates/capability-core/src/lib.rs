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
    /// Observe a camera device (still photo or frame stream). Always requires
    /// explicit user authorization; never implied by screen-capture grants.
    CameraObserve,
    /// Capture microphone audio. Always requires explicit user authorization;
    /// never implied by any other capability.
    MicrophoneCapture,
    /// Show a user-visible OS notification. Low-friction policy, but the
    /// authority is explicit: user-visible external behavior is never
    /// ambient.
    NotificationSend,
    /// Invoke a tool served by an external MCP server. Never implied by
    /// other capabilities: every MCP tool call authorizes on its own.
    McpInvoke,
    /// Invoke a tool served by a WASM plugin. Same rule as [`Capability::McpInvoke`]:
    /// third-party guest code authorizes per call, never by implication.
    PluginInvoke,
}

/// Concrete resource a capability acts on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Resource {
    Path(PathBuf),
    HostPort {
        host: String,
        port: u16,
    },
    Executable(PathBuf),
    /// One fully specified process invocation. Arguments, cwd, and explicit
    /// environment deltas are part of the authority so an approval for one
    /// command cannot silently expand into "this executable with anything".
    Process {
        executable: PathBuf,
        args: Vec<String>,
        cwd: PathBuf,
        env: Vec<(String, String)>,
    },
    Application(String),
    /// A camera device selected for capture. Device identities are matched
    /// exactly; a grant for one camera never covers another.
    Camera(String),
    /// A microphone device selected for capture. Matched exactly, like
    /// [`Resource::Camera`]: a grant for one microphone never covers
    /// another, and never falls back to the default input device.
    AudioDevice(String),
    /// The system clipboard as a whole. Clipboard contents can hold
    /// secrets; read and write tickets are always explicit and per call.
    Clipboard,
    /// The OS notification service. Used with
    /// [`Capability::NotificationSend`].
    NotificationService,
    /// One browser tab selected for observation or control. Tab identities
    /// are matched exactly; a tab grant never covers a desktop window and
    /// a desktop-window grant never covers a tab.
    BrowserTab(String),
    /// A URL being navigated, fetched, or submitted to. The scheme and host
    /// match case-insensitively; the port matches exactly. Used for
    /// domain-based browser/HTTP authorization.
    Url {
        scheme: String,
        host: String,
        port: u16,
    },
    Window(String),
    /// A display/output selected for screen capture. Display identities are
    /// matched exactly; a display grant never implicitly covers a window or
    /// the whole desktop.
    Display(String),
    /// A tool served by an MCP server. Matched exactly (server + tool):
    /// a grant for one MCP tool never covers another.
    McpTool {
        server: String,
        tool: String,
    },
    /// A tool served by a WASM plugin. Matched exactly like [`Resource::McpTool`].
    PluginTool {
        plugin: String,
        tool: String,
    },
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
    /// normalization (callers canonicalize filesystem resources before
    /// calling). Never accepts a scope escape — a resource outside every
    /// granted root fails closed.
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
        (Resource::Executable(s), Resource::Executable(r)) => path_covers(s, r),
        (
            Resource::Process {
                executable: se,
                args: sa,
                cwd: sc,
                env: sv,
            },
            Resource::Process {
                executable: re,
                args: ra,
                cwd: rc,
                env: rv,
            },
        ) => {
            normalized(se) == normalized(re)
                && sa == ra
                && normalized(sc) == normalized(rc)
                && sv == rv
        }
        (Resource::HostPort { host: sh, port: sp }, Resource::HostPort { host: rh, port: rp }) => {
            sh.eq_ignore_ascii_case(rh) && sp == rp
        }
        (Resource::Application(s), Resource::Application(r)) => s == r,
        (Resource::Camera(s), Resource::Camera(r)) => s == r,
        (Resource::AudioDevice(s), Resource::AudioDevice(r)) => s == r,
        (Resource::Clipboard, Resource::Clipboard) => true,
        (Resource::NotificationService, Resource::NotificationService) => true,
        (Resource::BrowserTab(s), Resource::BrowserTab(r)) => s == r,
        (
            Resource::Url {
                scheme: ss,
                host: sh,
                port: sp,
            },
            Resource::Url {
                scheme: rs,
                host: rh,
                port: rp,
            },
        ) => ss.eq_ignore_ascii_case(rs) && sh.eq_ignore_ascii_case(rh) && sp == rp,
        (Resource::Window(s), Resource::Window(r)) => s == r,
        (Resource::Display(s), Resource::Display(r)) => s == r,
        (
            Resource::McpTool {
                server: ss,
                tool: st,
            },
            Resource::McpTool {
                server: rs,
                tool: rt,
            },
        ) => ss == rs && st == rt,
        (
            Resource::PluginTool {
                plugin: sp,
                tool: st,
            },
            Resource::PluginTool {
                plugin: rp,
                tool: rt,
            },
        ) => sp == rp && st == rt,
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
        assert!(scope.allows(&Resource::Path(PathBuf::from(
            "/home/u/Projects/app/main.rs"
        ))));
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
    fn process_scope_covers_only_the_approved_invocation() {
        let scope = ResourceScope::new(vec![Resource::Process {
            executable: PathBuf::from("/usr/bin/python3"),
            args: vec!["script.py".to_string()],
            cwd: PathBuf::from("/work"),
            env: vec![("MODE".to_string(), "check".to_string())],
        }]);
        let same = Resource::Process {
            executable: PathBuf::from("/usr/bin/./python3"),
            args: vec!["script.py".to_string()],
            cwd: PathBuf::from("/work/./"),
            env: vec![("MODE".to_string(), "check".to_string())],
        };
        assert!(scope.allows(&same));
        assert!(!scope.allows(&Resource::Process {
            executable: PathBuf::from("/usr/bin/python3"),
            args: vec!["other.py".to_string()],
            cwd: PathBuf::from("/work"),
            env: vec![("MODE".to_string(), "check".to_string())],
        }));
        assert!(!scope.allows(&Resource::Process {
            executable: PathBuf::from("/usr/bin/python3"),
            args: vec!["script.py".to_string()],
            cwd: PathBuf::from("/tmp"),
            env: vec![("MODE".to_string(), "check".to_string())],
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
            inv,
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
            inv,
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
            ticket.check(
                &principal,
                &r(Capability::FilesystemRead),
                &InvocationId::fresh()
            ),
            Err(TicketError::InvocationMismatch)
        );
        let other = Principal::Agent(AgentId::new("x"));
        assert_eq!(
            ticket.check(&other, &r(Capability::FilesystemRead), &inv),
            Err(TicketError::PrincipalMismatch)
        );
    }

    #[test]
    fn url_scope_matches_host_case_insensitively_and_port_exactly() {
        let scope = ResourceScope::new(vec![Resource::Url {
            scheme: "https".to_string(),
            host: "example.com".to_string(),
            port: 443,
        }]);
        assert!(scope.allows(&Resource::Url {
            scheme: "HTTPS".to_string(),
            host: "EXAMPLE.com".to_string(),
            port: 443,
        }));
        assert!(!scope.allows(&Resource::Url {
            scheme: "https".to_string(),
            host: "example.com".to_string(),
            port: 80,
        }));
        assert!(!scope.allows(&Resource::Url {
            scheme: "http".to_string(),
            host: "example.com".to_string(),
            port: 443,
        }));
        assert!(!scope.allows(&Resource::Url {
            scheme: "https".to_string(),
            host: "evil.example.com".to_string(),
            port: 443,
        }));
    }

    #[test]
    fn camera_scope_is_device_exact() {
        let scope = ResourceScope::new(vec![Resource::Camera("front".to_string())]);
        assert!(scope.allows(&Resource::Camera("front".to_string())));
        assert!(!scope.allows(&Resource::Camera("rear".to_string())));
        // Camera grants never cover screen capture resources and vice versa.
        assert!(!scope.allows(&Resource::Window("w1".to_string())));
    }

    #[test]
    fn audio_device_scope_is_device_exact() {
        let scope = ResourceScope::new(vec![Resource::AudioDevice("mic-a".to_string())]);
        assert!(scope.allows(&Resource::AudioDevice("mic-a".to_string())));
        assert!(!scope.allows(&Resource::AudioDevice("mic-b".to_string())));
        assert!(!scope.allows(&Resource::AudioDevice("default".to_string())));
        // A microphone grant never covers a camera and never covers the
        // legacy overloaded application-shaped device scope.
        assert!(!scope.allows(&Resource::Camera("mic-a".to_string())));
        assert!(!scope.allows(&Resource::Application("mic-a".to_string())));
    }

    #[test]
    fn clipboard_and_notification_resources_match_only_themselves() {
        let clipboard = ResourceScope::new(vec![Resource::Clipboard]);
        assert!(clipboard.allows(&Resource::Clipboard));
        assert!(!clipboard.allows(&Resource::Application("clipboard".to_string())));

        let notifications = ResourceScope::new(vec![Resource::NotificationService]);
        assert!(notifications.allows(&Resource::NotificationService));
        assert!(!notifications.allows(&Resource::Application("notifications".to_string())));
    }

    #[test]
    fn browser_tab_scope_never_covers_desktop_windows() {
        let scope = ResourceScope::new(vec![Resource::BrowserTab("tab-1".to_string())]);
        assert!(scope.allows(&Resource::BrowserTab("tab-1".to_string())));
        assert!(!scope.allows(&Resource::BrowserTab("tab-2".to_string())));
        // A tab grant is not a desktop-window grant and vice versa: the
        // two surfaces authorize independently.
        assert!(!scope.allows(&Resource::Window("tab-1".to_string())));
        let windows = ResourceScope::new(vec![Resource::Window("tab-1".to_string())]);
        assert!(!windows.allows(&Resource::BrowserTab("tab-1".to_string())));
    }

    #[test]
    fn zero_ttl_ticket_is_expired() {
        let principal = Principal::User;
        let inv = InvocationId::fresh();
        let ticket = CapabilityTicket::mint(
            principal.clone(),
            Capability::ScreenCapture,
            ResourceScope::new(vec![]),
            inv,
            Duration::from_secs(0),
        );
        // Empty scope + immediate expiry: both fail; expiry is checked first.
        let r = req(
            principal.clone(),
            Capability::ScreenCapture,
            Resource::Window("w".into()),
        );
        assert_eq!(
            ticket.check(&principal, &r, &inv),
            Err(TicketError::Expired)
        );
    }
}
