//! Application management (`application.*`).
//!
//! `application.list` enumerates running applications from process state.
//! `application.launch` starts one validated application identity — no
//! shell, no argument strings (launch flags need separate process
//! authorization). `application.quit` signals by pid with a
//! [`capability_core::Capability::ProcessSignal`] ticket.
//! `application.activate` is macOS-only (`open -a`); other platforms
//! report `unsupported_operation` honestly.

use std::collections::BTreeSet;
use std::sync::Arc;
use tool_core::{
    CapabilityRequirement, Tool, ToolContext, ToolEffect, ToolError, ToolMetadata, ToolOutput,
};

fn invalid(tool: &str, message: impl Into<String>) -> ToolError {
    ToolError::InvalidArgs {
        tool: tool.to_string(),
        message: message.into(),
    }
}

fn require(
    tool: &str,
    ctx: &ToolContext,
    capability: capability_core::Capability,
    resource: capability_core::Resource,
) -> Result<(), ToolError> {
    if ctx.has_ticket(capability.clone(), resource) {
        Ok(())
    } else {
        Err(ToolError::structured_with_details(
            tool,
            "permission_required",
            "no capability ticket authorizes this application operation",
            serde_json::json!({ "capability": format!("{capability:?}") }),
        ))
    }
}

fn validated_application(tool: &str, value: &str) -> Result<String, ToolError> {
    if value.trim().is_empty() {
        return Err(invalid(tool, "application must be a non-empty identity"));
    }
    if value.chars().any(|ch| {
        ch.is_whitespace()
            || ch.is_control()
            || matches!(ch, ';' | '|' | '&' | '$' | '>' | '<' | '`' | '"' | '\'')
    }) {
        return Err(invalid(
            tool,
            "application must be one identity, not a shell command or argument string",
        ));
    }
    Ok(value.to_string())
}

pub struct ApplicationListTool;
pub struct ApplicationLaunchTool;
pub struct ApplicationQuitTool;
pub struct ApplicationActivateTool;

#[async_trait::async_trait]
impl Tool for ApplicationListTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("application.list"),
            description: "List running applications (deduplicated process names with pids)."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": { "filter": {"type": "string"}, "limit": {"type": "integer", "minimum": 1, "maximum": 200} },
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
            .clamp(1, 200) as usize;
        let mut system = sysinfo::System::new();
        system.refresh_processes(sysinfo::ProcessesToUpdate::All, false);
        let mut seen = BTreeSet::new();
        let mut applications = Vec::new();
        let mut pids: Vec<u32> = system.processes().keys().map(|pid| pid.as_u32()).collect();
        pids.sort_unstable();
        for pid in pids {
            let Some(process) = system.process(sysinfo::Pid::from_u32(pid)) else {
                continue;
            };
            let name = process.name().to_string_lossy().into_owned();
            if !filter.is_empty() && !name.to_lowercase().contains(&filter) {
                continue;
            }
            if !seen.insert(name.clone()) {
                continue;
            }
            applications.push(serde_json::json!({ "application": name, "pid": pid }));
            if applications.len() >= limit {
                break;
            }
        }
        Ok(ToolOutput::json(
            serde_json::json!({ "applications": applications }),
        ))
    }
}

#[async_trait::async_trait]
impl Tool for ApplicationLaunchTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("application.launch"),
            description: "Launch one validated application identity (resolved via PATH on Linux/Windows, `open -a` on macOS). No arguments or shell commands.".to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": { "application": {"type": "string", "minLength": 1} },
                "required": ["application"],
            }),
            effects: vec![ToolEffect::ExternalSideEffect],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        let application = args.get("application")?.as_str()?;
        Some(CapabilityRequirement {
            capability: capability_core::Capability::ApplicationLaunch,
            resource: capability_core::Resource::Application(application.to_string()),
        })
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let raw = args
            .get("application")
            .and_then(|value| value.as_str())
            .ok_or_else(|| invalid("application.launch", "missing string 'application'"))?;
        let application = validated_application("application.launch", raw)?;
        require(
            "application.launch",
            &ctx,
            capability_core::Capability::ApplicationLaunch,
            capability_core::Resource::Application(application.clone()),
        )?;
        launch_application(&application).await?;
        Ok(ToolOutput::json(
            serde_json::json!({ "ok": true, "application": application }),
        ))
    }
}

#[cfg(target_os = "macos")]
async fn launch_application(application: &str) -> Result<(), ToolError> {
    let status = tokio::process::Command::new("open")
        .arg("-a")
        .arg(application)
        .status()
        .await
        .map_err(|error| {
            ToolError::structured(
                "application.launch",
                "backend_unavailable",
                format!("cannot launch: {error}"),
            )
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(ToolError::structured(
            "application.launch",
            "action_failed",
            format!("launch exited with {status}"),
        ))
    }
}

#[cfg(not(target_os = "macos"))]
async fn launch_application(application: &str) -> Result<(), ToolError> {
    // Resolve through PATH only: never execute a relative or absolute path
    // the model typed in, and never pass arguments.
    let resolved = resolve_on_path(application).ok_or_else(|| {
        ToolError::structured_with_details(
            "application.launch",
            "invalid_target",
            format!("'{application}' is not on PATH"),
            serde_json::json!({ "application": application }),
        )
    })?;
    tokio::process::Command::new(resolved)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|error| {
            ToolError::structured("application.launch", "action_failed", error.to_string())
        })?;
    Ok(())
}

fn resolve_on_path(name: &str) -> Option<std::path::PathBuf> {
    if name.contains('/') || name.contains('\\') {
        return None;
    }
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            for extension in ["exe", "bat", "cmd"] {
                let candidate = dir.join(format!("{name}.{extension}"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

#[async_trait::async_trait]
impl Tool for ApplicationQuitTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("application.quit"),
            description: "Ask a process to exit by pid (SIGTERM semantics via the OS).".to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": { "pid": {"type": "integer", "minimum": 1} },
                "required": ["pid"],
            }),
            effects: vec![ToolEffect::Destructive],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        let pid = args.get("pid")?.as_u64()?;
        Some(CapabilityRequirement {
            capability: capability_core::Capability::ProcessSignal,
            // Pid-scoped would be ideal; the executable path is unknown
            // before inspection, so scope to the pid-keyed process handle.
            resource: capability_core::Resource::Application(format!("pid:{pid}")),
        })
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let pid = args
            .get("pid")
            .and_then(|value| value.as_u64())
            .ok_or_else(|| invalid("application.quit", "missing integer 'pid'"))?;
        require(
            "application.quit",
            &ctx,
            capability_core::Capability::ProcessSignal,
            capability_core::Resource::Application(format!("pid:{pid}")),
        )?;
        let mut system = sysinfo::System::new();
        system.refresh_processes(sysinfo::ProcessesToUpdate::All, false);
        let pid = sysinfo::Pid::from_u32(pid as u32);
        let Some(process) = system.process(pid) else {
            return Err(ToolError::structured_with_details(
                "application.quit",
                "invalid_target",
                "no process with that pid".to_string(),
                serde_json::json!({ "pid": pid.as_u32() }),
            ));
        };
        if !process.kill() {
            return Err(ToolError::structured(
                "application.quit",
                "action_failed",
                "the OS refused to signal the process",
            ));
        }
        Ok(ToolOutput::json(
            serde_json::json!({ "ok": true, "pid": pid.as_u32() }),
        ))
    }
}

#[async_trait::async_trait]
impl Tool for ApplicationActivateTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("application.activate"),
            description: "Bring an application to the front (macOS only; other platforms report unsupported).".to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": { "application": {"type": "string", "minLength": 1} },
                "required": ["application"],
            }),
            effects: vec![ToolEffect::DesktopControl],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        let application = args.get("application")?.as_str()?;
        Some(CapabilityRequirement {
            capability: capability_core::Capability::DesktopControl,
            resource: capability_core::Resource::Application(application.to_string()),
        })
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let raw = args
            .get("application")
            .and_then(|value| value.as_str())
            .ok_or_else(|| invalid("application.activate", "missing string 'application'"))?;
        let application = validated_application("application.activate", raw)?;
        require(
            "application.activate",
            &ctx,
            capability_core::Capability::DesktopControl,
            capability_core::Resource::Application(application.clone()),
        )?;
        #[cfg(target_os = "macos")]
        {
            let status = tokio::process::Command::new("open")
                .arg("-a")
                .arg(&application)
                .status()
                .await
                .map_err(|error| {
                    ToolError::structured(
                        "application.activate",
                        "backend_unavailable",
                        format!("cannot activate: {error}"),
                    )
                })?;
            if status.success() {
                return Ok(ToolOutput::json(
                    serde_json::json!({ "ok": true, "application": application }),
                ));
            }
            return Err(ToolError::structured(
                "application.activate",
                "action_failed",
                format!("activate exited with {status}"),
            ));
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = application;
            return Err(ToolError::structured(
                "application.activate",
                "unsupported_operation",
                "window activation is only implemented on macOS in this build",
            ));
        }
    }
}

/// Static application tool group.
pub struct ApplicationToolPack;

impl tool_sdk::ToolPack for ApplicationToolPack {
    fn id(&self) -> &'static str {
        "application"
    }

    fn tools(&self, _ctx: &tool_sdk::ToolLoadContext) -> Vec<Arc<dyn Tool>> {
        vec![
            Arc::new(ApplicationListTool),
            Arc::new(ApplicationLaunchTool),
            Arc::new(ApplicationQuitTool),
            Arc::new(ApplicationActivateTool),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use capability_core::{AgentId, Principal};
    use tool_sdk::ToolPack as _;

    #[test]
    fn shell_identities_are_rejected() {
        assert!(validated_application("application.launch", "firefox").is_ok());
        assert!(validated_application("application.launch", "a;b").is_err());
        assert!(validated_application("application.launch", "a b").is_err());
        assert!(validated_application("application.launch", "").is_err());
        assert!(validated_application("application.launch", "$(evil)").is_err());
    }

    #[tokio::test]
    async fn list_returns_running_applications() {
        let out = ApplicationListTool
            .invoke(
                ToolContext::new(Principal::Agent(AgentId::new("t"))),
                serde_json::json!({}),
            )
            .await
            .unwrap();
        assert!(out.content["applications"].is_array());
    }

    #[tokio::test]
    async fn launch_without_ticket_is_permission_required() {
        let err = ApplicationLaunchTool
            .invoke(
                ToolContext::new(Principal::Agent(AgentId::new("t"))),
                serde_json::json!({"application": "firefox"}),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some("permission_required"), "{err:?}");
    }

    #[test]
    fn pack_registers_four_tools() {
        let mut ids = ApplicationToolPack
            .tools(&tool_sdk::ToolLoadContext::default())
            .iter()
            .map(|tool| tool.metadata().id.0.clone())
            .collect::<Vec<_>>();
        ids.sort();
        assert_eq!(
            ids,
            vec![
                "application.activate",
                "application.launch",
                "application.list",
                "application.quit",
            ]
        );
    }
}
