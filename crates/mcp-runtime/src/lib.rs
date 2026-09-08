//! MCP runtime (plan Phase 21): external MCP servers as a tool source.
//!
//! Flow: MCP server → discover tools → [`McpToolBridge`] → [`ToolRegistry`]
//! → policy engine. MCP is a tool source, never the permission system:
//! every bridged tool reports [`Capability::McpInvoke`] scoped to its exact
//! server + tool, so nothing an MCP server advertises executes without
//! passing host policy (approval by default, grant only when the user
//! pre-approved that exact server tool).
//!
//! Server children launch with a default-deny environment: only names on
//! the config `env_allowlist` are copied from the parent plus explicit
//! `extra_env` entries. The whole parent environment never flows through.

use capability_core::{Capability, CapabilityRequest, Resource, ServerId};
use rmcp::{
    model::{CallToolRequestParams, CallToolResult, Tool as McpToolDef},
    service::RunningService,
    transport::TokioChildProcess,
    RoleClient, ServiceExt,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tool_core::{CapabilityRequirement, Tool, ToolContext, ToolError, ToolMetadata, ToolOutput};

/// Default RPC deadline for `tools/list` and `tools/call`.
pub const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_secs(30);

/// How much the host trusts a server's advertisements. Trust never grants
/// authority — it is risk metadata shown in tool descriptions and approval
/// prompts (plan Phase 25 extends this toward auto-approval policies).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum TrustLevel {
    /// Third-party or unverified origin. Default.
    #[default]
    Untrusted,
    /// Known origin, still externally controlled.
    Limited,
    /// Shipped or fully reviewed by the user.
    Trusted,
}

impl TrustLevel {
    fn as_str(self) -> &'static str {
        match self {
            TrustLevel::Untrusted => "untrusted",
            TrustLevel::Limited => "limited",
            TrustLevel::Trusted => "trusted",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("invalid MCP server config: {0}")]
    Config(String),
    #[error("cannot spawn MCP server '{server}': {message}")]
    Spawn { server: String, message: String },
    #[error("MCP protocol error with '{server}': {message}")]
    Protocol { server: String, message: String },
    #[error("MCP request to '{server}' timed out")]
    Timeout { server: String },
    #[error("MCP server '{0}' is not connected")]
    Disconnected(String),
    #[error("MCP server '{0}' is disabled")]
    Disabled(String),
    #[error("unknown MCP server '{0}'")]
    UnknownServer(String),
}

/// How to reach one MCP server. Only child-process (stdio) transport in
/// this milestone; remote transports arrive with authenticated fetch.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum McpTransport {
    Stdio {
        command: String,
        args: Vec<String>,
        /// Parent environment names copied into the child. Everything
        /// else is dropped — the full parent environment never passes.
        env_allowlist: Vec<String>,
        /// Explicit extra variables for the child.
        extra_env: HashMap<String, String>,
    },
}

/// Static configuration for one MCP server (plan Phase 21 config shape).
/// Serializable so it can live in the `mcp.servers` settings key.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct McpServerConfig {
    pub id: ServerId,
    pub transport: McpTransport,
    pub enabled: bool,
    pub trust: TrustLevel,
}

impl McpServerConfig {
    pub fn validate(&self) -> Result<(), McpError> {
        if self.id.0.is_empty() || self.id.0.len() > 128 {
            return Err(McpError::Config("server id must be 1-128 chars".to_string()));
        }
        if !self
            .id
            .0
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        {
            return Err(McpError::Config(
                "server id may only contain [a-zA-Z0-9-_.]".to_string(),
            ));
        }
        match &self.transport {
            McpTransport::Stdio {
                command,
                args,
                extra_env,
                ..
            } => {
                check_no_nul("command", command).map_err(McpError::Config)?;
                if command.is_empty() || command.len() > 1024 {
                    return Err(McpError::Config("'command' must be 1-1024 chars".to_string()));
                }
                if args.len() > 128 {
                    return Err(McpError::Config("'args' exceeds 128 entries".to_string()));
                }
                for arg in args {
                    check_no_nul("args[]", arg).map_err(McpError::Config)?;
                    if arg.len() > 4096 {
                        return Err(McpError::Config("an 'args' entry exceeds 4096 chars".to_string()));
                    }
                }
                if extra_env.len() > 64 {
                    return Err(McpError::Config("'extra_env' exceeds 64 entries".to_string()));
                }
                for (name, value) in extra_env {
                    check_no_nul("extra_env name", name).map_err(McpError::Config)?;
                    check_no_nul("extra_env value", value).map_err(McpError::Config)?;
                }
            }
        }
        Ok(())
    }
}

fn check_no_nul(what: &str, value: &str) -> Result<(), String> {
    if value.contains('\0') {
        return Err(format!("{what} must not contain NUL bytes"));
    }
    Ok(())
}

/// A connected MCP server: live SDK service over a child-process transport.
/// Dropping it closes the transport and reaps the child.
pub struct McpClient {
    server: String,
    service: RunningService<RoleClient, ()>,
}

impl McpClient {
    /// Spawn the server child (default-deny environment) and complete the
    /// MCP initialize handshake.
    pub async fn connect(config: &McpServerConfig) -> Result<Self, McpError> {
        config.validate()?;
        if !config.enabled {
            return Err(McpError::Disabled(config.id.0.clone()));
        }
        let McpTransport::Stdio {
            command,
            args,
            env_allowlist,
            extra_env,
        } = &config.transport;
        let mut cmd = tokio::process::Command::new(command);
        cmd.args(args);
        // Default-deny environment: allowlisted parent names plus explicit
        // extras only. Secrets in the parent environment never flow unless
        // the operator allowlisted that exact name.
        cmd.env_clear();
        for name in env_allowlist {
            if let Some(value) = std::env::var_os(name) {
                cmd.env(name, value);
            }
        }
        for (name, value) in extra_env {
            cmd.env(name, value);
        }
        let transport = TokioChildProcess::new(cmd).map_err(|e| McpError::Spawn {
            server: config.id.0.clone(),
            message: e.to_string(),
        })?;
        let service = ().serve(transport).await.map_err(|e| McpError::Protocol {
            server: config.id.0.clone(),
            message: format!("initialize failed: {e}"),
        })?;
        Ok(Self {
            server: config.id.0.clone(),
            service,
        })
    }

    pub async fn list_tools(&self) -> Result<Vec<McpToolDef>, McpError> {
        use tracing::Instrument as _;
        let span = tracing::debug_span!("mcp.list_tools", server = %self.server);
        let result = tokio::time::timeout(
            DEFAULT_RPC_TIMEOUT,
            self.service.list_tools(None).instrument(span),
        )
            .await
            .map_err(|_| McpError::Timeout {
                server: self.server.clone(),
            })?
            .map_err(|e| McpError::Protocol {
                server: self.server.clone(),
                message: format!("tools/list failed: {e}"),
            })?;
        Ok(result.tools)
    }

    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Option<rmcp::model::JsonObject>,
    ) -> Result<CallToolResult, McpError> {
        // Tool name only: arguments may carry secrets.
        use tracing::Instrument as _;
        let span = tracing::info_span!("mcp.call", server = %self.server, tool = %name);
        let mut params = CallToolRequestParams::new(name.to_string());
        if let Some(arguments) = arguments {
            params = params.with_arguments(arguments);
        }
        tokio::time::timeout(
            DEFAULT_RPC_TIMEOUT,
            self.service.call_tool(params).instrument(span),
        )
            .await
            .map_err(|_| McpError::Timeout {
                server: self.server.clone(),
            })?
            .map_err(|e| McpError::Protocol {
                server: self.server.clone(),
                message: format!("tools/call failed: {e}"),
            })
    }
}

/// Registry id for a server tool: `mcp.<server>.<tool>` with unsafe
/// characters flattened (server ids are already restricted; tool names
/// are server-advertised and cannot be trusted verbatim).
pub fn bridge_tool_id(server: &str, tool: &str) -> String {
    fn clean(part: &str) -> String {
        part.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    }
    format!("mcp.{}.{}", clean(server), clean(tool))
}

/// One MCP server tool bridged into the host [`ToolRegistry`].
pub struct McpToolBridge {
    server: String,
    tool: String,
    description: String,
    input_schema: serde_json::Value,
    client: Arc<tokio::sync::Mutex<McpClient>>,
}

impl McpToolBridge {
    pub fn new(
        server_id: &ServerId,
        tool: &McpToolDef,
        trust: TrustLevel,
        client: Arc<tokio::sync::Mutex<McpClient>>,
    ) -> Self {
        let advertised = tool.description.as_deref().unwrap_or("(no description)");
        // Host-side risk metadata, always present: the model and the
        // approval dialog see origin + trust, never just the ad copy.
        let mut description = format!(
            "[MCP server '{}' | trust: {}] {advertised}",
            server_id.0,
            trust.as_str()
        );
        if let Some(annotations) = &tool.annotations {
            if annotations.read_only_hint == Some(true) {
                description.push_str(" (server claims read-only; unverified)");
            }
            if annotations.destructive_hint == Some(true) {
                description.push_str(" (server flags destructive effects)");
            }
        }
        Self {
            server: server_id.0.clone(),
            tool: tool.name.to_string(),
            description,
            input_schema: serde_json::to_value(tool.input_schema.as_ref())
                .unwrap_or(serde_json::json!({"type": "object"})),
            client,
        }
    }

    fn denied(reason: impl Into<String>) -> ToolError {
        ToolError::Denied {
            tool: "mcp".to_string(),
            reason: reason.into(),
        }
    }

    /// The bridge ticket must cover `McpInvoke` on this exact server tool.
    fn authorize(&self, ctx: &ToolContext) -> Result<(), ToolError> {
        let ticket = ctx.ticket.as_ref().ok_or_else(|| {
            Self::denied("no capability ticket: MCP tools authorize through the agent + policy engine")
        })?;
        let request = CapabilityRequest {
            principal: ctx.principal.clone(),
            capability: Capability::McpInvoke,
            resource: Resource::McpTool {
                server: self.server.clone(),
                tool: self.tool.clone(),
            },
        };
        ticket
            .check(&ctx.principal, &request, &ctx.invocation_id)
            .map_err(|err| {
                Self::denied(format!(
                    "ticket does not authorize this MCP call: {}",
                    match err {
                        capability_core::TicketError::Expired => "capability ticket expired",
                        capability_core::TicketError::PrincipalMismatch =>
                            "ticket bound to a different principal",
                        capability_core::TicketError::InvocationMismatch =>
                            "ticket bound to a different invocation",
                        capability_core::TicketError::CapabilityMismatch
                        | capability_core::TicketError::ScopeMismatch =>
                            "ticket does not cover this server tool",
                    }
                ))
            })
    }
}

#[async_trait::async_trait]
impl Tool for McpToolBridge {
    fn metadata(&self) -> ToolMetadata {
        // Advertise the server's own input schema when it is a JSON
        // object; otherwise fall back to a generic object schema. Either
        // way the model sees what the server declares — unverified.
        let schema = if self.input_schema.is_object() {
            self.input_schema.clone()
        } else {
            serde_json::json!({ "type": "object" })
        };
        ToolMetadata {
            id: capability_core::ToolId::new(bridge_tool_id(&self.server, &self.tool)),
            description: self.description.clone(),
            input_schema: schema,
            effects: vec![tool_core::ToolEffect::ExternalSideEffect],
        }
    }

    /// Always `Some`: MCP tools are never pure, so they can never run
    /// without a policy decision (approval by default).
    fn required_capability(&self, _args: &serde_json::Value) -> Option<CapabilityRequirement> {
        Some(CapabilityRequirement {
            capability: Capability::McpInvoke,
            resource: Resource::McpTool {
                server: self.server.clone(),
                tool: self.tool.clone(),
            },
        })
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        self.authorize(&ctx)?;
        let map = args.as_object().cloned().ok_or_else(|| ToolError::InvalidArgs {
            tool: "mcp".to_string(),
            message: "MCP tool arguments must be a JSON object".to_string(),
        })?;
        let client = self.client.lock().await;
        let result = client
            .call_tool(&self.tool, Some(map))
            .await
            .map_err(|e| ToolError::Failed {
                tool: "mcp".to_string(),
                message: e.to_string(),
            })?;
        Ok(ToolOutput::new(mcp_result_to_json(&result)))
    }
}

/// Flatten an MCP result into model-facing JSON. Tool-level `is_error`
/// stays data (the model sees it and self-corrects); broker failures
/// above already returned `Err`.
fn mcp_result_to_json(result: &CallToolResult) -> serde_json::Value {
    const MAX_BYTES: usize = 16 * 1024;
    let mut texts = Vec::new();
    let mut images = 0usize;
    for block in &result.content {
        match block {
            rmcp::model::ContentBlock::Text(text) => texts.push(text.text.clone()),
            rmcp::model::ContentBlock::Image(_) => images += 1,
            // Audio/resources/links: acknowledged without dumping bytes.
            _ => texts.push("[non-text MCP content omitted]".to_string()),
        }
    }
    let mut text = texts.join("\n");
    let mut truncated = false;
    if text.len() > MAX_BYTES {
        text.truncate(MAX_BYTES);
        truncated = true;
    }
    serde_json::json!({
        "is_error": result.is_error == Some(true),
        "truncated": truncated,
        "text": text,
        "images": images,
        "structured": result.structured_content,
    })
}

/// Owns server configs, live clients, and their registry entries.
/// Enable/disable/reload flows through here so a disabled server can
/// never leave tools registered.
pub struct McpManager {
    inner: tokio::sync::Mutex<ManagerInner>,
}

struct ManagedServer {
    config: McpServerConfig,
    client: Option<Arc<tokio::sync::Mutex<McpClient>>>,
    /// Advertised tool names from the last discovery. Registry ids derive
    /// from these, so removal works against any registry the tools were
    /// added to (registries are rebuilt per agent turn).
    tool_names: Vec<String>,
}

#[derive(Default)]
struct ManagerInner {
    servers: HashMap<String, ManagedServer>,
}

impl McpManager {
    pub fn new() -> Self {
        Self {
            inner: tokio::sync::Mutex::new(ManagerInner::default()),
        }
    }

    /// Store (or replace) a server config. Replacing the config of a
    /// connected server drops its client; the next registration
    /// reconnects and rediscovers.
    pub async fn configure(&self, config: McpServerConfig) -> Result<(), McpError> {
        config.validate()?;
        let mut inner = self.inner.lock().await;
        let entry = inner
            .servers
            .entry(config.id.0.clone())
            .or_insert_with(|| ManagedServer {
                config: config.clone(),
                client: None,
                tool_names: Vec::new(),
            });
        if entry.config != config {
            entry.config = config;
            entry.client = None;
            entry.tool_names.clear();
        }
        Ok(())
    }

    /// Replace the whole server set (settings-driven): unknown ids are
    /// dropped with their clients, new configs are stored, changed
    /// configs reconnect lazily. Registries are rebuilt per turn, so no
    /// registry surgery is needed here.
    pub async fn sync_configs(&self, configs: Vec<McpServerConfig>) -> Result<(), McpError> {
        for config in &configs {
            config.validate()?;
        }
        let mut inner = self.inner.lock().await;
        let wanted: std::collections::HashSet<&str> =
            configs.iter().map(|c| c.id.0.as_str()).collect();
        inner.servers.retain(|id, _| wanted.contains(id.as_str()));
        for config in configs {
            let entry = inner
                .servers
                .entry(config.id.0.clone())
                .or_insert_with(|| ManagedServer {
                    config: config.clone(),
                    client: None,
                    tool_names: Vec::new(),
                });
            if entry.config != config {
                entry.config = config;
                entry.client = None;
                entry.tool_names.clear();
            }
        }
        Ok(())
    }

    /// Ids of servers currently configured, sorted.
    pub async fn server_ids(&self) -> Vec<String> {
        let inner = self.inner.lock().await;
        let mut ids: Vec<String> = inner.servers.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Connect (or reuse) the client, discover tools, and register one
    /// bridge per tool into `registry`. Safe to call on every agent turn:
    /// registries are rebuilt per turn and duplicate ids are tolerated.
    pub async fn register_into(
        &self,
        server_id: &str,
        registry: &mut tool_core::ToolRegistry,
    ) -> Result<Vec<String>, McpError> {
        let client = self.client_for(server_id).await?;
        let tools = client.lock().await.list_tools().await?;
        let mut inner = self.inner.lock().await;
        let managed = inner
            .servers
            .get_mut(server_id)
            .ok_or_else(|| McpError::UnknownServer(server_id.to_string()))?;
        managed.tool_names = tools.iter().map(|t| t.name.to_string()).collect();
        let mut added = Vec::new();
        for tool in &tools {
            let id = bridge_tool_id(&managed.config.id.0, &tool.name);
            let bridge = McpToolBridge::new(
                &managed.config.id,
                tool,
                managed.config.trust,
                Arc::clone(&client),
            );
            match registry.register(Arc::new(bridge)) {
                Ok(()) => added.push(id),
                Err(tool_core::ToolError::DuplicateId(_)) => {
                    // Another source owns this id; leave it alone rather
                    // than shadowing it.
                    continue;
                }
                Err(e) => {
                    return Err(McpError::Protocol {
                        server: server_id.to_string(),
                        message: format!("cannot register tool '{}': {e}", tool.name),
                    });
                }
            }
        }
        Ok(added)
    }

    /// Unregister every bridge id derived from the last discovery and
    /// drop the client (reaping the server child).
    pub async fn remove_from(
        &self,
        server_id: &str,
        registry: &mut tool_core::ToolRegistry,
    ) -> Result<(), McpError> {
        let mut inner = self.inner.lock().await;
        let managed = inner
            .servers
            .get_mut(server_id)
            .ok_or_else(|| McpError::UnknownServer(server_id.to_string()))?;
        for name in managed.tool_names.drain(..) {
            let _ = registry.unregister(&bridge_tool_id(&managed.config.id.0, &name));
        }
        managed.client = None;
        Ok(())
    }

    /// Disable a server: unregister its tools and drop the client.
    /// Re-enabling requires `set_enabled(true)` + `register_into`.
    pub async fn set_enabled(
        &self,
        server_id: &str,
        enabled: bool,
        registry: &mut tool_core::ToolRegistry,
    ) -> Result<(), McpError> {
        if !enabled {
            self.remove_from(server_id, registry).await?;
        }
        let mut inner = self.inner.lock().await;
        let managed = inner
            .servers
            .get_mut(server_id)
            .ok_or_else(|| McpError::UnknownServer(server_id.to_string()))?;
        managed.config.enabled = enabled;
        Ok(())
    }

    /// Snapshot: which servers exist, whether each is connected, and how
    /// many tools the last discovery reported.
    pub async fn status(&self) -> Vec<McpServerStatus> {
        let inner = self.inner.lock().await;
        let mut out: Vec<McpServerStatus> = inner
            .servers
            .values()
            .map(|managed| McpServerStatus {
                id: managed.config.id.0.clone(),
                enabled: managed.config.enabled,
                connected: managed.client.is_some(),
                tools: managed.tool_names.len(),
            })
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    async fn client_for(
        &self,
        server_id: &str,
    ) -> Result<Arc<tokio::sync::Mutex<McpClient>>, McpError> {
        // Fast path without holding the lock across the handshake.
        if let Some(client) = self.inner.lock().await.servers.get(server_id).and_then(|managed| {
            if managed.config.enabled {
                managed.client.clone()
            } else {
                None
            }
        }) {
            return Ok(client);
        }
        let config = self
            .inner
            .lock()
            .await
            .servers
            .get(server_id)
            .ok_or_else(|| McpError::UnknownServer(server_id.to_string()))?
            .config
            .clone();
        if !config.enabled {
            return Err(McpError::Disabled(server_id.to_string()));
        }
        let client = Arc::new(tokio::sync::Mutex::new(
            McpClient::connect(&config).await?,
        ));
        self.inner.lock().await.servers.get_mut(server_id).map(|managed| {
            managed.client = Some(Arc::clone(&client));
        });
        Ok(client)
    }
}

impl Default for McpManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Point-in-time server state for UIs and diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerStatus {
    pub id: String,
    pub enabled: bool,
    pub connected: bool,
    pub tools: usize,
}
