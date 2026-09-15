//! Native system inspection (`system.*`).
//!
//! Read-only host facts with bounded output: OS, CPU, memory, storage,
//! network interfaces, battery, process list, and a curated environment
//! summary. Environment variables are never dumped wholesale —
//! [`environment_summary`] exposes a fixed allowlist of harmless names and
//! redacts anything that looks like a secret.

use std::sync::Arc;
use tool_core::{Tool, ToolContext, ToolEffect, ToolError, ToolMetadata, ToolOutput};

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::InvalidArgs {
        tool: "system.info".to_string(),
        message: message.into(),
    }
}

/// Names safe to expose with their values. Everything else is either
/// omitted or value-redacted by [`environment_summary`].
const SAFE_ENV_VARS: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "LC_ALL",
    "TERM",
    "EDITOR",
    "PAGER",
    "OS",
    "OSTYPE",
    "HOSTNAME",
    "COMPUTERNAME",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
];

fn looks_sensitive(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    [
        "KEY",
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "CREDENTIAL",
        "PRIVATE",
        "AUTH",
        "SESSION",
        "COOKIE",
        "BEARER",
        "SIGNATURE",
        "SSH",
    ]
    .iter()
    .any(|needle| upper.contains(needle))
}

/// Curated environment summary: safe names with values, sensitive names
/// redacted, everything else omitted.
pub fn environment_summary() -> serde_json::Value {
    let mut entries = serde_json::Map::new();
    for (name, value) in std::env::vars() {
        if SAFE_ENV_VARS.contains(&name.as_str()) {
            entries.insert(name, serde_json::Value::String(value));
        } else if looks_sensitive(&name) {
            entries.insert(name, serde_json::Value::String("[redacted]".to_string()));
        }
    }
    serde_json::Value::Object(entries)
}

fn snapshot(kind: &str) -> Result<serde_json::Value, ToolError> {
    let value = match kind {
        "os" => serde_json::json!({
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "family": std::env::consts::FAMILY,
            "hostname": sysinfo::System::host_name(),
            "os_version": sysinfo::System::os_version(),
            "kernel_version": sysinfo::System::kernel_version(),
        }),
        "cpu" => {
            let mut system = sysinfo::System::new();
            system.refresh_cpu_all();
            let cpus = system
                .cpus()
                .iter()
                .map(|cpu| {
                    serde_json::json!({
                        "name": cpu.name(),
                        "usage_percent": cpu.cpu_usage(),
                        "frequency_mhz": cpu.frequency(),
                    })
                })
                .collect::<Vec<_>>();
            serde_json::json!({
                "physical_cores": sysinfo::System::physical_core_count(),
                "logical_cpus": cpus.len(),
                "global_usage_percent": system.global_cpu_usage(),
                "cpus": cpus,
            })
        }
        "memory" => {
            let mut system = sysinfo::System::new();
            system.refresh_memory();
            serde_json::json!({
                "total_bytes": system.total_memory(),
                "used_bytes": system.used_memory(),
                "free_bytes": system.free_memory(),
                "total_swap_bytes": system.total_swap(),
                "used_swap_bytes": system.used_swap(),
            })
        }
        "storage" => {
            let disks = sysinfo::Disks::new_with_refreshed_list();
            serde_json::json!({
                "disks": disks
                    .iter()
                    .map(|disk| serde_json::json!({
                        "name": disk.name().to_string_lossy(),
                        "mount_point": disk.mount_point().to_string_lossy(),
                        "file_system": disk.file_system().to_string_lossy(),
                        "total_bytes": disk.total_space(),
                        "available_bytes": disk.available_space(),
                    }))
                    .collect::<Vec<_>>(),
            })
        }
        "network" => {
            let networks = sysinfo::Networks::new_with_refreshed_list();
            serde_json::json!({
                "interfaces": networks
                    .iter()
                    .map(|(name, data)| serde_json::json!({
                        "name": name,
                        "received_bytes": data.received(),
                        "transmitted_bytes": data.transmitted(),
                    }))
                    .collect::<Vec<_>>(),
            })
        }
        "battery" => battery_snapshot().unwrap_or_else(|| {
            serde_json::json!({ "available": false, "detail": "battery information is not available on this host" })
        }),
        "environment_summary" => environment_summary(),
        _ => return Err(invalid(format!("unknown system snapshot '{kind}'"))),
    };
    Ok(value)
}

/// Battery state. Linux reads `/sys/class/power_supply` directly;
/// other platforms report unavailability honestly.
fn battery_snapshot() -> Option<serde_json::Value> {
    #[cfg(target_os = "linux")]
    {
        let root = std::path::Path::new("/sys/class/power_supply");
        let entries = std::fs::read_dir(root).ok()?;
        for entry in entries.flatten() {
            let kind = std::fs::read_to_string(entry.path().join("type")).ok()?;
            if kind.trim() != "Battery" {
                continue;
            }
            let read = |field: &str| {
                std::fs::read_to_string(entry.path().join(field))
                    .map(|value| value.trim().to_string())
                    .ok()
            };
            let capacity = read("capacity").and_then(|value| value.parse::<u8>().ok());
            return Some(serde_json::json!({
                "available": true,
                "status": read("status"),
                "capacity_percent": capacity,
                "technology": read("technology"),
            }));
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Read-only host-fact tool for one `snapshot` kind. Note: there is
/// deliberately no `"time"` kind here — `system.time` is owned by the
/// builtin system pack (`app-host::tooling::SystemToolPack`), which
/// returns full local/UTC clock facts. Registering another `system.time`
/// here produced a duplicate tool id every turn (the catalog keeps the
/// first and warns).
pub struct SystemInfoTool {
    pub kind: &'static str,
}

#[async_trait::async_trait]
impl Tool for SystemInfoTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new(match self.kind {
                "os" => "system.os",
                "cpu" => "system.cpu",
                "memory" => "system.memory",
                "storage" => "system.storage",
                "network" => "system.network",
                "battery" => "system.battery",
                _ => "system.environment_summary",
            }),
            description: format!("Read-only {} snapshot from the native host.", self.kind),
            input_schema: serde_json::json!({"type": "object", "additionalProperties": false, "properties": {}}),
            effects: vec![ToolEffect::ReadOnly],
        }
    }

    async fn invoke(
        &self,
        _ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        if !args.is_object() {
            return Err(invalid("args must be a JSON object"));
        }
        Ok(ToolOutput::json(snapshot(self.kind)?))
    }
}

pub struct SystemProcessesTool;
pub struct SystemProcessInfoTool;

#[async_trait::async_trait]
impl Tool for SystemProcessesTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("system.processes"),
            description: "List running processes (pid, name, cpu, memory). Bounded output."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "filter": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 500},
                },
            }),
            effects: vec![ToolEffect::ReadOnly],
        }
    }

    async fn invoke(
        &self,
        _ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let filter = args
            .get("filter")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_lowercase();
        let limit = args
            .get("limit")
            .and_then(|value| value.as_u64())
            .unwrap_or(100)
            .clamp(1, 500) as usize;
        let mut system = sysinfo::System::new();
        system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        let mut processes = system
            .processes()
            .iter()
            .filter(|(_, process)| {
                filter.is_empty()
                    || process
                        .name()
                        .to_string_lossy()
                        .to_lowercase()
                        .contains(&filter)
            })
            .map(|(pid, process)| {
                serde_json::json!({
                    "pid": pid.as_u32(),
                    "name": process.name().to_string_lossy(),
                    "cpu_percent": process.cpu_usage(),
                    "memory_bytes": process.memory(),
                })
            })
            .collect::<Vec<_>>();
        processes.sort_by_key(|process| process["pid"].as_u64().unwrap_or(0));
        processes.truncate(limit);
        Ok(ToolOutput::json(
            serde_json::json!({ "processes": processes }),
        ))
    }
}

#[async_trait::async_trait]
impl Tool for SystemProcessInfoTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("system.process_info"),
            description: "Inspect one process by pid (status, exe, command, memory). Arguments after the exe are redacted from output.".to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": { "pid": {"type": "integer", "minimum": 1} },
                "required": ["pid"],
            }),
            effects: vec![ToolEffect::ReadOnly],
        }
    }

    async fn invoke(
        &self,
        _ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let pid = args
            .get("pid")
            .and_then(|value| value.as_u64())
            .ok_or_else(|| invalid("missing integer 'pid'"))?;
        let mut system = sysinfo::System::new();
        system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        let pid = sysinfo::Pid::from_u32(pid as u32);
        let Some(process) = system.process(pid) else {
            return Err(ToolError::structured_with_details(
                "system.process_info",
                "invalid_target",
                format!("no process with pid {}", pid.as_u32()),
                serde_json::json!({ "pid": pid.as_u32() }),
            ));
        };
        // Command lines may contain secrets: expose the exe plus argv
        // count, never raw arguments.
        Ok(ToolOutput::json(serde_json::json!({
            "pid": pid.as_u32(),
            "name": process.name().to_string_lossy(),
            "exe": process.exe().map(|path| path.to_string_lossy().into_owned()),
            "status": format!("{:?}", process.status()),
            "cpu_percent": process.cpu_usage(),
            "memory_bytes": process.memory(),
            "argv_count": process.cmd().len(),
        })))
    }
}

/// Static system tool group.
pub struct SystemToolPack;

impl tool_sdk::ToolPack for SystemToolPack {
    fn id(&self) -> &'static str {
        "system"
    }

    fn tools(&self, _ctx: &tool_sdk::ToolLoadContext) -> Vec<Arc<dyn Tool>> {
        // No `system.time` here by design: the builtin system pack owns
        // that id (see `SystemInfoTool` docs). Duplicating it produced a
        // "duplicate tool id" warning on every turn.
        vec![
            Arc::new(SystemInfoTool { kind: "os" }),
            Arc::new(SystemInfoTool { kind: "cpu" }),
            Arc::new(SystemInfoTool { kind: "memory" }),
            Arc::new(SystemInfoTool { kind: "storage" }),
            Arc::new(SystemInfoTool { kind: "network" }),
            Arc::new(SystemInfoTool { kind: "battery" }),
            Arc::new(SystemInfoTool {
                kind: "environment_summary",
            }),
            Arc::new(SystemProcessesTool),
            Arc::new(SystemProcessInfoTool),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use capability_core::{AgentId, Principal};
    use tool_sdk::ToolPack as _;

    fn ctx() -> ToolContext {
        ToolContext::new(Principal::Agent(AgentId::new("test")))
    }

    #[test]
    fn sensitive_env_names_are_redacted_not_leaked() {
        assert!(looks_sensitive("OPENAI_API_KEY"));
        assert!(looks_sensitive("github_token"));
        assert!(looks_sensitive("DB_PASSWORD"));
        assert!(!looks_sensitive("PATH"));
        let summary = environment_summary();
        for (name, value) in summary.as_object().unwrap() {
            if looks_sensitive(name) {
                assert_eq!(value.as_str(), Some("[redacted]"), "{name}");
            }
        }
        // Full environment is never dumped.
        assert!(summary.as_object().unwrap().len() <= SAFE_ENV_VARS.len() + 64);
    }

    #[tokio::test]
    async fn snapshots_return_bounded_host_facts() {
        for kind in [
            "os",
            "cpu",
            "memory",
            "storage",
            "network",
            "battery",
            "environment_summary",
        ] {
            let tool = SystemInfoTool { kind };
            let out = tool.invoke(ctx(), serde_json::json!({})).await.unwrap();
            assert!(out.content.is_object(), "{kind}");
        }
        let processes = SystemProcessesTool;
        let out = processes
            .invoke(ctx(), serde_json::json!({"limit": 5}))
            .await
            .unwrap();
        assert!(out.content["processes"].as_array().unwrap().len() <= 5);
    }

    #[test]
    fn pack_registers_the_system_surface() {
        let mut ids = SystemToolPack
            .tools(&tool_sdk::ToolLoadContext::default())
            .iter()
            .map(|tool| tool.metadata().id.0.clone())
            .collect::<Vec<_>>();
        ids.sort();
        for expected in [
            "system.battery",
            "system.cpu",
            "system.environment_summary",
            "system.memory",
            "system.network",
            "system.os",
            "system.process_info",
            "system.processes",
            "system.storage",
        ] {
            assert!(ids.contains(&expected.to_string()), "{ids:?}");
        }
        // `system.time` lives in the builtin system pack, not here:
        // registering it twice warned on every turn.
        assert!(
            !ids.contains(&"system.time".to_string()),
            "system.time must not duplicate the builtin pack: {ids:?}"
        );
    }
}
