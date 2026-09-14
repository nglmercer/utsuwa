//! Native Linux semantic accessibility over AT-SPI2 / D-Bus.
//!
//! This crate is the Wayland-native accessibility layer: where the
//! X11/XWayland backend reads window hierarchies over the X protocol,
//! this backend talks to running applications through the AT-SPI
//! registry (`org.a11y.atspi.Registry`) on the session bus. Capture and
//! pointer/keyboard control stay on the XDG portals (see
//! `desktop-linux-wayland`); this crate owns *semantic* access only:
//!
//! ```text
//! enumerate accessible applications
//! enumerate top-level accessible windows
//! build semantic trees (role/name/description/value/state/bounds/…)
//! invoke / focus / set value / select / expand / collapse
//! ```
//!
//! The conversion from AT-SPI objects to [`tool_desktop::ElementNode`]
//! is pure ([`snapshot::AccessibleSnapshot`] → [`snapshot::element_from_snapshot`])
//! and unit-tested without a bus. The live D-Bus walk ([`live`]) degrades
//! to explicit `BackendUnavailable` errors when no registry answers —
//! never synthesized trees, never X11 inference.

pub mod snapshot;

#[cfg(target_os = "linux")]
pub mod live;

#[cfg(target_os = "linux")]
pub mod backend;

pub use snapshot::{
    element_from_snapshot, AccessibleSnapshot, AtspiError, AtspiRole, AtspiState, MAX_TREE_DEPTH,
    MAX_TREE_NODES,
};

#[cfg(target_os = "linux")]
pub use backend::plugin;
