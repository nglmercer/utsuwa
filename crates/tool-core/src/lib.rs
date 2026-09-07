//! Unified tool registry (plan Phases 11-12, Task 9).
//!
//! Every tool — builtin, MCP-sourced, WASM-plugin, or native-plugin —
//! implements [`Tool`] and is invoked through [`ToolRegistry`]. The
//! registry resolves names and calls code; it grants no authority.
//! Permission checks and capability tickets wrap `invoke` in later tasks.

use capability_core::{Capability, CapabilityTicket, InvocationId, Principal, Resource, ToolId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// What a tool can do to the world. Drives policy: reads may pass silently,
/// everything else needs approval or a standing grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ToolEffect {
    ReadOnly,
    FilesystemWrite,
    Destructive,
    Network,
    Process,
    DesktopControl,
    ExternalSideEffect,
}

#[derive(Debug, Clone)]
pub struct ToolMetadata {
    /// Namespaced id: `filesystem.read`, `mcp.github.search_code`, ….
    /// No duplicates across all tool sources.
    pub id: ToolId,
    pub description: String,
    /// JSON Schema for the tool's arguments.
    pub input_schema: serde_json::Value,
    pub effects: Vec<ToolEffect>,
}

/// Ambient facts for one tool call. Carries identity plus the
/// capability ticket that authorizes this exact call — never blanket
/// authority. Brokers validate the ticket before touching the OS.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub principal: Principal,
    pub invocation_id: InvocationId,
    pub ticket: Option<CapabilityTicket>,
}

impl ToolContext {
    pub fn new(principal: Principal) -> Self {
        Self {
            principal,
            invocation_id: InvocationId::fresh(),
            ticket: None,
        }
    }

    pub fn with_ticket(mut self, ticket: CapabilityTicket) -> Self {
        self.ticket = Some(ticket);
        self
    }
}

/// Model-facing tool result.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    pub content: serde_json::Value,
    /// True when the broker truncated an oversized result (limits, Phase 36).
    pub truncated: bool,
}

impl ToolOutput {
    pub fn new(content: serde_json::Value) -> Self {
        Self {
            content,
            truncated: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ToolError {
    #[error("unknown tool: {0}")]
    NotFound(String),
    #[error("duplicate tool id: {0}")]
    DuplicateId(String),
    #[error("invalid tool id (must be namespaced like 'scope.name'): {0}")]
    InvalidId(String),
    #[error("invalid arguments for {tool}: {message}")]
    InvalidArgs { tool: String, message: String },
    #[error("permission denied for {tool}: {reason}")]
    Denied { tool: String, reason: String },
    #[error("tool {tool} failed: {message}")]
    Failed { tool: String, message: String },
    #[error("tool {0} timed out")]
    Timeout(String),
}

/// The OS authority a tool needs for a given call. Returned by
/// [`Tool::required_capability`]; `None` means the tool is pure (like
/// `system.echo`) and executes without privilege escalation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityRequirement {
    pub capability: Capability,
    pub resource: Resource,
}

#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn metadata(&self) -> ToolMetadata;

    /// Declare the privileged capability this call needs, if any.
    /// Evaluated per-call because the resource often comes from args.
    /// The default (`None`) is for pure tools with no OS effects.
    fn required_capability(
        &self,
        _args: &serde_json::Value,
    ) -> Option<CapabilityRequirement> {
        None
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError>;
}

/// Runtime registry over all tool sources.
#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// IDs must be namespaced (`scope.name`, lowercase alphanumerics plus
    /// `_`, `-`, `.`) and unique across builtin/MCP/WASM/native sources.
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<(), ToolError> {
        let id = tool.metadata().id.0.clone();
        if !is_namespaced_id(&id) {
            return Err(ToolError::InvalidId(id));
        }
        if self.tools.contains_key(&id) {
            return Err(ToolError::DuplicateId(id));
        }
        self.tools.insert(id, tool);
        Ok(())
    }

    pub fn unregister(&mut self, id: &str) -> Result<(), ToolError> {
        self.tools
            .remove(id)
            .map(|_| ())
            .ok_or_else(|| ToolError::NotFound(id.to_string()))
    }

    pub fn list(&self) -> Vec<ToolMetadata> {
        let mut out: Vec<ToolMetadata> =
            self.tools.values().map(|t| t.metadata()).collect();
        out.sort_by(|a, b| a.id.0.cmp(&b.id.0));
        out
    }

    pub fn resolve(&self, id: &str) -> Result<Arc<dyn Tool>, ToolError> {
        self.tools
            .get(id)
            .cloned()
            .ok_or_else(|| ToolError::NotFound(id.to_string()))
    }

    pub async fn invoke(
        &self,
        id: &str,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        self.resolve(id)?.invoke(ctx, args).await
    }
}

fn is_namespaced_id(id: &str) -> bool {
    id.contains('.')
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

/// Harmless test tool (plan Task 9): echoes its arguments back.
/// Read-only, no effects — safe for agent-loop smoke tests.
pub struct EchoTool;

#[async_trait::async_trait]
impl Tool for EchoTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: ToolId::new("system.echo"),
            description: "Echo arguments back. Test tool with no effects.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
            }),
            effects: vec![ToolEffect::ReadOnly],
        }
    }

    async fn invoke(
        &self,
        _ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        if !args.is_object() {
            return Err(ToolError::InvalidArgs {
                tool: "system.echo".to_string(),
                message: "args must be a JSON object".to_string(),
            });
        }
        Ok(ToolOutput::new(args))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use capability_core::AgentId;

    fn ctx() -> ToolContext {
        ToolContext::new(Principal::Agent(AgentId::new("test")))
    }

    #[tokio::test]
    async fn echo_round_trips_through_registry() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool)).unwrap();
        let out = registry
            .invoke("system.echo", ctx(), serde_json::json!({"text": "hi"}))
            .await
            .unwrap();
        assert_eq!(out.content, serde_json::json!({"text": "hi"}));
        assert!(!out.truncated);
    }

    #[test]
    fn duplicate_ids_rejected() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool)).unwrap();
        assert_eq!(
            registry.register(Arc::new(EchoTool)),
            Err(ToolError::DuplicateId("system.echo".to_string()))
        );
    }

    #[test]
    fn non_namespaced_ids_rejected() {
        struct Bad;
        #[async_trait::async_trait]
        impl Tool for Bad {
            fn metadata(&self) -> ToolMetadata {
                ToolMetadata {
                    id: ToolId::new("echo"),
                    description: String::new(),
                    input_schema: serde_json::Value::Null,
                    effects: vec![],
                }
            }
            async fn invoke(
                &self,
                _ctx: ToolContext,
                _args: serde_json::Value,
            ) -> Result<ToolOutput, ToolError> {
                Ok(ToolOutput::new(serde_json::Value::Null))
            }
        }
        let mut registry = ToolRegistry::new();
        assert_eq!(
            registry.register(Arc::new(Bad)),
            Err(ToolError::InvalidId("echo".to_string()))
        );
    }

    #[tokio::test]
    async fn unknown_tool_and_bad_args_are_typed_errors() {
        let registry = ToolRegistry::new();
        assert_eq!(
            registry.invoke("nope.x", ctx(), serde_json::json!({})).await,
            Err(ToolError::NotFound("nope.x".to_string()))
        );
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool)).unwrap();
        assert!(matches!(
            registry.invoke("system.echo", ctx(), serde_json::json!([1])).await,
            Err(ToolError::InvalidArgs { .. })
        ));
    }

    #[test]
    fn unregister_and_list() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool)).unwrap();
        assert_eq!(registry.list().len(), 1);
        registry.unregister("system.echo").unwrap();
        assert!(registry.list().is_empty());
        assert_eq!(
            registry.unregister("system.echo"),
            Err(ToolError::NotFound("system.echo".to_string()))
        );
    }
}
