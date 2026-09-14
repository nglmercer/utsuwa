//! Explicit clipboard tools (`clipboard.*`).
//!
//! Clipboard contents can hold secrets, so they never enter model context
//! implicitly: `clipboard.read` returns text only after a per-call
//! [`capability_core::Capability::ClipboardRead`] ticket, and the agent
//! layer must not copy it into prompts. `clipboard.clear` overwrites with
//! an empty string.

use std::sync::Arc;
use tool_core::{
    CapabilityRequirement, Tool, ToolContext, ToolEffect, ToolError, ToolMetadata, ToolOutput,
};

const MAX_CLIPBOARD_CHARS: usize = 64 * 1024;

fn clipboard_error(tool: &str, error: impl std::fmt::Display) -> ToolError {
    ToolError::structured_with_details(
        tool,
        "backend_unavailable",
        format!("clipboard is not available in this session: {error}"),
        serde_json::json!({}),
    )
}

fn require(
    tool: &str,
    ctx: &ToolContext,
    capability: capability_core::Capability,
) -> Result<(), ToolError> {
    let resource = capability_core::Resource::Application("clipboard".to_string());
    if ctx.has_ticket(capability.clone(), resource.clone()) {
        Ok(())
    } else {
        Err(ToolError::structured_with_details(
            tool,
            "permission_required",
            "clipboard access needs an explicit per-call ticket",
            serde_json::json!({ "capability": format!("{capability:?}") }),
        ))
    }
}

pub struct ClipboardReadTool;
pub struct ClipboardWriteTool;
pub struct ClipboardClearTool;

#[async_trait::async_trait]
impl Tool for ClipboardReadTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("clipboard.read"),
            description: "Read text/plain from the native clipboard. Returns text only with explicit per-call authorization; never call speculatively.".to_string(),
            input_schema: serde_json::json!({"type": "object", "additionalProperties": false, "properties": {}}),
            effects: vec![ToolEffect::ReadOnly],
        }
    }

    fn required_capability(&self, _args: &serde_json::Value) -> Option<CapabilityRequirement> {
        Some(CapabilityRequirement {
            capability: capability_core::Capability::ClipboardRead,
            resource: capability_core::Resource::Application("clipboard".to_string()),
        })
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        _args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        require(
            "clipboard.read",
            &ctx,
            capability_core::Capability::ClipboardRead,
        )?;
        let text = tokio::task::spawn_blocking(|| {
            arboard::Clipboard::new()
                .map_err(|error| error.to_string())
                .and_then(|mut clipboard| clipboard.get_text().map_err(|error| error.to_string()))
        })
        .await
        .map_err(|error| clipboard_error("clipboard.read", error))?
        .map_err(|error| clipboard_error("clipboard.read", error))?;
        if text.chars().count() > MAX_CLIPBOARD_CHARS {
            return Err(ToolError::structured(
                "clipboard.read",
                "response_too_large",
                "clipboard text exceeds the size limit",
            ));
        }
        Ok(ToolOutput::json(serde_json::json!({ "text": text })))
    }
}

#[async_trait::async_trait]
impl Tool for ClipboardWriteTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("clipboard.write"),
            description: "Write text/plain to the native clipboard.".to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": { "text": {"type": "string"} }, "required": ["text"],
            }),
            effects: vec![ToolEffect::ExternalSideEffect],
        }
    }

    fn required_capability(&self, _args: &serde_json::Value) -> Option<CapabilityRequirement> {
        Some(CapabilityRequirement {
            capability: capability_core::Capability::ClipboardWrite,
            resource: capability_core::Resource::Application("clipboard".to_string()),
        })
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let text = args
            .get("text")
            .and_then(|value| value.as_str())
            .ok_or_else(|| ToolError::InvalidArgs {
                tool: "clipboard.write".to_string(),
                message: "missing string 'text'".to_string(),
            })?;
        if text.chars().count() > MAX_CLIPBOARD_CHARS {
            return Err(ToolError::InvalidArgs {
                tool: "clipboard.write".to_string(),
                message: "clipboard text is too long".to_string(),
            });
        }
        require(
            "clipboard.write",
            &ctx,
            capability_core::Capability::ClipboardWrite,
        )?;
        let text = text.to_string();
        tokio::task::spawn_blocking(move || {
            arboard::Clipboard::new()
                .map_err(|error| error.to_string())
                .and_then(|mut clipboard| {
                    clipboard.set_text(text).map_err(|error| error.to_string())
                })
        })
        .await
        .map_err(|error| clipboard_error("clipboard.write", error))?
        .map_err(|error| clipboard_error("clipboard.write", error))?;
        Ok(ToolOutput::json(serde_json::json!({ "ok": true })))
    }
}

#[async_trait::async_trait]
impl Tool for ClipboardClearTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("clipboard.clear"),
            description: "Clear the native clipboard (overwrites with an empty string)."
                .to_string(),
            input_schema: serde_json::json!({"type": "object", "additionalProperties": false, "properties": {}}),
            effects: vec![ToolEffect::ExternalSideEffect],
        }
    }

    fn required_capability(&self, _args: &serde_json::Value) -> Option<CapabilityRequirement> {
        Some(CapabilityRequirement {
            capability: capability_core::Capability::ClipboardWrite,
            resource: capability_core::Resource::Application("clipboard".to_string()),
        })
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        _args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        require(
            "clipboard.clear",
            &ctx,
            capability_core::Capability::ClipboardWrite,
        )?;
        tokio::task::spawn_blocking(|| {
            arboard::Clipboard::new()
                .map_err(|error| error.to_string())
                .and_then(|mut clipboard| {
                    clipboard
                        .set_text(String::new())
                        .map_err(|error| error.to_string())
                })
        })
        .await
        .map_err(|error| clipboard_error("clipboard.clear", error))?
        .map_err(|error| clipboard_error("clipboard.clear", error))?;
        Ok(ToolOutput::json(serde_json::json!({ "ok": true })))
    }
}

/// Static clipboard tool group.
pub struct ClipboardToolPack;

impl tool_sdk::ToolPack for ClipboardToolPack {
    fn id(&self) -> &'static str {
        "clipboard"
    }

    fn tools(&self, _ctx: &tool_sdk::ToolLoadContext) -> Vec<Arc<dyn Tool>> {
        vec![
            Arc::new(ClipboardReadTool),
            Arc::new(ClipboardWriteTool),
            Arc::new(ClipboardClearTool),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use capability_core::{AgentId, Principal};
    use tool_sdk::ToolPack as _;

    #[test]
    fn pack_registers_read_write_clear() {
        let mut ids = ClipboardToolPack
            .tools(&tool_sdk::ToolLoadContext::default())
            .iter()
            .map(|tool| tool.metadata().id.0.clone())
            .collect::<Vec<_>>();
        ids.sort();
        assert_eq!(
            ids,
            vec!["clipboard.clear", "clipboard.read", "clipboard.write"]
        );
    }

    #[test]
    fn read_declares_clipboard_read_capability() {
        let tool = ClipboardReadTool;
        let requirement = tool.required_capability(&serde_json::json!({})).unwrap();
        assert_eq!(
            requirement.capability,
            capability_core::Capability::ClipboardRead
        );
    }

    #[tokio::test]
    async fn read_without_ticket_is_permission_required() {
        let tool = ClipboardReadTool;
        let err = tool
            .invoke(
                ToolContext::new(Principal::Agent(AgentId::new("t"))),
                serde_json::json!({}),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some("permission_required"), "{err:?}");
    }
}
