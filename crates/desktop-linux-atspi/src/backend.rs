//! [`DesktopPlugin`] with native AT-SPI semantics.
//!
//! Semantic-only on purpose: window enumeration, trees, and element
//! actions come from the AT-SPI registry; pixels and pointer/keyboard
//! control stay with the portal backend (`desktop-linux-wayland`).
//! Every capture/control method therefore fails closed with
//! [`DesktopError::BackendUnavailable`] instead of guessing.

use std::sync::Arc;

use super::live::{self, AtspiService};
use super::snapshot::{flatten_window_tree, AtspiError};
use tool_desktop::plugin::{DesktopCapability, DesktopPlugin, DesktopPluginManifest};
use tool_desktop::{
    AccessibilitySnapshot, DesktopBackend, DesktopError, ElementNode, Point, Screenshot, WindowInfo,
};

fn describe(error: AtspiError) -> DesktopError {
    match error {
        AtspiError::BackendUnavailable(detail) => DesktopError::BackendUnavailable(detail),
        AtspiError::Unsupported(detail) => DesktopError::BackendUnavailable(detail),
        AtspiError::Bus { operation, detail } => {
            DesktopError::ActionFailed(format!("AT-SPI {operation}: {detail}"))
        }
        AtspiError::ActionFailed { operation, detail } => {
            DesktopError::ActionFailed(format!("AT-SPI {operation}: {detail}"))
        }
        AtspiError::TreeTooLarge { depth, nodes } => DesktopError::ActionFailed(format!(
            "AT-SPI tree exceeded limits (depth {depth}, nodes {nodes})"
        )),
    }
}

#[derive(Debug, Clone)]
struct AtspiBackend {
    service: AtspiService,
}

impl AtspiBackend {
    fn ensure_available(&self, operation: &str) -> Result<(), DesktopError> {
        if self.service.is_available() {
            Ok(())
        } else {
            Err(DesktopError::BackendUnavailable(format!(
                "AT-SPI is not available for {operation} (state: {:?})",
                self.service.availability()
            )))
        }
    }
}

#[async_trait::async_trait]
impl DesktopBackend for AtspiBackend {
    fn is_available(&self) -> bool {
        self.service.is_available()
    }

    async fn list_windows(&self) -> Result<Vec<WindowInfo>, DesktopError> {
        tracing::trace!("desktop.atspi.list_windows.begin");
        self.ensure_available("window enumeration")?;
        let trees = self
            .service
            .run_live("collect_window_trees", live::collect_window_trees)
            .await
            .map_err(describe)?;
        tracing::trace!(count = trees.len(), "desktop.atspi.list_windows.ok");
        Ok(trees
            .iter()
            .map(|tree| WindowInfo {
                id: tree.id.clone(),
                title: tree.name.clone(),
                app: tree.app.clone(),
            })
            .collect())
    }

    async fn accessibility_tree(&self, window_id: &str) -> Result<Vec<ElementNode>, DesktopError> {
        self.ensure_available("accessibility tree")?;
        let trees = self
            .service
            .run_live("collect_window_trees", live::collect_window_trees)
            .await
            .map_err(describe)?;
        let tree = trees
            .iter()
            .find(|tree| tree.id == window_id)
            .ok_or_else(|| DesktopError::UnknownWindow(window_id.to_string()))?;
        Ok(flatten_window_tree(tree))
    }

    async fn accessibility_snapshot(
        &self,
        window_id: &str,
        _since: Option<&str>,
    ) -> Result<AccessibilitySnapshot, DesktopError> {
        self.ensure_available("accessibility snapshot")?;
        let nodes = self.accessibility_tree(window_id).await?;
        let focused = nodes
            .iter()
            .find(|node| node.focused == Some(true))
            .map(|node| node.id.clone());
        Ok(AccessibilitySnapshot {
            snapshot_id: uuid::Uuid::new_v4().to_string(),
            window_id: window_id.to_string(),
            generation: 0,
            nodes,
            removed_node_ids: Vec::new(),
            focused_node_id: focused,
        })
    }

    async fn invoke_element(&self, _window_id: &str, element_id: &str) -> Result<(), DesktopError> {
        self.ensure_available("element invocation")?;
        self.service
            .run_live("invoke_element", |conn| {
                live::invoke_element(conn, element_id)
            })
            .await
            .map_err(|error| match error {
                AtspiError::Unsupported(detail) => DesktopError::UnknownElement(detail),
                other => describe(other),
            })
    }

    async fn focus_element(&self, _window_id: &str, element_id: &str) -> Result<(), DesktopError> {
        self.ensure_available("element focus")?;
        self.service
            .run_live("focus_element", |conn| {
                live::focus_element(conn, element_id)
            })
            .await
            .map_err(|error| match error {
                AtspiError::Unsupported(detail) => DesktopError::UnknownElement(detail),
                other => describe(other),
            })
    }

    async fn set_value(
        &self,
        _window_id: &str,
        element_id: &str,
        value: &str,
    ) -> Result<(), DesktopError> {
        self.ensure_available("semantic value setting")?;
        self.service
            .run_live("set_value", |conn| live::set_value(conn, element_id, value))
            .await
            .map_err(|error| match error {
                AtspiError::Unsupported(detail) => DesktopError::UnknownElement(detail),
                other => describe(other),
            })
    }

    async fn select_element(&self, _window_id: &str, element_id: &str) -> Result<(), DesktopError> {
        self.ensure_available("semantic selection")?;
        self.service
            .run_live("select_element", |conn| {
                live::select_element(conn, element_id)
            })
            .await
            .map_err(describe)
    }

    async fn expand_element(&self, _window_id: &str, element_id: &str) -> Result<(), DesktopError> {
        self.ensure_available("semantic expansion")?;
        self.service
            .run_live("expand_element", |conn| {
                live::expand_element(conn, element_id, true)
            })
            .await
            .map_err(describe)
    }

    async fn collapse_element(
        &self,
        _window_id: &str,
        element_id: &str,
    ) -> Result<(), DesktopError> {
        self.ensure_available("semantic collapse")?;
        self.service
            .run_live("expand_element", |conn| {
                live::expand_element(conn, element_id, false)
            })
            .await
            .map_err(describe)
    }

    async fn focus_window(&self, window_id: &str) -> Result<(), DesktopError> {
        self.ensure_available("window focus")?;
        self.service
            .run_live("focus_element", |conn| live::focus_element(conn, window_id))
            .await
            .map_err(|error| match error {
                AtspiError::Unsupported(_) => DesktopError::UnknownWindow(window_id.to_string()),
                other => describe(other),
            })
    }

    async fn screenshot(&self, _window_id: Option<&str>) -> Result<Screenshot, DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "AT-SPI carries no pixels; capture belongs to the portal backend".to_string(),
        ))
    }

    async fn click(&self, _window_id: Option<&str>, _at: Point) -> Result<(), DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "coordinate clicks bypass accessibility semantics; use invoke_element or the portal backend"
                .to_string(),
        ))
    }

    async fn type_text(&self, _window_id: Option<&str>, _text: &str) -> Result<(), DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "raw key synthesis belongs to the portal backend; use set_value for editable elements"
                .to_string(),
        ))
    }

    async fn key_down(&self, _key: &str) -> Result<(), DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "raw key synthesis belongs to the portal backend".to_string(),
        ))
    }

    async fn key_up(&self, _key: &str) -> Result<(), DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "raw key synthesis belongs to the portal backend".to_string(),
        ))
    }

    async fn clipboard_read(&self, _mime_type: &str) -> Result<String, DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "clipboard access is not part of the AT-SPI semantic backend".to_string(),
        ))
    }

    async fn clipboard_write(&self, _mime_type: &str, _data: &str) -> Result<(), DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "clipboard access is not part of the AT-SPI semantic backend".to_string(),
        ))
    }

    async fn launch_application(&self, _app: &str) -> Result<(), DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "application launching is not part of the AT-SPI semantic backend".to_string(),
        ))
    }
}

/// Build the native AT-SPI semantic plugin over a shared host service. The
/// plugin is registered while the service is still `Unknown`/`Probing`; its
/// availability gate stays fail-closed until the one background probe says
/// the registry answers.
pub fn plugin_with_service(service: AtspiService) -> DesktopPlugin {
    DesktopPlugin::new(
        DesktopPluginManifest {
            id: "desktop.linux-atspi".to_string(),
            name: "Linux AT-SPI semantic backend".to_string(),
            version: "0.1.0".to_string(),
            platforms: vec!["linux".to_string()],
            capabilities: vec![
                DesktopCapability::ListWindows,
                DesktopCapability::AccessibilityTree,
                DesktopCapability::Observe,
                DesktopCapability::InvokeElement,
                DesktopCapability::FocusElement,
                DesktopCapability::FocusWindow,
                DesktopCapability::SetValue,
                DesktopCapability::SelectElement,
                DesktopCapability::ExpandElement,
                DesktopCapability::CollapseElement,
            ],
            description: "Native AT-SPI2/D-Bus accessibility trees and element actions; capture and raw input stay with the portal backend."
                .to_string(),
        },
        Arc::new(AtspiBackend { service }),
    )
}

/// Compatibility constructor for non-host callers. Host composition should
/// use [`plugin_with_service`] so Wayland and standalone registration share
/// one probe.
pub fn plugin() -> Option<DesktopPlugin> {
    Some(plugin_with_service(AtspiService::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_unavailable_maps_to_backend_unavailable() {
        let mapped = describe(AtspiError::BackendUnavailable("no bus".to_string()));
        assert!(matches!(mapped, DesktopError::BackendUnavailable(_)));
    }

    #[test]
    fn action_failure_maps_to_action_failed() {
        let mapped = describe(AtspiError::ActionFailed {
            operation: "do action",
            detail: "rejected".to_string(),
        });
        assert!(matches!(mapped, DesktopError::ActionFailed(_)));
    }
}
