//! Agent tools for operating-system configured user directories.
//!
//! These tools keep directory-name interpretation in the native host. The
//! model supplies a stable enum such as `desktop`; the host resolves it to the
//! configured absolute path and delegates the actual write to the existing
//! filesystem broker.

use crate::host_environment::{HostEnvironment, UserDirectory};
use serde_json::{Map, Value};
use std::path::{Component, Path, PathBuf};
use tool_core::{CapabilityRequirement, Tool, ToolContext, ToolError, ToolMetadata, ToolOutput};
use tool_filesystem::{PatchTool, WriteTool};

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

const CREATE_USER_FILE_TOOL: &str = "filesystem.create_user_file";
const WRITE_USER_FILE_TOOL: &str = "filesystem.write_user_file";
const EDIT_USER_FILE_TOOL: &str = "filesystem.edit_user_file";
const EDIT_FILE_TOOL: &str = "filesystem.edit_file";
const REPLACE_USER_FILE_TOOL: &str = "filesystem.replace_user_file";
const APPEND_USER_FILE_TOOL: &str = "filesystem.append_user_file";
const APPEND_FILE_TOOL: &str = "filesystem.append_file";
const DEFAULT_USER_FILENAME: &str = "note.txt";

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

fn invalid_user_directory(tool: &str, received: &str, environment: &HostEnvironment) -> ToolError {
    let mut available_directories = Map::new();
    for directory in UserDirectory::ALL {
        if let Some(path) = environment.user_dirs.get(directory) {
            available_directories.insert(
                directory.json_key().to_string(),
                Value::String(path.to_string_lossy().into_owned()),
            );
        }
    }
    let detail = serde_json::json!({
        "error": "invalid_user_directory",
        "received": received,
        "valid_directory_ids": USER_DIRECTORY_ENUM,
        "available_directories": available_directories,
    });
    invalid_args(tool, format!("invalid_user_directory: {detail}"))
}

fn configured_directory_for_path(
    value: &str,
    environment: &HostEnvironment,
) -> Option<UserDirectory> {
    let path = Path::new(value);
    if !path.is_absolute() {
        return None;
    }
    let mut matches = UserDirectory::ALL
        .into_iter()
        .filter(|directory| environment.user_dirs.get(*directory) == Some(path));
    let directory = matches.next()?;
    matches.next().is_none().then_some(directory)
}

fn configured_directory_for_basename(
    value: &str,
    environment: &HostEnvironment,
) -> Option<UserDirectory> {
    let path = Path::new(value);
    if path.components().count() != 1 {
        return None;
    }
    let basename = path.file_name()?;
    let mut matches = UserDirectory::ALL.into_iter().filter(|directory| {
        environment
            .user_dirs
            .get(*directory)
            .and_then(Path::file_name)
            == Some(basename)
    });
    let directory = matches.next()?;
    matches.next().is_none().then_some(directory)
}

fn parse_directory_value(
    value: &str,
    environment: &HostEnvironment,
    tool: &str,
) -> Result<UserDirectory, ToolError> {
    match value.to_ascii_lowercase().as_str() {
        "desktop" => Ok(UserDirectory::Desktop),
        "documents" => Ok(UserDirectory::Documents),
        "downloads" => Ok(UserDirectory::Downloads),
        "pictures" => Ok(UserDirectory::Pictures),
        "music" => Ok(UserDirectory::Music),
        "videos" => Ok(UserDirectory::Videos),
        "public_share" => Ok(UserDirectory::PublicShare),
        "templates" => Ok(UserDirectory::Templates),
        _ => configured_directory_for_path(value, environment)
            .or_else(|| configured_directory_for_basename(value, environment))
            .ok_or_else(|| invalid_user_directory(tool, value, environment)),
    }
}

fn parse_directory(
    args: &serde_json::Value,
    environment: &HostEnvironment,
    tool: &str,
) -> Result<UserDirectory, ToolError> {
    let object = args
        .as_object()
        .ok_or_else(|| invalid_args(tool, "args must be a JSON object"))?;
    let (field, value) = if let Some(value) = object.get("directory_id") {
        (
            "directory_id",
            value
                .as_str()
                .ok_or_else(|| invalid_args(tool, "'directory_id' must be a string"))?,
        )
    } else if let Some(value) = object.get("directory") {
        (
            "directory",
            value
                .as_str()
                .ok_or_else(|| invalid_args(tool, "'directory' must be a string"))?,
        )
    } else {
        return Err(invalid_args(
            tool,
            "missing string 'directory_id' argument (legacy 'directory' is also accepted)",
        ));
    };
    parse_directory_value(value, environment, tool).map_err(|error| match error {
        ToolError::InvalidArgs { message, .. } => invalid_args(tool, format!("{field}: {message}")),
        error => error,
    })
}

/// The simpler user-file tool calls its semantic selector "location". Keep
/// accepting the two older spellings as a compatibility bridge, but always
/// resolve the value through the same host-owned directory table.
fn parse_location(
    args: &serde_json::Value,
    environment: &HostEnvironment,
    tool: &str,
) -> Result<UserDirectory, ToolError> {
    let object = args
        .as_object()
        .ok_or_else(|| invalid_args(tool, "args must be a JSON object"))?;
    let (field, value) = if let Some(value) = object.get("location") {
        (
            "location",
            value
                .as_str()
                .ok_or_else(|| invalid_args(tool, "'location' must be a string"))?,
        )
    } else if let Some(value) = object.get("directory_id") {
        (
            "directory_id",
            value
                .as_str()
                .ok_or_else(|| invalid_args(tool, "'directory_id' must be a string"))?,
        )
    } else if let Some(value) = object.get("directory") {
        (
            "directory",
            value
                .as_str()
                .ok_or_else(|| invalid_args(tool, "'directory' must be a string"))?,
        )
    } else {
        return Err(invalid_args(tool, "missing string 'location' argument"));
    };
    parse_directory_value(value, environment, tool).map_err(|error| match error {
        ToolError::InvalidArgs { message, .. } => invalid_args(tool, format!("{field}: {message}")),
        error => error,
    })
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
    resolved_directory: PathBuf,
    relative_path: PathBuf,
    generated_filename: bool,
    write_args: serde_json::Value,
}

fn write_args_for_path(object: &Map<String, Value>, path: &Path) -> serde_json::Value {
    let mut write_args = serde_json::Value::Object(object.clone());
    let write_object = write_args
        .as_object_mut()
        .expect("write args cloned from a JSON object");
    write_object.remove("directory");
    write_object.remove("directory_id");
    write_object.remove("location");
    write_object.remove("relative_path");
    write_object.remove("filename");
    write_object.insert(
        "path".to_string(),
        serde_json::Value::String(path.to_string_lossy().into_owned()),
    );
    write_args
}

fn resolve_user_file_args(
    args: &serde_json::Value,
    environment: &HostEnvironment,
    tool: &str,
) -> Result<ResolvedUserFile, ToolError> {
    let object = args
        .as_object()
        .ok_or_else(|| invalid_args(tool, "args must be a JSON object"))?;
    let directory = parse_directory(args, environment, tool)?;
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
    let resolved_directory = base.to_path_buf();
    let path = base.join(relative_path);

    Ok(ResolvedUserFile {
        directory,
        resolved_directory,
        relative_path: relative_path.to_path_buf(),
        generated_filename: false,
        write_args: write_args_for_path(object, &path),
    })
}

fn lexical_absolute_path(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Some(normalized)
}

fn looks_like_windows_absolute_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/')
}

/// Normalize the model-facing filename against one already validated host
/// directory. Absolute compatibility values are accepted only when their
/// normalized path is below that exact directory; arbitrary absolute paths
/// never become an alternate write API.
fn normalize_filename(
    filename: &str,
    base: &Path,
    tool: &str,
) -> Result<(PathBuf, PathBuf), ToolError> {
    if filename.trim().is_empty() {
        return Err(invalid_args(tool, "'filename' must name a file"));
    }
    if !cfg!(target_os = "windows") && looks_like_windows_absolute_path(filename) {
        return Err(invalid_args(
            tool,
            "'filename' must use the host-native path style and stay inside the selected directory",
        ));
    }
    let supplied = Path::new(filename);
    if supplied
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(invalid_args(
            tool,
            "'filename' must not contain '..' path components",
        ));
    }
    let normalized_base = lexical_absolute_path(base).unwrap_or_else(|| base.to_path_buf());
    let (relative, target) = if supplied.is_absolute() {
        let target = lexical_absolute_path(supplied).ok_or_else(|| {
            invalid_args(tool, "'filename' must be a valid absolute host-native path")
        })?;
        let relative = target.strip_prefix(&normalized_base).map_err(|_| {
            invalid_args(
                tool,
                "an absolute 'filename' is accepted only when it is inside the selected host directory",
            )
        })?;
        (relative.to_path_buf(), target)
    } else {
        let relative = supplied.to_path_buf();
        (relative.clone(), normalized_base.join(&relative))
    };
    if reject_unsafe_relative_path(&relative) {
        return Err(invalid_args(
            tool,
            "'filename' must name a file below the selected user directory",
        ));
    }
    Ok((relative, target))
}

fn optional_filename(object: &Map<String, Value>, tool: &str) -> Result<(String, bool), ToolError> {
    match object.get("filename") {
        None | Some(Value::Null) => Ok((DEFAULT_USER_FILENAME.to_string(), true)),
        Some(value) => {
            let filename = value
                .as_str()
                .ok_or_else(|| invalid_args(tool, "'filename' must be a string when provided"))?;
            if filename.trim().is_empty() {
                Ok((DEFAULT_USER_FILENAME.to_string(), true))
            } else {
                Ok((filename.to_string(), false))
            }
        }
    }
}

fn resolve_create_user_file_args(
    args: &serde_json::Value,
    environment: &HostEnvironment,
    tool: &str,
) -> Result<ResolvedUserFile, ToolError> {
    let object = args
        .as_object()
        .ok_or_else(|| invalid_args(tool, "args must be a JSON object"))?;
    let directory = parse_location(args, environment, tool)?;
    let base = environment.user_dirs.get(directory).ok_or_else(|| {
        failed(
            tool,
            format!(
                "the host-configured {} directory is not available",
                directory.prompt_label()
            ),
        )
    })?;
    let (filename, generated_filename) = optional_filename(object, tool)?;
    let (relative_path, path) = normalize_filename(&filename, base, tool)?;
    Ok(ResolvedUserFile {
        directory,
        resolved_directory: base.to_path_buf(),
        relative_path,
        generated_filename,
        write_args: write_args_for_path(object, &path),
    })
}

/// The raw path write tool, with host-aware recovery guidance for the common
/// stale `~/Desktop` mistake. Its actual invocation remains the filesystem
/// broker's normal `WriteTool` implementation.
pub(crate) struct HostAwareWriteTool {
    inner: WriteTool,
    environment: HostEnvironment,
}

impl HostAwareWriteTool {
    pub(crate) fn new(
        limits: tool_filesystem::FilesystemLimits,
        environment: HostEnvironment,
    ) -> Self {
        Self {
            inner: WriteTool { limits },
            environment,
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
            "{message}. The host-configured {} directory is {}. Use filesystem.create_user_file with location='{}', or retry using that exact resolved path",
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
            " Prefer filesystem.create_user_file for OS-configured Desktop, Documents, Downloads, Pictures, Music, Videos, Public, or Templates directories; use filesystem.write only for arbitrary explicit paths.",
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
        self.inner
            .invoke(ctx, args)
            .await
            .map_err(|error| enrich_write_error(error, &self.environment))
    }
}

/// Write inside a validated, OS-configured user directory while preserving
/// the existing filesystem write broker and its exact capability scope.
pub(crate) struct UserDirectoryWriteTool {
    inner: WriteTool,
    environment: HostEnvironment,
}

impl UserDirectoryWriteTool {
    pub(crate) fn new(
        limits: tool_filesystem::FilesystemLimits,
        environment: HostEnvironment,
    ) -> Self {
        Self {
            inner: WriteTool { limits },
            environment,
        }
    }

    #[cfg(test)]
    fn with_environment(
        limits: tool_filesystem::FilesystemLimits,
        environment: HostEnvironment,
    ) -> Self {
        Self::new(limits, environment)
    }
}

#[async_trait::async_trait]
impl Tool for UserDirectoryWriteTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new(WRITE_USER_FILE_TOOL),
            description: "Compatibility alias for creating or overwriting a UTF-8 file inside an operating-system configured user directory. Prefer filesystem.create_user_file with location and filename. Set directory_id to a semantic identifier such as desktop; the host resolves it. For compatibility, an exact host-resolved directory path or configured basename is accepted. Never use an arbitrary absolute path or translate directory names. The relative path must stay below the selected directory, and parent directories must already exist unless create_parents=true.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "directory_id": {
                        "type": "string",
                        "enum": USER_DIRECTORY_ENUM,
                        "description": "Semantic operating-system user directory identifier; use desktop, documents, downloads, pictures, music, videos, public_share, or templates. Do not put an absolute path here."
                    },
                    "directory": {
                        "type": "string",
                        "description": "Legacy alias for directory_id. An exact path is accepted only when it matches a validated host user directory."
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
                "required": ["directory_id", "relative_path", "content"]
            }),
            effects: vec![tool_core::ToolEffect::FilesystemWrite],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        let resolved =
            resolve_user_file_args(args, &self.environment, WRITE_USER_FILE_TOOL).ok()?;
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
        let resolved = resolve_user_file_args(&args, &self.environment, WRITE_USER_FILE_TOOL)?;
        let directory = resolved.directory.json_key();
        let mut output = self.inner.invoke(ctx, resolved.write_args).await?;
        if let Some(object) = output.content.as_object_mut() {
            object.insert(
                "directory_id".to_string(),
                serde_json::Value::String(directory.to_string()),
            );
            object.insert(
                "resolved_directory".to_string(),
                serde_json::Value::String(
                    resolved.resolved_directory.to_string_lossy().into_owned(),
                ),
            );
            // Keep the original output keys for older frontend consumers.
            object.insert(
                "directory".to_string(),
                serde_json::Value::String(directory.to_string()),
            );
        }
        Ok(output)
    }
}

/// Preferred small-model interface for files in OS-configured user
/// directories. It resolves the semantic location and safe filename in the
/// native host, then delegates to the existing filesystem write broker.
pub(crate) struct CreateUserFileTool {
    inner: WriteTool,
    environment: HostEnvironment,
}

impl CreateUserFileTool {
    pub(crate) fn new(
        limits: tool_filesystem::FilesystemLimits,
        environment: HostEnvironment,
    ) -> Self {
        Self {
            inner: WriteTool { limits },
            environment,
        }
    }

    #[cfg(test)]
    fn with_environment(
        limits: tool_filesystem::FilesystemLimits,
        environment: HostEnvironment,
    ) -> Self {
        Self::new(limits, environment)
    }
}

#[async_trait::async_trait]
impl Tool for CreateUserFileTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new(CREATE_USER_FILE_TOOL),
            description: "Create or overwrite a UTF-8 file inside an operating-system configured user directory such as Desktop or Documents. Prefer this tool whenever the user refers to Desktop, Documents, Downloads, Pictures, Music, Public, or Templates; the host resolves the location and you must not construct its path. Use a semantic location such as desktop, not an absolute directory path. The filename may be a file name or relative subpath; parent directories must already exist unless create_parents=true. An absolute filename is accepted only when it is inside the selected host directory.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "location": {
                        "type": "string",
                        "enum": USER_DIRECTORY_ENUM,
                        "description": "Semantic operating-system user directory identifier: desktop, documents, downloads, pictures, music, videos, public_share, or templates. Do not construct or translate a filesystem path."
                    },
                    "filename": {
                        "type": "string",
                        "description": "File name such as hello.txt, or a relative subpath such as notes/hello.txt. The host may normalize an absolute path only when it is inside the selected configured directory. If omitted, the host uses a safe deterministic default."
                    },
                    "content": {
                        "type": "string",
                        "description": "UTF-8 file contents."
                    },
                    "create_parents": {
                        "type": "boolean",
                        "default": false,
                        "description": "Explicitly create missing parent directories for a relative subpath. Omit or set false to require existing parents."
                    }
                },
                "required": ["location", "content"]
            }),
            effects: vec![tool_core::ToolEffect::FilesystemWrite],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        let resolved =
            resolve_create_user_file_args(args, &self.environment, CREATE_USER_FILE_TOOL).ok()?;
        self.inner.required_capability(&resolved.write_args)
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let resolved =
            resolve_create_user_file_args(&args, &self.environment, CREATE_USER_FILE_TOOL)?;
        let directory_id = resolved.directory.json_key();
        let resolved_filename = resolved.relative_path.to_string_lossy().into_owned();
        let generated_filename = resolved.generated_filename;
        let mut output = self.inner.invoke(ctx, resolved.write_args).await?;
        if let Some(object) = output.content.as_object_mut() {
            object.insert(
                "location".to_string(),
                Value::String(directory_id.to_string()),
            );
            object.insert(
                "directory_id".to_string(),
                Value::String(directory_id.to_string()),
            );
            object.insert(
                "resolved_directory".to_string(),
                Value::String(resolved.resolved_directory.to_string_lossy().into_owned()),
            );
            object.insert("filename".to_string(), Value::String(resolved_filename));
            object.insert(
                "generated_filename".to_string(),
                Value::Bool(generated_filename),
            );
        }
        Ok(output)
    }
}

/// One validated edit target inside an OS-configured user directory: the
/// semantic directory, its resolved host path, the relative filename, and
/// the exact absolute target the broker will mutate.
struct ResolvedEditTarget {
    directory: UserDirectory,
    resolved_directory: PathBuf,
    relative_path: PathBuf,
    path: PathBuf,
}

fn required_string_field(
    object: &Map<String, Value>,
    tool: &str,
    field: &str,
) -> Result<String, ToolError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| invalid_args(tool, format!("missing string '{field}' argument")))
}

fn required_filename(object: &Map<String, Value>, tool: &str) -> Result<String, ToolError> {
    let filename = required_string_field(object, tool, "filename")?;
    if filename.trim().is_empty() {
        return Err(invalid_args(tool, "'filename' must name a file"));
    }
    Ok(filename)
}

/// Resolve `location` + `filename` to one exact absolute target without ever
/// asking the model to construct the host path. Unlike creation, editing
/// always names its file: no default filename is generated.
fn resolve_edit_target(
    args: &serde_json::Value,
    environment: &HostEnvironment,
    tool: &str,
) -> Result<ResolvedEditTarget, ToolError> {
    let object = args
        .as_object()
        .ok_or_else(|| invalid_args(tool, "args must be a JSON object"))?;
    let directory = parse_location(args, environment, tool)?;
    let base = environment.user_dirs.get(directory).ok_or_else(|| {
        failed(
            tool,
            format!(
                "the host-configured {} directory is not available",
                directory.prompt_label()
            ),
        )
    })?;
    let filename = required_filename(object, tool)?;
    let (relative_path, path) = normalize_filename(&filename, base, tool)?;
    Ok(ResolvedEditTarget {
        directory,
        resolved_directory: base.to_path_buf(),
        relative_path,
        path,
    })
}

fn patch_args_for(path: &Path, old_text: &str, new_text: &str) -> serde_json::Value {
    serde_json::json!({
        "path": path.to_string_lossy(),
        "replacements": [{ "old": old_text, "new": new_text }],
    })
}

fn path_only_args(path: &Path) -> serde_json::Value {
    serde_json::json!({ "path": path.to_string_lossy() })
}

fn parse_old_new(args: &serde_json::Value, tool: &str) -> Result<(String, String), ToolError> {
    let object = args
        .as_object()
        .ok_or_else(|| invalid_args(tool, "args must be a JSON object"))?;
    let old_text = required_string_field(object, tool, "old_text")?;
    if old_text.is_empty() {
        return Err(invalid_args(tool, "'old_text' must not be empty"));
    }
    let new_text = required_string_field(object, tool, "new_text")?;
    Ok((old_text, new_text))
}

fn tag_user_file_output(
    output: &mut ToolOutput,
    directory: UserDirectory,
    resolved_directory: &Path,
    relative_path: &Path,
) {
    if let Some(object) = output.content.as_object_mut() {
        object.insert(
            "location".to_string(),
            Value::String(directory.json_key().to_string()),
        );
        object.insert(
            "directory_id".to_string(),
            Value::String(directory.json_key().to_string()),
        );
        object.insert(
            "resolved_directory".to_string(),
            Value::String(resolved_directory.to_string_lossy().into_owned()),
        );
        object.insert(
            "filename".to_string(),
            Value::String(relative_path.to_string_lossy().into_owned()),
        );
    }
}

/// Small-model partial-edit interface for files in OS-configured user
/// directories. Resolves the semantic location in the native host, converts
/// the simple arguments into `PatchTool` arguments, and delegates the actual
/// mutation to the existing filesystem patch broker — ticket validation,
/// exact-match safety, and atomic write all stay intact.
pub(crate) struct EditUserFileTool {
    inner: PatchTool,
    environment: HostEnvironment,
}

impl EditUserFileTool {
    pub(crate) fn new(
        limits: tool_filesystem::FilesystemLimits,
        environment: HostEnvironment,
    ) -> Self {
        Self {
            inner: PatchTool { limits },
            environment,
        }
    }
}

#[async_trait::async_trait]
impl Tool for EditUserFileTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new(EDIT_USER_FILE_TOOL),
            description: "Replace one exact text block inside an existing file in an operating-system configured user directory such as Desktop or Documents. Use a semantic location such as desktop, never a constructed path. Read the file first with filesystem.read when the exact old text is unknown. The old text must occur exactly once.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "location": {
                        "type": "string",
                        "enum": USER_DIRECTORY_ENUM,
                        "description": "Semantic operating-system user directory identifier: desktop, documents, downloads, pictures, music, videos, public_share, or templates. Do not construct or translate a filesystem path."
                    },
                    "filename": {
                        "type": "string",
                        "description": "Existing file name such as note.txt, or a relative subpath such as notes/note.txt. An absolute path is accepted only when it is inside the selected configured directory."
                    },
                    "old_text": {
                        "type": "string",
                        "description": "Exact text to replace. It must occur exactly once in the file."
                    },
                    "new_text": {
                        "type": "string",
                        "description": "Replacement text."
                    }
                },
                "required": ["location", "filename", "old_text", "new_text"]
            }),
            effects: vec![tool_core::ToolEffect::FilesystemWrite],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        let resolved = resolve_edit_target(args, &self.environment, EDIT_USER_FILE_TOOL).ok()?;
        // The inner patch tool computes the same canonical resource it will
        // validate in `invoke`; no capability is minted for the symbolic
        // location name supplied by the model.
        self.inner
            .required_capability(&path_only_args(&resolved.path))
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let resolved = resolve_edit_target(&args, &self.environment, EDIT_USER_FILE_TOOL)?;
        let (old_text, new_text) = parse_old_new(&args, EDIT_USER_FILE_TOOL)?;
        let mut output = self
            .inner
            .invoke(ctx, patch_args_for(&resolved.path, &old_text, &new_text))
            .await?;
        tag_user_file_output(
            &mut output,
            resolved.directory,
            &resolved.resolved_directory,
            &resolved.relative_path,
        );
        if let Some(object) = output.content.as_object_mut() {
            object.insert("updated".to_string(), Value::Bool(true));
        }
        Ok(output)
    }
}

/// Partial-edit interface for an existing file at an explicit absolute path.
/// Delegates the mutation to the existing filesystem patch broker.
pub(crate) struct EditFileTool {
    inner: PatchTool,
}

impl EditFileTool {
    pub(crate) fn new(limits: tool_filesystem::FilesystemLimits) -> Self {
        Self {
            inner: PatchTool { limits },
        }
    }
}

#[async_trait::async_trait]
impl Tool for EditFileTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new(EDIT_FILE_TOOL),
            description: "Replace one exact text block inside an existing file at an explicit absolute host-native path. Use this only for arbitrary paths; for Desktop/Documents/etc. prefer filesystem.edit_user_file. Read the file first with filesystem.read when the exact old text is unknown. The old text must occur exactly once.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute host-native path of the existing file."
                    },
                    "old_text": {
                        "type": "string",
                        "description": "Exact text to replace. It must occur exactly once in the file."
                    },
                    "new_text": {
                        "type": "string",
                        "description": "Replacement text."
                    }
                },
                "required": ["path", "old_text", "new_text"]
            }),
            effects: vec![tool_core::ToolEffect::FilesystemWrite],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        let path = args.get("path")?.as_str()?;
        self.inner
            .required_capability(&serde_json::json!({ "path": path }))
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let object = args
            .as_object()
            .ok_or_else(|| invalid_args(EDIT_FILE_TOOL, "args must be a JSON object"))?;
        let path = required_string_field(object, EDIT_FILE_TOOL, "path")?;
        let (old_text, new_text) = parse_old_new(&args, EDIT_FILE_TOOL)?;
        let mut output = self
            .inner
            .invoke(ctx, patch_args_for(Path::new(&path), &old_text, &new_text))
            .await?;
        if let Some(object) = output.content.as_object_mut() {
            object.insert("updated".to_string(), Value::Bool(true));
        }
        Ok(output)
    }
}

/// Whole-file replacement for an existing file in an OS-configured user
/// directory. Delegates to the existing filesystem write broker; the target
/// must already exist so a replace call can never silently create a file —
/// use filesystem.create_user_file to create one.
pub(crate) struct ReplaceUserFileTool {
    inner: WriteTool,
    environment: HostEnvironment,
}

impl ReplaceUserFileTool {
    pub(crate) fn new(
        limits: tool_filesystem::FilesystemLimits,
        environment: HostEnvironment,
    ) -> Self {
        Self {
            inner: WriteTool { limits },
            environment,
        }
    }
}

#[async_trait::async_trait]
impl Tool for ReplaceUserFileTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new(REPLACE_USER_FILE_TOOL),
            description: "Replace the entire contents of an existing file in an operating-system configured user directory such as Desktop or Documents. Use a semantic location such as desktop, never a constructed path. The file must already exist; use filesystem.create_user_file to create a new file.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "location": {
                        "type": "string",
                        "enum": USER_DIRECTORY_ENUM,
                        "description": "Semantic operating-system user directory identifier: desktop, documents, downloads, pictures, music, videos, public_share, or templates. Do not construct or translate a filesystem path."
                    },
                    "filename": {
                        "type": "string",
                        "description": "Existing file name such as note.txt, or a relative subpath such as notes/note.txt. An absolute path is accepted only when it is inside the selected configured directory."
                    },
                    "content": {
                        "type": "string",
                        "description": "Complete new file contents."
                    }
                },
                "required": ["location", "filename", "content"]
            }),
            effects: vec![tool_core::ToolEffect::FilesystemWrite],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        let resolved = resolve_edit_target(args, &self.environment, REPLACE_USER_FILE_TOOL).ok()?;
        self.inner
            .required_capability(&path_only_args(&resolved.path))
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let resolved = resolve_edit_target(&args, &self.environment, REPLACE_USER_FILE_TOOL)?;
        let object = args
            .as_object()
            .ok_or_else(|| invalid_args(REPLACE_USER_FILE_TOOL, "args must be a JSON object"))?;
        let content = required_string_field(object, REPLACE_USER_FILE_TOOL, "content")?;
        if !resolved.path.is_file() {
            return Err(failed(
                REPLACE_USER_FILE_TOOL,
                format!(
                    "'{}' does not exist or is not a file: use filesystem.create_user_file to create a new file",
                    resolved.path.display()
                ),
            ));
        }
        let mut output = self
            .inner
            .invoke(
                ctx,
                serde_json::json!({
                    "path": resolved.path.to_string_lossy(),
                    "content": content,
                }),
            )
            .await?;
        tag_user_file_output(
            &mut output,
            resolved.directory,
            &resolved.resolved_directory,
            &resolved.relative_path,
        );
        if let Some(object) = output.content.as_object_mut() {
            object.insert("updated".to_string(), Value::Bool(true));
        }
        Ok(output)
    }
}

/// Read the current file, append content, and delegate the write to the
/// existing filesystem write broker so ticket validation, audit evidence,
/// and symlink protections stay intact. Missing files are created with the
/// appended content; anything else that is not a regular file is refused.
fn append_content_for(
    tool: &str,
    path: &Path,
    content: &str,
    limits: &tool_filesystem::FilesystemLimits,
) -> Result<String, ToolError> {
    let current = match std::fs::read(path) {
        Ok(bytes) => {
            if !path.is_file() {
                return Err(failed(tool, format!("'{}' is not a file", path.display())));
            }
            if bytes.len() > limits.max_read_bytes {
                return Err(failed(
                    tool,
                    format!("'{}' exceeds the readable size limit", path.display()),
                ));
            }
            String::from_utf8(bytes)
                .map_err(|_| failed(tool, format!("'{}' is not valid UTF-8", path.display())))?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(failed(
                tool,
                format!("cannot read '{}': {error}", path.display()),
            ));
        }
    };
    let appended = format!("{current}{content}");
    if appended.len() > limits.max_write_bytes {
        return Err(ToolError::InvalidArgs {
            tool: "filesystem".to_string(),
            message: format!(
                "appended content exceeds the {} byte limit",
                limits.max_write_bytes
            ),
        });
    }
    Ok(appended)
}

/// Append-only interface for files in OS-configured user directories. The
/// model never reproduces the whole file — the host reads, appends, and
/// writes atomically through the existing broker.
pub(crate) struct AppendUserFileTool {
    inner: WriteTool,
    limits: tool_filesystem::FilesystemLimits,
    environment: HostEnvironment,
}

impl AppendUserFileTool {
    pub(crate) fn new(
        limits: tool_filesystem::FilesystemLimits,
        environment: HostEnvironment,
    ) -> Self {
        Self {
            inner: WriteTool {
                limits: limits.clone(),
            },
            limits,
            environment,
        }
    }
}

#[async_trait::async_trait]
impl Tool for AppendUserFileTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new(APPEND_USER_FILE_TOOL),
            description: "Append text to the end of a file in an operating-system configured user directory such as Desktop or Documents. Use a semantic location such as desktop, never a constructed path. The file is created with the appended text when it does not exist yet.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "location": {
                        "type": "string",
                        "enum": USER_DIRECTORY_ENUM,
                        "description": "Semantic operating-system user directory identifier: desktop, documents, downloads, pictures, music, videos, public_share, or templates. Do not construct or translate a filesystem path."
                    },
                    "filename": {
                        "type": "string",
                        "description": "File name such as note.txt, or a relative subpath such as notes/note.txt. An absolute path is accepted only when it is inside the selected configured directory."
                    },
                    "content": {
                        "type": "string",
                        "description": "Text to append to the end of the file."
                    }
                },
                "required": ["location", "filename", "content"]
            }),
            effects: vec![tool_core::ToolEffect::FilesystemWrite],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        let resolved = resolve_edit_target(args, &self.environment, APPEND_USER_FILE_TOOL).ok()?;
        self.inner
            .required_capability(&path_only_args(&resolved.path))
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let resolved = resolve_edit_target(&args, &self.environment, APPEND_USER_FILE_TOOL)?;
        let object = args
            .as_object()
            .ok_or_else(|| invalid_args(APPEND_USER_FILE_TOOL, "args must be a JSON object"))?;
        let content = required_string_field(object, APPEND_USER_FILE_TOOL, "content")?;
        let appended = append_content_for(
            APPEND_USER_FILE_TOOL,
            &resolved.path,
            &content,
            &self.limits,
        )?;
        let mut output = self
            .inner
            .invoke(
                ctx,
                serde_json::json!({
                    "path": resolved.path.to_string_lossy(),
                    "content": appended,
                }),
            )
            .await?;
        tag_user_file_output(
            &mut output,
            resolved.directory,
            &resolved.resolved_directory,
            &resolved.relative_path,
        );
        if let Some(object) = output.content.as_object_mut() {
            object.insert("appended".to_string(), Value::Bool(true));
        }
        Ok(output)
    }
}

/// Append-only interface for a file at an explicit absolute host-native
/// path. Reads, appends, and delegates the write to the existing broker.
pub(crate) struct AppendFileTool {
    inner: WriteTool,
    limits: tool_filesystem::FilesystemLimits,
}

impl AppendFileTool {
    pub(crate) fn new(limits: tool_filesystem::FilesystemLimits) -> Self {
        Self {
            inner: WriteTool {
                limits: limits.clone(),
            },
            limits,
        }
    }
}

#[async_trait::async_trait]
impl Tool for AppendFileTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new(APPEND_FILE_TOOL),
            description: "Append text to the end of a file at an explicit absolute host-native path. Use this only for arbitrary paths; for Desktop/Documents/etc. prefer filesystem.append_user_file. The file is created with the appended text when it does not exist yet.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute host-native path of the file."
                    },
                    "content": {
                        "type": "string",
                        "description": "Text to append to the end of the file."
                    }
                },
                "required": ["path", "content"]
            }),
            effects: vec![tool_core::ToolEffect::FilesystemWrite],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        let path = args.get("path")?.as_str()?;
        self.inner
            .required_capability(&serde_json::json!({ "path": path }))
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let object = args
            .as_object()
            .ok_or_else(|| invalid_args(APPEND_FILE_TOOL, "args must be a JSON object"))?;
        let path = required_string_field(object, APPEND_FILE_TOOL, "path")?;
        let content = required_string_field(object, APPEND_FILE_TOOL, "content")?;
        if !Path::new(&path).is_absolute() {
            return Err(ToolError::InvalidArgs {
                tool: "filesystem".to_string(),
                message: "path must be absolute".to_string(),
            });
        }
        let appended =
            append_content_for(APPEND_FILE_TOOL, Path::new(&path), &content, &self.limits)?;
        let mut output = self
            .inner
            .invoke(
                ctx,
                serde_json::json!({ "path": path, "content": appended }),
            )
            .await?;
        if let Some(object) = output.content.as_object_mut() {
            object.insert("appended".to_string(), Value::Bool(true));
        }
        Ok(output)
    }
}

/// Resolve one validated special directory without exposing an OS mutation
/// API or requiring a capability ticket.
pub(crate) struct ResolveUserDirectoryTool {
    environment: HostEnvironment,
}

impl ResolveUserDirectoryTool {
    pub(crate) fn new(environment: HostEnvironment) -> Self {
        Self { environment }
    }

    #[cfg(test)]
    fn with_environment(environment: HostEnvironment) -> Self {
        Self::new(environment)
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
                    "directory_id": {
                        "type": "string",
                        "enum": USER_DIRECTORY_ENUM,
                        "description": "Semantic user directory identifier; use desktop, documents, downloads, pictures, music, videos, public_share, or templates."
                    },
                    "directory": {
                        "type": "string",
                        "description": "Legacy alias for directory_id. An exact path or configured basename is accepted only when it matches a validated host user directory."
                    }
                },
                "required": ["directory_id"]
            }),
            effects: vec![tool_core::ToolEffect::ReadOnly],
        }
    }

    async fn invoke(
        &self,
        _ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let directory = parse_directory(&args, &self.environment, "filesystem.resolve_user_dir")?;
        let path = self.environment.user_dirs.get(directory).ok_or_else(|| {
            failed(
                "filesystem.resolve_user_dir",
                format!(
                    "the host-configured {} directory is not available",
                    directory.prompt_label()
                ),
            )
        })?;
        Ok(ToolOutput::new(serde_json::json!({
            "directory_id": directory.json_key(),
            "resolved_path": path.to_string_lossy(),
            // Keep the original output keys for older model/tool consumers.
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
            "directory_id": "desktop",
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
        assert_eq!(output.content["directory_id"], "desktop");
        assert_eq!(
            output.content["resolved_directory"],
            desktop.to_string_lossy().as_ref()
        );
        assert_eq!(output.content["directory"], "desktop");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello");
        assert!(!wrong.exists());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn write_user_file_normalizes_exact_path_and_configured_basename() {
        let home = std::env::temp_dir().join(format!(
            "utsuwa-user-directory-normalization-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&home);
        let desktop = home.join("Escritorio");
        std::fs::create_dir_all(&desktop).unwrap();
        let tool = UserDirectoryWriteTool::with_environment(
            tool_filesystem::FilesystemLimits::default(),
            environment(&home, &desktop),
        );

        let exact_target = desktop.join("hello.txt");
        let exact_path = desktop.to_string_lossy().into_owned();
        let exact_args = serde_json::json!({
            "directory": exact_path,
            "relative_path": "hello.txt",
            "content": "hello",
        });
        let exact_requirement = tool
            .required_capability(&exact_args)
            .expect("an exact configured directory path needs write capability");
        assert_eq!(
            exact_requirement.resource,
            Resource::Path(exact_target.canonicalize().unwrap_or(exact_target.clone()))
        );
        let exact_output = tool
            .invoke(ticketed_context(&exact_target), exact_args)
            .await
            .unwrap();
        assert_eq!(exact_output.content["directory_id"], "desktop");
        assert_eq!(
            exact_output.content["resolved_directory"],
            desktop.to_string_lossy().as_ref()
        );
        assert_eq!(
            exact_output.content["path"],
            exact_target.to_string_lossy().as_ref()
        );

        let basename_target = desktop.join("hello2.txt");
        let basename_args = serde_json::json!({
            "directory_id": "Escritorio",
            "relative_path": "hello2.txt",
            "content": "hello2",
        });
        tool.invoke(ticketed_context(&basename_target), basename_args)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&basename_target).unwrap(), "hello2");
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn create_user_file_resolves_location_without_constructing_a_path() {
        let home =
            std::env::temp_dir().join(format!("utsuwa-create-user-file-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let desktop = home.join("Escritorio");
        std::fs::create_dir_all(&desktop).unwrap();
        let target = desktop.join("date.txt");
        let wrong = home.join("Desktop");
        let tool = CreateUserFileTool::with_environment(
            tool_filesystem::FilesystemLimits::default(),
            environment(&home, &desktop),
        );
        let args = serde_json::json!({
            "location": "desktop",
            "filename": "date.txt",
            "content": "today",
        });
        let requirement = tool
            .required_capability(&args)
            .expect("the host resolved target needs filesystem write");
        assert_eq!(requirement.capability, Capability::FilesystemWrite);
        assert_eq!(
            requirement.resource,
            Resource::Path(target.canonicalize().unwrap_or(target.clone()))
        );
        let output = tool.invoke(ticketed_context(&target), args).await.unwrap();
        assert_eq!(output.content["location"], "desktop");
        assert_eq!(output.content["filename"], "date.txt");
        assert_eq!(
            output.content["resolved_directory"],
            desktop.to_string_lossy().as_ref()
        );
        assert_eq!(output.content["path"], target.to_string_lossy().as_ref());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "today");
        assert!(!wrong.exists());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn create_user_file_normalizes_an_absolute_filename_only_inside_selected_dir() {
        let home = std::env::temp_dir().join(format!(
            "utsuwa-create-user-file-absolute-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&home);
        let desktop = home.join("Escritorio");
        std::fs::create_dir_all(&desktop).unwrap();
        let target = desktop.join("date.txt");
        let tool = CreateUserFileTool::with_environment(
            tool_filesystem::FilesystemLimits::default(),
            environment(&home, &desktop),
        );
        let output = tool
            .invoke(
                ticketed_context(&desktop),
                serde_json::json!({
                    "location": "desktop",
                    "filename": target.to_string_lossy(),
                    "content": "absolute compatibility",
                }),
            )
            .await
            .unwrap();
        assert_eq!(output.content["filename"], "date.txt");
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "absolute compatibility"
        );

        let error = tool
            .invoke(
                ticketed_context(&home.join("outside.txt")),
                serde_json::json!({
                    "location": "desktop",
                    "filename": "/etc/date.txt",
                    "content": "must reject",
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::InvalidArgs { .. }), "{error:?}");
        assert!(error
            .to_string()
            .contains("inside the selected host directory"));
        assert!(!home.join("outside.txt").exists());

        let parent_escape = tool
            .invoke(
                ticketed_context(&home.join("escape.txt")),
                serde_json::json!({
                    "location": "desktop",
                    "filename": "../escape.txt",
                    "content": "must reject",
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(parent_escape, ToolError::InvalidArgs { .. }));
        assert!(!home.join("escape.txt").exists());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn create_user_file_allows_explicit_nested_parent_creation() {
        let home = std::env::temp_dir().join(format!(
            "utsuwa-create-user-file-nested-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&home);
        let desktop = home.join("Escritorio");
        std::fs::create_dir_all(&desktop).unwrap();
        let target = desktop.join("notes/hello.txt");
        let tool = CreateUserFileTool::with_environment(
            tool_filesystem::FilesystemLimits::default(),
            environment(&home, &desktop),
        );
        let output = tool
            .invoke(
                ticketed_context(&desktop),
                serde_json::json!({
                    "location": "desktop",
                    "filename": "notes/hello.txt",
                    "content": "nested",
                    "create_parents": true,
                }),
            )
            .await
            .unwrap();
        assert_eq!(output.content["path"], target.to_string_lossy().as_ref());
        assert!(target.is_file());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn create_user_file_uses_a_safe_default_filename_when_omitted() {
        let home = std::env::temp_dir().join(format!(
            "utsuwa-create-user-file-default-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&home);
        let desktop = home.join("Escritorio");
        std::fs::create_dir_all(&desktop).unwrap();
        let target = desktop.join(DEFAULT_USER_FILENAME);
        let tool = CreateUserFileTool::with_environment(
            tool_filesystem::FilesystemLimits::default(),
            environment(&home, &desktop),
        );
        let output = tool
            .invoke(
                ticketed_context(&target),
                serde_json::json!({
                    "location": "desktop",
                    "content": "default name",
                }),
            )
            .await
            .unwrap();
        assert_eq!(output.content["filename"], DEFAULT_USER_FILENAME);
        assert_eq!(output.content["generated_filename"], true);
        assert!(target.is_file());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn write_user_file_rejects_arbitrary_absolute_directory_path() {
        let home = std::env::temp_dir().join(format!(
            "utsuwa-user-directory-reject-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&home);
        let desktop = home.join("Escritorio");
        std::fs::create_dir_all(&desktop).unwrap();
        let arbitrary = home.join("random");
        let target = arbitrary.join("hello.txt");
        let tool = UserDirectoryWriteTool::with_environment(
            tool_filesystem::FilesystemLimits::default(),
            environment(&home, &desktop),
        );
        let error = tool
            .invoke(
                ticketed_context(&target),
                serde_json::json!({
                    "directory": arbitrary.to_string_lossy(),
                    "relative_path": "hello.txt",
                    "content": "hello",
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::InvalidArgs { .. }));
        let message = error.to_string();
        assert!(message.contains("invalid_user_directory"));
        assert!(message.contains("valid_directory_ids"));
        assert!(message.contains("Escritorio"));
        assert!(!arbitrary.exists());
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
                serde_json::json!({"directory_id": "desktop"}),
            )
            .await
            .unwrap();
        assert_eq!(output.content["directory_id"], "desktop");
        assert_eq!(
            output.content["resolved_path"],
            desktop.to_string_lossy().as_ref()
        );
        assert_eq!(output.content["directory"], "desktop");
        assert_eq!(output.content["path"], desktop.to_string_lossy().as_ref());

        let legacy_exact = tool
            .invoke(
                ToolContext::new(Principal::Agent(AgentId::new("resolve-test-legacy"))),
                serde_json::json!({"directory": desktop.to_string_lossy()}),
            )
            .await
            .unwrap();
        assert_eq!(legacy_exact.content["directory_id"], "desktop");
        assert_eq!(
            legacy_exact.content["resolved_path"],
            desktop.to_string_lossy().as_ref()
        );
        std::fs::remove_dir_all(&home).unwrap();
    }

    fn edit_harness(home: &Path) -> (PathBuf, PathBuf) {
        let _ = std::fs::remove_dir_all(home);
        let desktop = home.join("Escritorio");
        std::fs::create_dir_all(&desktop).unwrap();
        (home.to_path_buf(), desktop)
    }

    #[tokio::test]
    async fn edit_user_file_updates_the_existing_file_without_creating_a_new_one() {
        let home =
            std::env::temp_dir().join(format!("utsuwa-edit-user-file-{}", std::process::id()));
        let (_home, desktop) = edit_harness(&home);
        let target = desktop.join("note.txt");
        std::fs::write(&target, "hello").unwrap();
        let wrong = home.join("Desktop").join("note.txt");
        let tool = EditUserFileTool::new(
            tool_filesystem::FilesystemLimits::default(),
            environment(&home, &desktop),
        );
        let args = serde_json::json!({
            "location": "desktop",
            "filename": "note.txt",
            "old_text": "hello",
            "new_text": "hello world",
        });
        let requirement = tool
            .required_capability(&args)
            .expect("the host resolved target needs filesystem write");
        assert_eq!(requirement.capability, Capability::FilesystemWrite);
        assert_eq!(
            requirement.resource,
            Resource::Path(target.canonicalize().unwrap_or(target.clone()))
        );
        let output = tool.invoke(ticketed_context(&target), args).await.unwrap();
        assert_eq!(output.content["path"], target.to_string_lossy().as_ref());
        assert_eq!(output.content["updated"], true);
        assert_eq!(output.content["location"], "desktop");
        assert_eq!(output.content["filename"], "note.txt");
        assert_eq!(
            output.content["resolved_directory"],
            desktop.to_string_lossy().as_ref()
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello world");
        assert!(!wrong.exists());
        assert!(!home.join("Desktop").exists());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn edit_user_file_rejects_unknown_and_ambiguous_old_text() {
        let home = std::env::temp_dir().join(format!(
            "utsuwa-edit-user-file-safety-{}",
            std::process::id()
        ));
        let (_home, desktop) = edit_harness(&home);
        let target = desktop.join("note.txt");
        std::fs::write(&target, "alpha beta alpha").unwrap();
        let tool = EditUserFileTool::new(
            tool_filesystem::FilesystemLimits::default(),
            environment(&home, &desktop),
        );
        let missing = tool
            .invoke(
                ticketed_context(&target),
                serde_json::json!({
                    "location": "desktop",
                    "filename": "note.txt",
                    "old_text": "gamma",
                    "new_text": "delta",
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(missing, ToolError::Failed { .. }), "{missing:?}");
        assert!(missing.to_string().contains("0 times"));
        let ambiguous = tool
            .invoke(
                ticketed_context(&target),
                serde_json::json!({
                    "location": "desktop",
                    "filename": "note.txt",
                    "old_text": "alpha",
                    "new_text": "delta",
                }),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(ambiguous, ToolError::Failed { .. }),
            "{ambiguous:?}"
        );
        assert!(ambiguous.to_string().contains("2 times"));
        // Both failures leave the file untouched.
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "alpha beta alpha"
        );
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn edit_file_updates_an_arbitrary_absolute_path() {
        let dir = std::env::temp_dir().join(format!("utsuwa-edit-file-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("arbitrary.txt");
        std::fs::write(&target, "old value here").unwrap();
        let tool = EditFileTool::new(tool_filesystem::FilesystemLimits::default());
        let args = serde_json::json!({
            "path": target.to_string_lossy(),
            "old_text": "old",
            "new_text": "new",
        });
        let requirement = tool
            .required_capability(&args)
            .expect("an absolute path needs filesystem write");
        assert_eq!(requirement.capability, Capability::FilesystemWrite);
        let output = tool.invoke(ticketed_context(&target), args).await.unwrap();
        assert_eq!(output.content["updated"], true);
        assert_eq!(output.content["path"], target.to_string_lossy().as_ref());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new value here");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn edit_file_rejects_relative_paths_like_the_broker() {
        let tool = EditFileTool::new(tool_filesystem::FilesystemLimits::default());
        let error = tool
            .invoke(
                ToolContext::new(Principal::Agent(AgentId::new("edit-relative"))),
                serde_json::json!({
                    "path": "relative/note.txt",
                    "old_text": "a",
                    "new_text": "b",
                }),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                error,
                ToolError::InvalidArgs { .. } | ToolError::Denied { .. }
            ),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn replace_user_file_replaces_whole_contents_but_never_creates() {
        let home =
            std::env::temp_dir().join(format!("utsuwa-replace-user-file-{}", std::process::id()));
        let (_home, desktop) = edit_harness(&home);
        let target = desktop.join("note.txt");
        std::fs::write(&target, "stale contents").unwrap();
        let tool = ReplaceUserFileTool::new(
            tool_filesystem::FilesystemLimits::default(),
            environment(&home, &desktop),
        );
        let output = tool
            .invoke(
                ticketed_context(&target),
                serde_json::json!({
                    "location": "desktop",
                    "filename": "note.txt",
                    "content": "entire new contents",
                }),
            )
            .await
            .unwrap();
        assert_eq!(output.content["updated"], true);
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "entire new contents"
        );

        let missing = tool
            .invoke(
                ticketed_context(&desktop.join("absent.txt")),
                serde_json::json!({
                    "location": "desktop",
                    "filename": "absent.txt",
                    "content": "must not create",
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(missing, ToolError::Failed { .. }), "{missing:?}");
        assert!(!desktop.join("absent.txt").exists());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn append_user_file_appends_without_reproducing_the_file() {
        let home =
            std::env::temp_dir().join(format!("utsuwa-append-user-file-{}", std::process::id()));
        let (_home, desktop) = edit_harness(&home);
        let target = desktop.join("note.txt");
        std::fs::write(&target, "line one\n").unwrap();
        let tool = AppendUserFileTool::new(
            tool_filesystem::FilesystemLimits::default(),
            environment(&home, &desktop),
        );
        let output = tool
            .invoke(
                ticketed_context(&target),
                serde_json::json!({
                    "location": "desktop",
                    "filename": "note.txt",
                    "content": "line two\n",
                }),
            )
            .await
            .unwrap();
        assert_eq!(output.content["appended"], true);
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "line one\nline two\n"
        );
        assert!(output.mutation.is_some());

        // Appending to a missing file creates it with the appended text.
        let fresh = desktop.join("fresh.txt");
        tool.invoke(
            ticketed_context(&fresh),
            serde_json::json!({
                "location": "desktop",
                "filename": "fresh.txt",
                "content": "first line\n",
            }),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&fresh).unwrap(), "first line\n");
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn append_file_appends_at_an_arbitrary_absolute_path() {
        let dir = std::env::temp_dir().join(format!("utsuwa-append-file-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("log.txt");
        std::fs::write(&target, "a").unwrap();
        let tool = AppendFileTool::new(tool_filesystem::FilesystemLimits::default());
        let output = tool
            .invoke(
                ticketed_context(&target),
                serde_json::json!({
                    "path": target.to_string_lossy(),
                    "content": "b",
                }),
            )
            .await
            .unwrap();
        assert_eq!(output.content["appended"], true);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "ab");
        std::fs::remove_dir_all(&dir).unwrap();
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
        assert!(enriched.to_string().contains("filesystem.create_user_file"));
    }
}
