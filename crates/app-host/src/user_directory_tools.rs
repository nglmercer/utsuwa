//! Agent tools for operating-system configured user directories.
//!
//! These tools keep directory-name interpretation in the native host. The
//! model supplies a stable enum such as `desktop`; the host resolves it to the
//! configured absolute path and delegates the actual write to the existing
//! filesystem broker.

use crate::host_environment::{HostEnvironment, UserDirectory};
use std::path::{Component, Path, PathBuf};
use tool_core::{CapabilityRequirement, Tool, ToolContext, ToolError, ToolMetadata, ToolOutput};
use tool_filesystem::WriteTool;

const USER_DIRECTORY_ENUM: [&str; 8] = [
    "desktop",
    "documents",
    "downloads",
    "pictures",
    "music",
    "videos",
    "public_share",
    "templates",
];

fn invalid_args(tool: &str, message: impl Into<String>) -> ToolError {
    ToolError::InvalidArgs {
        tool: tool.to_string(),
        message: message.into(),
    }
}

fn failed(tool: &str, message: impl Into<String>) -> ToolError {
    ToolError::Failed {
        tool: tool.to_string(),
        message: message.into(),
    }
}

fn parse_directory(args: &serde_json::Value, tool: &str) -> Result<UserDirectory, ToolError> {
    let object = args
        .as_object()
        .ok_or_else(|| invalid_args(tool, "args must be a JSON object"))?;
    let value = object
        .get("directory")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid_args(tool, "missing string 'directory' argument"))?;
    match value {
        "desktop" => Ok(UserDirectory::Desktop),
        "documents" => Ok(UserDirectory::Documents),
        "downloads" => Ok(UserDirectory::Downloads),
        "pictures" => Ok(UserDirectory::Pictures),
        "music" => Ok(UserDirectory::Music),
        "videos" => Ok(UserDirectory::Videos),
        "public_share" => Ok(UserDirectory::PublicShare),
        "templates" => Ok(UserDirectory::Templates),
        _ => Err(invalid_args(
            tool,
            format!(
                "'directory' must be one of: {}",
                USER_DIRECTORY_ENUM.join(", ")
            ),
        )),
    }
}

fn reject_unsafe_relative_path(relative_path: &Path) -> bool {
    relative_path.components().any(|component| {
        matches!(
            component,
            Component::Prefix(_) | Component::RootDir | Component::ParentDir
        )
    }) || relative_path.file_name().is_none()
}

struct ResolvedUserFile {
    directory: UserDirectory,
    path: PathBuf,
    write_args: serde_json::Value,
}

fn resolve_user_file_args(
    args: &serde_json::Value,
    environment: &HostEnvironment,
    tool: &str,
) -> Result<ResolvedUserFile, ToolError> {
    let object = args
        .as_object()
        .ok_or_else(|| invalid_args(tool, "args must be a JSON object"))?;
    let directory = parse_directory(args, tool)?;
    let relative = object
        .get("relative_path")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid_args(tool, "missing string 'relative_path' argument"))?;
    if relative.is_empty() {
        return Err(invalid_args(tool, "'relative_path' must name a file"));
    }
    let relative_path = Path::new(relative);
    if reject_unsafe_relative_path(relative_path) {
        return Err(invalid_args(
            tool,
            "'relative_path' must be a non-empty path below the selected user directory",
        ));
    }
    let base = environment.user_dirs.get(directory).ok_or_else(|| {
        failed(
            tool,
            format!(
                "the host-configured {} directory is not available",
                directory.prompt_label()
            ),
        )
    })?;
    let path = base.join(relative_path);

    let mut write_args = serde_json::Value::Object(object.clone());
    let write_object = write_args
        .as_object_mut()
        .expect("write args cloned from a JSON object");
    write_object.remove("directory");
    write_object.remove("relative_path");
    write_object.insert(
        "path".to_string(),
        serde_json::Value::String(path.to_string_lossy().into_owned()),
    );

    Ok(ResolvedUserFile {
        directory,
        path,
        write_args,
    })
}

/// The raw path write tool, with host-aware recovery guidance for the common
/// stale `~/Desktop` mistake. Its actual invocation remains the filesystem
/// broker's normal `WriteTool` implementation.
pub(crate) struct HostAwareWriteTool {
    inner: WriteTool,
}

impl HostAwareWriteTool {
    pub(crate) fn new(limits: tool_filesystem::FilesystemLimits) -> Self {
        Self {
            inner: WriteTool { limits },
        }
    }
}

pub(crate) fn enrich_write_error(error: ToolError, environment: &HostEnvironment) -> ToolError {
    let ToolError::Failed { tool, message } = error else {
        return error;
    };
    let Some(missing_parent) = message.strip_prefix("parent directory does not exist: ") else {
        return ToolError::Failed { tool, message };
    };
    let Some(home) = environment.home.as_deref() else {
        return ToolError::Failed { tool, message };
    };
    let missing_parent = Path::new(missing_parent);
    let suggestion = UserDirectory::ALL.into_iter().find_map(|directory| {
        let configured = environment.user_dirs.get(directory)?;
        let conventional = home.join(directory.conventional_name());
        (missing_parent == conventional && configured != conventional)
            .then(|| (directory, configured.to_path_buf()))
    });
    let Some((directory, configured)) = suggestion else {
        return ToolError::Failed { tool, message };
    };
    ToolError::Failed {
        tool,
        message: format!(
            "{message}. The host-configured {} directory is {}. Use filesystem.write_user_file with directory='{}', or retry using that exact resolved path",
            directory.prompt_label(),
            configured.display(),
            directory.json_key(),
        ),
    }
}

#[async_trait::async_trait]
impl Tool for HostAwareWriteTool {
    fn metadata(&self) -> ToolMetadata {
        let mut metadata = self.inner.metadata();
        metadata.description.push_str(
            " Prefer filesystem.write_user_file for OS-configured Desktop, Documents, Downloads, Pictures, Music, Videos, Public, or Templates directories.",
        );
        metadata
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        self.inner.required_capability(args)
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let environment = HostEnvironment::snapshot();
        self.inner
            .invoke(ctx, args)
            .await
            .map_err(|error| enrich_write_error(error, &environment))
    }
}

/// Write inside a validated, OS-configured user directory while preserving
/// the existing filesystem write broker and its exact capability scope.
pub(crate) struct UserDirectoryWriteTool {
    inner: WriteTool,
    /// Tests inject a deterministic environment. Production always snapshots
    /// the native host at authorization and invocation time.
    test_environment: Option<HostEnvironment>,
}

impl UserDirectoryWriteTool {
    pub(crate) fn new(limits: tool_filesystem::FilesystemLimits) -> Self {
        Self {
            inner: WriteTool { limits },
            test_environment: None,
        }
    }

    #[cfg(test)]
    fn with_environment(
        limits: tool_filesystem::FilesystemLimits,
        environment: HostEnvironment,
    ) -> Self {
        Self {
            inner: WriteTool { limits },
            test_environment: Some(environment),
        }
    }

    fn environment(&self) -> HostEnvironment {
        self.test_environment
            .clone()
            .unwrap_or_else(HostEnvironment::snapshot)
    }
}

#[async_trait::async_trait]
impl Tool for UserDirectoryWriteTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("filesystem.write_user_file"),
            description: "Create or overwrite a UTF-8 file inside an operating-system configured user directory such as Desktop or Documents. Prefer this tool whenever the user refers to Desktop, Documents, Downloads, Pictures, Music, Videos, Public, or Templates. The host resolves the directory; do not manually construct those directory paths. The relative path must stay below the selected directory, and parent directories must already exist unless create_parents=true.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "directory": {
                        "type": "string",
                        "enum": USER_DIRECTORY_ENUM,
                        "description": "Operating-system user directory identifier; use desktop, documents, downloads, pictures, music, videos, public_share, or templates."
                    },
                    "relative_path": {
                        "type": "string",
                        "description": "File path relative to the selected configured user directory; never provide an absolute path or .. components."
                    },
                    "content": {
                        "type": "string",
                        "description": "UTF-8 file contents."
                    },
                    "create_parents": {
                        "type": "boolean",
                        "default": false,
                        "description": "Explicitly create missing parent directories recursively. Omit or set false to require existing parents."
                    }
                },
                "required": ["directory", "relative_path", "content"]
            }),
            effects: vec![tool_core::ToolEffect::FilesystemWrite],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        let environment = self.environment();
        let resolved =
            resolve_user_file_args(args, &environment, "filesystem.write_user_file").ok()?;
        // The inner tool computes the same canonical/lexical resource that it
        // will validate in `invoke`; no capability is minted for the symbolic
        // directory name supplied by the model.
        self.inner.required_capability(&resolved.write_args)
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let environment = self.environment();
        let resolved = resolve_user_file_args(&args, &environment, "filesystem.write_user_file")?;
        let directory = resolved.directory.json_key();
        let resolved_path = resolved.path.clone();
        let mut output = self.inner.invoke(ctx, resolved.write_args).await?;
        if let Some(object) = output.content.as_object_mut() {
            object.insert(
                "directory".to_string(),
                serde_json::Value::String(directory.to_string()),
            );
            object.insert(
                "resolved_directory".to_string(),
                serde_json::Value::String(
                    environment
                        .user_dirs
                        .get(resolved.directory)
                        .map(|path| path.to_string_lossy().into_owned())
                        .unwrap_or_else(|| {
                            resolved_path
                                .parent()
                                .map(|path| path.to_string_lossy().into_owned())
                                .unwrap_or_default()
                        }),
                ),
            );
        }
        Ok(output)
    }
}

/// Resolve one validated special directory without exposing an OS mutation
/// API or requiring a capability ticket.
pub(crate) struct ResolveUserDirectoryTool {
    test_environment: Option<HostEnvironment>,
}

impl ResolveUserDirectoryTool {
    pub(crate) fn new() -> Self {
        Self {
            test_environment: None,
        }
    }

    #[cfg(test)]
    fn with_environment(environment: HostEnvironment) -> Self {
        Self {
            test_environment: Some(environment),
        }
    }

    fn environment(&self) -> HostEnvironment {
        self.test_environment
            .clone()
            .unwrap_or_else(HostEnvironment::snapshot)
    }
}

#[async_trait::async_trait]
impl Tool for ResolveUserDirectoryTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("filesystem.resolve_user_dir"),
            description: "Resolve an operating-system configured user directory to its exact native path. Use this read-only lookup instead of guessing or translating Desktop, Documents, Downloads, Pictures, Music, Public, or Templates paths.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "directory": {
                        "type": "string",
                        "enum": USER_DIRECTORY_ENUM,
                        "description": "User directory identifier to resolve."
                    }
                },
                "required": ["directory"]
            }),
            effects: vec![tool_core::ToolEffect::ReadOnly],
        }
    }

    async fn invoke(
        &self,
        _ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let directory = parse_directory(&args, "filesystem.resolve_user_dir")?;
        let environment = self.environment();
        let path = environment.user_dirs.get(directory).ok_or_else(|| {
            failed(
                "filesystem.resolve_user_dir",
                format!(
                    "the host-configured {} directory is not available",
                    directory.prompt_label()
                ),
            )
        })?;
        Ok(ToolOutput::new(serde_json::json!({
            "directory": directory.json_key(),
            "path": path.to_string_lossy(),
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use capability_core::{
        AgentId, Capability, CapabilityTicket, InvocationId, Principal, Resource, ResourceScope,
    };
    use std::time::Duration;

    fn environment(home: &Path, desktop: &Path) -> HostEnvironment {
        HostEnvironment {
            os: "linux",
            architecture: "x86_64",
            home: Some(home.to_path_buf()),
            cwd: Some(home.to_path_buf()),
            user_dirs: crate::host_environment::UserDirectories {
                desktop: Some(desktop.to_path_buf()),
                ..Default::default()
            },
            xdg_config_source: Some(home.join(".config/user-dirs.dirs")),
            path_style: "POSIX",
            path_separator: "/",
        }
    }

    fn ticketed_context(path: &Path) -> ToolContext {
        let principal = Principal::Agent(AgentId::new("user-directory-test"));
        let invocation = InvocationId::fresh();
        let ticket = CapabilityTicket::mint(
            principal.clone(),
            Capability::FilesystemWrite,
            ResourceScope::new(vec![Resource::Path(path.to_path_buf())]),
            invocation,
            Duration::from_secs(60),
        );
        ToolContext {
            principal,
            invocation_id: invocation,
            ticket: Some(ticket),
        }
    }

    #[tokio::test]
    async fn write_user_file_resolves_localized_desktop_without_creating_wrong_path() {
        let home =
            std::env::temp_dir().join(format!("utsuwa-user-directory-tool-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let desktop = home.join("Escritorio");
        std::fs::create_dir_all(&desktop).unwrap();
        let target = desktop.join("hello.txt");
        let wrong = home.join("Desktop");
        let tool = UserDirectoryWriteTool::with_environment(
            tool_filesystem::FilesystemLimits::default(),
            environment(&home, &desktop),
        );

        let args = serde_json::json!({
            "directory": "desktop",
            "relative_path": "hello.txt",
            "content": "hello",
        });
        let requirement = tool
            .required_capability(&args)
            .expect("resolved user file needs write capability");
        assert_eq!(requirement.capability, Capability::FilesystemWrite);
        assert_eq!(
            requirement.resource,
            Resource::Path(target.canonicalize().unwrap_or(target.clone()))
        );
        let output = tool.invoke(ticketed_context(&target), args).await.unwrap();
        assert_eq!(output.content["path"], target.to_string_lossy().as_ref());
        assert_eq!(output.content["directory"], "desktop");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello");
        assert!(!wrong.exists());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn resolve_user_dir_returns_the_exact_configured_path() {
        let home = std::env::temp_dir().join(format!(
            "utsuwa-user-directory-resolve-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&home);
        let desktop = home.join("Escritorio");
        std::fs::create_dir_all(&desktop).unwrap();
        let tool = ResolveUserDirectoryTool::with_environment(environment(&home, &desktop));
        let output = tool
            .invoke(
                ToolContext::new(Principal::Agent(AgentId::new("resolve-test"))),
                serde_json::json!({"directory": "desktop"}),
            )
            .await
            .unwrap();
        assert_eq!(output.content["directory"], "desktop");
        assert_eq!(output.content["path"], desktop.to_string_lossy().as_ref());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn raw_write_error_explains_the_configured_localized_desktop() {
        let home = PathBuf::from("/home/meme");
        let environment = environment(&home, &home.join("Escritorio"));
        let error = ToolError::Failed {
            tool: "filesystem".to_string(),
            message: "parent directory does not exist: /home/meme/Desktop".to_string(),
        };
        let enriched = enrich_write_error(error, &environment);
        assert!(enriched.to_string().contains("/home/meme/Escritorio"));
        assert!(enriched.to_string().contains("filesystem.write_user_file"));
    }
}
