//! Pure AT-SPI → [`tool_desktop::ElementNode`] conversion.
//!
//! No D-Bus here: the live walk ([`crate::live`]) builds
//! [`AccessibleSnapshot`] trees from bus objects, and everything below
//! turns them into model-facing nodes. Unit tests construct snapshots
//! directly, so conversion semantics are pinned without a bus.

use atspi::{Role, State, StateSet};
use tool_desktop::{ElementNode, ElementSensitivity, Rect};

/// Local names for the AT-SPI role/state types used across this crate.
pub use atspi::{Role as AtspiRole, State as AtspiState};

/// Failures from the AT-SPI backend. Every variant names the layer that
/// failed; none of them synthesize accessibility data.
#[derive(Debug, thiserror::Error)]
pub enum AtspiError {
    /// No registry answered on the session bus (or the bus itself is
    /// unreachable). Callers must surface this, never a fake tree.
    #[error("AT-SPI backend unavailable: {0}")]
    BackendUnavailable(String),
    /// The object does not implement the interface an action needs
    /// (no `Action` for invoke, no `EditableText`/`Value` for set, …).
    #[error("AT-SPI object does not support this operation: {0}")]
    Unsupported(String),
    /// A D-Bus round-trip failed mid-walk or mid-action.
    #[error("AT-SPI D-Bus call failed ({operation}): {detail}")]
    Bus {
        operation: &'static str,
        detail: String,
    },
    /// The remote object reported failure for an action (returned
    /// `false` from `DoAction`, rejected a value, …).
    #[error("AT-SPI action failed ({operation}): {detail}")]
    ActionFailed {
        operation: &'static str,
        detail: String,
    },
    /// The live tree blew past depth/node budgets.
    #[error("AT-SPI tree exceeded limits (depth {depth}, nodes {nodes})")]
    TreeTooLarge { depth: usize, nodes: usize },
}

impl AtspiError {
    pub(crate) fn bus(operation: &'static str, detail: impl std::fmt::Display) -> Self {
        Self::Bus {
            operation,
            detail: detail.to_string(),
        }
    }
}

/// Maximum recursion depth when flattening one window tree.
pub const MAX_TREE_DEPTH: usize = 24;
/// Maximum nodes flattened from one window tree.
pub const MAX_TREE_NODES: usize = 2000;
/// Bounds for bus-provided strings (a malicious app must not blow up
/// model context through a giant accessible name).
pub const MAX_NAME_CHARS: usize = 256;
pub const MAX_VALUE_CHARS: usize = 4096;

/// Bus-independent snapshot of one accessible object. The live layer
/// fills this from AT-SPI proxies; tests fill it by hand.
#[derive(Debug, Clone)]
pub struct AccessibleSnapshot {
    /// Global element identity: `atspi://{bus}/{object-path}`.
    pub id: String,
    /// Human application name (for `WindowInfo.app` attribution).
    pub app: String,
    pub role: Role,
    pub name: String,
    pub description: String,
    pub states: StateSet,
    /// Interface names present on the object (`Action`, `Component`,
    /// `EditableText`, `Selection`, `Value`, `Text`, …).
    pub interfaces: Vec<String>,
    /// Action names from the `Action` interface (`click`, `press`, …).
    pub actions: Vec<String>,
    /// Current text/value when the object exposes one. Never populated
    /// for password fields (see [`element_from_snapshot`]).
    pub value_text: Option<String>,
    pub bounds: Option<Rect>,
    pub children: Vec<AccessibleSnapshot>,
}

/// AT-SPI roles that count as top-level windows for enumeration.
pub fn is_window_role(role: &Role) -> bool {
    matches!(
        role,
        Role::Frame | Role::Dialog | Role::Window | Role::Alert | Role::Notification
    )
}

/// Normalize an AT-SPI role to a compact model role string.
pub fn model_role(role: &Role) -> String {
    match role {
        Role::Frame | Role::Dialog | Role::Window => "window".to_string(),
        Role::Button | Role::PushButtonMenu | Role::ToggleButton => "button".to_string(),
        Role::CheckBox | Role::CheckMenuItem => "checkbox".to_string(),
        Role::RadioButton | Role::RadioMenuItem => "radio".to_string(),
        Role::Text | Role::Entry | Role::PasswordText | Role::SpinButton => "textbox".to_string(),
        Role::ComboBox => "combobox".to_string(),
        Role::List | Role::ListBox => "list".to_string(),
        Role::ListItem => "listitem".to_string(),
        Role::Menu | Role::PopupMenu | Role::TearoffMenuItem => "menu".to_string(),
        Role::MenuBar => "menubar".to_string(),
        Role::MenuItem => "menuitem".to_string(),
        Role::Table => "table".to_string(),
        Role::TableCell => "tablecell".to_string(),
        Role::TableRow => "tablerow".to_string(),
        Role::Tree | Role::TreeTable => "tree".to_string(),
        Role::TreeItem => "treeitem".to_string(),
        Role::PageTab => "tab".to_string(),
        Role::PageTabList => "tablist".to_string(),
        Role::Link => "link".to_string(),
        Role::Image => "image".to_string(),
        Role::Heading => "heading".to_string(),
        Role::Paragraph => "paragraph".to_string(),
        Role::ScrollBar => "scrollbar".to_string(),
        Role::Slider | Role::Dial => "slider".to_string(),
        Role::ProgressBar | Role::LevelBar => "progressbar".to_string(),
        Role::Static | Role::Label => "label".to_string(),
        Role::ToolBar => "toolbar".to_string(),
        Role::StatusBar => "statusbar".to_string(),
        Role::ToolTip => "tooltip".to_string(),
        Role::Terminal => "terminal".to_string(),
        _ => role.name().to_lowercase().replace(' ', ""),
    }
}

fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        text.to_string()
    } else {
        text.chars().take(limit).collect()
    }
}

/// Convert one snapshot (plus its subtree) into flat model nodes with
/// parent/child links. `depth`/`budget` enforce [`MAX_TREE_DEPTH`] and
/// [`MAX_TREE_NODES`]: over-budget subtrees are dropped, never walked
/// unboundedly.
pub fn element_from_snapshot(
    snapshot: &AccessibleSnapshot,
    parent_id: Option<&str>,
    depth: usize,
    budget: &mut usize,
    out: &mut Vec<ElementNode>,
) {
    if *budget == 0 {
        return;
    }
    *budget = budget.saturating_sub(1);
    let states = snapshot.states;
    let enabled = Some(states.contains(State::Enabled));
    let focused = states
        .contains(State::Focusable)
        .then(|| states.contains(State::Focused));
    let selected = states
        .contains(State::Selectable)
        .then(|| states.contains(State::Selected));
    let checked = states
        .contains(State::Checkable)
        .then(|| states.contains(State::Checked));
    let expanded = states
        .contains(State::Expandable)
        .then(|| states.contains(State::Expanded));
    // Password fields never expose values: native sensitivity is set and
    // the value is dropped before any model path can see it.
    let password = snapshot.role == Role::PasswordText;
    let value = if password {
        None
    } else {
        snapshot
            .value_text
            .as_deref()
            .map(|value| truncate_chars(value, MAX_VALUE_CHARS))
            .filter(|value| !value.is_empty())
    };
    let mut node = ElementNode {
        id: snapshot.id.clone(),
        role: model_role(&snapshot.role),
        name: truncate_chars(&snapshot.name, MAX_NAME_CHARS),
        description: if snapshot.description.is_empty() {
            None
        } else {
            Some(truncate_chars(&snapshot.description, MAX_NAME_CHARS))
        },
        value,
        bounds: snapshot.bounds,
        enabled,
        focused,
        selected,
        checked,
        expanded,
        parent_id: parent_id.map(str::to_string),
        child_ids: snapshot
            .children
            .iter()
            .map(|child| child.id.clone())
            .collect(),
        actions: snapshot.actions.clone(),
        is_sensitive: password,
        sensitivity: password.then_some(ElementSensitivity::Password),
    };
    // Shared heuristic covers every non-password secret; native
    // classification above always wins.
    node.ensure_sensitivity();
    out.push(node);
    if depth >= MAX_TREE_DEPTH {
        return;
    }
    let id = snapshot.id.clone();
    for child in &snapshot.children {
        element_from_snapshot(child, Some(&id), depth + 1, budget, out);
        if *budget == 0 {
            return;
        }
    }
}

/// Flatten one window-rooted snapshot tree into model nodes.
pub fn flatten_window_tree(snapshot: &AccessibleSnapshot) -> Vec<ElementNode> {
    let mut budget = MAX_TREE_NODES;
    let mut out = Vec::new();
    element_from_snapshot(snapshot, None, 0, &mut budget, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(role: Role) -> AccessibleSnapshot {
        AccessibleSnapshot {
            id: "atspi://:1.1/root".to_string(),
            app: "test-app".to_string(),
            role,
            name: "name".to_string(),
            description: String::new(),
            states: StateSet::empty(),
            interfaces: Vec::new(),
            actions: Vec::new(),
            value_text: None,
            bounds: None,
            children: Vec::new(),
        }
    }

    #[test]
    fn password_value_is_dropped_and_flagged() {
        let mut snapshot = node(Role::PasswordText);
        snapshot.value_text = Some("hunter2".to_string());
        let nodes = flatten_window_tree(&snapshot);
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].value, None);
        assert!(nodes[0].is_sensitive);
    }

    #[test]
    fn child_links_and_budget_hold() {
        let mut root = node(Role::Frame);
        for index in 0..5 {
            let mut child = node(Role::Button);
            child.id = format!("atspi://:1.1/child{index}");
            root.children.push(child);
        }
        let mut budget = 3;
        let mut out = Vec::new();
        element_from_snapshot(&root, None, 0, &mut budget, &mut out);
        // Root plus two children: the budget caps the walk.
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].child_ids.len(), 5);
        assert_eq!(out[1].parent_id.as_deref(), Some(out[0].id.as_str()));
    }

    #[test]
    fn long_strings_are_truncated() {
        let mut snapshot = node(Role::Entry);
        snapshot.name = "n".repeat(MAX_NAME_CHARS + 10);
        snapshot.value_text = Some("v".repeat(MAX_VALUE_CHARS + 10));
        let nodes = flatten_window_tree(&snapshot);
        assert_eq!(nodes[0].name.chars().count(), MAX_NAME_CHARS);
        assert_eq!(
            nodes[0].value.as_deref().map(|v| v.chars().count()),
            Some(MAX_VALUE_CHARS)
        );
    }
}
