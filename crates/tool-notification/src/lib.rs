//! Native desktop notifications (`notification.show`).
//!
//! Title and body are length-bounded; urgency and icon are optional. The
//! tool deliberately exposes no notification actions: actions would be
//! executable callbacks into the agent, which the security model forbids.
//! Delivery failures surface as explicit `backend_unavailable` errors —
//! never fake success. Showing a notification is user-visible external
//! behavior, so it requires an explicit [`capability_core::Capability::NotificationSend`]
//! ticket (low-friction policy, but explicit authority, never ambient).

use capability_core::{Capability, Resource};
use std::sync::Arc;
use tool_core::{
    CapabilityRequirement, Tool, ToolContext, ToolEffect, ToolError, ToolMetadata, ToolOutput,
};

const MAX_TITLE_CHARS: usize = 128;
const MAX_BODY_CHARS: usize = 1_024;

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::InvalidArgs {
        tool: "notification.show".to_string(),
        message: message.into(),
    }
}

pub struct NotificationShowTool;

#[async_trait::async_trait]
impl Tool for NotificationShowTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("notification.show"),
            description: "Show a native OS notification (title + body, optional urgency/icon). No actions or callbacks.".to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "title": {"type": "string", "minLength": 1},
                    "body": {"type": "string"},
                    "urgency": {"type": "string", "enum": ["low", "normal", "critical"]},
                    "icon": {"type": "string"},
                },
                "required": ["title", "body"],
            }),
            effects: vec![ToolEffect::ExternalSideEffect],
        }
    }

    fn required_capability(&self, _args: &serde_json::Value) -> Option<CapabilityRequirement> {
        Some(CapabilityRequirement {
            capability: Capability::NotificationSend,
            resource: Resource::NotificationService,
        })
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let title = args
            .get("title")
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| invalid("missing non-empty string 'title'"))?;
        let body = args
            .get("body")
            .and_then(|value| value.as_str())
            .ok_or_else(|| invalid("missing string 'body'"))?;
        if title.chars().count() > MAX_TITLE_CHARS {
            return Err(invalid("title is too long"));
        }
        if body.chars().count() > MAX_BODY_CHARS {
            return Err(invalid("body is too long"));
        }
        let urgency = match args.get("urgency").and_then(|value| value.as_str()) {
            None | Some("normal") => notify_rust::Urgency::Normal,
            Some("low") => notify_rust::Urgency::Low,
            Some("critical") => notify_rust::Urgency::Critical,
            Some(other) => return Err(invalid(format!("unknown urgency '{other}'"))),
        };
        // User-visible external behavior: explicit authority required.
        // Validated first so malformed calls fail as InvalidArgs even
        // without a ticket (no backend is touched either way).
        if !ctx.has_ticket(Capability::NotificationSend, Resource::NotificationService) {
            return Err(ToolError::structured_with_details(
                "notification.show",
                "permission_required",
                "showing a notification needs explicit user authorization",
                serde_json::json!({
                    "capability": "NotificationSend",
                    "resource": "NotificationService",
                }),
            ));
        }
        let mut notification = notify_rust::Notification::new();
        notification.summary(title).body(body).urgency(urgency);
        if let Some(icon) = args.get("icon").and_then(|value| value.as_str()) {
            if icon.chars().count() > 256 {
                return Err(invalid("icon is too long"));
            }
            notification.icon(icon);
        }
        // `show` is blocking D-Bus/IPC: run off the async executor thread.
        tokio::task::spawn_blocking(move || notification.show())
            .await
            .map_err(|error| {
                ToolError::structured(
                    "notification.show",
                    "backend_unavailable",
                    format!("notification task failed: {error}"),
                )
            })?
            .map_err(|error| {
                ToolError::structured_with_details(
                    "notification.show",
                    "backend_unavailable",
                    format!("no notification backend delivered this message: {error}"),
                    serde_json::json!({}),
                )
            })?;
        Ok(ToolOutput::json(serde_json::json!({ "ok": true })))
    }
}

/// Static notification tool group.
pub struct NotificationToolPack;

impl tool_sdk::ToolPack for NotificationToolPack {
    fn id(&self) -> &'static str {
        "notification"
    }

    fn tools(&self, _ctx: &tool_sdk::ToolLoadContext) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(NotificationShowTool)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use capability_core::{AgentId, Principal};
    use tool_sdk::ToolPack as _;

    fn ctx() -> ToolContext {
        ToolContext::new(Principal::Agent(AgentId::new("test")))
    }

    #[tokio::test]
    async fn validation_rejects_bad_input_without_touching_the_backend() {
        let tool = NotificationShowTool;
        assert!(matches!(
            tool.invoke(ctx(), serde_json::json!({})).await,
            Err(ToolError::InvalidArgs { .. })
        ));
        assert!(matches!(
            tool.invoke(ctx(), serde_json::json!({"title": "t"})).await,
            Err(ToolError::InvalidArgs { .. })
        ));
        assert!(matches!(
            tool.invoke(
                ctx(),
                serde_json::json!({"title": "t", "body": "b", "urgency": "loud"}),
            )
            .await,
            Err(ToolError::InvalidArgs { .. })
        ));
        assert!(matches!(
            tool.invoke(
                ctx(),
                serde_json::json!({"title": "x".repeat(200), "body": "b"}),
            )
            .await,
            Err(ToolError::InvalidArgs { .. })
        ));
    }

    #[test]
    fn pack_registers_show_with_side_effect_marker() {
        let tools = NotificationToolPack.tools(&tool_sdk::ToolLoadContext::default());
        assert_eq!(tools.len(), 1);
        let meta = tools[0].metadata();
        assert_eq!(meta.id.0, "notification.show");
        assert!(meta.effects.contains(&ToolEffect::ExternalSideEffect));
    }

    #[tokio::test]
    async fn show_requires_notification_send_authority() {
        use capability_core::ResourceScope;
        use std::time::Duration;
        let tool = NotificationShowTool;
        let args = serde_json::json!({"title": "hi", "body": "there"});
        // Declared preflight names the capability for the agent loop.
        let declared = tool
            .required_capabilities(&args)
            .pop()
            .expect("one requirement");
        assert_eq!(
            declared.capability,
            capability_core::Capability::NotificationSend
        );
        assert_eq!(
            declared.resource,
            capability_core::Resource::NotificationService
        );
        // No ticket: denied before the backend is touched.
        let err = tool.invoke(ctx(), args.clone()).await.unwrap_err();
        assert_eq!(err.code(), Some("permission_required"), "{err:?}");
        // With a ticket the backend runs (and reports honestly when no
        // notification daemon exists in CI).
        let ctx = ctx();
        let ticket = capability_core::CapabilityTicket::mint(
            ctx.principal.clone(),
            capability_core::Capability::NotificationSend,
            ResourceScope::new(vec![capability_core::Resource::NotificationService]),
            ctx.invocation_id,
            Duration::from_secs(120),
        );
        let outcome = tool.invoke(ctx.with_ticket(ticket), args).await;
        match outcome {
            Ok(out) => assert_eq!(out.content["ok"], true),
            Err(err) => assert_eq!(err.code(), Some("backend_unavailable"), "{err:?}"),
        }
    }
}
