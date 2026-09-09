//! Per-turn tool composition (refactor step 3).
//!
//! This module owns *what* the agent can call each turn; `runtime`
//! only decides *when* to snapshot. Contents:
//!
//! - [`SystemToolPack`]: read-only host facts (`system.environment`,
//!   `system.time`).
//! - [`ProcessToolPack`]: structured process execution (`process.spawn`,
//!   `process.status`, `process.kill`).
//! - Settings synchronization (`sync_mcp_from_settings`,
//!   `discover_plugins_from_settings`): the composition root reads
//!   settings into the extension managers. The leaf-crate sources
//!   (`McpToolSource`, `PluginToolSource`, `MemoryToolPack`,
//!   `DesktopToolPack`) only collect from already-configured managers —
//!   app-host never learns how their tools are built.
//!
//! The host filesystem surface arrives via `tool-filesystem-host`'s pack.

use super::runtime::{SETTING_MCP_SERVERS, SETTING_PLUGIN_DIR};
use host_core::HostEnvironment;
use serde::Deserialize;
use std::sync::{Arc, Mutex};
use storage_core::Storage;
use tool_sdk::{ToolLoadContext, ToolPack, TypedTool, TypedToolAdapter};

/// Empty argument object shared by the system-fact tools. Deserializing
/// (rather than asserting `is_object`) keeps unknown-field tolerance
/// identical to the historical manual implementations.
#[derive(Deserialize)]
struct NoArgs {}

/// Read-only host facts for models that need a small, explicit lookup
/// instead of relying on the larger trusted system context. No capability
/// requirement; never exposes a raw OS handle or mutation API.
struct HostEnvironmentTool {
    environment: HostEnvironment,
}

#[async_trait::async_trait]
impl TypedTool for HostEnvironmentTool {
    type Args = NoArgs;
    type Output = serde_json::Value;

    fn id(&self) -> &'static str {
        "system.environment"
    }

    fn description(&self) -> &'static str {
        "Return the native operating system, home directory, current working directory, path style, and validated special user directories. Read-only; use these exact paths instead of guessing or translating directory names."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {},
        })
    }

    fn effects(&self) -> Vec<tool_core::ToolEffect> {
        vec![tool_core::ToolEffect::ReadOnly]
    }

    async fn call(
        &self,
        _ctx: tool_core::ToolContext,
        _args: Self::Args,
    ) -> Result<Self::Output, tool_core::ToolError> {
        Ok(self.environment.json_value())
    }
}

/// Read-only clock facts for models that must use the host's actual
/// current time instead of inferring it from training data or a
/// conversation date. Snapshots the clock on every invocation.
struct SystemTimeTool;

#[async_trait::async_trait]
impl TypedTool for SystemTimeTool {
    type Args = NoArgs;
    type Output = serde_json::Value;

    fn id(&self) -> &'static str {
        "system.time"
    }

    fn description(&self) -> &'static str {
        "Return the current local and UTC time from the native host. Read-only, fresh on every call; use this instead of guessing the date or time."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {},
        })
    }

    fn effects(&self) -> Vec<tool_core::ToolEffect> {
        vec![tool_core::ToolEffect::ReadOnly]
    }

    async fn call(
        &self,
        _ctx: tool_core::ToolContext,
        _args: Self::Args,
    ) -> Result<Self::Output, tool_core::ToolError> {
        let utc = chrono::Utc::now();
        let local = utc.with_timezone(&chrono::Local);
        let mut content = serde_json::json!({
            "local": local.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "utc": utc.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "date": local.format("%Y-%m-%d").to_string(),
            "time": local.format("%H:%M:%S").to_string(),
            "utc_offset": local.format("%:z").to_string(),
            "unix_timestamp": utc.timestamp(),
        });
        if let Ok(timezone) = iana_time_zone::get_timezone() {
            if let Some(object) = content.as_object_mut() {
                object.insert("timezone".to_string(), serde_json::Value::String(timezone));
            }
        }
        Ok(content)
    }
}

/// Static system-fact tools. Pure reads, infallible to collect.
pub struct SystemToolPack {
    environment: HostEnvironment,
}

impl SystemToolPack {
    pub fn new(environment: HostEnvironment) -> Self {
        Self { environment }
    }
}

impl ToolPack for SystemToolPack {
    fn id(&self) -> &'static str {
        "builtin.system"
    }

    fn tools(&self, _ctx: &ToolLoadContext) -> Vec<Arc<dyn tool_core::Tool>> {
        vec![
            TypedToolAdapter::arc(HostEnvironmentTool {
                environment: self.environment.clone(),
            }),
            TypedToolAdapter::arc(SystemTimeTool),
        ]
    }
}

/// Structured process execution tools behind the process broker.
pub struct ProcessToolPack {
    manager: Arc<tool_process::ProcessManager>,
}

impl ProcessToolPack {
    pub fn new(manager: Arc<tool_process::ProcessManager>) -> Self {
        Self { manager }
    }
}

impl ToolPack for ProcessToolPack {
    fn id(&self) -> &'static str {
        "builtin.process"
    }

    fn tools(&self, _ctx: &ToolLoadContext) -> Vec<Arc<dyn tool_core::Tool>> {
        vec![
            Arc::new(tool_process::SpawnTool {
                manager: Arc::clone(&self.manager),
                limits: tool_process::ProcessLimits::default(),
            }) as Arc<dyn tool_core::Tool>,
            Arc::new(tool_process::StatusTool {
                manager: Arc::clone(&self.manager),
            }),
            Arc::new(tool_process::KillTool {
                manager: Arc::clone(&self.manager),
            }),
        ]
    }
}

/// Sync the MCP server set from settings into the manager. Runs before
/// the catalog snapshot so installs take effect without a restart.
/// Failures warn and keep the previous set; collection itself is the
/// [`mcp_runtime::McpToolSource`]'s job.
pub(crate) async fn sync_mcp_from_settings(
    mcp: &mcp_runtime::McpManager,
    storage: Option<&Arc<Mutex<Storage>>>,
) {
    let Some(storage) = storage else {
        return;
    };
    let configs: Option<Vec<mcp_runtime::McpServerConfig>> = storage
        .lock()
        .ok()
        .and_then(|store| store.get_setting(SETTING_MCP_SERVERS).ok())
        .flatten()
        .and_then(|value| {
            serde_json::from_value(value)
                .map_err(|e| {
                    tracing::warn!(%e, "mcp.servers setting is not a server array; ignoring");
                })
                .ok()
        });
    if let Some(configs) = configs {
        if let Err(e) = mcp.sync_configs(configs).await {
            tracing::warn!(%e, "mcp settings sync failed");
        }
    }
}

/// Discover the configured plugin directory into the WASM runtime. Runs
/// before the snapshot; collection itself is the
/// [`plugin_wasm::PluginToolSource`]'s job.
pub(crate) fn discover_plugins_from_settings(
    plugins: &Arc<plugin_wasm::PluginRuntime>,
    storage: Option<&Arc<Mutex<Storage>>>,
) {
    let Some(storage) = storage else {
        return;
    };
    let dir: Option<String> = storage
        .lock()
        .ok()
        .and_then(|store| store.get_setting(SETTING_PLUGIN_DIR).ok())
        .flatten()
        .and_then(|value| {
            serde_json::from_value(value)
                .map_err(|e| {
                    tracing::warn!(%e, "plugin.dir setting is not a path string; ignoring");
                })
                .ok()
        });
    if let Some(dir) = dir {
        if let Err(e) = plugins.discover_dir(std::path::Path::new(&dir)) {
            tracing::warn!(dir = %dir, error = %e, "plugin discovery failed");
        }
    }
}
