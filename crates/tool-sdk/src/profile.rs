//! Model-facing tool profiles and visibility policies.
//!
//! `ToolProfile::Simple` hides redundant low-level variants for small
//! models. The hidden-id list is the verbatim policy previously hardcoded
//! in `app-host`'s `AgentRuntime`; it now lives here so every source and
//! the catalog filter consistently. Future work replaces id matching with
//! `audience` metadata (`Simple | Advanced | Internal`) on the tool.

use std::sync::Arc;

/// Which tool subset the model sees this turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolProfile {
    /// Smallest surface: clock, environment, and limited reads.
    Minimal,
    /// Small-model profile: one edit interface, no raw low-level variants.
    Simple,
    /// Everyday local work: filesystem, HTTP, archives, notifications,
    /// system facts, clipboard, documents.
    Standard,
    /// Standard plus process execution and Git.
    Developer,
    /// Desktop control surface: desktop, browser, clipboard, applications,
    /// camera, audio, media.
    ComputerUse,
    /// Complete tool surface.
    #[default]
    Full,
}

impl ToolProfile {
    /// Back-compat predicate used by per-source pre-filtering. The catalog
    /// also enforces the profile centrally at snapshot time, so keeping
    /// both is idempotent during migration.
    pub fn allows_tool(self, tool_id: &str) -> bool {
        self.policy().visible(tool_id)
    }

    /// Visibility policy for this profile (boxed for catalog use).
    pub fn policy(self) -> Arc<dyn ToolVisibilityPolicy> {
        match self {
            Self::Full => Arc::new(FullToolPolicy),
            Self::Simple => Arc::new(SimpleToolPolicy),
            Self::Minimal => Arc::new(PrefixToolPolicy::minimal()),
            Self::Standard => Arc::new(PrefixToolPolicy::standard()),
            Self::Developer => Arc::new(PrefixToolPolicy::developer()),
            Self::ComputerUse => Arc::new(PrefixToolPolicy::computer_use()),
        }
    }
}

/// Prefix-based visibility for the named capability profiles. A tool is
/// visible when any allowed prefix matches and no denied id matches.
pub struct PrefixToolPolicy {
    allowed: &'static [&'static str],
    denied: &'static [&'static str],
}

impl PrefixToolPolicy {
    fn minimal() -> Self {
        Self {
            allowed: &[
                "system.time",
                "system.environment",
                "system.os",
                "filesystem.read",
                "filesystem.stat",
                "document.metadata",
            ],
            denied: &[],
        }
    }

    fn standard() -> Self {
        Self {
            allowed: &[
                "system.",
                "filesystem.",
                "http.",
                "archive.",
                "notification.",
                "clipboard.",
                "document.",
                "image.",
                "pdf.",
                "media.metadata",
                "media.audio_metadata",
                "media.video_metadata",
            ],
            denied: &[
                "filesystem.patch",
                "filesystem.edit_user_file",
                "filesystem.edit_file",
            ],
        }
    }

    fn developer() -> Self {
        Self {
            allowed: &[
                "system.",
                "filesystem.",
                "http.",
                "archive.",
                "notification.",
                "process.",
                "git.",
                "document.",
                "image.",
                "pdf.",
            ],
            denied: &[],
        }
    }

    fn computer_use() -> Self {
        Self {
            allowed: &[
                "desktop.",
                "browser.",
                "clipboard.",
                "application.",
                "camera.",
                "audio.",
                "media.",
                "system.time",
                "system.os",
                "notification.show",
            ],
            denied: &[],
        }
    }
}

impl ToolVisibilityPolicy for PrefixToolPolicy {
    fn visible(&self, tool_id: &str) -> bool {
        if self.denied.contains(&tool_id) {
            return false;
        }
        self.allowed
            .iter()
            .any(|prefix| tool_id == *prefix || tool_id.starts_with(prefix))
    }
}

/// Decides whether a tool id is visible to the model this turn.
pub trait ToolVisibilityPolicy: Send + Sync {
    fn visible(&self, tool_id: &str) -> bool;
}

/// Complete surface: everything registered is visible.
pub struct FullToolPolicy;

impl ToolVisibilityPolicy for FullToolPolicy {
    fn visible(&self, _tool_id: &str) -> bool {
        true
    }
}

/// Small-model surface: hides redundant low-level variants (raw patch,
/// range reads, search/glob, legacy aliases) and overlapping edit tools,
/// leaving exactly one edit interface (`filesystem.edit`).
pub struct SimpleToolPolicy;

impl ToolVisibilityPolicy for SimpleToolPolicy {
    fn visible(&self, tool_id: &str) -> bool {
        !matches!(
            tool_id,
            "filesystem.stat"
                | "filesystem.read_range"
                | "filesystem.search_text"
                | "filesystem.glob"
                | "filesystem.patch"
                | "filesystem.edit_user_file"
                | "filesystem.edit_file"
                | "filesystem.resolve_user_dir"
                | "filesystem.write_user_file"
                | "desktop.list_windows"
                | "desktop.accessibility_tree"
                | "desktop.invoke_element"
                | "desktop.set_value"
                | "desktop.focus_element"
                | "desktop.select_element"
                | "desktop.expand_element"
                | "desktop.collapse_element"
                | "desktop.double_click"
                | "desktop.move_pointer"
                | "desktop.mouse_down"
                | "desktop.mouse_up"
                | "desktop.drag"
                | "desktop.scroll"
                | "desktop.key_down"
                | "desktop.key_up"
                | "desktop.hotkey"
                | "desktop.press_key"
                | "desktop.focus_window"
                | "desktop.close_window"
                | "desktop.move_window"
                | "desktop.resize_window"
                | "desktop.minimize_window"
                | "desktop.maximize_window"
                | "desktop.restore_window"
                // Control-heavy and side-effecting local-machine tools stay
                // out of the small-model profile; their read-only
                // counterparts remain visible. Every call is ticket-gated.
                | "http.request"
                | "http.download"
                | "archive.extract"
                | "archive.create"
                | "git.branch.create"
                | "git.checkout"
                | "git.add"
                | "git.commit"
                | "git.restore"
                | "git.fetch"
                | "git.pull"
                | "git.push"
                | "git.reset"
                | "git.clean"
                | "notification.show"
                | "browser.open"
                | "browser.close_tab"
                | "browser.navigate"
                | "browser.back"
                | "browser.forward"
                | "browser.reload"
                | "browser.click"
                | "browser.type"
                | "browser.set_value"
                | "browser.select"
                | "browser.scroll"
                | "browser.cookies.set"
                | "browser.cookies.delete"
                | "camera.capture_photo"
                | "camera.capture_start"
                | "camera.capture_frame"
                | "camera.capture_stop"
                | "audio.capture_start"
                | "audio.capture_stop"
                | "audio.record"
                | "media.video_frame"
                | "media.video_frames"
                | "media.video_keyframes"
                | "media.thumbnail"
                | "media.waveform"
                | "clipboard.write"
                | "clipboard.clear"
                | "application.launch"
                | "application.quit"
                | "application.activate"
                | "image.resize"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_profile_shows_everything() {
        let policy = ToolProfile::Full.policy();
        assert!(policy.visible("filesystem.patch"));
        assert!(policy.visible("mcp.github.search_code"));
    }

    #[test]
    fn simple_profile_hides_low_level_variants_only() {
        let profile = ToolProfile::Simple;
        assert!(!profile.allows_tool("filesystem.patch"));
        assert!(!profile.allows_tool("filesystem.stat"));
        assert!(!profile.allows_tool("filesystem.resolve_user_dir"));
        assert!(profile.allows_tool("filesystem.edit"));
        assert!(profile.allows_tool("filesystem.read"));
        assert!(profile.allows_tool("system.time"));
    }

    #[test]
    fn default_profile_is_full() {
        assert_eq!(ToolProfile::default(), ToolProfile::Full);
    }

    #[test]
    fn simple_profile_hides_new_control_tools_keeps_reads() {
        let policy = ToolProfile::Simple.policy();
        for hidden in [
            "browser.click",
            "browser.navigate",
            "camera.capture_photo",
            "audio.record",
            "git.push",
            "http.download",
            "archive.extract",
            "application.launch",
            "clipboard.write",
        ] {
            assert!(!policy.visible(hidden), "{hidden}");
        }
        for visible in [
            "browser.snapshot",
            "browser.query",
            "camera.list",
            "audio.list_devices",
            "git.status",
            "http.get",
            "archive.list",
            "application.list",
            "clipboard.read",
            "system.cpu",
            "document.metadata",
            "media.metadata",
        ] {
            assert!(policy.visible(visible), "{visible}");
        }
    }

    #[test]
    fn named_profiles_scope_the_visible_surface() {
        assert!(ToolProfile::Minimal.allows_tool("system.time"));
        assert!(!ToolProfile::Minimal.allows_tool("desktop.click"));
        assert!(!ToolProfile::Minimal.allows_tool("http.get"));

        assert!(ToolProfile::Standard.allows_tool("http.get"));
        assert!(ToolProfile::Standard.allows_tool("archive.extract"));
        assert!(ToolProfile::Standard.allows_tool("notification.show"));
        assert!(!ToolProfile::Standard.allows_tool("desktop.click"));
        assert!(!ToolProfile::Standard.allows_tool("process.spawn"));
        assert!(!ToolProfile::Standard.allows_tool("git.push"));

        assert!(ToolProfile::Developer.allows_tool("git.status"));
        assert!(ToolProfile::Developer.allows_tool("process.spawn"));
        assert!(!ToolProfile::Developer.allows_tool("desktop.click"));

        assert!(ToolProfile::ComputerUse.allows_tool("desktop.screenshot"));
        assert!(ToolProfile::ComputerUse.allows_tool("browser.snapshot"));
        assert!(ToolProfile::ComputerUse.allows_tool("clipboard.read"));
        assert!(ToolProfile::ComputerUse.allows_tool("application.launch"));
        assert!(!ToolProfile::ComputerUse.allows_tool("git.push"));
        assert!(!ToolProfile::ComputerUse.allows_tool("filesystem.read"));
    }
}
