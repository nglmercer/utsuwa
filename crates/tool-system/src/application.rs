//! Application management (`application.*`).
//!
//! `application.list` enumerates running applications from process state. It
//! is intentionally read-only and remains unrestricted: the existing privacy
//! policy allows discovery of process names/pids, while actions on one
//! application are separately capability- and sharing-scope-gated.
//! `application.launch` starts one validated application identity — no
//! shell, no argument strings (launch flags need separate process
//! authorization). `application.quit` signals by pid with a
//! [`capability_core::Capability::ProcessSignal`] ticket.
//! `application.activate` is macOS-only (`open -a`); other platforms
//! report `unsupported_operation` honestly.

use std::collections::BTreeSet;
use std::sync::Arc;
use tool_core::{
    canonical_application_identity, canonical_process_application_identity, CapabilityRequirement,
    Tool, ToolContext, ToolEffect, ToolError, ToolMetadata, ToolOutput,
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
    canonical_application_identity(value).map_err(|error| invalid(tool, error.to_string()))?;
    Ok(value.to_string())
}

fn application_identity(tool: &str, value: &str) -> Result<String, ToolError> {
    canonical_application_identity(value).map_err(|error| invalid(tool, error.to_string()))
}

fn check_scope(tool: &str, ctx: &ToolContext, application: &str) -> Result<(), ToolError> {
    if let Some(policy) = &ctx.application_scope_policy {
        policy.check_application_allowed(tool, application)?;
    }
    Ok(())
}

fn identity_unverified(tool: &str, pid: u32) -> ToolError {
    ToolError::structured_with_details(
        tool,
        "application_identity_unverified",
        "the target application could not be verified against the active application allowlist",
        serde_json::json!({ "pid": pid }),
    )
}

fn process_application_identity(
    executable: Option<&std::path::Path>,
    name: &std::ffi::OsStr,
) -> Option<String> {
    if let Some(path) = executable {
        // A host-reported executable path is stronger evidence than the
        // display name. If it is present but cannot be safely canonicalized,
        // do not fall back to a weaker value and accidentally authorize an
        // unverified process.
        return path
            .to_str()
            .and_then(canonical_process_application_identity);
    }
    name.to_str()
        .and_then(canonical_process_application_identity)
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
        let identity = application_identity("application.launch", application).ok()?;
        Some(CapabilityRequirement {
            capability: capability_core::Capability::ApplicationLaunch,
            resource: capability_core::Resource::Application(identity),
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
        let identity = application_identity("application.launch", &application)?;
        require(
            "application.launch",
            &ctx,
            capability_core::Capability::ApplicationLaunch,
            capability_core::Resource::Application(identity.clone()),
        )?;
        check_scope("application.launch", &ctx, &identity)?;
        launch_application(&application).await?;
        Ok(ToolOutput::json(
            serde_json::json!({ "ok": true, "application": application, "identity": identity }),
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
        #[cfg(windows)]
        let candidate_is_executable = candidate
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"));
        #[cfg(not(windows))]
        let candidate_is_executable = true;
        if candidate.is_file() && candidate_is_executable {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            // Never resolve batch files: Windows may route them through
            // cmd.exe, which would violate this tool's argument-free,
            // shell-free launch contract.
            for extension in ["exe"] {
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
        let pid = u32::try_from(args.get("pid")?.as_u64()?).ok()?;
        if pid == 0 {
            return None;
        }
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
        let pid = u32::try_from(pid)
            .map_err(|_| invalid("application.quit", "'pid' is outside the host pid range"))?;
        if pid == 0 {
            return Err(invalid(
                "application.quit",
                "'pid' must be greater than zero",
            ));
        }
        require(
            "application.quit",
            &ctx,
            capability_core::Capability::ProcessSignal,
            capability_core::Resource::Application(format!("pid:{pid}")),
        )?;
        let mut system = sysinfo::System::new();
        system.refresh_processes(sysinfo::ProcessesToUpdate::All, false);
        let pid = sysinfo::Pid::from_u32(pid);
        let Some(process) = system.process(pid) else {
            return Err(ToolError::structured_with_details(
                "application.quit",
                "invalid_target",
                "no process with that pid".to_string(),
                serde_json::json!({ "pid": pid.as_u32() }),
            ));
        };
        if let Some(policy) = &ctx.application_scope_policy {
            if policy.is_restricted() {
                let application = process_application_identity(process.exe(), process.name())
                    .ok_or_else(|| identity_unverified("application.quit", pid.as_u32()))?;
                policy.check_application_allowed("application.quit", &application)?;
            }
        }
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
        let identity = application_identity("application.activate", application).ok()?;
        Some(CapabilityRequirement {
            capability: capability_core::Capability::DesktopControl,
            resource: capability_core::Resource::Application(identity),
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
        let identity = application_identity("application.activate", &application)?;
        require(
            "application.activate",
            &ctx,
            capability_core::Capability::DesktopControl,
            capability_core::Resource::Application(identity.clone()),
        )?;
        check_scope("application.activate", &ctx, &identity)?;
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
            let _ = (application, identity);
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
    use capability_core::{AgentId, CapabilityTicket, Principal, ResourceScope};
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use std::time::Duration;
    use tool_core::ApplicationScopePolicy;
    use tool_sdk::ToolPack as _;

    struct TestScope {
        allowed: BTreeSet<String>,
    }

    impl TestScope {
        fn new(allowed: &[&str]) -> Arc<Self> {
            Arc::new(Self {
                allowed: allowed.iter().map(|value| (*value).to_string()).collect(),
            })
        }
    }

    impl ApplicationScopePolicy for TestScope {
        fn is_restricted(&self) -> bool {
            !self.allowed.is_empty()
        }

        fn check_application_allowed(
            &self,
            tool: &str,
            application: &str,
        ) -> Result<(), ToolError> {
            if !self.is_restricted() || self.allowed.contains(application) {
                return Ok(());
            }
            Err(ToolError::structured_with_details(
                tool,
                "application_not_allowed",
                "the target application is outside the active application allowlist",
                serde_json::json!({ "application": application }),
            ))
        }
    }

    fn ticketed_context_without_scope(
        capability: capability_core::Capability,
        resource: capability_core::Resource,
    ) -> ToolContext {
        let ctx = ToolContext::new(Principal::Agent(AgentId::new("t")));
        let ticket = CapabilityTicket::mint(
            ctx.principal.clone(),
            capability,
            ResourceScope::new(vec![resource]),
            ctx.invocation_id,
            Duration::from_secs(120),
        );
        ctx.with_ticket(ticket)
    }

    fn ticketed_context(
        capability: capability_core::Capability,
        resource: capability_core::Resource,
        scope: Arc<TestScope>,
    ) -> ToolContext {
        ticketed_context_without_scope(capability, resource).with_application_scope_policy(scope)
    }

    #[test]
    fn shell_identities_are_rejected() {
        assert!(validated_application("application.launch", "firefox").is_ok());
        assert!(validated_application("application.launch", "a;b").is_err());
        assert!(validated_application("application.launch", "a b").is_err());
        assert!(validated_application("application.launch", "").is_err());
        assert!(validated_application("application.launch", "$(evil)").is_err());
        assert!(validated_application("application.launch", "firefox --private").is_err());
        assert!(validated_application("application.launch", "../firefox").is_err());
        assert!(validated_application("application.launch", "C:\\firefox").is_err());
    }

    #[test]
    fn application_identity_canonicalization_is_exact_and_platform_aware() {
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        assert_eq!(
            canonical_application_identity("Firefox").unwrap(),
            "firefox"
        );
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        assert_eq!(
            canonical_application_identity("Firefox").unwrap(),
            "Firefox"
        );
        assert_eq!(
            canonical_application_identity("firefox").unwrap(),
            "firefox"
        );
        assert_eq!(
            canonical_application_identity("org.mozilla.firefox").unwrap(),
            "org.mozilla.firefox"
        );
        let bundle_id = if cfg!(any(target_os = "windows", target_os = "macos")) {
            "com.apple.safari"
        } else {
            "com.apple.Safari"
        };
        assert_eq!(
            canonical_application_identity("com.apple.Safari").unwrap(),
            bundle_id
        );
        #[cfg(windows)]
        assert_eq!(
            canonical_application_identity("firefox.exe").unwrap(),
            "firefox"
        );
        #[cfg(target_os = "macos")]
        assert_eq!(
            canonical_application_identity("Safari.app").unwrap(),
            "safari"
        );
        #[cfg(not(windows))]
        assert_eq!(
            canonical_application_identity("firefox.exe").unwrap(),
            "firefox.exe"
        );
        assert!(canonical_application_identity("/usr/bin/firefox").is_err());
        assert!(canonical_application_identity("../firefox").is_err());
        assert!(canonical_application_identity("firefox;touch").is_err());
        assert!(canonical_application_identity("-firefox").is_err());
        assert_eq!(
            canonical_process_application_identity("/usr/bin/firefox"),
            Some("firefox".to_string())
        );
        let process_name = if cfg!(any(target_os = "windows", target_os = "macos")) {
            Some("firefox".to_string())
        } else {
            Some("Firefox".to_string())
        };
        assert_eq!(
            canonical_process_application_identity("Firefox"),
            process_name
        );
        assert_eq!(canonical_process_application_identity("../firefox"), None);
        assert_eq!(
            canonical_process_application_identity("/tmp/../firefox"),
            None
        );
    }

    #[test]
    fn application_list_is_read_only_and_unrestricted_by_scope() {
        assert_eq!(
            ApplicationListTool.metadata().effects,
            vec![ToolEffect::ReadOnly]
        );
        // `application.list` intentionally has no scope check: it exposes
        // process metadata for discovery, while actions are separately gated.
        assert!(application_identity("application.list", "Firefox").is_ok());
    }

    #[test]
    fn unknown_pid_identity_has_a_precise_structured_error() {
        let error = identity_unverified("application.quit", 42);
        assert_eq!(error.code(), Some("application_identity_unverified"));
        assert!(error.model_message().contains("\"pid\":42"));
        assert_eq!(
            process_application_identity(None, std::ffi::OsStr::new("../firefox")),
            None
        );
        assert_eq!(
            process_application_identity(
                Some(std::path::Path::new("/usr/bin/firefox")),
                std::ffi::OsStr::new("unknown")
            ),
            Some("firefox".to_string())
        );
        assert_eq!(
            process_application_identity(
                Some(std::path::Path::new("../firefox")),
                std::ffi::OsStr::new("firefox")
            ),
            None
        );
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

    #[cfg(unix)]
    #[tokio::test]
    async fn unrestricted_and_allowed_launches_reach_the_argument_free_launcher() {
        let empty_scope = ticketed_context(
            capability_core::Capability::ApplicationLaunch,
            capability_core::Resource::Application("true".to_string()),
            TestScope::new(&[]),
        );
        let out = ApplicationLaunchTool
            .invoke(empty_scope, serde_json::json!({"application": "true"}))
            .await
            .unwrap();
        assert_eq!(out.content["ok"], true);

        let unrestricted = ticketed_context_without_scope(
            capability_core::Capability::ApplicationLaunch,
            capability_core::Resource::Application("true".to_string()),
        );
        let out = ApplicationLaunchTool
            .invoke(unrestricted, serde_json::json!({"application": "true"}))
            .await
            .unwrap();
        assert_eq!(out.content["ok"], true);

        let allowed = ticketed_context(
            capability_core::Capability::ApplicationLaunch,
            capability_core::Resource::Application("firefox".to_string()),
            TestScope::new(&["firefox"]),
        );
        assert!(check_scope("application.launch", &allowed, "firefox").is_ok());
    }

    #[tokio::test]
    async fn restrictive_scope_denies_launch_and_activation_before_the_backend() {
        let launch = ApplicationLaunchTool
            .invoke(
                ticketed_context(
                    capability_core::Capability::ApplicationLaunch,
                    capability_core::Resource::Application("terminal".to_string()),
                    TestScope::new(&["firefox"]),
                ),
                serde_json::json!({"application": "terminal"}),
            )
            .await
            .unwrap_err();
        assert_eq!(launch.code(), Some("application_not_allowed"), "{launch:?}");

        let activate = ApplicationActivateTool
            .invoke(
                ticketed_context(
                    capability_core::Capability::DesktopControl,
                    capability_core::Resource::Application("terminal".to_string()),
                    TestScope::new(&["firefox"]),
                ),
                serde_json::json!({"application": "terminal"}),
            )
            .await
            .unwrap_err();
        assert_eq!(
            activate.code(),
            Some("application_not_allowed"),
            "{activate:?}"
        );
    }

    #[tokio::test]
    async fn restrictive_scope_resolves_quit_pid_before_signalling() {
        let pid = std::process::id();
        let error = ApplicationQuitTool
            .invoke(
                ticketed_context(
                    capability_core::Capability::ProcessSignal,
                    capability_core::Resource::Application(format!("pid:{pid}")),
                    TestScope::new(&["firefox"]),
                ),
                serde_json::json!({"pid": pid}),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code(), Some("application_not_allowed"), "{error:?}");
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
