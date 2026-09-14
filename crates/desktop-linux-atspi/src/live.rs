//! Live AT-SPI walk over the session bus.
//!
//! This module talks to the real `org.a11y.atspi.Registry` and builds
//! [`AccessibleSnapshot`] trees from live objects. It never synthesizes
//! nodes: an unreachable bus is [`AtspiError::BackendUnavailable`], a
//! vanished object aborts its own subtree, and every walk is bounded by
//! [`MAX_TREE_DEPTH`] / [`MAX_TREE_NODES`].
//!
//! Element and window ids are `atspi://{bus}/{object-path}` URIs, so any
//! id handed back by the model resolves to exactly one bus object.

use super::snapshot::{
    is_window_role, AccessibleSnapshot, AtspiError, MAX_NAME_CHARS, MAX_TREE_DEPTH, MAX_TREE_NODES,
    MAX_VALUE_CHARS,
};
use atspi::proxy::accessible::AccessibleProxy;
use atspi::proxy::proxy_ext::ProxyExt;
use atspi::zbus::names::BusName;
use atspi::{AccessibilityConnection, CoordType, Role, StateSet};
use tool_desktop::Rect;

const ID_SCHEME: &str = "atspi://";
/// Longest text read from a `Text` object in one walk (bus-provided).
const MAX_TEXT_READ: i32 = 4096;
/// Longest action list kept per object.
const MAX_ACTIONS: usize = 16;
/// Probe timeout for [`is_available`] so boot never blocks on a dead bus.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        text.to_string()
    } else {
        text.chars().take(limit).collect()
    }
}

/// Global identity for one bus object.
fn object_id(bus: &str, path: &str) -> String {
    format!("{ID_SCHEME}{bus}/{}", path.trim_start_matches('/'))
}

/// Split an `atspi://` id back into `(bus, path)`.
fn parse_id(id: &str) -> Result<(String, String), AtspiError> {
    let rest = id
        .strip_prefix(ID_SCHEME)
        .ok_or_else(|| AtspiError::Unsupported(format!("not an AT-SPI element id: {id}")))?;
    let (bus, path) = rest
        .split_once('/')
        .ok_or_else(|| AtspiError::Unsupported(format!("malformed AT-SPI element id: {id}")))?;
    if bus.is_empty() || path.is_empty() {
        return Err(AtspiError::Unsupported(format!(
            "malformed AT-SPI element id: {id}"
        )));
    }
    Ok((bus.to_string(), format!("/{path}")))
}

async fn proxy_for<'a>(
    connection: &'a atspi::zbus::Connection,
    bus: &'a str,
    path: &'a str,
) -> Result<AccessibleProxy<'a>, AtspiError> {
    let destination =
        BusName::try_from(bus).map_err(|detail| AtspiError::bus("parse bus name", detail))?;
    AccessibleProxy::builder(connection)
        .destination(destination)
        .map_err(|detail| AtspiError::bus("proxy destination", detail))?
        .path(path)
        .map_err(|detail| AtspiError::bus("proxy path", detail))?
        .build()
        .await
        .map_err(|detail| AtspiError::bus("proxy build", detail))
}

/// Cheap probe: true only when a registry answers. Runs on its own
/// thread with a fresh runtime so sync plugin selection can call it from
/// any executor context without panicking.
pub fn is_available() -> bool {
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let reachable = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()
            .and_then(|runtime| {
                runtime.block_on(async {
                    tokio::time::timeout(PROBE_TIMEOUT, AccessibilityConnection::new())
                        .await
                        .ok()
                        .and_then(|result| result.ok())
                })
            })
            .is_some();
        let _ = done_tx.send(reachable);
    });
    done_rx
        .recv_timeout(PROBE_TIMEOUT + std::time::Duration::from_secs(2))
        .unwrap_or(false)
}

struct Walker<'a> {
    connection: &'a atspi::zbus::Connection,
    budget: usize,
}

impl Walker<'_> {
    fn take_budget(&mut self) -> Result<(), AtspiError> {
        if self.budget == 0 {
            return Err(AtspiError::TreeTooLarge {
                depth: MAX_TREE_DEPTH,
                nodes: MAX_TREE_NODES,
            });
        }
        self.budget -= 1;
        Ok(())
    }

    async fn snapshot_object(
        &mut self,
        proxy: &AccessibleProxy<'_>,
        bus: &str,
        path: &str,
        app: &str,
        depth: usize,
    ) -> Result<AccessibleSnapshot, AtspiError> {
        self.take_budget()?;
        let role = proxy
            .get_role()
            .await
            .map_err(|detail| AtspiError::bus("get role", detail))?;
        let name = proxy
            .name()
            .await
            .map(|name| truncate(&name, MAX_NAME_CHARS))
            .unwrap_or_default();
        let description = proxy
            .description()
            .await
            .map(|description| truncate(&description, MAX_NAME_CHARS))
            .unwrap_or_default();
        let states = proxy
            .get_state()
            .await
            .unwrap_or_else(|_| StateSet::empty());
        let interfaces = proxy
            .get_interfaces()
            .await
            .map(|set| {
                set.iter()
                    .map(|interface| format!("{interface:?}"))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        // Actions and text are best-effort: a racing app may drop an
        // interface between `get_interfaces` and the follow-up call.
        let actions = match proxy.proxies().await {
            Ok(proxies) => match proxies.action().await {
                Ok(action) => action
                    .get_actions()
                    .await
                    .map(|actions| {
                        actions
                            .into_iter()
                            .take(MAX_ACTIONS)
                            .map(|action| truncate(&action.name, MAX_NAME_CHARS))
                            .collect()
                    })
                    .unwrap_or_default(),
                Err(_) => Vec::new(),
            },
            Err(_) => Vec::new(),
        };

        let value_text = if role == Role::PasswordText {
            None
        } else if interfaces.iter().any(|name| name == "Text") {
            match proxy.proxies().await {
                Ok(proxies) => match proxies.text().await {
                    Ok(text) => {
                        let count = text.character_count().await.unwrap_or(0).max(0);
                        let end = count.min(MAX_TEXT_READ);
                        text.get_text(0, end)
                            .await
                            .ok()
                            .map(|value| truncate(&value, MAX_VALUE_CHARS))
                            .filter(|value| !value.is_empty())
                    }
                    Err(_) => None,
                },
                Err(_) => None,
            }
        } else {
            None
        };

        let bounds = match proxy.proxies().await {
            Ok(proxies) => match proxies.component().await {
                Ok(component) => component.get_extents(CoordType::Screen).await.ok().map(
                    |(x, y, width, height)| Rect {
                        x,
                        y,
                        width,
                        height,
                    },
                ),
                Err(_) => None,
            },
            Err(_) => None,
        };

        let mut children = Vec::new();
        if depth < MAX_TREE_DEPTH {
            let child_refs = proxy.get_children().await.unwrap_or_default();
            for child_ref in child_refs {
                let Some(child_bus) = child_ref.name_as_str().map(|name| name.to_owned()) else {
                    continue;
                };
                let child_path = child_ref.path_as_str().to_string();
                let Ok(child_proxy) = proxy_for(self.connection, &child_bus, &child_path).await
                else {
                    continue;
                };
                match Box::pin(self.snapshot_object(
                    &child_proxy,
                    &child_bus,
                    &child_path,
                    app,
                    depth + 1,
                ))
                .await
                {
                    Ok(child) => children.push(child),
                    Err(AtspiError::TreeTooLarge { .. }) => {
                        return Err(AtspiError::TreeTooLarge {
                            depth,
                            nodes: MAX_TREE_NODES,
                        });
                    }
                    // The child vanished mid-walk; its siblings still count.
                    Err(_) => continue,
                }
                if self.budget == 0 {
                    break;
                }
            }
        }

        Ok(AccessibleSnapshot {
            id: object_id(bus, path),
            app: app.to_string(),
            role,
            name,
            description,
            states,
            interfaces,
            actions,
            value_text,
            bounds,
            children,
        })
    }
}

async fn connect() -> Result<(AccessibilityConnection, atspi::zbus::Connection), AtspiError> {
    let accessible = AccessibilityConnection::new().await.map_err(|detail| {
        AtspiError::BackendUnavailable(format!("AT-SPI registry unreachable: {detail}"))
    })?;
    let connection = accessible.connection().clone();
    Ok((accessible, connection))
}

/// One snapshot tree per top-level window, across all applications.
pub async fn collect_window_trees() -> Result<Vec<AccessibleSnapshot>, AtspiError> {
    let (accessible, connection) = connect().await?;
    let root = accessible
        .root_accessible_on_registry()
        .await
        .map_err(|detail| AtspiError::bus("registry root", detail))?;
    let mut walker = Walker {
        connection: &connection,
        budget: MAX_TREE_NODES,
    };
    let mut windows = Vec::new();
    let apps = root
        .get_children()
        .await
        .map_err(|detail| AtspiError::bus("list applications", detail))?;
    for app_ref in apps {
        let Some(app_bus) = app_ref.name_as_str().map(|name| name.to_owned()) else {
            continue;
        };
        let app_path = app_ref.path_as_str().to_string();
        let Ok(app_proxy) = proxy_for(&connection, &app_bus, &app_path).await else {
            continue;
        };
        let app_name = app_proxy.name().await.unwrap_or_default();
        let app_name = if app_name.is_empty() {
            app_bus.clone()
        } else {
            truncate(&app_name, MAX_NAME_CHARS)
        };
        let tops = app_proxy.get_children().await.unwrap_or_default();
        for top_ref in tops {
            let Some(top_bus) = top_ref.name_as_str().map(|name| name.to_owned()) else {
                continue;
            };
            let top_path = top_ref.path_as_str().to_string();
            let Ok(top_proxy) = proxy_for(&connection, &top_bus, &top_path).await else {
                continue;
            };
            let role = top_proxy
                .get_role()
                .await
                .map_err(|detail| AtspiError::bus("get role", detail))?;
            if !is_window_role(&role) {
                continue;
            }
            match walker
                .snapshot_object(&top_proxy, &top_bus, &top_path, &app_name, 0)
                .await
            {
                Ok(tree) => windows.push(tree),
                Err(AtspiError::TreeTooLarge { .. }) => return Ok(windows),
                Err(_) => continue,
            }
        }
    }
    Ok(windows)
}

/// Bind an element id to owned `(bus, path)` parts. Call sites keep the
/// pair alive while the proxy borrows it (the builder borrows both, so
/// they cannot be hidden inside a helper that returns the proxy).
fn resolve(id: &str) -> Result<(String, String), AtspiError> {
    parse_id(id)
}

/// Invoke the element's default action (first `click`-like action, else
/// the first action the object advertises).
pub async fn invoke_element(element_id: &str) -> Result<(), AtspiError> {
    let (_accessible, connection) = connect().await?;
    let (bus, path) = resolve(element_id)?;
    let proxy = proxy_for(&connection, &bus, &path).await?;
    let proxies = proxy
        .proxies()
        .await
        .map_err(|detail| AtspiError::bus("list interfaces", detail))?;
    let action = proxies
        .action()
        .await
        .map_err(|_| AtspiError::Unsupported(format!("{element_id} has no Action interface")))?;
    let actions = action
        .get_actions()
        .await
        .map_err(|detail| AtspiError::bus("list actions", detail))?;
    if actions.is_empty() {
        return Err(AtspiError::Unsupported(format!(
            "{element_id} advertises no actions"
        )));
    }
    let preferred = ["click", "press", "toggle", "activate", "jump", "open"];
    let index = actions
        .iter()
        .position(|action| {
            let name = action.name.to_lowercase();
            preferred.iter().any(|want| name.contains(want))
        })
        .unwrap_or(0) as i32;
    let done = action
        .do_action(index)
        .await
        .map_err(|detail| AtspiError::bus("do action", detail))?;
    if done {
        Ok(())
    } else {
        Err(AtspiError::ActionFailed {
            operation: "do action",
            detail: format!("object rejected action index {index}"),
        })
    }
}

/// Move keyboard focus to the element.
pub async fn focus_element(element_id: &str) -> Result<(), AtspiError> {
    let (_accessible, connection) = connect().await?;
    let (bus, path) = resolve(element_id)?;
    let proxy = proxy_for(&connection, &bus, &path).await?;
    let proxies = proxy
        .proxies()
        .await
        .map_err(|detail| AtspiError::bus("list interfaces", detail))?;
    let component = proxies
        .component()
        .await
        .map_err(|_| AtspiError::Unsupported(format!("{element_id} has no Component interface")))?;
    let done = component
        .grab_focus()
        .await
        .map_err(|detail| AtspiError::bus("grab focus", detail))?;
    if done {
        Ok(())
    } else {
        Err(AtspiError::ActionFailed {
            operation: "grab focus",
            detail: "object refused focus".to_string(),
        })
    }
}

/// Set an editable element's text (or numeric value for `Value` objects).
pub async fn set_value(element_id: &str, value: &str) -> Result<(), AtspiError> {
    let (_accessible, connection) = connect().await?;
    let (bus, path) = resolve(element_id)?;
    let proxy = proxy_for(&connection, &bus, &path).await?;
    let proxies = proxy
        .proxies()
        .await
        .map_err(|detail| AtspiError::bus("list interfaces", detail))?;
    if let Ok(editable) = proxies.editable_text().await {
        let done = editable
            .set_text_contents(value)
            .await
            .map_err(|detail| AtspiError::bus("set text", detail))?;
        return if done {
            Ok(())
        } else {
            Err(AtspiError::ActionFailed {
                operation: "set text",
                detail: "object rejected the new contents".to_string(),
            })
        };
    }
    if let Ok(number) = proxies.value().await {
        let parsed: f64 = value.trim().parse().map_err(|_| {
            AtspiError::Unsupported(format!(
                "{element_id} takes a numeric value, got non-numeric text"
            ))
        })?;
        number
            .set_current_value(parsed)
            .await
            .map_err(|detail| AtspiError::bus("set value", detail))?;
        return Ok(());
    }
    Err(AtspiError::Unsupported(format!(
        "{element_id} is not editable (no EditableText or Value interface)"
    )))
}

/// Select the element through its parent's `Selection` interface.
pub async fn select_element(element_id: &str) -> Result<(), AtspiError> {
    let (_accessible, connection) = connect().await?;
    let (bus, path) = resolve(element_id)?;
    let proxy = proxy_for(&connection, &bus, &path).await?;
    let parent_ref = proxy
        .parent()
        .await
        .map_err(|detail| AtspiError::bus("get parent", detail))?;
    let Some(parent_bus) = parent_ref.name_as_str().map(|name| name.to_owned()) else {
        return Err(AtspiError::Unsupported(
            "element has no addressable parent".to_string(),
        ));
    };
    let parent_path = parent_ref.path_as_str().to_string();
    let parent = proxy_for(&connection, &parent_bus, &parent_path).await?;
    let proxies = parent
        .proxies()
        .await
        .map_err(|detail| AtspiError::bus("list interfaces", detail))?;
    let selection = proxies
        .selection()
        .await
        .map_err(|_| AtspiError::Unsupported("parent has no Selection interface".to_string()))?;
    let siblings = parent
        .get_children()
        .await
        .map_err(|detail| AtspiError::bus("list siblings", detail))?;
    let (_, wanted_path) = parse_id(element_id)?;
    let index = siblings
        .iter()
        .position(|sibling| sibling.path_as_str() == wanted_path)
        .ok_or_else(|| {
            AtspiError::Unsupported("element is no longer among its parent's children".to_string())
        })? as i32;
    let done = selection
        .select_child(index)
        .await
        .map_err(|detail| AtspiError::bus("select child", detail))?;
    if done {
        Ok(())
    } else {
        Err(AtspiError::ActionFailed {
            operation: "select child",
            detail: "parent rejected the selection".to_string(),
        })
    }
}

/// Run an `expand`- or `collapse`-named action on the element.
pub async fn expand_element(element_id: &str, expand: bool) -> Result<(), AtspiError> {
    let wanted = if expand { "expand" } else { "collapse" };
    let (_accessible, connection) = connect().await?;
    let (bus, path) = resolve(element_id)?;
    let proxy = proxy_for(&connection, &bus, &path).await?;
    let proxies = proxy
        .proxies()
        .await
        .map_err(|detail| AtspiError::bus("list interfaces", detail))?;
    let action = proxies
        .action()
        .await
        .map_err(|_| AtspiError::Unsupported(format!("{element_id} has no Action interface")))?;
    let actions = action
        .get_actions()
        .await
        .map_err(|detail| AtspiError::bus("list actions", detail))?;
    let index = actions
        .iter()
        .position(|action| action.name.to_lowercase().contains(wanted))
        .ok_or_else(|| AtspiError::Unsupported(format!("{element_id} has no {wanted} action")))?
        as i32;
    let done = action
        .do_action(index)
        .await
        .map_err(|detail| AtspiError::bus("do action", detail))?;
    if done {
        Ok(())
    } else {
        Err(AtspiError::ActionFailed {
            operation: "do action",
            detail: format!("object rejected the {wanted} action"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip() {
        let id = object_id(":1.42", "/org/a11y/atspi/accessible/1");
        assert_eq!(id, "atspi://:1.42/org/a11y/atspi/accessible/1");
        let (bus, path) = parse_id(&id).unwrap();
        assert_eq!(
            (bus.as_str(), path.as_str()),
            (":1.42", "/org/a11y/atspi/accessible/1")
        );
    }

    #[test]
    fn foreign_ids_are_rejected_not_resolved() {
        assert!(parse_id("0x3400012").is_err());
        assert!(parse_id("atspi://").is_err());
        assert!(parse_id("atspi:///path-only").is_err());
    }
}
