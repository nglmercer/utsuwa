//! Desktop / computer-use core (plan Phase 27): the `DesktopBackend`
//! trait plus permissioned `desktop.*` agent tools.
//!
//! Strategy from the plan: accessibility APIs first (native element
//! actions), screenshots as fallback — never default to coordinate
//! clicking. Every tool declares its capability so the agent + policy
//! engine approve each call; tickets are re-validated inside `invoke`.
//!
//! Platform backends (Windows UI Automation, macOS AX, Linux portals)
//! are Phases 28–30. This crate ships the trait, the tools, and a stub
//! backend that reports unavailability — tool/policy/audit wiring is
//! testable on any machine, and no tool ever pretends to act when no
//! backend exists.

use std::sync::Arc;

/// A visible top-level window.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WindowInfo {
    pub id: String,
    pub title: String,
    pub app: String,
}

/// One node of an accessibility tree.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ElementNode {
    pub id: String,
    pub role: String,
    pub name: String,
    /// Action names the element supports (`invoke`, `set_value`, …).
    pub actions: Vec<String>,
}

/// A captured screenshot: raw PNG bytes plus dimensions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Screenshot {
    pub width: u32,
    pub height: u32,
    pub png_bytes: Vec<u8>,
}

/// A point in screen coordinates (fallback path only — element actions
/// stay preferred; see the crate docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, thiserror::Error)]
pub enum DesktopError {
    #[error("no desktop backend on this platform/session: {0}")]
    BackendUnavailable(String),
    #[error("unknown window '{0}'")]
    UnknownWindow(String),
    #[error("unknown element '{0}'")]
    UnknownElement(String),
    #[error("action failed: {0}")]
    ActionFailed(String),
}

/// High-level desktop interface (plan Phase 27). Async because real
/// backends cross IPC (portals, UI Automation COM, AX). Implementations
/// must never synthesize success: without OS access they return
/// [`DesktopError::BackendUnavailable`].
#[async_trait::async_trait]
pub trait DesktopBackend: Send + Sync {
    /// False when no OS access exists (stub, missing session). The host
    /// only registers `desktop.*` tools when the backend is available,
    /// so the model never sees actions that cannot run.
    fn is_available(&self) -> bool {
        true
    }
    async fn list_windows(&self) -> Result<Vec<WindowInfo>, DesktopError>;
    async fn accessibility_tree(&self, window_id: &str) -> Result<Vec<ElementNode>, DesktopError>;
    async fn invoke_element(&self, window_id: &str, element_id: &str) -> Result<(), DesktopError>;
    async fn set_value(&self, window_id: &str, element_id: &str, value: &str) -> Result<(), DesktopError>;
    async fn screenshot(&self, window_id: Option<&str>) -> Result<Screenshot, DesktopError>;
    async fn click(&self, window_id: Option<&str>, at: Point) -> Result<(), DesktopError>;
    async fn type_text(&self, window_id: Option<&str>, text: &str) -> Result<(), DesktopError>;
}

/// Builds the backend for this host. Today every platform reports
/// unavailability until its Phase 28–30 backend lands; the selection
/// point stays in one place so that work plugs in here.
pub fn backend() -> Arc<dyn DesktopBackend> {
    Arc::new(StubBackend)
}

/// Largest typed-text payload per call (Phase 36 thrift).
pub const MAX_TYPE_CHARS: usize = 4_096;
/// Largest screenshot the tools will pass to the model.
pub const MAX_SCREENSHOT_BYTES: usize = 2 << 20;

/// Minimal base64 (RFC 4648, standard alphabet, padded) for screenshot
/// bytes. Hand-rolled to avoid a new dependency for one call site.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut n: u32 = 0;
        for (i, &b) in chunk.iter().enumerate() {
            n |= (b as u32) << (16 - 8 * i);
        }
        let pad = 3 - chunk.len();
        for i in 0..4 - pad {
            out.push(ALPHABET[((n >> (18 - 6 * i)) & 0x3F) as usize] as char);
        }
        for _ in 0..pad {
            out.push('=');
        }
    }
    out
}

/// Agent tools over a [`DesktopBackend`]. Every action declares an exact
/// capability + resource so policy approves per call; `invoke`
/// re-validates the ticket (broker pattern). An empty window id means
/// "the whole desktop" for observe tools — it never matches a real
/// window grant, so unfiltered observation always prompts.
pub mod tools {
    use super::{
        base64_encode, DesktopBackend, DesktopError, Point, MAX_SCREENSHOT_BYTES, MAX_TYPE_CHARS,
    };
    use capability_core::{Capability, CapabilityRequest, Resource};
    use std::sync::Arc;
    use tool_core::{
        CapabilityRequirement, Tool, ToolContext, ToolEffect, ToolError, ToolMetadata, ToolOutput,
    };

    fn failed(tool: &str, message: impl Into<String>) -> ToolError {
        ToolError::Failed {
            tool: tool.to_string(),
            message: message.into(),
        }
    }

    fn denied(tool: &str, reason: impl Into<String>) -> ToolError {
        ToolError::Denied {
            tool: tool.to_string(),
            reason: reason.into(),
        }
    }

    fn invalid(tool: &str, message: impl Into<String>) -> ToolError {
        ToolError::InvalidArgs {
            tool: tool.to_string(),
            message: message.into(),
        }
    }

    fn require_ticket(
        tool: &str,
        ctx: &ToolContext,
        capability: Capability,
        resource: Resource,
    ) -> Result<(), ToolError> {
        let ticket = ctx.ticket.as_ref().ok_or_else(|| {
            denied(
                tool,
                "no capability ticket: route desktop access through the agent + policy engine",
            )
        })?;
        let request = CapabilityRequest {
            principal: ctx.principal.clone(),
            capability: capability.clone(),
            resource,
        };
        ticket
            .check(&ctx.principal, &request, &ctx.invocation_id)
            .map_err(|e| denied(tool, format!("ticket does not authorize this desktop call: {e}")))?;
        Ok(())
    }

    fn backend_error(tool: &str, e: DesktopError) -> ToolError {
        match e {
            DesktopError::BackendUnavailable(detail) => failed(tool, detail),
            other => failed(tool, other.to_string()),
        }
    }

    fn opt_window(args: &serde_json::Value) -> String {
        args.get("window_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    }

    fn req_window(args: &serde_json::Value, tool: &str) -> Result<String, ToolError> {
        let id = opt_window(args);
        if id.is_empty() {
            return Err(invalid(tool, "missing string 'window_id'"));
        }
        Ok(id)
    }

    macro_rules! simple_tool {
        ($name:ident, $id:literal, $desc:literal, $effect:expr) => {
            pub struct $name {
                pub backend: Arc<dyn DesktopBackend>,
            }
            impl $name {
                const TOOL: &'static str = $id;
            }
        };
    }

    simple_tool!(
        ListWindowsTool,
        "desktop.list_windows",
        "List visible top-level windows (id, title, app). Filter by app when possible: unfiltered listing always asks approval.",
        ToolEffect::ReadOnly
    );

    #[async_trait::async_trait]
    impl Tool for ListWindowsTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: capability_core::ToolId::new(Self::TOOL),
                description: "List visible top-level windows (id, title, app). Filter by app when possible: unfiltered listing always asks approval.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": { "app": { "type": "string" } },
                }),
                effects: vec![ToolEffect::ReadOnly],
            }
        }

        fn required_capability(
            &self,
            args: &serde_json::Value,
        ) -> Option<CapabilityRequirement> {
            let app = args.get("app").and_then(|v| v.as_str()).unwrap_or("");
            Some(CapabilityRequirement {
                capability: Capability::DesktopObserve,
                resource: Resource::Application(app.to_string()),
            })
        }

        async fn invoke(
            &self,
            ctx: ToolContext,
            args: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            let app = args.get("app").and_then(|v| v.as_str()).unwrap_or("");
            require_ticket(
                Self::TOOL,
                &ctx,
                Capability::DesktopObserve,
                Resource::Application(app.to_string()),
            )?;
            let mut windows = self
                .backend
                .list_windows()
                .await
                .map_err(|e| backend_error(Self::TOOL, e))?;
            if !app.is_empty() {
                windows.retain(|w| w.app == app);
            }
            Ok(ToolOutput::new(serde_json::json!({ "windows": windows })))
        }
    }

    simple_tool!(
        AccessibilityTreeTool,
        "desktop.accessibility_tree",
        "Read one window's accessibility tree (element ids, roles, names, actions). Prefer element actions over coordinates.",
        ToolEffect::ReadOnly
    );

    #[async_trait::async_trait]
    impl Tool for AccessibilityTreeTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: capability_core::ToolId::new(Self::TOOL),
                description: "Read one window's accessibility tree (element ids, roles, names, actions). Prefer element actions over coordinates.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": { "window_id": { "type": "string" } },
                    "required": ["window_id"],
                }),
                effects: vec![ToolEffect::ReadOnly],
            }
        }

        fn required_capability(
            &self,
            args: &serde_json::Value,
        ) -> Option<CapabilityRequirement> {
            req_window(args, Self::TOOL).ok().map(|id| CapabilityRequirement {
                capability: Capability::DesktopObserve,
                resource: Resource::Window(id),
            })
        }

        async fn invoke(
            &self,
            ctx: ToolContext,
            args: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            let window_id = req_window(&args, Self::TOOL)?;
            require_ticket(
                Self::TOOL,
                &ctx,
                Capability::DesktopObserve,
                Resource::Window(window_id.clone()),
            )?;
            let tree = self
                .backend
                .accessibility_tree(&window_id)
                .await
                .map_err(|e| backend_error(Self::TOOL, e))?;
            Ok(ToolOutput::new(
                serde_json::json!({ "window_id": window_id, "elements": tree }),
            ))
        }
    }

    simple_tool!(
        InvokeElementTool,
        "desktop.invoke_element",
        "Invoke one accessibility element (press a button, toggle a checkbox). Element actions beat coordinate clicking.",
        ToolEffect::DesktopControl
    );

    #[async_trait::async_trait]
    impl Tool for InvokeElementTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: capability_core::ToolId::new(Self::TOOL),
                description: "Invoke one accessibility element (press a button, toggle a checkbox). Element actions beat coordinate clicking.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "window_id": { "type": "string" },
                        "element_id": { "type": "string" },
                    },
                    "required": ["window_id", "element_id"],
                }),
                effects: vec![ToolEffect::DesktopControl],
            }
        }

        fn required_capability(
            &self,
            args: &serde_json::Value,
        ) -> Option<CapabilityRequirement> {
            req_window(args, Self::TOOL).ok().map(|id| CapabilityRequirement {
                capability: Capability::DesktopControl,
                resource: Resource::Window(id),
            })
        }

        async fn invoke(
            &self,
            ctx: ToolContext,
            args: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            let window_id = req_window(&args, Self::TOOL)?;
            let element_id = args
                .get("element_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| invalid(Self::TOOL, "missing string 'element_id'"))?;
            require_ticket(
                Self::TOOL,
                &ctx,
                Capability::DesktopControl,
                Resource::Window(window_id.clone()),
            )?;
            self.backend
                .invoke_element(&window_id, element_id)
                .await
                .map_err(|e| backend_error(Self::TOOL, e))?;
            Ok(ToolOutput::new(
                serde_json::json!({ "ok": true, "element_id": element_id }),
            ))
        }
    }

    simple_tool!(
        SetValueTool,
        "desktop.set_value",
        "Set an accessibility element's value (text field contents, slider position).",
        ToolEffect::DesktopControl
    );

    #[async_trait::async_trait]
    impl Tool for SetValueTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: capability_core::ToolId::new(Self::TOOL),
                description: "Set an accessibility element's value (text field contents, slider position).".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "window_id": { "type": "string" },
                        "element_id": { "type": "string" },
                        "value": { "type": "string" },
                    },
                    "required": ["window_id", "element_id", "value"],
                }),
                effects: vec![ToolEffect::DesktopControl],
            }
        }

        fn required_capability(
            &self,
            args: &serde_json::Value,
        ) -> Option<CapabilityRequirement> {
            req_window(args, Self::TOOL).ok().map(|id| CapabilityRequirement {
                capability: Capability::DesktopControl,
                resource: Resource::Window(id),
            })
        }

        async fn invoke(
            &self,
            ctx: ToolContext,
            args: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            let window_id = req_window(&args, Self::TOOL)?;
            let element_id = args
                .get("element_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| invalid(Self::TOOL, "missing string 'element_id'"))?;
            let value = args
                .get("value")
                .and_then(|v| v.as_str())
                .ok_or_else(|| invalid(Self::TOOL, "missing string 'value'"))?;
            if value.chars().count() > MAX_TYPE_CHARS {
                return Err(invalid(Self::TOOL, "value is too long"));
            }
            require_ticket(
                Self::TOOL,
                &ctx,
                Capability::DesktopControl,
                Resource::Window(window_id.clone()),
            )?;
            self.backend
                .set_value(&window_id, element_id, value)
                .await
                .map_err(|e| backend_error(Self::TOOL, e))?;
            Ok(ToolOutput::new(serde_json::json!({ "ok": true })))
        }
    }

    simple_tool!(
        ScreenshotTool,
        "desktop.screenshot",
        "Capture a PNG screenshot (whole desktop or one window). Vision fallback — accessibility actions come first.",
        ToolEffect::ReadOnly
    );

    #[async_trait::async_trait]
    impl Tool for ScreenshotTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: capability_core::ToolId::new(Self::TOOL),
                description: "Capture a PNG screenshot (whole desktop or one window). Vision fallback — accessibility actions come first.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": { "window_id": { "type": "string" } },
                }),
                effects: vec![ToolEffect::ReadOnly],
            }
        }

        fn required_capability(
            &self,
            args: &serde_json::Value,
        ) -> Option<CapabilityRequirement> {
            Some(CapabilityRequirement {
                capability: Capability::ScreenCapture,
                resource: Resource::Window(opt_window(args)),
            })
        }

        async fn invoke(
            &self,
            ctx: ToolContext,
            args: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            let window_id = opt_window(&args);
            require_ticket(
                Self::TOOL,
                &ctx,
                Capability::ScreenCapture,
                Resource::Window(window_id.clone()),
            )?;
            let shot = self
                .backend
                .screenshot(if window_id.is_empty() { None } else { Some(&window_id) })
                .await
                .map_err(|e| backend_error(Self::TOOL, e))?;
            if shot.png_bytes.len() > MAX_SCREENSHOT_BYTES {
                return Err(failed(Self::TOOL, "screenshot exceeds 2 MiB"));
            }
            Ok(ToolOutput::new(serde_json::json!({
                "width": shot.width,
                "height": shot.height,
                "png_base64": base64_encode(&shot.png_bytes),
            })))
        }
    }

    simple_tool!(
        ClickTool,
        "desktop.click",
        "Click at screen coordinates. Last resort: prefer desktop.invoke_element on an accessibility element.",
        ToolEffect::DesktopControl
    );

    #[async_trait::async_trait]
    impl Tool for ClickTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: capability_core::ToolId::new(Self::TOOL),
                description: "Click at screen coordinates. Last resort: prefer desktop.invoke_element on an accessibility element.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "window_id": { "type": "string" },
                        "x": { "type": "integer" },
                        "y": { "type": "integer" },
                    },
                    "required": ["x", "y"],
                }),
                effects: vec![ToolEffect::DesktopControl],
            }
        }

        fn required_capability(
            &self,
            args: &serde_json::Value,
        ) -> Option<CapabilityRequirement> {
            Some(CapabilityRequirement {
                capability: Capability::DesktopControl,
                resource: Resource::Window(opt_window(args)),
            })
        }

        async fn invoke(
            &self,
            ctx: ToolContext,
            args: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            let window_id = opt_window(&args);
            let x = args
                .get("x")
                .and_then(|v| v.as_i64())
                .ok_or_else(|| invalid(Self::TOOL, "missing integer 'x'"))?;
            let y = args
                .get("y")
                .and_then(|v| v.as_i64())
                .ok_or_else(|| invalid(Self::TOOL, "missing integer 'y'"))?;
            let (x, y) = (i32::try_from(x).map_err(|_| invalid(Self::TOOL, "'x' out of range"))?, i32::try_from(y).map_err(|_| invalid(Self::TOOL, "'y' out of range"))?);
            require_ticket(
                Self::TOOL,
                &ctx,
                Capability::DesktopControl,
                Resource::Window(window_id.clone()),
            )?;
            self.backend
                .click(
                    if window_id.is_empty() { None } else { Some(&window_id) },
                    Point { x, y },
                )
                .await
                .map_err(|e| backend_error(Self::TOOL, e))?;
            Ok(ToolOutput::new(serde_json::json!({ "ok": true })))
        }
    }

    simple_tool!(
        TypeTextTool,
        "desktop.type_text",
        "Type text into the focused control (optionally scoped to a window). Prefer desktop.set_value on a named element.",
        ToolEffect::DesktopControl
    );

    #[async_trait::async_trait]
    impl Tool for TypeTextTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: capability_core::ToolId::new(Self::TOOL),
                description: "Type text into the focused control (optionally scoped to a window). Prefer desktop.set_value on a named element.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "window_id": { "type": "string" },
                        "text": { "type": "string" },
                    },
                    "required": ["text"],
                }),
                effects: vec![ToolEffect::DesktopControl],
            }
        }

        fn required_capability(
            &self,
            args: &serde_json::Value,
        ) -> Option<CapabilityRequirement> {
            Some(CapabilityRequirement {
                capability: Capability::DesktopControl,
                resource: Resource::Window(opt_window(args)),
            })
        }

        async fn invoke(
            &self,
            ctx: ToolContext,
            args: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            let window_id = opt_window(&args);
            let text = args
                .get("text")
                .and_then(|v| v.as_str())
                .ok_or_else(|| invalid(Self::TOOL, "missing string 'text'"))?;
            if text.chars().count() > MAX_TYPE_CHARS {
                return Err(invalid(Self::TOOL, "text is too long"));
            }
            require_ticket(
                Self::TOOL,
                &ctx,
                Capability::DesktopControl,
                Resource::Window(window_id.clone()),
            )?;
            self.backend
                .type_text(
                    if window_id.is_empty() { None } else { Some(&window_id) },
                    text,
                )
                .await
                .map_err(|e| backend_error(Self::TOOL, e))?;
            Ok(ToolOutput::new(serde_json::json!({ "ok": true })))
        }
    }
}

/// Placeholder backend: honest failure instead of fake control.
pub struct StubBackend;

#[async_trait::async_trait]
impl DesktopBackend for StubBackend {
    fn is_available(&self) -> bool {
        false
    }
    async fn list_windows(&self) -> Result<Vec<WindowInfo>, DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "desktop backend not implemented on this platform yet (plan Phases 28-30)".to_string(),
        ))
    }
    async fn accessibility_tree(&self, _window_id: &str) -> Result<Vec<ElementNode>, DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "desktop backend not implemented on this platform yet (plan Phases 28-30)".to_string(),
        ))
    }
    async fn invoke_element(&self, _window_id: &str, _element_id: &str) -> Result<(), DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "desktop backend not implemented on this platform yet (plan Phases 28-30)".to_string(),
        ))
    }
    async fn set_value(&self, _window_id: &str, _element_id: &str, _value: &str) -> Result<(), DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "desktop backend not implemented on this platform yet (plan Phases 28-30)".to_string(),
        ))
    }
    async fn screenshot(&self, _window_id: Option<&str>) -> Result<Screenshot, DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "desktop backend not implemented on this platform yet (plan Phases 28-30)".to_string(),
        ))
    }
    async fn click(&self, _window_id: Option<&str>, _at: Point) -> Result<(), DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "desktop backend not implemented on this platform yet (plan Phases 28-30)".to_string(),
        ))
    }
    async fn type_text(&self, _window_id: Option<&str>, _text: &str) -> Result<(), DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "desktop backend not implemented on this platform yet (plan Phases 28-30)".to_string(),
        ))
    }
}

/// Controllable fake backend for tool tests: scripted windows/elements,
/// captures clicks and typed text, optional screenshot bytes.
#[cfg(test)]
pub struct FakeBackend {
    pub windows: Vec<WindowInfo>,
    pub clicks: std::sync::Mutex<Vec<(Option<String>, Point)>>,
    pub typed: std::sync::Mutex<Vec<(Option<String>, String)>>,
}

#[cfg(test)]
impl FakeBackend {
    fn new() -> Self {
        Self {
            windows: vec![WindowInfo {
                id: "w1".to_string(),
                title: "Notes".to_string(),
                app: "notes".to_string(),
            }],
            clicks: std::sync::Mutex::new(Vec::new()),
            typed: std::sync::Mutex::new(Vec::new()),
        }
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl DesktopBackend for FakeBackend {
    async fn list_windows(&self) -> Result<Vec<WindowInfo>, DesktopError> {
        Ok(self.windows.clone())
    }
    async fn accessibility_tree(&self, window_id: &str) -> Result<Vec<ElementNode>, DesktopError> {
        if window_id != "w1" {
            return Err(DesktopError::UnknownWindow(window_id.to_string()));
        }
        Ok(vec![ElementNode {
            id: "e1".to_string(),
            role: "button".to_string(),
            name: "Save".to_string(),
            actions: vec!["invoke".to_string()],
        }])
    }
    async fn invoke_element(&self, window_id: &str, element_id: &str) -> Result<(), DesktopError> {
        if window_id != "w1" {
            return Err(DesktopError::UnknownWindow(window_id.to_string()));
        }
        if element_id != "e1" {
            return Err(DesktopError::UnknownElement(element_id.to_string()));
        }
        Ok(())
    }
    async fn set_value(&self, window_id: &str, element_id: &str, _value: &str) -> Result<(), DesktopError> {
        self.invoke_element(window_id, element_id).await
    }
    async fn screenshot(&self, _window_id: Option<&str>) -> Result<Screenshot, DesktopError> {
        Ok(Screenshot {
            width: 2,
            height: 2,
            png_bytes: vec![0x89, b'P', b'N', b'G'],
        })
    }
    async fn click(&self, window_id: Option<&str>, at: Point) -> Result<(), DesktopError> {
        self.clicks.lock().unwrap().push((window_id.map(|s| s.to_string()), at));
        Ok(())
    }
    async fn type_text(&self, window_id: Option<&str>, text: &str) -> Result<(), DesktopError> {
        self.typed.lock().unwrap().push((window_id.map(|s| s.to_string()), text.to_string()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::tools::*;
    use super::{base64_encode, FakeBackend};
    use capability_core::{
        Capability, CapabilityTicket, InvocationId, Principal, Resource, ResourceScope,
    };
    use std::sync::Arc;
    use std::time::Duration;
    use tool_core::{Tool, ToolContext};

    fn ticket(
        capability: Capability,
        resource: Resource,
        invocation: &InvocationId,
    ) -> CapabilityTicket {
        CapabilityTicket::mint(
            Principal::User,
            capability,
            ResourceScope::new(vec![resource]),
            invocation.clone(),
            Duration::from_secs(120),
        )
    }

    fn ctx_for(capability: Capability, resource: Resource) -> ToolContext {
        let ctx = ToolContext::new(Principal::User);
        let t = ticket(capability, resource, &ctx.invocation_id);
        ctx.with_ticket(t)
    }

    #[test]
    fn base64_vectors() {
        // RFC 4648 §10 test vectors.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[tokio::test]
    async fn stub_backend_is_honest() {
        let tool = ListWindowsTool {
            backend: super::backend(),
        };
        let ctx = ctx_for(
            Capability::DesktopObserve,
            Resource::Application("notes".to_string()),
        );
        let err = tool
            .invoke(ctx, serde_json::json!({"app": "notes"}))
            .await
            .unwrap_err();
        assert!(matches!(err, tool_core::ToolError::Failed { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn calls_without_tickets_are_denied() {
        let backend: Arc<dyn super::DesktopBackend> = Arc::new(FakeBackend::new());
        let tool = ListWindowsTool { backend };
        let err = tool
            .invoke(ToolContext::new(Principal::User), serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, tool_core::ToolError::Denied { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn list_and_tree_with_fake_backend() {
        let backend: Arc<dyn super::DesktopBackend> = Arc::new(FakeBackend::new());
        let list = ListWindowsTool {
            backend: backend.clone(),
        };
        // Unfiltered listing declares the whole-desktop resource.
        let req = list.required_capability(&serde_json::json!({})).unwrap();
        assert_eq!(req.capability, Capability::DesktopObserve);
        let out = list
            .invoke(
                ctx_for(Capability::DesktopObserve, Resource::Application(String::new())),
                serde_json::json!({}),
            )
            .await
            .unwrap();
        assert_eq!(out.content["windows"][0]["title"], "Notes");

        let tree = AccessibilityTreeTool {
            backend: backend.clone(),
        };
        let out = tree
            .invoke(
                ctx_for(Capability::DesktopObserve, Resource::Window("w1".to_string())),
                serde_json::json!({"window_id": "w1"}),
            )
            .await
            .unwrap();
        assert_eq!(out.content["elements"][0]["name"], "Save");

        // Unknown windows surface backend errors, never panics.
        let err = tree
            .invoke(
                ctx_for(Capability::DesktopObserve, Resource::Window("nope".to_string())),
                serde_json::json!({"window_id": "nope"}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, tool_core::ToolError::Failed { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn invoke_click_type_roundtrip() {
        let fake = Arc::new(FakeBackend::new());
        let backend: Arc<dyn super::DesktopBackend> = fake.clone();

        let invoke = InvokeElementTool {
            backend: backend.clone(),
        };
        let out = invoke
            .invoke(
                ctx_for(Capability::DesktopControl, Resource::Window("w1".to_string())),
                serde_json::json!({"window_id": "w1", "element_id": "e1"}),
            )
            .await
            .unwrap();
        assert_eq!(out.content["ok"], true);

        let click = ClickTool {
            backend: backend.clone(),
        };
        click
            .invoke(
                ctx_for(Capability::DesktopControl, Resource::Window(String::new())),
                serde_json::json!({"x": 10, "y": 20}),
            )
            .await
            .unwrap();
        assert_eq!(
            *fake.clicks.lock().unwrap(),
            vec![(None, super::Point { x: 10, y: 20 })]
        );

        let type_tool = TypeTextTool { backend };
        type_tool
            .invoke(
                ctx_for(Capability::DesktopControl, Resource::Window("w1".to_string())),
                serde_json::json!({"window_id": "w1", "text": "hello"}),
            )
            .await
            .unwrap();
        assert_eq!(
            *fake.typed.lock().unwrap(),
            vec![(Some("w1".to_string()), "hello".to_string())]
        );

        // Oversized text is rejected before touching the backend.
        let err = type_tool
            .invoke(
                ctx_for(Capability::DesktopControl, Resource::Window("w1".to_string())),
                serde_json::json!({"window_id": "w1", "text": "x".repeat(5000)}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, tool_core::ToolError::InvalidArgs { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn screenshot_returns_png_base64() {
        let backend: Arc<dyn super::DesktopBackend> = Arc::new(FakeBackend::new());
        let shot = ScreenshotTool { backend };
        let out = shot
            .invoke(
                ctx_for(Capability::ScreenCapture, Resource::Window(String::new())),
                serde_json::json!({}),
            )
            .await
            .unwrap();
        assert_eq!(out.content["width"], 2);
        assert_eq!(out.content["png_base64"], "iVBORw==");
    }
}
