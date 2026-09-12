//! macOS desktop backend for Utsuwa.
//!
//! This crate is target-gated. On macOS it combines ScreenCaptureKit for
//! user-approved display/window capture, Accessibility (AX) for semantic
//! inspection/actions, and CGEvent for the explicit input fallback.

#[cfg(target_os = "macos")]
mod backend;

#[cfg(target_os = "macos")]
pub use backend::plugin;

#[cfg(not(target_os = "macos"))]
pub fn plugin() -> Option<tool_desktop::plugin::DesktopPlugin> {
    None
}
