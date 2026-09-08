//! Desktop backends as native plugins (plan Phase 26/30).
//!
//! Each platform backend ships as a [`DesktopPlugin`]: a manifest
//! declaring its id, platforms, and capabilities plus the
//! [`DesktopBackend`](super::DesktopBackend) implementation. The host
//! collects plugins in a [`DesktopPluginRegistry`] and activates at
//! most one per session; agent tools are built only for the active
//! plugin's declared capabilities, so the model never sees actions the
//! platform cannot perform (Linux X11 has no `set_value`, Windows and
//! macOS have no backend on this host yet).
//!
//! Manifests are `serde` data (TOML-ready) so a future native plugin
//! loader can discover them from disk; today each backend crate builds
//! its own in code. This stays in-process on purpose: screenshots are
//! megabytes per call and input synthesis is already the privileged
//! act, so out-of-process IPC would add cost without adding isolation.
//! The trust boundary is *which plugin is active and what it declares*,
//! enforced here, plus per-call policy tickets enforced by the tools.

use std::sync::Arc;

use super::DesktopBackend;

/// One desktop action a plugin may provide. Names match the
/// `desktop.*` tool ids after the `desktop.` prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesktopCapability {
    ListWindows,
    AccessibilityTree,
    InvokeElement,
    SetValue,
    Screenshot,
    Click,
    TypeText,
}

impl DesktopCapability {
    /// Short name used in manifests.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ListWindows => "list_windows",
            Self::AccessibilityTree => "accessibility_tree",
            Self::InvokeElement => "invoke_element",
            Self::SetValue => "set_value",
            Self::Screenshot => "screenshot",
            Self::Click => "click",
            Self::TypeText => "type_text",
        }
    }

    /// Agent tool id this capability unlocks.
    pub fn tool_id(self) -> String {
        format!("desktop.{}", self.as_str())
    }
}

/// Every capability the full multiplatform tool offers. Platform
/// plugins declare a subset; the registry only builds tools for
/// declared capabilities.
pub const FULL_CAPABILITIES: [DesktopCapability; 7] = [
    DesktopCapability::ListWindows,
    DesktopCapability::AccessibilityTree,
    DesktopCapability::InvokeElement,
    DesktopCapability::SetValue,
    DesktopCapability::Screenshot,
    DesktopCapability::Click,
    DesktopCapability::TypeText,
];

/// The host's view of one desktop backend plugin: identity, platform
/// scope, and the capabilities it honestly implements.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DesktopPluginManifest {
    /// Unique plugin id, e.g. `desktop.linux-x11`.
    pub id: String,
    /// Human name for logs and settings UI.
    pub name: String,
    /// Plugin version (semver string, informational).
    pub version: String,
    /// OS names from [`std::env::consts::OS`] this plugin serves.
    pub platforms: Vec<String>,
    /// Actions the backend implements. Anything unlisted never
    /// becomes an agent tool while this plugin is active.
    pub capabilities: Vec<DesktopCapability>,
    /// Why this plugin exists / what it needs (display, portals…).
    pub description: String,
}

/// One installed desktop backend: manifest + implementation.
#[derive(Clone)]
pub struct DesktopPlugin {
    pub manifest: DesktopPluginManifest,
    pub backend: Arc<dyn DesktopBackend>,
}

impl std::fmt::Debug for DesktopPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DesktopPlugin")
            .field("manifest", &self.manifest)
            .finish()
    }
}

impl DesktopPlugin {
    pub fn new(manifest: DesktopPluginManifest, backend: Arc<dyn DesktopBackend>) -> Self {
        Self { manifest, backend }
    }

    /// True when the backend currently reaches the OS (display
    /// answered, session present). Unavailable plugins never activate.
    pub fn is_available(&self) -> bool {
        self.backend.is_available()
    }

    /// True when this plugin serves the running OS.
    pub fn serves_current_platform(&self) -> bool {
        self.manifest.platforms.iter().any(|p| p == std::env::consts::OS)
    }

    pub fn supports(&self, capability: DesktopCapability) -> bool {
        self.manifest.capabilities.contains(&capability)
    }

    /// The always-available placeholder: no platform, no capabilities,
    /// honest failure. Active only when nothing real is installed.
    pub fn stub() -> Self {
        Self::new(
            DesktopPluginManifest {
                id: "desktop.stub".to_string(),
                name: "No desktop backend".to_string(),
                version: "0.1.0".to_string(),
                platforms: Vec::new(),
                capabilities: Vec::new(),
                description: "Placeholder: no desktop backend is installed on this host."
                    .to_string(),
            },
            super::stub(),
        )
    }

    /// A declared-but-unimplemented platform backend (Windows UI
    /// Automation, macOS AX). The manifest states intent and full
    /// capabilities so a future crate can drop in behind the same id;
    /// until then it never activates.
    pub fn unimplemented(
        id: &str,
        name: &str,
        platform: &str,
        description: &str,
    ) -> Self {
        Self::new(
            DesktopPluginManifest {
                id: id.to_string(),
                name: name.to_string(),
                version: "0.1.0".to_string(),
                platforms: vec![platform.to_string()],
                capabilities: FULL_CAPABILITIES.to_vec(),
                description: description.to_string(),
            },
            super::stub(),
        )
    }
}

/// Installed desktop plugins for one host. Activation picks the first
/// registered plugin that both serves this OS and is available, so
/// platform backends win over the fallback stub by registration order.
#[derive(Debug, Default)]
pub struct DesktopPluginRegistry {
    plugins: Vec<DesktopPlugin>,
}

impl DesktopPluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, plugin: DesktopPlugin) {
        self.plugins.push(plugin);
    }

    /// The plugin to serve this session, if any backend reaches the OS.
    pub fn select(&self) -> Option<DesktopPlugin> {
        self.plugins
            .iter()
            .find(|p| p.serves_current_platform() && p.is_available())
            .cloned()
    }

    pub fn manifests(&self) -> Vec<DesktopPluginManifest> {
        self.plugins.iter().map(|p| p.manifest.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DesktopCapability, DesktopPlugin, DesktopPluginManifest, DesktopPluginRegistry,
        FULL_CAPABILITIES,
    };
    use crate::DesktopBackend;

    fn manifest(id: &str, platforms: &[&str], capabilities: &[DesktopCapability]) -> DesktopPluginManifest {
        DesktopPluginManifest {
            id: id.to_string(),
            name: id.to_string(),
            version: "0.1.0".to_string(),
            platforms: platforms.iter().map(|s| s.to_string()).collect(),
            capabilities: capabilities.to_vec(),
            description: "test".to_string(),
        }
    }

    #[test]
    fn manifest_roundtrips_through_json() {
        let m = manifest("desktop.linux-x11", &["linux"], &FULL_CAPABILITIES);
        let back: DesktopPluginManifest = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(back, m);
        assert_eq!(DesktopCapability::Click.tool_id(), "desktop.click");
    }

    #[test]
    fn select_prefers_available_plugin_for_this_os() {
        struct Down;
        #[async_trait::async_trait]
        impl DesktopBackend for Down {
            fn is_available(&self) -> bool {
                false
            }
            async fn list_windows(&self) -> Result<Vec<crate::WindowInfo>, crate::DesktopError> {
                Err(crate::DesktopError::BackendUnavailable("down".to_string()))
            }
            async fn accessibility_tree(&self, _w: &str) -> Result<Vec<crate::ElementNode>, crate::DesktopError> {
                Err(crate::DesktopError::BackendUnavailable("down".to_string()))
            }
            async fn invoke_element(&self, _w: &str, _e: &str) -> Result<(), crate::DesktopError> {
                Err(crate::DesktopError::BackendUnavailable("down".to_string()))
            }
            async fn set_value(&self, _w: &str, _e: &str, _v: &str) -> Result<(), crate::DesktopError> {
                Err(crate::DesktopError::BackendUnavailable("down".to_string()))
            }
            async fn screenshot(&self, _w: Option<&str>) -> Result<crate::Screenshot, crate::DesktopError> {
                Err(crate::DesktopError::BackendUnavailable("down".to_string()))
            }
            async fn click(&self, _w: Option<&str>, _at: crate::Point) -> Result<(), crate::DesktopError> {
                Err(crate::DesktopError::BackendUnavailable("down".to_string()))
            }
            async fn type_text(&self, _w: Option<&str>, _t: &str) -> Result<(), crate::DesktopError> {
                Err(crate::DesktopError::BackendUnavailable("down".to_string()))
            }
        }

        let here = std::env::consts::OS;
        let mut registry = DesktopPluginRegistry::new();
        // Wrong platform is skipped even though it claims availability.
        registry.register(DesktopPlugin::new(
            manifest("desktop.other", &["plan9"], &FULL_CAPABILITIES),
            std::sync::Arc::new(crate::FakeBackend::new()),
        ));
        // Right platform but unavailable backend is skipped.
        registry.register(DesktopPlugin::new(
            manifest("desktop.down", &[here], &FULL_CAPABILITIES),
            std::sync::Arc::new(Down),
        ));
        assert!(registry.select().is_none());

        // Right platform + available backend wins.
        registry.register(DesktopPlugin::new(
            manifest("desktop.fake", &[here], &FULL_CAPABILITIES),
            std::sync::Arc::new(crate::FakeBackend::new()),
        ));
        let active = registry.select().unwrap();
        assert_eq!(active.manifest.id, "desktop.fake");
        assert!(active.supports(DesktopCapability::Click));
    }

    #[test]
    fn unimplemented_plugin_never_activates() {
        let p = DesktopPlugin::unimplemented(
            "desktop.windows-uia",
            "Windows UI Automation",
            "windows",
            "Not built on this host.",
        );
        assert!(!p.is_available());
        // Intent is declared (full caps) but the backend stays down.
        assert!(p.supports(DesktopCapability::Click));
        let mut registry = DesktopPluginRegistry::new();
        registry.register(p);
        assert!(registry.select().is_none());
        assert_eq!(registry.manifests().len(), 1);
    }
}
