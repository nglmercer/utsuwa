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
        GitSpec { id: "git.checkout", description: "Switch to a branch (git switch) or detach at a tag/commit SHA (git checkout <revision>). File paths are rejected: a target that is not a branch, tag, or commit fails instead of restoring a same-named file. No --force.", risk: GitRisk::Mutate, extra_schema: serde_json::json!({"properties": {"target": {"type": "string"}}, "required": ["target"]}) },
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
            serde_json::json!({
                "capability": format!("{capability:?}"),
                "resource": format!("{resource:?}"),
            }),
        ))
    }
}

/// Require a `NetworkConnect` ticket for one remote host/port. The remote
/// is resolved from the repository's own config at invoke time, so the
/// agent preflight cannot declare it statically: this error is the
/// structured retriable result (`permission_required` + host/port) the
/// agent uses to authorize and retry. A ticket for one remote never
/// covers another.
fn require_remote_ticket(
    tool: &str,
    ctx: &ToolContext,
    host: &str,
    port: u16,
) -> Result<(), ToolError> {
    if ctx.has_ticket(
        Capability::NetworkConnect,
        Resource::HostPort {
            host: host.to_string(),
            port,
        },
    ) {
        Ok(())
    } else {
        Err(ToolError::structured_with_details(
            tool,
            "permission_required",
            format!("git remote '{host}:{port}' needs its own NetworkConnect ticket"),
            serde_json::json!({
                "capability": "NetworkConnect",
                "host": host,
                "port": port,
                "retry_after_authorization": true,
            }),
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

    /// Every statically knowable ticket `invoke` enforces. Reads declare
    /// the repository read; mutations declare read *and* write (the agent
    /// loop mints both before execution). Network remotes are resolved
    /// from repository config at invoke time and enforced there with a
    /// retriable `permission_required` carrying the exact host/port — see
    /// [`require_remote_ticket`].
    fn required_capabilities(&self, args: &serde_json::Value) -> Vec<CapabilityRequirement> {
        let Some(repo) = args.get("repo").and_then(|value| value.as_str()) else {
            return self.required_capability(args).into_iter().collect();
        };
        match self.spec().risk {
            GitRisk::Read => vec![CapabilityRequirement {
                capability: Capability::FilesystemRead,
                resource: Resource::Path(PathBuf::from(repo)),
            }],
            GitRisk::Mutate | GitRisk::Network | GitRisk::Destructive => vec![
                CapabilityRequirement {
                    capability: Capability::FilesystemRead,
                    resource: Resource::Path(PathBuf::from(repo)),
                },
                CapabilityRequirement {
                    capability: Capability::FilesystemWrite,
                    resource: Resource::Path(PathBuf::from(repo)),
                },
            ],
        }
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
            require_remote_ticket(tool, &ctx, &destination.0, destination.1)?;
        }
        let argv = if tool == "git.checkout" {
            build_checkout_argv(&repo, &args).await?
        } else {
            build_argv(tool, &args)?
        };
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
            // Unreachable: `invoke` routes checkout through
            // `build_checkout_argv`, which resolves branch-vs-revision
            // without the `--` pathspec boundary. Kept out of the argv
            // table so no caller can accidentally restore a same-named
            // file path instead of switching branches.
            return Err(invalid(tool, "use the checkout path, not the argv table"));
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

/// Build a safe checkout argv without the `--` pathspec boundary.
/// Resolution order is deliberate:
/// 1. `refs/heads/<target>` exists → `git switch <target>` (branch).
/// 2. `<target>` is otherwise rev-parseable (tag, SHA, `HEAD~2`, …) →
///    `git checkout <target>` (detached revision).
/// 3. Anything else — including file paths — is `invalid_target`: with no
///    `--` in the final argv, git would otherwise silently restore a
///    same-named working-tree file instead of switching anything.
async fn build_checkout_argv(
    repo: &Path,
    args: &serde_json::Value,
) -> Result<Vec<String>, ToolError> {
    let tool = "git.checkout";
    let target = args
        .get("target")
        .and_then(|value| value.as_str())
        .map(|value| {
            validate_token(tool, "target", value)?;
            Ok::<_, ToolError>(value.to_string())
        })
        .transpose()?
        .ok_or_else(|| invalid(tool, "missing string 'target'"))?;
    if ref_exists(repo, &format!("refs/heads/{target}")).await {
        return Ok(vec!["switch".to_string(), target]);
    }
    if ref_exists(repo, &target).await {
        return Ok(vec!["checkout".to_string(), target]);
    }
    Err(ToolError::structured_with_details(
        tool,
        "invalid_target",
        format!("'{target}' is not a branch, tag, or commit in this repository"),
        serde_json::json!({ "target": target }),
    ))
}

/// Quiet existence probe: success flag only, never parsed output.
async fn ref_exists(repo: &Path, reference: &str) -> bool {
    let argv = vec![
        "rev-parse".to_string(),
        "--verify".to_string(),
        "--quiet".to_string(),
        reference.to_string(),
    ];
    run_git(repo, &argv)
        .await
        .map(|output| output.0)
        .unwrap_or(false)
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

    // --- Capability matrix + checkout regression tests --------------------

    fn tool_by_id(id: &str) -> Arc<dyn Tool> {
        GitToolPack
            .tools(&tool_sdk::ToolLoadContext::default())
            .into_iter()
            .find(|tool| tool.metadata().id.0 == id)
            .unwrap()
    }

    fn ctx_with(caps: &[(Capability, Resource)]) -> ToolContext {
        let ctx = ToolContext::new(capability_core::Principal::Agent(
            capability_core::AgentId::new("test"),
        ));
        let mut ctx = ctx;
        for (capability, resource) in caps {
            let ticket = capability_core::CapabilityTicket::mint(
                ctx.principal.clone(),
                capability.clone(),
                capability_core::ResourceScope::new(vec![resource.clone()]),
                ctx.invocation_id,
                std::time::Duration::from_secs(120),
            );
            ctx = ctx.with_ticket(ticket);
        }
        ctx
    }

    fn repo_args(dir: &Path, extra: serde_json::Value) -> serde_json::Value {
        let mut object = extra.as_object().cloned().unwrap_or_default();
        object.insert(
            "repo".to_string(),
            serde_json::Value::String(dir.to_string_lossy().into_owned()),
        );
        serde_json::Value::Object(object)
    }

    async fn init_repo_with_commit() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("utsuwa-git-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        for argv in [
            vec!["init"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "test"],
            vec!["config", "commit.gpgsign", "false"],
        ] {
            let args = argv.into_iter().map(str::to_string).collect::<Vec<_>>();
            assert!(run_git(&dir, &args).await.unwrap().0, "{args:?}");
        }
        std::fs::write(dir.join("file.txt"), "v1").unwrap();
        assert!(
            run_git(&dir, &[("add".to_string()), ("file.txt".to_string())])
                .await
                .unwrap()
                .0
        );
        assert!(
            run_git(
                &dir,
                &[
                    ("commit".to_string()),
                    ("-m".to_string()),
                    ("init".to_string())
                ]
            )
            .await
            .unwrap()
            .0
        );
        dir
    }

    fn head_ref(dir: &Path) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["symbolic-ref", "--quiet", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn head_sha(dir: &Path) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// Declared preflight must match invoke enforcement for every tool:
    /// reads need read, everything else needs read + write.
    #[test]
    fn capability_matrix_declares_every_enforced_ticket() {
        for read_only in [
            "git.status",
            "git.diff",
            "git.log",
            "git.show",
            "git.branch.list",
            "git.remote.list",
        ] {
            let declared = tool_by_id(read_only)
                .required_capabilities(&serde_json::json!({"repo": "/work", "revision": "HEAD"}));
            assert_eq!(declared.len(), 1, "{read_only}");
            assert_eq!(declared[0].capability, Capability::FilesystemRead);
        }
        for mutation in [
            "git.branch.create",
            "git.checkout",
            "git.add",
            "git.commit",
            "git.restore",
            "git.fetch",
            "git.pull",
            "git.push",
            "git.reset",
            "git.clean",
        ] {
            let mut args = serde_json::json!({"repo": "/work"});
            if mutation == "git.checkout" {
                args["target"] = serde_json::json!("main");
            }
            let declared = tool_by_id(mutation).required_capabilities(&args);
            assert_eq!(declared.len(), 2, "{mutation}");
            assert!(declared
                .iter()
                .any(|requirement| { requirement.capability == Capability::FilesystemRead }));
            assert!(declared
                .iter()
                .any(|requirement| { requirement.capability == Capability::FilesystemWrite }));
        }
    }

    #[tokio::test]
    async fn mutation_without_write_ticket_is_denied_before_git_runs() {
        let dir = init_repo_with_commit().await;
        let commit = tool_by_id("git.commit");
        let args = repo_args(&dir, serde_json::json!({"message": "x"}));
        // Read-only context: the write half of the declared pair denies.
        let read_only = ctx_with(&[(Capability::FilesystemRead, Resource::Path(dir.clone()))]);
        let err = commit.invoke(read_only, args.clone()).await.unwrap_err();
        assert_eq!(err.code(), Some("permission_required"), "{err:?}");
        // No ticket at all: the read half denies first.
        let none = ctx_with(&[]);
        let err = commit.invoke(none, args).await.unwrap_err();
        assert_eq!(err.code(), Some("permission_required"), "{err:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn checkout_switches_branches_and_detaches_at_revisions() {
        let dir = init_repo_with_commit().await;
        let checkout = tool_by_id("git.checkout");
        let rw = || {
            ctx_with(&[
                (Capability::FilesystemRead, Resource::Path(dir.clone())),
                (Capability::FilesystemWrite, Resource::Path(dir.clone())),
            ])
        };
        let first_sha = head_sha(&dir);

        // Create a branch through the tool, then switch to it: HEAD must
        // track the new branch (no `--` pathspec in the executed argv).
        let create = tool_by_id("git.branch.create");
        let out = create
            .invoke(
                rw(),
                repo_args(&dir, serde_json::json!({"name": "feature"})),
            )
            .await
            .unwrap();
        assert!(out.content["success"].as_bool().unwrap(), "{out:?}");
        let out = checkout
            .invoke(
                rw(),
                repo_args(&dir, serde_json::json!({"target": "feature"})),
            )
            .await
            .unwrap();
        assert!(out.content["success"].as_bool().unwrap(), "{out:?}");
        assert!(!out.content["argv"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("--")));
        assert_eq!(head_ref(&dir), "refs/heads/feature");

        // Detach at the earlier commit SHA: HEAD equals that SHA.
        let out = checkout
            .invoke(
                rw(),
                repo_args(&dir, serde_json::json!({"target": first_sha})),
            )
            .await
            .unwrap();
        assert!(out.content["success"].as_bool().unwrap(), "{out:?}");
        assert_eq!(head_sha(&dir), first_sha);

        // A file path that is not a revision is rejected — never restored.
        std::fs::write(dir.join("file.txt"), "dirty").unwrap();
        let err = checkout
            .invoke(
                rw(),
                repo_args(&dir, serde_json::json!({"target": "file.txt"})),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some("invalid_target"), "{err:?}");
        assert_eq!(std::fs::read(dir.join("file.txt")).unwrap(), b"dirty");

        // Option-shaped targets are rejected before git runs.
        let err = checkout
            .invoke(
                rw(),
                repo_args(&dir, serde_json::json!({"target": "--force"})),
            )
            .await
            .unwrap_err();
        assert!(err.code().is_none(), "{err:?}");

        // Unknown revisions fail honestly.
        let err = checkout
            .invoke(
                rw(),
                repo_args(&dir, serde_json::json!({"target": "no-such-branch-xyz"})),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some("invalid_target"), "{err:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn checkout_conflicts_surface_honestly_without_force() {
        let dir = init_repo_with_commit().await;
        let rw = || {
            ctx_with(&[
                (Capability::FilesystemRead, Resource::Path(dir.clone())),
                (Capability::FilesystemWrite, Resource::Path(dir.clone())),
            ])
        };
        let checkout = tool_by_id("git.checkout");
        // Record the starting branch, diverge a second branch, then dirty
        // the tree: switching back must refuse (not force) and report
        // the conflict honestly.
        let first_branch = {
            let (success, stdout, _) =
                run_git(&dir, &["branch".to_string(), "--show-current".to_string()])
                    .await
                    .unwrap();
            assert!(success);
            stdout.trim().to_string()
        };
        assert!(
            run_git(
                &dir,
                &[
                    "checkout".to_string(),
                    "-b".to_string(),
                    "other".to_string()
                ]
            )
            .await
            .unwrap()
            .0
        );
        std::fs::write(dir.join("file.txt"), "v2").unwrap();
        assert!(
            run_git(
                &dir,
                &["commit".to_string(), "-am".to_string(), "v2".to_string()]
            )
            .await
            .unwrap()
            .0
        );
        std::fs::write(dir.join("file.txt"), "dirty").unwrap();
        let out = checkout
            .invoke(
                rw(),
                repo_args(&dir, serde_json::json!({"target": first_branch})),
            )
            .await
            .unwrap();
        assert!(!out.content["argv"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("--force")));
        assert!(
            !out.content["success"].as_bool().unwrap(),
            "conflicting checkout must fail, not force: {out:?}"
        );
        assert_eq!(std::fs::read(dir.join("file.txt")).unwrap(), b"dirty");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn fetch_needs_a_ticket_for_the_resolved_remote() {
        let dir = init_repo_with_commit().await;
        assert!(
            run_git(
                &dir,
                &[
                    "remote".to_string(),
                    "add".to_string(),
                    "origin".to_string(),
                    "https://127.0.0.1:9/repo.git".to_string(),
                ],
            )
            .await
            .unwrap()
            .0
        );
        let fetch = tool_by_id("git.fetch");
        let args = repo_args(&dir, serde_json::json!({}));
        let rw = vec![
            (Capability::FilesystemRead, Resource::Path(dir.clone())),
            (Capability::FilesystemWrite, Resource::Path(dir.clone())),
        ];
        // Declared preflight covers the filesystem pair only…
        assert_eq!(fetch.required_capabilities(&args).len(), 2);
        // …so the remote ticket gates at invoke with a retriable error
        // naming the exact host/port.
        let err = fetch.invoke(ctx_with(&rw), args.clone()).await.unwrap_err();
        assert_eq!(err.code(), Some("permission_required"), "{err:?}");
        let details = err.model_message();
        assert!(details.contains("127.0.0.1"), "{details}");
        // A ticket for a different remote does not authorize this one.
        let mut wrong = rw.clone();
        wrong.push((
            Capability::NetworkConnect,
            Resource::HostPort {
                host: "github.com".to_string(),
                port: 443,
            },
        ));
        let err = fetch
            .invoke(ctx_with(&wrong), args.clone())
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some("permission_required"), "{err:?}");
        // The exact remote ticket lets git run (which then fails honestly
        // against the dead endpoint — no network in tests).
        let mut right = rw.clone();
        // Explicit ports are preserved: the URL names port 9, so the
        // port-443 ticket must NOT authorize it (proves exact matching).
        right.push((
            Capability::NetworkConnect,
            Resource::HostPort {
                host: "127.0.0.1".to_string(),
                port: 9,
            },
        ));
        let out = fetch.invoke(ctx_with(&right), args).await.unwrap();
        assert!(!out.content["success"].as_bool().unwrap(), "{out:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn scp_like_remote_resolves_to_ssh_port() {
        let dir = init_repo_with_commit().await;
        assert!(
            run_git(
                &dir,
                &[
                    "remote".to_string(),
                    "add".to_string(),
                    "origin".to_string(),
                    "git@github.com:org/repo.git".to_string(),
                ],
            )
            .await
            .unwrap()
            .0
        );
        let fetch = tool_by_id("git.fetch");
        let args = repo_args(&dir, serde_json::json!({}));
        let rw = vec![
            (Capability::FilesystemRead, Resource::Path(dir.clone())),
            (Capability::FilesystemWrite, Resource::Path(dir.clone())),
        ];
        let err = fetch.invoke(ctx_with(&rw), args).await.unwrap_err();
        assert_eq!(err.code(), Some("permission_required"), "{err:?}");
        let details = err.model_message();
        assert!(details.contains("github.com"), "{details}");
        assert!(details.contains("22"), "{details}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ssh_remote_url_defaults_to_port_22() {
        assert_eq!(
            host_port_of("ssh://git@github.com/org/repo.git"),
            Some(("github.com".to_string(), 22))
        );
    }
}
