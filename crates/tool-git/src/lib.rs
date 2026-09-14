//! Native Git tools (`git.*`).
//!
//! Implemented over the `git` CLI with a strict subcommand allowlist — no
//! shell, no argument injection (every argument passes through
//! [`std::process::Command`], never a shell string). Read-only inspection
//! (`status`, `diff`, `log`, `show`, `branch list`, `remote list`) needs a
//! filesystem-read ticket for the repository; mutations (`add`, `commit`,
//! `checkout`, `branch create`, `restore`) additionally need a
//! filesystem-write ticket and are marked [`ToolEffect::FilesystemWrite`].
//! Network operations (`fetch`, `pull`, `push`) require a `NetworkConnect`
//! ticket for the remote host, and destructive operations (`reset`,
//! `clean`, `push --force`) are marked [`ToolEffect::Destructive`] so they
//! always route through stronger approval. The model is never allowed to
//! push merely because it created a commit: `git.push` is a separate,
//! explicitly authorized call.

use capability_core::{Capability, Resource};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tool_core::{
    CapabilityRequirement, Tool, ToolContext, ToolEffect, ToolError, ToolMetadata, ToolOutput,
};

const GIT_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_GIT_OUTPUT: usize = 256 * 1024;

/// Risk class for one git operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GitRisk {
    Read,
    Mutate,
    Network,
    Destructive,
}

struct GitSpec {
    id: &'static str,
    description: &'static str,
    risk: GitRisk,
    /// Extra argv appended after `git -C <repo> <subcommand…>`.
    extra_schema: serde_json::Value,
}

fn specs() -> Vec<GitSpec> {
    vec![
        GitSpec { id: "git.status", description: "Show working-tree status (short porcelain).", risk: GitRisk::Read, extra_schema: serde_json::json!({}) },
        GitSpec { id: "git.diff", description: "Show unstaged/staged diff (bounded output).", risk: GitRisk::Read, extra_schema: serde_json::json!({"properties": {"staged": {"type": "boolean"}, "path": {"type": "string"}}, "required": []}) },
        GitSpec { id: "git.log", description: "Show recent commits (bounded count).", risk: GitRisk::Read, extra_schema: serde_json::json!({"properties": {"limit": {"type": "integer", "minimum": 1, "maximum": 100}}, "required": []}) },
        GitSpec { id: "git.show", description: "Show one commit or revision.", risk: GitRisk::Read, extra_schema: serde_json::json!({"properties": {"revision": {"type": "string"}}, "required": ["revision"]}) },
        GitSpec { id: "git.branch.list", description: "List local branches.", risk: GitRisk::Read, extra_schema: serde_json::json!({}) },
        GitSpec { id: "git.branch.create", description: "Create a branch at HEAD (or a given start point).", risk: GitRisk::Mutate, extra_schema: serde_json::json!({"properties": {"name": {"type": "string"}, "start_point": {"type": "string"}}, "required": ["name"]}) },
        GitSpec { id: "git.checkout", description: "Checkout a branch or revision (no --force; use git.restore to discard changes).", risk: GitRisk::Mutate, extra_schema: serde_json::json!({"properties": {"target": {"type": "string"}}, "required": ["target"]}) },
        GitSpec { id: "git.add", description: "Stage paths (must be inside the repository).", risk: GitRisk::Mutate, extra_schema: serde_json::json!({"properties": {"paths": {"type": "array", "items": {"type": "string"}}}, "required": ["paths"]}) },
        GitSpec { id: "git.commit", description: "Create a commit from staged changes. Never pushes.", risk: GitRisk::Mutate, extra_schema: serde_json::json!({"properties": {"message": {"type": "string"}}, "required": ["message"]}) },
        GitSpec { id: "git.restore", description: "Restore working-tree paths (discards local changes to those paths).", risk: GitRisk::Destructive, extra_schema: serde_json::json!({"properties": {"paths": {"type": "array", "items": {"type": "string"}}}, "required": ["paths"]}) },
        GitSpec { id: "git.remote.list", description: "List remotes with URLs.", risk: GitRisk::Read, extra_schema: serde_json::json!({}) },
        GitSpec { id: "git.fetch", description: "Fetch from a remote (network).", risk: GitRisk::Network, extra_schema: serde_json::json!({"properties": {"remote": {"type": "string"}}, "required": []}) },
        GitSpec { id: "git.pull", description: "Pull the current branch (network, merges remote changes).", risk: GitRisk::Network, extra_schema: serde_json::json!({"properties": {"remote": {"type": "string"}}, "required": []}) },
        GitSpec { id: "git.push", description: "Push the current branch. Separate explicit authorization: creating a commit never implies pushing.", risk: GitRisk::Destructive, extra_schema: serde_json::json!({"properties": {"remote": {"type": "string"}, "force": {"type": "boolean"}}, "required": []}) },
        GitSpec { id: "git.reset", description: "Reset the current branch (destructive; no --hard without explicit user approval routing).", risk: GitRisk::Destructive, extra_schema: serde_json::json!({"properties": {"mode": {"type": "string", "enum": ["soft", "mixed"]}, "target": {"type": "string"}}, "required": []}) },
        GitSpec { id: "git.clean", description: "Remove untracked files (destructive; always dry-run first via git.status).", risk: GitRisk::Destructive, extra_schema: serde_json::json!({"properties": {"paths": {"type": "array", "items": {"type": "string"}}} , "required": []}) },
    ]
}

fn invalid(tool: &str, message: impl Into<String>) -> ToolError {
    ToolError::InvalidArgs {
        tool: tool.to_string(),
        message: message.into(),
    }
}

/// Reject revision/path inputs that could escape into option injection or
/// shell metacharacters. `git` is invoked without a shell, but a leading
/// `-` would still be parsed as a flag.
fn validate_token(tool: &str, field: &str, value: &str) -> Result<(), ToolError> {
    if value.trim().is_empty() {
        return Err(invalid(tool, format!("'{field}' must be non-empty")));
    }
    if value.starts_with('-') {
        return Err(invalid(tool, format!("'{field}' must not start with '-'")));
    }
    if value.contains(['\0', '\n']) {
        return Err(invalid(
            tool,
            format!("'{field}' contains invalid characters"),
        ));
    }
    Ok(())
}

struct GitTool {
    spec_id: &'static str,
}

impl GitTool {
    fn spec(&self) -> GitSpec {
        specs()
            .into_iter()
            .find(|spec| spec.id == self.spec_id)
            .expect("known git tool")
    }
}

fn repo_arg(args: &serde_json::Value, tool: &str) -> Result<PathBuf, ToolError> {
    args.get("repo")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| invalid(tool, "missing non-empty string 'repo' (repository root)"))
}

fn require_ticket(
    tool: &str,
    ctx: &ToolContext,
    capability: Capability,
    resource: Resource,
) -> Result<(), ToolError> {
    if ctx.has_ticket(capability.clone(), resource.clone()) {
        Ok(())
    } else {
        Err(ToolError::structured_with_details(
            tool,
            "permission_required",
            "no capability ticket authorizes this git operation",
            serde_json::json!({ "capability": format!("{capability:?}") }),
        ))
    }
}

#[async_trait::async_trait]
impl Tool for GitTool {
    fn metadata(&self) -> ToolMetadata {
        let spec = self.spec();
        let mut properties = serde_json::json!({ "repo": {"type": "string"} });
        if let Some(extra) = spec.extra_schema.get("properties") {
            for (key, value) in extra.as_object().cloned().unwrap_or_default() {
                properties[key] = value;
            }
        }
        let mut required = vec![serde_json::Value::String("repo".to_string())];
        for item in spec
            .extra_schema
            .get("required")
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default()
        {
            required.push(item);
        }
        let effects = match spec.risk {
            GitRisk::Read => vec![ToolEffect::ReadOnly],
            GitRisk::Mutate => vec![ToolEffect::FilesystemWrite],
            GitRisk::Network => vec![ToolEffect::Network, ToolEffect::FilesystemWrite],
            GitRisk::Destructive => vec![ToolEffect::Destructive],
        };
        ToolMetadata {
            id: capability_core::ToolId::new(spec.id),
            description: spec.description.to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": properties, "required": required,
            }),
            effects,
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        let repo = args.get("repo")?.as_str()?;
        let capability = match self.spec().risk {
            GitRisk::Read => Capability::FilesystemRead,
            GitRisk::Mutate | GitRisk::Network | GitRisk::Destructive => {
                Capability::FilesystemWrite
            }
        };
        Some(CapabilityRequirement {
            capability,
            resource: Resource::Path(PathBuf::from(repo)),
        })
    }

    fn required_capabilities(&self, args: &serde_json::Value) -> Vec<CapabilityRequirement> {
        self.required_capability(args).into_iter().collect()
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let tool = self.spec_id;
        let spec = self.spec();
        let repo = repo_arg(&args, tool)?;
        let (read_cap, write_cap) = (Capability::FilesystemRead, Capability::FilesystemWrite);
        match spec.risk {
            GitRisk::Read => {
                require_ticket(tool, &ctx, read_cap, Resource::Path(repo.clone()))?;
            }
            GitRisk::Mutate | GitRisk::Network | GitRisk::Destructive => {
                require_ticket(tool, &ctx, read_cap, Resource::Path(repo.clone()))?;
                require_ticket(tool, &ctx, write_cap, Resource::Path(repo.clone()))?;
            }
        }
        // Network operations additionally require a ticket for the remote.
        if matches!(spec.risk, GitRisk::Network | GitRisk::Destructive)
            && matches!(tool, "git.fetch" | "git.pull" | "git.push")
        {
            let remote = args
                .get("remote")
                .and_then(|value| value.as_str())
                .unwrap_or("origin");
            validate_token(tool, "remote", remote)?;
            let remote_url = remote_url(&repo, remote).await?;
            let destination = host_port_of(&remote_url).ok_or_else(|| {
                ToolError::structured(
                    tool,
                    "invalid_target",
                    format!("cannot parse remote URL for '{remote}'"),
                )
            })?;
            require_ticket(
                tool,
                &ctx,
                Capability::NetworkConnect,
                Resource::HostPort {
                    host: destination.0,
                    port: destination.1,
                },
            )?;
        }
        let argv = build_argv(tool, &args)?;
        let output = run_git(&repo, &argv).await?;
        Ok(ToolOutput::json(serde_json::json!({
            "tool": tool,
            "repo": repo.to_string_lossy(),
            "argv": argv,
            "success": output.0,
            "stdout": output.1,
            "stderr": output.2,
        })))
    }
}

fn build_argv(tool: &str, args: &serde_json::Value) -> Result<Vec<String>, ToolError> {
    let str_field = |field: &str| {
        args.get(field)
            .and_then(|value| value.as_str())
            .map(|value| {
                validate_token(tool, field, value)?;
                Ok::<_, ToolError>(value.to_string())
            })
            .transpose()
    };
    let argv = match tool {
        "git.status" => vec![
            "status".to_string(),
            "--short".to_string(),
            "--branch".to_string(),
        ],
        "git.diff" => {
            let mut argv = vec!["diff".to_string()];
            if args
                .get("staged")
                .and_then(|value| value.as_bool())
                .unwrap_or(false)
            {
                argv.push("--staged".to_string());
            }
            argv.push("--no-color".to_string());
            if let Some(path) = str_field("path")? {
                argv.push("--".to_string());
                argv.push(path);
            }
            argv
        }
        "git.log" => {
            let limit = args
                .get("limit")
                .and_then(|value| value.as_u64())
                .unwrap_or(20)
                .clamp(1, 100);
            vec![
                "log".to_string(),
                format!("-{limit}"),
                "--pretty=format:%H%x00%an%x00%ad%x00%s".to_string(),
                "--no-color".to_string(),
            ]
        }
        "git.show" => {
            let revision =
                str_field("revision")?.ok_or_else(|| invalid(tool, "missing string 'revision'"))?;
            vec![
                "show".to_string(),
                "--no-color".to_string(),
                "--stat".to_string(),
                revision,
            ]
        }
        "git.branch.list" => vec![
            "branch".to_string(),
            "--list".to_string(),
            "--no-color".to_string(),
        ],
        "git.branch.create" => {
            let name = str_field("name")?.ok_or_else(|| invalid(tool, "missing string 'name'"))?;
            let mut argv = vec!["branch".to_string(), name];
            if let Some(start) = str_field("start_point")? {
                argv.push(start);
            }
            argv
        }
        "git.checkout" => {
            let target =
                str_field("target")?.ok_or_else(|| invalid(tool, "missing string 'target'"))?;
            vec!["checkout".to_string(), "--".to_string(), target]
        }
        "git.add" => {
            let paths = paths_field(tool, args, "paths")?;
            let mut argv = vec!["add".to_string(), "--".to_string()];
            argv.extend(paths);
            argv
        }
        "git.commit" => {
            let message =
                str_field("message")?.ok_or_else(|| invalid(tool, "missing string 'message'"))?;
            if message.len() > 4 * 1024 {
                return Err(invalid(tool, "commit message is too long"));
            }
            vec!["commit".to_string(), "-m".to_string(), message]
        }
        "git.restore" => {
            let paths = paths_field(tool, args, "paths")?;
            let mut argv = vec!["restore".to_string(), "--".to_string()];
            argv.extend(paths);
            argv
        }
        "git.remote.list" => vec!["remote".to_string(), "-v".to_string()],
        "git.fetch" => {
            let remote = str_field("remote")?.unwrap_or_else(|| "origin".to_string());
            vec!["fetch".to_string(), remote]
        }
        "git.pull" => {
            let remote = str_field("remote")?.unwrap_or_else(|| "origin".to_string());
            vec!["pull".to_string(), "--ff-only".to_string(), remote]
        }
        "git.push" => {
            let remote = str_field("remote")?.unwrap_or_else(|| "origin".to_string());
            let mut argv = vec!["push".to_string(), remote];
            if args
                .get("force")
                .and_then(|value| value.as_bool())
                .unwrap_or(false)
            {
                argv.push("--force-with-lease".to_string());
            }
            argv
        }
        "git.reset" => {
            let mode = str_field("mode")?.unwrap_or_else(|| "mixed".to_string());
            if mode != "soft" && mode != "mixed" {
                return Err(invalid(
                    tool,
                    "'mode' must be 'soft' or 'mixed' (--hard is not offered)",
                ));
            }
            let mut argv = vec!["reset".to_string(), format!("--{mode}")];
            if let Some(target) = str_field("target")? {
                argv.push(target);
            }
            argv
        }
        "git.clean" => {
            let mut argv = vec![
                "clean".to_string(),
                "-f".to_string(),
                "-d".to_string(),
                "--".to_string(),
            ];
            if let Some(paths) = args.get("paths").and_then(|value| value.as_array()) {
                for path in paths {
                    let path = path
                        .as_str()
                        .ok_or_else(|| invalid(tool, "'paths' must be strings"))?;
                    validate_token(tool, "paths", path)?;
                    argv.push(path.to_string());
                }
            }
            argv
        }
        _ => return Err(invalid(tool, "unknown git tool")),
    };
    Ok(argv)
}

fn paths_field(
    tool: &str,
    args: &serde_json::Value,
    field: &str,
) -> Result<Vec<String>, ToolError> {
    let paths = args
        .get(field)
        .and_then(|value| value.as_array())
        .ok_or_else(|| invalid(tool, format!("missing array '{field}'")))?;
    if paths.is_empty() || paths.len() > 1_000 {
        return Err(invalid(
            tool,
            format!("'{field}' must be a non-empty bounded array"),
        ));
    }
    paths
        .iter()
        .map(|path| {
            let path = path
                .as_str()
                .ok_or_else(|| invalid(tool, format!("'{field}' must be strings")))?;
            validate_token(tool, field, path)?;
            if path == "." || path.starts_with("../") || path.contains("/../") {
                return Err(invalid(
                    tool,
                    format!("'{field}' must stay inside the repository"),
                ));
            }
            Ok(path.to_string())
        })
        .collect()
}

async fn remote_url(repo: &Path, remote: &str) -> Result<String, ToolError> {
    let (success, stdout, stderr) = run_git(
        repo,
        &[
            "remote".to_string(),
            "get-url".to_string(),
            remote.to_string(),
        ],
    )
    .await?;
    if !success {
        return Err(ToolError::structured(
            "git.fetch",
            "invalid_target",
            format!("unknown remote '{remote}': {stderr}"),
        ));
    }
    Ok(stdout.trim().to_string())
}

fn host_port_of(remote_url: &str) -> Option<(String, u16)> {
    // scp-like syntax: git@github.com:org/repo.git
    if let Some(after_at) = remote_url.split('@').next_back().filter(|_| {
        remote_url.contains('@') && remote_url.contains(':') && !remote_url.contains("://")
    }) {
        let host = after_at.split(':').next()?.to_string();
        return Some((host, 22));
    }
    let parsed = url::Url::parse(remote_url).ok()?;
    let host = parsed.host_str()?.to_string();
    let port = parsed.port_or_known_default().unwrap_or(22);
    Some((host, port))
}

async fn run_git(repo: &Path, argv: &[String]) -> Result<(bool, String, String), ToolError> {
    let output = tokio::time::timeout(
        GIT_TIMEOUT,
        tokio::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(argv)
            .output(),
    )
    .await
    .map_err(|_| ToolError::structured("git.status", "action_failed", "git timed out"))?
    .map_err(|error| {
        ToolError::structured(
            "git.status",
            "backend_unavailable",
            format!("cannot run git: {error}"),
        )
    })?;
    let truncate = |bytes: &[u8]| {
        let mut text = String::from_utf8_lossy(bytes).into_owned();
        if text.len() > MAX_GIT_OUTPUT {
            text.truncate(MAX_GIT_OUTPUT);
            text.push_str("…[truncated]");
        }
        text
    };
    Ok((
        output.status.success(),
        truncate(&output.stdout),
        truncate(&output.stderr),
    ))
}

/// Static git tool group.
pub struct GitToolPack;

impl tool_sdk::ToolPack for GitToolPack {
    fn id(&self) -> &'static str {
        "git"
    }

    fn tools(&self, _ctx: &tool_sdk::ToolLoadContext) -> Vec<Arc<dyn Tool>> {
        specs()
            .iter()
            .map(|spec| Arc::new(GitTool { spec_id: spec.id }) as Arc<dyn Tool>)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tool_sdk::ToolPack as _;

    #[test]
    fn pack_registers_read_and_high_risk_tools() {
        let tools = GitToolPack.tools(&tool_sdk::ToolLoadContext::default());
        let ids = tools
            .iter()
            .map(|tool| tool.metadata().id.0.clone())
            .collect::<Vec<_>>();
        for expected in [
            "git.status",
            "git.diff",
            "git.log",
            "git.push",
            "git.reset",
            "git.clean",
            "git.remote.list",
        ] {
            assert!(ids.contains(&expected.to_string()), "{ids:?}");
        }
        let push = tools
            .iter()
            .find(|tool| tool.metadata().id.0 == "git.push")
            .unwrap();
        assert!(push.metadata().effects.contains(&ToolEffect::Destructive));
        let status = tools
            .iter()
            .find(|tool| tool.metadata().id.0 == "git.status")
            .unwrap();
        assert_eq!(status.metadata().effects, vec![ToolEffect::ReadOnly]);
    }

    #[test]
    fn option_injection_is_rejected() {
        assert!(validate_token("git.show", "revision", "--help").is_err());
        assert!(validate_token("git.show", "revision", "HEAD").is_ok());
        assert!(validate_token("git.add", "paths", "").is_err());
        assert!(build_argv("git.reset", &serde_json::json!({"mode": "hard"})).is_err());
        // push --force degrades to --force-with-lease, never bare --force.
        let argv = build_argv("git.push", &serde_json::json!({"force": true})).unwrap();
        assert!(argv.contains(&"--force-with-lease".to_string()));
        assert!(!argv.contains(&"--force".to_string()));
    }

    #[test]
    fn scp_and_https_remote_hosts_parse() {
        assert_eq!(
            host_port_of("git@github.com:org/repo.git"),
            Some(("github.com".to_string(), 22))
        );
        assert_eq!(
            host_port_of("https://github.com/org/repo.git"),
            Some(("github.com".to_string(), 443))
        );
    }

    #[tokio::test]
    async fn status_runs_against_a_real_repo() {
        let dir = std::env::temp_dir().join(format!("utsuwa-git-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let (success, _, _) = run_git(&dir, &["init".to_string()]).await.unwrap();
        assert!(success);
        std::fs::write(dir.join("file.txt"), "hi").unwrap();
        let (success, stdout, _) = run_git(&dir, &["status".to_string(), "--short".to_string()])
            .await
            .unwrap();
        assert!(success);
        assert!(stdout.contains("file.txt"), "{stdout}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
