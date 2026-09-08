//! Plugin identity, manifests, trust, and lifecycle (plan Phases 22/24/25).
//!
//! Engine-free: parsing, validation, and state live here; `plugin-wasm`
//! executes. Lifecycle state never carries authority — activation is
//! independent from permission grants by construction (there is simply no
//! grant field anywhere in this crate).
//!
//! Plugin layout:
//!
//! ```text
//! plugins/
//! └── dev.example.plugin/
//!     ├── plugin.toml
//!     └── plugin.wasm        # or plugin.native for unsafe plugins
//! ```
//!
//! Unsafe (native) plugins validate and gate but never execute here:
//! enabling one needs [`PluginRegistry::allow_native`], and no loader
//! exists in this process (plan Phase 26 reserves execution for the
//! out-of-process plugin-host).

use capability_core::PluginId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// Manifest format version this host loads.
pub const SUPPORTED_MANIFEST_API: u32 = 1;

/// Expected filenames inside a plugin directory.
pub const MANIFEST_FILE: &str = "plugin.toml";
pub const MODULE_FILE: &str = "plugin.wasm";
/// Native module filename. Read for discovery only — the host ships no
/// native loader, so these bytes are never executed in-process (plan
/// Phase 26 reserves execution for the out-of-process plugin-host).
pub const NATIVE_MODULE_FILE: &str = "plugin.native";

/// WASM magic prefix, checked before any engine touches the bytes.
pub const WASM_MAGIC: [u8; 4] = [0x00, 0x61, 0x73, 0x6D];

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PluginError {
    #[error("invalid manifest: {0}")]
    Manifest(String),
    #[error("invalid module: {0}")]
    Module(String),
    #[error("unknown plugin '{0}'")]
    Unknown(String),
    #[error("illegal transition from {0} via {1}")]
    Transition(String, String),
    #[error("io error: {0}")]
    Io(String),
    /// A native (unsafe) plugin was asked to enable without explicit
    /// user allow-listing. Default-deny: this is the only error that
    /// distinguishes "not yet approved" from a broken plugin.
    #[error("native plugin '{0}' is not allow-listed: unsafe plugins need explicit user approval")]
    NativeNotAllowed(String),
}

/// Trust tier of a plugin origin (plan Phase 25). Risk metadata only —
/// never authority. Unsigned WASM runs sandboxed; unsigned *native*
/// libraries stay disabled (the host ships no native loader at all).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TrustLevel {
    Official,
    Verified,
    #[default]
    UnsignedWasm,
    #[serde(rename = "local-dev")]
    LocalDevelopment,
    TrustedNative,
}

impl TrustLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            TrustLevel::Official => "official",
            TrustLevel::Verified => "verified",
            TrustLevel::UnsignedWasm => "unsigned-wasm",
            TrustLevel::LocalDevelopment => "local-dev",
            TrustLevel::TrustedNative => "trusted-native",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PluginMeta {
    id: String,
    name: String,
    version: String,
    api: u32,
    #[serde(default)]
    trust: TrustLevel,
}

/// Plugin execution kind. `wasm` runs sandboxed in-process; `native`
/// (unsafe) marks a module for the out-of-process plugin-host, which
/// this host does not ship yet — native records validate and gate but
/// never execute here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeKind {
    #[default]
    Wasm,
    Native,
}

impl RuntimeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RuntimeKind::Wasm => "wasm",
            RuntimeKind::Native => "native",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RuntimeSpec {
    #[serde(rename = "type")]
    #[serde(default)]
    kind: RuntimeKind,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilesystemPermissions {
    #[serde(default)]
    pub read: Vec<String>,
    #[serde(default)]
    pub write: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkPermissions {
    #[serde(default)]
    pub hosts: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessPermissions {
    #[serde(default)]
    pub commands: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct PermissionsSection {
    #[serde(default)]
    pub filesystem: FilesystemPermissions,
    #[serde(default)]
    pub network: NetworkPermissions,
    #[serde(default)]
    pub process: ProcessPermissions,
}

/// One guest-exported function bridged as a host tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginToolDecl {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ManifestFile {
    plugin: PluginMeta,
    runtime: RuntimeSpec,
    #[serde(default)]
    permissions: PermissionsSection,
    #[serde(default)]
    tools: Vec<PluginToolDecl>,
}

/// A validated manifest: the host's view of one plugin.
#[derive(Debug, Clone)]
pub struct PluginManifest {
    pub id: PluginId,
    pub name: String,
    pub version: String,
    pub trust: TrustLevel,
    pub runtime: RuntimeKind,
    pub filesystem: FilesystemPermissions,
    pub network: NetworkPermissions,
    pub process: ProcessPermissions,
    pub tools: Vec<PluginToolDecl>,
}

impl PluginManifest {
    pub fn parse_toml(text: &str) -> Result<Self, PluginError> {
        let file: ManifestFile =
            toml::from_str(text).map_err(|e| PluginError::Manifest(e.to_string()))?;
        Self::validate(file)
    }

    fn validate(file: ManifestFile) -> Result<Self, PluginError> {
        let bad = |m: &str| PluginError::Manifest(m.to_string());
        let meta = file.plugin;
        if meta.id.is_empty() || meta.id.len() > 128 {
            return Err(bad("plugin.id must be 1-128 chars"));
        }
        if !meta
            .id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(bad("plugin.id may only contain [a-zA-Z0-9-_.]"));
        }
        if meta.name.is_empty() || meta.name.len() > 128 {
            return Err(bad("plugin.name must be 1-128 chars"));
        }
        if meta.version.is_empty() || meta.version.len() > 32 {
            return Err(bad("plugin.version must be 1-32 chars"));
        }
        if meta.api != SUPPORTED_MANIFEST_API {
            return Err(bad(&format!(
                "unsupported manifest api {} (host supports {SUPPORTED_MANIFEST_API})",
                meta.api
            )));
        }
        // Unsafe pairing rules (plan Phase 25): native execution and
        // the trusted-native label imply each other. Anything else is a
        // confused manifest and rejected here, not at load time.
        match (file.runtime.kind, meta.trust) {
            (RuntimeKind::Native, TrustLevel::TrustedNative) => {}
            (RuntimeKind::Native, _) => {
                return Err(bad(
                    "runtime.type = \"native\" requires trust = \"trusted-native\"",
                ));
            }
            (_, TrustLevel::TrustedNative) => {
                return Err(bad(
                    "trust = \"trusted-native\" requires runtime.type = \"native\"",
                ));
            }
            _ => {}
        }
        let mut seen = std::collections::HashSet::new();
        for tool in &file.tools {
            if tool.name.is_empty()
                || tool.name.len() > 64
                || !tool
                    .name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return Err(bad("tool names must be 1-64 chars of [a-zA-Z0-9-_]"));
            }
            if !seen.insert(tool.name.clone()) {
                return Err(bad(&format!("duplicate tool '{}'", tool.name)));
            }
            if tool.description.len() > 512 {
                return Err(bad("tool description exceeds 512 chars"));
            }
        }
        Ok(Self {
            id: PluginId::new(meta.id),
            name: meta.name,
            version: meta.version,
            trust: meta.trust,
            runtime: file.runtime.kind,
            filesystem: file.permissions.filesystem,
            network: file.permissions.network,
            process: file.permissions.process,
            tools: file.tools,
        })
    }
}

/// Lifecycle state. No authority travels with any of these — enabling a
/// plugin only loads its code and registers its tools behind policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginState {
    /// Manifest + bytes found on disk, not yet validated.
    Discovered,
    /// Manifest and module bytes validated.
    Validated,
    /// Instantiated in the engine, tools registered, not serving.
    Loaded,
    /// Serving: tools callable through policy.
    Enabled,
    /// Registered but not serving (also the post-failure state).
    Disabled,
    /// Terminal load/validation failure with a reason.
    Failed(String),
}

impl PluginState {
    pub fn as_str(&self) -> &'static str {
        match self {
            PluginState::Discovered => "discovered",
            PluginState::Validated => "validated",
            PluginState::Loaded => "loaded",
            PluginState::Enabled => "enabled",
            PluginState::Disabled => "disabled",
            PluginState::Failed(_) => "failed",
        }
    }
}

/// One known plugin: its manifest, raw module bytes, and lifecycle state.
pub struct PluginRecord {
    pub manifest: PluginManifest,
    pub module_bytes: Vec<u8>,
    pub state: PluginState,
}

/// Directory-backed plugin registry: discover → validate → load → enable,
/// with disable / unload / reload / remove. State transitions are checked;
/// illegal ones fail instead of silently reordering.
///
/// Native (unsafe) plugins default to denied: [`PluginRegistry::enable`]
/// refuses them unless the id was explicitly allow-listed through
/// [`PluginRegistry::allow_native`] — the host wires that call to a user
/// approval (settings UI), never to discovery.
#[derive(Default)]
pub struct PluginRegistry {
    plugins: HashMap<String, PluginRecord>,
    native_allowlist: std::collections::HashSet<String>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Scan one directory: each immediate child holding `plugin.toml` is
    /// discovered (manifest parsed, module bytes read, magic checked).
    /// Failures are recorded as `Failed`, never raised: one broken plugin
    /// must not hide the rest.
    pub fn discover_dir(&mut self, dir: &Path) {
        let entries = std::fs::read_dir(dir).map(|it| it.collect::<Vec<_>>()).unwrap_or_default();
        for entry in entries {
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let id = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let record = match load_plugin_dir(&path) {
                Ok(record) => record,
                Err(e) => {
                    // Synthesize a minimal failed record so the UI can show
                    // *why* this directory did not load.
                    PluginRecord {
                        manifest: PluginManifest {
                            id: PluginId::new(id.clone()),
                            name: id.clone(),
                            version: String::new(),
                            trust: TrustLevel::UnsignedWasm,
                            runtime: RuntimeKind::Wasm,
                            filesystem: FilesystemPermissions::default(),
                            network: NetworkPermissions::default(),
                            process: ProcessPermissions::default(),
                            tools: Vec::new(),
                        },
                        module_bytes: Vec::new(),
                        state: PluginState::Failed(e.to_string()),
                    }
                }
            };
            self.plugins.insert(record.manifest.id.0.clone(), record);
        }
    }

    pub fn get(&self, id: &str) -> Option<&PluginRecord> {
        self.plugins.get(id)
    }

    /// Explicit user approval for one unsafe plugin: records that the
    /// user accepted native execution for this id. Never called during
    /// discovery — only from a user approval surface.
    pub fn allow_native(&mut self, id: &str) {
        self.native_allowlist.insert(id.to_string());
    }

    /// Withdraw a previous unsafe approval. A serving native plugin
    /// keeps serving until disabled; the next `enable` is refused.
    pub fn deny_native(&mut self, id: &str) {
        self.native_allowlist.remove(id);
    }

    pub fn is_native_allowed(&self, id: &str) -> bool {
        self.native_allowlist.contains(id)
    }

    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.plugins.keys().cloned().collect();
        ids.sort();
        ids
    }

    fn transition(&mut self, id: &str, to: PluginState, from: &[PluginState]) -> Result<(), PluginError> {
        let record = self
            .plugins
            .get_mut(id)
            .ok_or_else(|| PluginError::Unknown(id.to_string()))?;
        if !from.iter().any(|s| std::mem::discriminant(s) == std::mem::discriminant(&record.state)) {
            return Err(PluginError::Transition(
                record.state.as_str().to_string(),
                to.as_str().to_string(),
            ));
        }
        record.state = to;
        Ok(())
    }

    /// Re-validate a discovered/failed/disabled plugin (manifest + bytes).
    pub fn validate(&mut self, id: &str) -> Result<(), PluginError> {
        self.transition(
            id,
            PluginState::Validated,
            &[
                PluginState::Discovered,
                PluginState::Failed(String::new()),
                PluginState::Disabled,
            ],
        )
    }

    /// Mark instantiated (the engine reports back through this).
    pub fn mark_loaded(&mut self, id: &str) -> Result<(), PluginError> {
        self.transition(
            id,
            PluginState::Loaded,
            &[PluginState::Validated, PluginState::Disabled],
        )
    }

    pub fn enable(&mut self, id: &str) -> Result<(), PluginError> {
        let record = self
            .plugins
            .get(id)
            .ok_or_else(|| PluginError::Unknown(id.to_string()))?;
        // Unsafe gate: a native module runs outside every sandbox, so
        // enabling one needs explicit user approval even when its
        // manifest claims trusted-native. Self-claimed trust is a label;
        // the allowlist is the decision.
        if record.manifest.runtime == RuntimeKind::Native && !self.is_native_allowed(id) {
            return Err(PluginError::NativeNotAllowed(id.to_string()));
        }
        self.transition(
            id,
            PluginState::Enabled,
            &[PluginState::Loaded, PluginState::Disabled],
        )
    }

    pub fn disable(&mut self, id: &str) -> Result<(), PluginError> {
        self.transition(
            id,
            PluginState::Disabled,
            &[PluginState::Enabled, PluginState::Loaded],
        )
    }

    /// Drop the instance but keep the validated plugin on disk state.
    pub fn unload(&mut self, id: &str) -> Result<(), PluginError> {
        self.transition(
            id,
            PluginState::Validated,
            &[PluginState::Disabled, PluginState::Loaded],
        )
    }

    /// Forget a plugin entirely (bytes stay on disk; use the filesystem
    /// to delete them).
    pub fn remove(&mut self, id: &str) -> Result<(), PluginError> {
        self.plugins
            .remove(id)
            .map(|_| ())
            .ok_or_else(|| PluginError::Unknown(id.to_string()))
    }

    /// Re-read manifest + module bytes from the plugin directory (Phase 24
    /// `update`). Lands on `Validated`: the bytes were just parsed and
    /// magic-checked, so the engine can re-instantiate directly. A broken
    /// on-disk state returns `Err` and keeps the previous record intact —
    /// a bad edit must not destroy a serving plugin.
    pub fn update(&mut self, id: &str, dir: &Path) -> Result<(), PluginError> {
        if !self.plugins.contains_key(id) {
            return Err(PluginError::Unknown(id.to_string()));
        }
        let mut fresh = load_plugin_dir(dir)?;
        if fresh.manifest.id.0 != id {
            return Err(PluginError::Manifest(format!(
                "directory manifest id '{}' does not match plugin '{id}'",
                fresh.manifest.id.0
            )));
        }
        fresh.state = PluginState::Validated;
        self.plugins.insert(id.to_string(), fresh);
        Ok(())
    }

    /// Enabled plugins and their manifests, for tool registration.
    pub fn enabled(&self) -> Vec<&PluginRecord> {
        self.plugins
            .values()
            .filter(|r| r.state == PluginState::Enabled)
            .collect()
    }
}

fn load_plugin_dir(dir: &Path) -> Result<PluginRecord, PluginError> {
    let manifest_path = dir.join(MANIFEST_FILE);
    let text = std::fs::read_to_string(&manifest_path)
        .map_err(|e| PluginError::Io(format!("{}: {e}", manifest_path.display())))?;
    let manifest = PluginManifest::parse_toml(&text)?;
    // Native modules are discovered (bytes read for hashing/display) but
    // never validated as code and never executed in-process: no loader
    // lives in this host. A native manifest pointing at WASM bytes is a
    // confused package and rejected.
    let module_path = dir.join(match manifest.runtime {
        RuntimeKind::Wasm => MODULE_FILE,
        RuntimeKind::Native => NATIVE_MODULE_FILE,
    });
    let bytes = std::fs::read(&module_path)
        .map_err(|e| PluginError::Io(format!("{}: {e}", module_path.display())))?;
    match manifest.runtime {
        RuntimeKind::Wasm => check_wasm_magic(&bytes)?,
        RuntimeKind::Native => {
            if bytes.is_empty() {
                return Err(PluginError::Module("native module is empty".to_string()));
            }
            if bytes.len() >= 4 && bytes[..4] == WASM_MAGIC {
                return Err(PluginError::Module(
                    "native module holds WebAssembly bytes: use runtime.type = \"wasm\"".to_string(),
                ));
            }
        }
    }
    // Directory name and manifest id must agree: prevents a directory
    // named `trusted` from loading a manifest claiming another id.
    if let Some(dir_name) = dir.file_name().map(|n| n.to_string_lossy().to_string()) {
        if dir_name != manifest.id.0 {
            return Err(PluginError::Manifest(format!(
                "directory '{dir_name}' does not match plugin.id '{}'",
                manifest.id.0
            )));
        }
    }
    Ok(PluginRecord {
        manifest,
        module_bytes: bytes,
        state: PluginState::Discovered,
    })
}

fn check_wasm_magic(bytes: &[u8]) -> Result<(), PluginError> {
    if bytes.len() < 8 || bytes[..4] != WASM_MAGIC {
        return Err(PluginError::Module(
            "missing \\0asm magic: not a WebAssembly module".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"
[plugin]
id = "dev.example.demo"
name = "Demo"
version = "1.0.0"
api = 1
trust = "unsigned-wasm"

[runtime]
type = "wasm"

[permissions.filesystem]
read = ["/work"]

[[tools]]
name = "summarize"
description = "Summarize text"
"#;

    #[test]
    fn manifest_parses_with_defaults() {
        let manifest = PluginManifest::parse_toml(MANIFEST).unwrap();
        assert_eq!(manifest.id.0, "dev.example.demo");
        assert_eq!(manifest.trust, TrustLevel::UnsignedWasm);
        assert_eq!(manifest.filesystem.read, vec!["/work".to_string()]);
        assert!(manifest.filesystem.write.is_empty());
        assert_eq!(manifest.tools.len(), 1);
    }

    #[test]
    fn manifest_rejects_bad_api_type_and_dupes() {
        assert!(PluginManifest::parse_toml("[plugin]\nid = \"x\"\nname = \"x\"\nversion = \"1\"\napi = 99\n[runtime]\ntype = \"wasm\"\n").is_err());
        assert!(PluginManifest::parse_toml("[plugin]\nid = \"x\"\nname = \"x\"\nversion = \"1\"\napi = 1\n[runtime]\ntype = \"native\"\n").is_err());
        assert!(PluginManifest::parse_toml(
            "[plugin]\nid = \"bad id\"\nname = \"x\"\nversion = \"1\"\napi = 1\n[runtime]\ntype = \"wasm\"\n"
        )
        .is_err());
        let dupe = MANIFEST.to_string()
            + "\n[[tools]]\nname = \"summarize\"\n";
        assert!(PluginManifest::parse_toml(&dupe).is_err());
    }

    #[test]
    fn trust_levels_parse() {
        for (text, level) in [
            ("official", TrustLevel::Official),
            ("verified", TrustLevel::Verified),
            ("unsigned-wasm", TrustLevel::UnsignedWasm),
            ("local-dev", TrustLevel::LocalDevelopment),
        ] {
            let m = PluginManifest::parse_toml(&format!(
                "[plugin]\nid = \"x\"\nname = \"x\"\nversion = \"1\"\napi = 1\ntrust = \"{text}\"\n[runtime]\ntype = \"wasm\"\n"
            ))
            .unwrap();
            assert_eq!(m.trust, level);
        }
        // trusted-native is not a WASM label: it requires runtime native
        // (covered in native_manifest_requires_trusted_native_and_vice_versa).
        assert!(PluginManifest::parse_toml(
            "[plugin]\nid = \"x\"\nname = \"x\"\nversion = \"1\"\napi = 1\ntrust = \"trusted-native\"\n[runtime]\ntype = \"wasm\"\n"
        )
        .is_err());
    }

    fn plugin_dir(tag: &str, manifest: &str, module: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("utsuwa-plugin-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let plug = dir.join("dev.example.demo");
        std::fs::create_dir_all(&plug).unwrap();
        std::fs::write(plug.join(MANIFEST_FILE), manifest).unwrap();
        std::fs::write(plug.join(MODULE_FILE), module).unwrap();
        dir
    }

    fn wasm_bytes() -> Vec<u8> {
        // Minimal valid module header: magic + version, no sections.
        vec![0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00]
    }

    #[test]
    fn lifecycle_transitions_are_checked() {
        let dir = plugin_dir("lifecycle", MANIFEST, &wasm_bytes());
        let mut registry = PluginRegistry::new();
        registry.discover_dir(&dir);
        assert_eq!(registry.ids(), vec!["dev.example.demo".to_string()]);
        let record = registry.get("dev.example.demo").unwrap();
        assert_eq!(record.state, PluginState::Discovered);

        // Enabling before load is illegal.
        assert!(registry.enable("dev.example.demo").is_err());
        registry.validate("dev.example.demo").unwrap();
        registry.mark_loaded("dev.example.demo").unwrap();
        registry.enable("dev.example.demo").unwrap();
        assert_eq!(registry.enabled().len(), 1);
        registry.disable("dev.example.demo").unwrap();
        assert!(registry.enabled().is_empty());
        registry.unload("dev.example.demo").unwrap();
        registry.remove("dev.example.demo").unwrap();
        assert!(registry.get("dev.example.demo").is_none());
        assert!(registry.enable("dev.example.demo").is_err());
    }

    #[test]
    fn broken_plugins_fail_loudly_without_hiding_siblings() {
        let dir = std::env::temp_dir().join(format!(
            "utsuwa-plugin-test-{}-broken",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let bad = dir.join("bad.plugin");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join(MANIFEST_FILE), "not = [valid").unwrap();
        std::fs::write(bad.join(MODULE_FILE), b"nope").unwrap();
        let good = dir.join("dev.example.demo");
        std::fs::create_dir_all(&good).unwrap();
        std::fs::write(good.join(MANIFEST_FILE), MANIFEST).unwrap();
        std::fs::write(good.join(MODULE_FILE), wasm_bytes()).unwrap();

        let mut registry = PluginRegistry::new();
        registry.discover_dir(&dir);
        assert!(matches!(
            registry.get("bad.plugin").unwrap().state,
            PluginState::Failed(_)
        ));
        assert_eq!(
            registry.get("dev.example.demo").unwrap().state,
            PluginState::Discovered
        );
    }

    #[test]
    fn update_refreshes_bytes_and_keeps_old_on_failure() {
        let dir = plugin_dir("update", MANIFEST, &wasm_bytes());
        let mut registry = PluginRegistry::new();
        registry.discover_dir(&dir);
        registry.validate("dev.example.demo").unwrap();

        // New version on disk refreshes the record to Validated.
        let plug = dir.join("dev.example.demo");
        let bumped = MANIFEST.replace("version = \"1.0.0\"", "version = \"2.0.0\"");
        std::fs::write(plug.join(MANIFEST_FILE), &bumped).unwrap();
        registry.update("dev.example.demo", &plug).unwrap();
        let record = registry.get("dev.example.demo").unwrap();
        assert_eq!(record.state, PluginState::Validated);
        assert_eq!(record.manifest.version, "2.0.0");

        // Broken on-disk state fails and keeps the previous record.
        std::fs::write(plug.join(MANIFEST_FILE), "not = [valid").unwrap();
        assert!(registry.update("dev.example.demo", &plug).is_err());
        let record = registry.get("dev.example.demo").unwrap();
        assert_eq!(record.state, PluginState::Validated);
        assert_eq!(record.manifest.version, "2.0.0");

        // Unknown plugins stay unknown.
        assert!(registry.update("no.such.plugin", &plug).is_err());
    }

    const NATIVE_MANIFEST: &str = r#"
[plugin]
id = "dev.example.native"
name = "Native"
version = "1.0.0"
api = 1
trust = "trusted-native"

[runtime]
type = "native"
"#;

    #[test]
    fn native_manifest_requires_trusted_native_and_vice_versa() {
        // Native + trusted-native validates.
        let m = PluginManifest::parse_toml(NATIVE_MANIFEST).unwrap();
        assert_eq!(m.runtime, RuntimeKind::Native);
        assert_eq!(m.trust, TrustLevel::TrustedNative);
        // Native without the label is rejected…
        let unsigned = NATIVE_MANIFEST.replace("trusted-native", "unsigned-wasm");
        assert!(PluginManifest::parse_toml(&unsigned).is_err());
        // …and the label on a WASM plugin is rejected too.
        let mislabeled = MANIFEST.replace("unsigned-wasm", "trusted-native");
        assert!(PluginManifest::parse_toml(&mislabeled).is_err());
        // Unknown runtime types stay rejected.
        let bogus = MANIFEST.replace("type = \"wasm\"", "type = \"cuda\"");
        assert!(PluginManifest::parse_toml(&bogus).is_err());
    }

    fn native_dir(tag: &str, manifest: &str, module: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("utsuwa-native-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let plug = dir.join("dev.example.native");
        std::fs::create_dir_all(&plug).unwrap();
        std::fs::write(plug.join(MANIFEST_FILE), manifest).unwrap();
        std::fs::write(plug.join(NATIVE_MODULE_FILE), module).unwrap();
        dir
    }

    #[test]
    fn native_plugins_default_denied_until_allow_listed() {
        // Fake ELF magic: non-empty, not WebAssembly.
        let dir = native_dir("gate", NATIVE_MANIFEST, &[0x7f, b'E', b'L', b'F', 0x01]);
        let mut registry = PluginRegistry::new();
        registry.discover_dir(&dir);
        let record = registry.get("dev.example.native").unwrap();
        assert_eq!(record.state, PluginState::Discovered);
        assert_eq!(record.manifest.runtime, RuntimeKind::Native);

        registry.validate("dev.example.native").unwrap();
        registry.mark_loaded("dev.example.native").unwrap();
        // Default-deny: enabling an unsafe plugin without user approval
        // fails with a dedicated error, not a lifecycle error.
        assert_eq!(
            registry.enable("dev.example.native"),
            Err(PluginError::NativeNotAllowed("dev.example.native".to_string()))
        );
        // Explicit user approval unlocks the transition…
        registry.allow_native("dev.example.native");
        assert!(registry.is_native_allowed("dev.example.native"));
        registry.enable("dev.example.native").unwrap();
        assert_eq!(registry.enabled().len(), 1);
        // …and withdrawing it blocks the next enable.
        registry.disable("dev.example.native").unwrap();
        registry.deny_native("dev.example.native");
        assert!(registry.enable("dev.example.native").is_err());
    }

    #[test]
    fn native_discovery_rejects_confused_packages() {
        // WASM bytes behind a native manifest: rejected at discovery.
        let dir = native_dir("confused", NATIVE_MANIFEST, &wasm_bytes());
        let mut registry = PluginRegistry::new();
        registry.discover_dir(&dir);
        assert!(matches!(
            registry.get("dev.example.native").unwrap().state,
            PluginState::Failed(_)
        ));
        // Empty native module: rejected too.
        let dir = native_dir("empty", NATIVE_MANIFEST, &[]);
        let mut registry = PluginRegistry::new();
        registry.discover_dir(&dir);
        assert!(matches!(
            registry.get("dev.example.native").unwrap().state,
            PluginState::Failed(_)
        ));
    }

    #[test]
    fn directory_id_mismatch_is_rejected() {
        let dir = std::env::temp_dir().join(format!(
            "utsuwa-plugin-test-{}-mismatch",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let plug = dir.join("trusted.lookalike");
        std::fs::create_dir_all(&plug).unwrap();
        std::fs::write(plug.join(MANIFEST_FILE), MANIFEST).unwrap();
        std::fs::write(plug.join(MODULE_FILE), wasm_bytes()).unwrap();
        let mut registry = PluginRegistry::new();
        registry.discover_dir(&dir);
        assert!(matches!(
            registry.get("trusted.lookalike").unwrap().state,
            PluginState::Failed(_)
        ));
    }
}
