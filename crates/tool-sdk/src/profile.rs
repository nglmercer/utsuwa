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
    /// Small-model profile: one edit interface, no raw low-level variants.
    Simple,
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
        }
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
}
