//! Filesystem backends as native plugins (same model as the desktop
//! plugin registry in `tool-desktop`): each backend ships a manifest
//! declaring its id and capabilities, the host activates one, and agent
//! tools are built only for declared capabilities.
//!
//! Today there is one backend — the local filesystem (`filesystem.local`,
//! full capabilities). The seam exists so future backends (a sandboxed or
//! read-only view, a remote mapping) register beside it without touching
//! the tools or the host wiring.

use std::sync::Arc;

use super::{FilesystemLimits, SearchLimits};

/// One filesystem action a plugin may provide. Names match the
/// `filesystem.*` tool ids after the `filesystem.` prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FsCapability {
    List,
    Stat,
    Read,
    ReadRange,
    SearchText,
    Glob,
    Patch,
    Write,
}

impl FsCapability {
    /// Short name used in manifests.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Stat => "stat",
            Self::Read => "read",
            Self::ReadRange => "read_range",
            Self::SearchText => "search_text",
            Self::Glob => "glob",
            Self::Patch => "patch",
            Self::Write => "write",
        }
    }

    /// Agent tool id this capability unlocks.
    pub fn tool_id(self) -> String {
        format!("filesystem.{}", self.as_str())
    }
}

/// Every capability the full filesystem tool offers. Plugins declare a
/// subset; the host only builds tools for declared capabilities.
pub const FULL_CAPABILITIES: [FsCapability; 8] = [
    FsCapability::List,
    FsCapability::Stat,
    FsCapability::Read,
    FsCapability::ReadRange,
    FsCapability::SearchText,
    FsCapability::Glob,
    FsCapability::Patch,
    FsCapability::Write,
];

/// The host's view of one filesystem backend plugin: identity plus the
/// capabilities it honestly implements.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FsPluginManifest {
    /// Unique plugin id, e.g. `filesystem.local`.
    pub id: String,
    /// Human name for logs and settings UI.
    pub name: String,
    /// Plugin version (semver string, informational).
    pub version: String,
    /// Actions the backend implements. Anything unlisted never becomes
    /// an agent tool while this plugin is active.
    pub capabilities: Vec<FsCapability>,
    /// Why this plugin exists / what it needs.
    pub description: String,
}

/// One installed filesystem backend: manifest + broker limits.
#[derive(Debug, Clone)]
pub struct FsPlugin {
    pub manifest: FsPluginManifest,
    pub limits: FilesystemLimits,
    pub search_limits: SearchLimits,
}

impl FsPlugin {
    pub fn new(
        manifest: FsPluginManifest,
        limits: FilesystemLimits,
        search_limits: SearchLimits,
    ) -> Self {
        Self {
            manifest,
            limits,
            search_limits,
        }
    }

    /// The local filesystem backend: full capabilities, default limits.
    pub fn local() -> Self {
        Self::new(
            FsPluginManifest {
                id: "filesystem.local".to_string(),
                name: "Local filesystem".to_string(),
                version: "0.1.0".to_string(),
                capabilities: FULL_CAPABILITIES.to_vec(),
                description: "Direct local filesystem access, brokered through capability tickets and scope checks.".to_string(),
            },
            FilesystemLimits::default(),
            SearchLimits::default(),
        )
    }

    /// The local backend is available wherever the process runs.
    /// (Future remote backends override this when unreachable.)
    pub fn is_available(&self) -> bool {
        true
    }

    pub fn supports(&self, capability: FsCapability) -> bool {
        self.manifest.capabilities.contains(&capability)
    }
}

/// Installed filesystem plugins for one host. Activation picks the
/// first available plugin, so the local backend wins by registration
/// order unless a future backend takes precedence.
#[derive(Debug, Default)]
pub struct FsPluginRegistry {
    plugins: Vec<FsPlugin>,
}

impl FsPluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, plugin: FsPlugin) {
        self.plugins.push(plugin);
    }

    /// The plugin to serve this session, if any backend is available.
    pub fn select(&self) -> Option<FsPlugin> {
        self.plugins.iter().find(|p| p.is_available()).cloned()
    }

    pub fn manifests(&self) -> Vec<FsPluginManifest> {
        self.plugins.iter().map(|p| p.manifest.clone()).collect()
    }
}

/// Build the agent tools one plugin unlocks: exactly one tool per
/// declared capability. Anything the manifest omits never reaches the
/// model — e.g. a read-only view exposes no `patch` or `write`.
pub fn tools_for_plugin(plugin: &FsPlugin) -> Vec<Arc<dyn tool_core::Tool>> {
    let mut out: Vec<Arc<dyn tool_core::Tool>> = Vec::new();
    let fs = plugin.limits.clone();
    let search = plugin.search_limits.clone();
    if plugin.supports(FsCapability::List) {
        out.push(Arc::new(super::ListTool { limits: fs.clone() }));
    }
    if plugin.supports(FsCapability::Stat) {
        out.push(Arc::new(super::StatTool));
    }
    if plugin.supports(FsCapability::Read) {
        out.push(Arc::new(super::ReadTool { limits: fs.clone() }));
    }
    if plugin.supports(FsCapability::ReadRange) {
        out.push(Arc::new(super::ReadRangeTool { limits: fs.clone() }));
    }
    if plugin.supports(FsCapability::SearchText) {
        out.push(Arc::new(super::SearchTextTool { limits: search.clone() }));
    }
    if plugin.supports(FsCapability::Glob) {
        out.push(Arc::new(super::GlobTool { limits: search }));
    }
    if plugin.supports(FsCapability::Patch) {
        out.push(Arc::new(super::PatchTool { limits: fs.clone() }));
    }
    if plugin.supports(FsCapability::Write) {
        out.push(Arc::new(super::WriteTool { limits: fs }));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{FsCapability, FsPlugin, FsPluginManifest, FsPluginRegistry, FULL_CAPABILITIES};

    #[test]
    fn manifest_roundtrips_through_json() {
        let m = FsPluginManifest {
            id: "filesystem.local".to_string(),
            name: "Local".to_string(),
            version: "0.1.0".to_string(),
            capabilities: FULL_CAPABILITIES.to_vec(),
            description: "test".to_string(),
        };
        let back: FsPluginManifest =
            serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(back, m);
        assert_eq!(FsCapability::Write.tool_id(), "filesystem.write");
    }

    #[test]
    fn local_plugin_serves_everything() {
        let plugin = FsPlugin::local();
        assert_eq!(plugin.manifest.id, "filesystem.local");
        assert!(plugin.is_available());
        assert!(plugin.supports(FsCapability::Write));
        let mut registry = FsPluginRegistry::new();
        assert!(registry.select().is_none());
        registry.register(plugin);
        let active = registry.select().unwrap();
        assert_eq!(active.manifest.id, "filesystem.local");
        assert_eq!(registry.manifests().len(), 1);
    }

    #[test]
    fn tools_for_plugin_exposes_only_declared_capabilities() {
        use super::tools_for_plugin;
        let full = FsPlugin::local();
        let ids: Vec<String> = tools_for_plugin(&full)
            .iter()
            .map(|t| t.metadata().id.0.clone())
            .collect();
        assert_eq!(ids.len(), 8);
        assert!(ids.contains(&"filesystem.write".to_string()));

        // Read-only view: observe tools only, no patch or write.
        let read_only_caps = [
            FsCapability::List,
            FsCapability::Stat,
            FsCapability::Read,
            FsCapability::ReadRange,
            FsCapability::SearchText,
            FsCapability::Glob,
        ];
        let mut read_only = FsPlugin::local();
        read_only.manifest.id = "filesystem.read-only".to_string();
        read_only.manifest.capabilities = read_only_caps.to_vec();
        let ids: Vec<String> = tools_for_plugin(&read_only)
            .iter()
            .map(|t| t.metadata().id.0.clone())
            .collect();
        assert_eq!(ids.len(), 6);
        assert!(!ids.contains(&"filesystem.write".to_string()));
        assert!(!ids.contains(&"filesystem.patch".to_string()));
    }
}
