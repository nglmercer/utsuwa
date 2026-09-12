//! Windows desktop backend for Utsuwa.
//!
//! The backend keeps the privileged surface behind `DesktopBackend`: Windows
//! UI Automation is used for semantic tree inspection and element actions,
//! Windows Graphics Capture supplies monitor/window frames, and `SendInput`
//! is only used for the explicit low-level fallback tools. The crate is
//! target-gated so the host workspace remains buildable on other systems.

#[cfg(target_os = "windows")]
mod backend;

#[cfg(target_os = "windows")]
pub use backend::plugin;

#[cfg(not(target_os = "windows"))]
pub fn plugin() -> Option<tool_desktop::plugin::DesktopPlugin> {
    None
}
