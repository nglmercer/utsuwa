//! Agent tools for operating-system configured user directories.
//!
//! These tools keep directory-name interpretation in the native host. The
//! model supplies a stable enum such as `desktop`; the host resolves it to the
//! configured absolute path and delegates the actual write to the existing
//! filesystem broker.

use crate::host_environment::{HostEnvironment, UserDirectory};
use serde_json::{Map, Value};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
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
const EDIT_TOOL: &str = "filesystem.edit";
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

    /// Normalize a stale conventional special-directory path before the
    /// ticket is minted, so the ticket always scopes the final path.
    /// Normalization is conservative: an existing conventional parent
    /// (or both directories) keeps the explicit path.
    fn normalized_write_path(&self, args: &serde_json::Value) -> Option<NormalizedSpecialPath> {
        args.get("path").and_then(Value::as_str).and_then(|path| {
            normalize_special_user_path(
                Path::new(path),
                &self.environment,
                "filesystem.write",
                SpecialPathPurpose::Write,
                CREATE_USER_FILE_TOOL,
            )
            .ok()
        })
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
        let mut forwarded = args.clone();
        if let (Some(object), Some(normalized)) = (
            forwarded.as_object_mut(),
            self.normalized_write_path(args).as_ref(),
        ) {
            object.insert(
                "path".to_string(),
                Value::String(normalized.path.to_string_lossy().into_owned()),
            );
        }
        self.inner.required_capability(&forwarded)
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let mut args = args;
        let normalized = self.normalized_write_path(&args);
        if let (Some(object), Some(normalized)) = (args.as_object_mut(), normalized.as_ref()) {
            object.insert(
                "path".to_string(),
                Value::String(normalized.path.to_string_lossy().into_owned()),
            );
        }
        let mut output = self
            .inner
            .invoke(ctx, args)
            .await
            .map_err(|error| enrich_write_error(error, &self.environment))?;
        if let Some(normalized) = normalized {
            tag_normalized_from(&mut output, &normalized);
        }
        Ok(output)
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
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedEditTarget {
    directory: UserDirectory,
    resolved_directory: PathBuf,
    relative_path: PathBuf,
    path: PathBuf,
}

#[derive(Debug, Clone)]
struct PendingEditState {
    target: ResolvedEditTarget,
    old_text: Option<String>,
    new_text: Option<String>,
}

struct MergedEditArgs {
    args: serde_json::Value,
    target: Option<ResolvedEditTarget>,
    explicit_target: bool,
    supplied_old_text: Option<String>,
    supplied_new_text: Option<String>,
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

/// If a location is supplied without a filename, infer it only when that
/// configured directory contains exactly one direct regular file. This keeps
/// malformed small-model calls useful without guessing among multiple files.
fn infer_single_edit_filename(base: &Path, tool: &str) -> Result<String, ToolError> {
    let entries = std::fs::read_dir(base).map_err(|error| {
        failed(
            tool,
            format!(
                "cannot inspect the host-configured edit directory '{}': {error}",
                base.display()
            ),
        )
    })?;
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| failed(tool, error.to_string()))?;
        if entry
            .file_type()
            .map_err(|error| failed(tool, error.to_string()))?
            .is_file()
        {
            files.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    match files.as_slice() {
        [filename] => Ok(filename.clone()),
        [] => Err(invalid_args(
            tool,
            format!(
                "missing 'filename': no existing direct file was found in '{}'; call filesystem.list or pass filename explicitly",
                base.display()
            ),
        )),
        _ => Err(invalid_args(
            tool,
            format!(
                "missing 'filename': multiple existing files were found in '{}': {}; pass the requested filename explicitly",
                base.display(),
                files.iter().map(|file| format!("'{file}'")).collect::<Vec<_>>().join(", ")
            ),
        )),
    }
}

/// Resolve a user-directory edit to one exact absolute target without ever
/// asking the model to construct the host path. A missing location is
/// accepted only when the relative filename identifies exactly one existing
/// file among the host-configured user directories.
fn infer_edit_directory(
    filename: &str,
    environment: &HostEnvironment,
    tool: &str,
) -> Result<UserDirectory, ToolError> {
    if Path::new(filename).is_absolute() {
        return Err(invalid_args(
            tool,
            "an absolute filename without 'location' is ambiguous; pass location+filename or use path",
        ));
    }

    let mut matches = Vec::new();
    for directory in UserDirectory::ALL {
        let Some(base) = environment.user_dirs.get(directory) else {
            continue;
        };
        let (_, path) = normalize_filename(filename, base, tool)?;
        if path.is_file() {
            matches.push(directory);
        }
    }
    match matches.as_slice() {
        [directory] => Ok(*directory),
        [] => Err(invalid_args(
            tool,
            format!(
                "could not resolve filename '{filename}' without a location; pass location='desktop' for a Desktop file or use an absolute path"
            ),
        )),
        _ => {
            let available = matches
                .iter()
                .map(|directory| format!("'{}'", directory.json_key()))
                .collect::<Vec<_>>()
                .join(", ");
            Err(invalid_args(
                tool,
                format!(
                    "filename '{filename}' exists in multiple configured user directories ({available}); pass the location explicitly"
                ),
            ))
        }
    }
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
    let has_location = object.contains_key("location")
        || object.contains_key("directory_id")
        || object.contains_key("directory");
    let directory = if has_location {
        parse_location(args, environment, tool)?
    } else {
        let filename = required_filename(object, tool)?;
        infer_edit_directory(&filename, environment, tool)?
    };
    let base = environment.user_dirs.get(directory).ok_or_else(|| {
        failed(
            tool,
            format!(
                "the host-configured {} directory is not available",
                directory.prompt_label()
            ),
        )
    })?;
    let filename = if object.contains_key("filename") {
        required_filename(object, tool)?
    } else if has_location {
        infer_single_edit_filename(base, tool)?
    } else {
        required_filename(object, tool)?
    };
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

/// How strictly a stale conventional-path compatibility remap must treat
/// file existence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpecialPathPurpose {
    /// Existing-file mutations (edit): remap only when the attempted path is
    /// missing and the configured candidate exists. An existing attempted
    /// path always wins (including the both-exist case); a missing candidate
    /// is a structured error instead of a guess.
    ExistingFile,
    /// Writes and appends (which may create): remap when the attempted
    /// conventional parent directory is missing and the configured directory
    /// exists. Never remap when the conventional parent exists, even if the
    /// configured directory exists too.
    Write,
}

pub(crate) struct NormalizedSpecialPath {
    pub path: PathBuf,
    /// Whether the attempted path was remapped to the configured location.
    /// Callers surface the original as `normalized_from` for transparency.
    pub mapped: bool,
    pub attempted: PathBuf,
}

fn unchanged_special_path(path: &Path) -> NormalizedSpecialPath {
    NormalizedSpecialPath {
        path: path.to_path_buf(),
        mapped: false,
        attempted: path.to_path_buf(),
    }
}

/// Record a stale-path remap in the tool output so the model (and the
/// receipt UI) can see which explicit path was normalized to the result.
fn tag_normalized_from(output: &mut ToolOutput, normalized: &NormalizedSpecialPath) {
    if normalized.mapped {
        if let Some(object) = output.content.as_object_mut() {
            object.insert(
                "normalized_from".to_string(),
                Value::String(normalized.attempted.to_string_lossy().into_owned()),
            );
        }
    }
}

/// Map a stale conventional special-directory path (`$HOME/Desktop/...`)
/// to the OS-configured location (`$HOME/Escritorio/...`) using only
/// `HostEnvironment`/XDG data — never hardcoded translations.
///
/// Only a path directly below the conventional directory is eligible:
/// nested lookalikes (`$HOME/projects/Desktop/x`), other roots
/// (`/tmp/Desktop/x`), and relative paths are returned unchanged. The
/// mapping consults the live filesystem for evidence (see
/// [`SpecialPathPurpose`]) and runs before any capability ticket is minted,
/// so the ticket always scopes the final resolved path.
pub(crate) fn normalize_special_user_path(
    path: &Path,
    environment: &HostEnvironment,
    tool: &str,
    purpose: SpecialPathPurpose,
    next_tool: &str,
) -> Result<NormalizedSpecialPath, ToolError> {
    let Some(home) = environment.home.as_deref() else {
        return Ok(unchanged_special_path(path));
    };
    if !path.is_absolute() {
        return Ok(unchanged_special_path(path));
    }
    let (Some(parent), Some(file_name)) = (path.parent(), path.file_name()) else {
        return Ok(unchanged_special_path(path));
    };
    for directory in UserDirectory::ALL {
        let conventional = home.join(directory.conventional_name());
        if parent != conventional.as_path() {
            continue;
        }
        let Some(configured) = environment.user_dirs.get(directory) else {
            return Ok(unchanged_special_path(path));
        };
        if configured == conventional {
            // The conventional path is already the configured one.
            return Ok(unchanged_special_path(path));
        }
        let candidate = configured.join(file_name);
        match purpose {
            SpecialPathPurpose::ExistingFile => {
                if path.exists() {
                    // The explicit path works (or both exist): never guess.
                    return Ok(unchanged_special_path(path));
                }
                if candidate.exists() {
                    return Ok(NormalizedSpecialPath {
                        path: candidate,
                        mapped: true,
                        attempted: path.to_path_buf(),
                    });
                }
                return Err(failed(
                    tool,
                    format!(
                        "file_not_found: '{}' does not exist. The host-configured {} directory is '{}'; the same file name was not found there either (checked '{}'). {}",
                        path.display(),
                        directory.prompt_label(),
                        configured.display(),
                        candidate.display(),
                        format!(
                            "Use {next_tool} with location='{}' after creating the file, or retry with the exact absolute path",
                            directory.json_key(),
                        ),
                    ),
                ));
            }
            SpecialPathPurpose::Write => {
                if conventional.is_dir() {
                    // The explicit destination (or its parent) exists, or
                    // both directories exist: the explicit path wins.
                    return Ok(unchanged_special_path(path));
                }
                if configured.is_dir() {
                    return Ok(NormalizedSpecialPath {
                        path: candidate,
                        mapped: true,
                        attempted: path.to_path_buf(),
                    });
                }
                return Ok(unchanged_special_path(path));
            }
        }
    }
    Ok(unchanged_special_path(path))
}

/// Convert an exact-match argument failure into structured retry guidance.
/// A wrong text match must never read as missing editing capability, and must
/// never detour into creating another file.
fn with_read_retry_guidance(path: &Path, error: ToolError) -> ToolError {
    let ToolError::Failed { tool, message } = error else {
        return error;
    };
    // Couples to PatchTool's exact diagnostic below; an exact-match count
    // other than one is still a model-argument recovery problem, not a
    // native mutation failure.
    if !message.contains("replacement block occurs") {
        return ToolError::Failed { tool, message };
    }
    ToolError::RetryRequired {
        tool,
        message: "Edit needs more information: old_text did not match exactly once".to_string(),
        recovery: serde_json::json!({
            "error": "old_text_mismatch",
            "target": path.to_string_lossy(),
            "next_tool": "filesystem.read",
        }),
    }
}

fn path_only_args(path: &Path) -> serde_json::Value {
    serde_json::json!({ "path": path.to_string_lossy() })
}

/// Return the exact existing line only when the target is a UTF-8 file with
/// one non-empty line. The caller uses this internally for a safe edit; the
/// contents are never placed in a generic validation diagnostic.
fn single_line_edit_text(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut non_empty = content.lines().filter(|line| !line.is_empty());
    let line = non_empty.next()?;
    if non_empty.next().is_some() {
        return None;
    }
    Some(line.to_string())
}

fn parse_old_new(
    args: &serde_json::Value,
    tool: &str,
    resolved_path: &Path,
) -> Result<(String, String), ToolError> {
    let object = args
        .as_object()
        .ok_or_else(|| invalid_args(tool, "args must be a JSON object"))?;
    let old_text = required_string_field(object, tool, "old_text").map_err(|_| {
        invalid_args(
            tool,
            format!(
                "missing 'old_text': read '{}' with filesystem.read, then retry with the exact text. Do not guess or use a placeholder such as 'Updated date'.",
                resolved_path.display()
            ),
        )
    })?;
    if old_text.is_empty() {
        return Err(invalid_args(
            tool,
            format!(
                "'old_text' must not be empty: call filesystem.read with path '{}' and copy one exact current line before retrying.",
                resolved_path.display()
            ),
        ));
    }
    let new_text = required_string_field(object, tool, "new_text").map_err(|_| {
        invalid_args(
            tool,
            format!(
                "missing 'new_text': retry filesystem.edit only after both old_text and new_text are present. For the current date, call system.time first and use its returned date; pass an empty string only when deleting the old text. Target: '{}'.",
                resolved_path.display()
            ),
        )
    })?;
    if new_text.trim().eq_ignore_ascii_case("updated date") {
        return Err(invalid_args(
            tool,
            "'new_text' is a placeholder, not a date. Call system.time first, then retry with its exact 'date' value.",
        ));
    }
    Ok((old_text, new_text))
}

fn optional_edit_text(
    object: &Map<String, Value>,
    tool: &str,
    field: &str,
) -> Result<Option<String>, ToolError> {
    let Some(value) = object.get(field) else {
        return Ok(None);
    };
    let text = value
        .as_str()
        .ok_or_else(|| invalid_args(tool, format!("'{field}' must be a string when provided")))?;
    Ok(Some(text.to_string()))
}

fn supplied_edit_texts(
    args: &serde_json::Value,
    tool: &str,
) -> Result<(Option<String>, Option<String>), ToolError> {
    let object = args
        .as_object()
        .ok_or_else(|| invalid_args(tool, "args must be a JSON object"))?;
    let old_text = optional_edit_text(object, tool, "old_text")?;
    if old_text.as_deref().is_some_and(str::is_empty) {
        return Err(invalid_args(tool, "'old_text' must not be empty"));
    }
    let new_text = optional_edit_text(object, tool, "new_text")?;
    if new_text
        .as_deref()
        .is_some_and(|text| text.trim().eq_ignore_ascii_case("updated date"))
    {
        return Err(invalid_args(
            tool,
            "'new_text' is a placeholder; use the host's current date or another concrete replacement",
        ));
    }
    Ok((old_text, new_text))
}

fn resolve_new_text_source(
    args: &serde_json::Value,
    tool: &str,
) -> Result<serde_json::Value, ToolError> {
    let Some(object) = args.as_object() else {
        return Ok(args.clone());
    };
    let Some(source) = object.get("new_text_source") else {
        return Ok(args.clone());
    };
    let source = source
        .as_str()
        .ok_or_else(|| invalid_args(tool, "'new_text_source' must be a string when provided"))?;
    let local = chrono::Local::now();
    let new_text = match source {
        "current_date" => local.format("%Y-%m-%d").to_string(),
        "current_time" => local.format("%H:%M:%S").to_string(),
        "current_datetime" => local.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        _ => {
            return Err(invalid_args(
                tool,
                "'new_text_source' must be one of: current_date, current_time, current_datetime",
            ));
        }
    };
    let mut normalized = object.clone();
    normalized.remove("new_text_source");
    normalized.insert("new_text".to_string(), Value::String(new_text));
    Ok(Value::Object(normalized))
}

fn missing_edit_argument(path: &Path, missing: Vec<&str>, preserved: Vec<&str>) -> ToolError {
    let mut recovery = serde_json::Map::new();
    recovery.insert(
        "error".to_string(),
        Value::String(if missing.len() == 1 && missing[0] == "old_text" {
            "old_text_required".to_string()
        } else {
            "missing_edit_argument".to_string()
        }),
    );
    recovery.insert(
        "missing".to_string(),
        Value::Array(
            missing
                .iter()
                .map(|field| Value::String((*field).to_string()))
                .collect(),
        ),
    );
    recovery.insert(
        "target".to_string(),
        Value::String(path.to_string_lossy().into_owned()),
    );
    recovery.insert(
        "preserved".to_string(),
        Value::Array(
            preserved
                .iter()
                .map(|field| Value::String((*field).to_string()))
                .collect(),
        ),
    );
    if missing.contains(&"old_text") {
        recovery.insert(
            "next_tool".to_string(),
            Value::String("filesystem.read".to_string()),
        );
    }
    let message = if missing.len() == 1 && missing[0] == "old_text" {
        "Edit needs more information: provide old_text or read the target file"
    } else if missing.len() == 1 && missing[0] == "new_text" {
        "Edit needs more information: provide new_text"
    } else {
        "Edit needs more information: provide old_text and new_text"
    };
    ToolError::RetryRequired {
        tool: EDIT_TOOL.to_string(),
        message: message.to_string(),
        recovery: Value::Object(recovery),
    }
}

/// Validate the unified edit arguments after pending values have been merged.
/// A single-line target is safe to infer; arbitrary multi-line files are not.
fn parse_unified_old_new(
    args: &serde_json::Value,
    resolved_path: &Path,
) -> Result<(String, String), ToolError> {
    let (mut old_text, new_text) = supplied_edit_texts(args, EDIT_TOOL)?;
    if old_text.is_none() && new_text.is_some() {
        old_text = single_line_edit_text(resolved_path);
    }
    let missing_old = old_text.is_none();
    let missing_new = new_text.is_none();
    if missing_old || missing_new {
        let mut missing = Vec::new();
        if missing_old {
            missing.push("old_text");
        }
        if missing_new {
            missing.push("new_text");
        }
        let mut preserved = Vec::new();
        if old_text.is_some() {
            preserved.push("old_text");
        }
        if new_text.is_some() {
            preserved.push("new_text");
        }
        return Err(missing_edit_argument(resolved_path, missing, preserved));
    }
    Ok((old_text.unwrap_or_default(), new_text.unwrap_or_default()))
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
/// the simple arguments into `PatchTool` arguments and delegates the actual
/// mutation to the existing filesystem patch broker — ticket validation,
/// exact-match safety, and atomic writes stay intact.
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
            description: "Edit one exact text block inside an existing file in an operating-system configured user directory such as Desktop or Documents. Use a semantic location such as desktop, never a constructed path. Read the file first with filesystem.read, then pass the exact old_text and replacement new_text; old_text must occur exactly once. Do not use a placeholder such as 'Updated date'. Use filesystem.replace_user_file for a complete replacement.".to_string(),
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
                        "description": "Existing file name such as note.txt, or a relative subpath such as notes/note.txt. If omitted, it is inferred only when the selected directory contains exactly one direct file. An absolute path is accepted only when it is inside the selected configured directory."
                    },
                    "old_text": {
                        "type": "string",
                        "description": "Exact current text to replace. It must occur exactly once in the file. Read the file first when unknown."
                    },
                    "new_text": {
                        "type": "string",
                        "description": "Replacement text."
                    }
                },
                "required": ["location", "old_text", "new_text"]
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
        let (old_text, new_text) = parse_old_new(&args, EDIT_USER_FILE_TOOL, &resolved.path)?;
        let mut output = self
            .inner
            .invoke(ctx, patch_args_for(&resolved.path, &old_text, &new_text))
            .await
            .map_err(|error| with_read_retry_guidance(&resolved.path, error))?;
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
/// Host-aware: a stale conventional special-directory path
/// (`$HOME/Desktop/...`) is normalized to the OS-configured location before
/// any capability ticket is minted, so the ticket always scopes the final
/// resolved path. Delegates the mutation to the existing filesystem patch
/// broker.
pub(crate) struct EditFileTool {
    inner: PatchTool,
    environment: HostEnvironment,
}

impl EditFileTool {
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

/// Resolve one explicit absolute path through stale conventional
/// special-directory compatibility. The returned path is what the ticket
/// must scope and what the broker must mutate.
fn resolve_edit_file_path(
    args: &serde_json::Value,
    environment: &HostEnvironment,
    tool: &str,
    next_tool: &str,
) -> Result<NormalizedSpecialPath, ToolError> {
    let object = args
        .as_object()
        .ok_or_else(|| invalid_args(tool, "args must be a JSON object"))?;
    let path = required_string_field(object, tool, "path")?;
    normalize_special_user_path(
        Path::new(&path),
        environment,
        tool,
        SpecialPathPurpose::ExistingFile,
        next_tool,
    )
}

#[async_trait::async_trait]
impl Tool for EditFileTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new(EDIT_FILE_TOOL),
            description: "Edit one exact text block inside an existing file at an explicit absolute host-native path. Use this only for arbitrary paths; for Desktop/Documents/etc. prefer filesystem.edit or filesystem.edit_user_file. A stale conventional path such as $HOME/Desktop is normalized to the OS-configured directory when unambiguous. Read the file first with filesystem.read, then pass the exact old_text and replacement new_text; old_text must occur exactly once. Do not use a placeholder such as 'Updated date'. Use filesystem.replace_user_file for a complete replacement.".to_string(),
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
                        "description": "Exact current text to replace. It must occur exactly once in the file. Read the file first when unknown."
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
        let normalized =
            resolve_edit_file_path(args, &self.environment, EDIT_FILE_TOOL, EDIT_USER_FILE_TOOL)
                .ok()?;
        self.inner
            .required_capability(&path_only_args(&normalized.path))
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let normalized = resolve_edit_file_path(
            &args,
            &self.environment,
            EDIT_FILE_TOOL,
            EDIT_USER_FILE_TOOL,
        )?;
        let (old_text, new_text) = parse_old_new(&args, EDIT_FILE_TOOL, &normalized.path)?;
        let mut output = self
            .inner
            .invoke(ctx, patch_args_for(&normalized.path, &old_text, &new_text))
            .await
            .map_err(|error| with_read_retry_guidance(&normalized.path, error))?;
        tag_normalized_from(&mut output, &normalized);
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
/// path. Host-aware like [`EditFileTool`]: a stale conventional
/// special-directory path is normalized before authorization. Reads,
/// appends, and delegates the write to the existing broker.
pub(crate) struct AppendFileTool {
    inner: WriteTool,
    limits: tool_filesystem::FilesystemLimits,
    environment: HostEnvironment,
}

impl AppendFileTool {
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

/// Resolve one explicit absolute append path through stale conventional
/// special-directory compatibility. Appends may create, so the write
/// evidence rules apply: an existing conventional parent (or both
/// directories) keeps the explicit path.
fn resolve_append_file_path(
    args: &serde_json::Value,
    environment: &HostEnvironment,
    tool: &str,
    next_tool: &str,
) -> Result<NormalizedSpecialPath, ToolError> {
    let object = args
        .as_object()
        .ok_or_else(|| invalid_args(tool, "args must be a JSON object"))?;
    let path = required_string_field(object, tool, "path")?;
    normalize_special_user_path(
        Path::new(&path),
        environment,
        tool,
        SpecialPathPurpose::Write,
        next_tool,
    )
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
        let normalized = resolve_append_file_path(
            args,
            &self.environment,
            APPEND_FILE_TOOL,
            APPEND_USER_FILE_TOOL,
        )
        .ok()?;
        self.inner
            .required_capability(&path_only_args(&normalized.path))
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let normalized = resolve_append_file_path(
            &args,
            &self.environment,
            APPEND_FILE_TOOL,
            APPEND_USER_FILE_TOOL,
        )?;
        let object = args
            .as_object()
            .ok_or_else(|| invalid_args(APPEND_FILE_TOOL, "args must be a JSON object"))?;
        let content = required_string_field(object, APPEND_FILE_TOOL, "content")?;
        if !normalized.path.is_absolute() {
            return Err(ToolError::InvalidArgs {
                tool: "filesystem".to_string(),
                message: "path must be absolute".to_string(),
            });
        }
        let appended =
            append_content_for(APPEND_FILE_TOOL, &normalized.path, &content, &self.limits)?;
        let mut output = self
            .inner
            .invoke(
                ctx,
                serde_json::json!({
                    "path": normalized.path.to_string_lossy(),
                    "content": appended,
                }),
            )
            .await?;
        tag_normalized_from(&mut output, &normalized);
        if let Some(object) = output.content.as_object_mut() {
            object.insert("appended".to_string(), Value::Bool(true));
        }
        Ok(output)
    }
}

/// Which target style a unified [`EditTool`] call uses. Exactly one style
/// is allowed per call so the model never has to guess precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditTargetStyle {
    /// `location` + `filename`: resolved through `HostEnvironment`.
    UserFile,
    /// `path`: explicit absolute path with stale conventional
    /// special-directory compatibility.
    ExplicitPath,
}

/// Keep older callers that put a bare filename in `path` working, but never
/// reinterpret an arbitrary path. A relative value is converted to the
/// semantic filename form and then resolved only if it uniquely identifies an
/// existing configured user-directory file.
fn normalize_unified_edit_args(
    args: &serde_json::Value,
    environment: &HostEnvironment,
    tool: &str,
) -> Result<serde_json::Value, ToolError> {
    let Some(object) = args.as_object() else {
        return Ok(args.clone());
    };
    let has_location = object.contains_key("location")
        || object.contains_key("directory_id")
        || object.contains_key("directory");
    let has_filename = object.contains_key("filename");
    let has_path = object.contains_key("path");
    if has_path && (has_location || has_filename) {
        let path = required_string_field(object, tool, "path")?;
        let mut semantic = object.clone();
        semantic.remove("path");
        let semantic_args = serde_json::Value::Object(semantic.clone());
        let semantic_target = resolve_edit_target(&semantic_args, environment, tool)?;
        let path_matches = if Path::new(&path).is_absolute() {
            let normalized = normalize_special_user_path(
                Path::new(&path),
                environment,
                tool,
                SpecialPathPurpose::ExistingFile,
                EDIT_USER_FILE_TOOL,
            )?;
            lexical_absolute_path(&normalized.path) == lexical_absolute_path(&semantic_target.path)
        } else if looks_like_windows_absolute_path(&path) {
            false
        } else {
            let (_, normalized) =
                normalize_filename(&path, &semantic_target.resolved_directory, tool)?;
            lexical_absolute_path(&normalized) == lexical_absolute_path(&semantic_target.path)
        };
        if !path_matches {
            return Err(invalid_args(
                tool,
                "when both target styles are provided, path must identify the same file as location+filename; otherwise send only one target style",
            ));
        }
        return Ok(semantic_args);
    }
    let has_semantic_target = object.contains_key("location")
        || object.contains_key("directory_id")
        || object.contains_key("directory")
        || object.contains_key("filename");
    let Some(path) = object.get("path").and_then(Value::as_str) else {
        return Ok(args.clone());
    };
    if has_semantic_target
        || Path::new(path).is_absolute()
        || looks_like_windows_absolute_path(path)
    {
        return Ok(args.clone());
    }
    let mut normalized = object.clone();
    normalized.remove("path");
    normalized.insert("filename".to_string(), Value::String(path.to_string()));
    Ok(Value::Object(normalized))
}

fn edit_target_style(args: &serde_json::Value, tool: &str) -> Result<EditTargetStyle, ToolError> {
    let object = args
        .as_object()
        .ok_or_else(|| invalid_args(tool, "args must be a JSON object"))?;
    let has_location = object.contains_key("location")
        || object.contains_key("directory_id")
        || object.contains_key("directory");
    let has_filename = object.contains_key("filename");
    let has_path = object.contains_key("path");
    if has_path && (has_location || has_filename) {
        return Err(invalid_args(
            tool,
            "provide either location+filename or path, not both",
        ));
    }
    if has_path
        && !has_location
        && !has_filename
        && object
            .get("path")
            .and_then(Value::as_str)
            .is_some_and(|path| {
                !Path::new(path).is_absolute() && !looks_like_windows_absolute_path(path)
            })
    {
        return Ok(EditTargetStyle::UserFile);
    }
    if has_location || has_filename {
        return Ok(EditTargetStyle::UserFile);
    }
    if has_path {
        return Ok(EditTargetStyle::ExplicitPath);
    }
    Err(invalid_args(
        tool,
        "an edit requires filename (with optional location) or path, plus old_text and new_text",
    ))
}

/// The single model-facing edit tool: one interface for both the semantic
/// user-directory style (`location` + `filename`) and the explicit absolute
/// path style (`path`). It routes to [`EditUserFileTool`] or
/// [`EditFileTool`], so stale-path normalization, read-retry guidance,
/// ticket validation, exact-match safety, and atomic writes are all
/// inherited rather than reimplemented.
pub(crate) struct EditTool {
    user_file: EditUserFileTool,
    file: EditFileTool,
    pending_edit: Arc<Mutex<Option<PendingEditState>>>,
}

impl EditTool {
    pub(crate) fn new(
        limits: tool_filesystem::FilesystemLimits,
        environment: HostEnvironment,
    ) -> Self {
        Self {
            user_file: EditUserFileTool::new(limits.clone(), environment.clone()),
            file: EditFileTool::new(limits, environment),
            pending_edit: Arc::new(Mutex::new(None)),
        }
    }

    fn pending_snapshot(&self) -> Option<PendingEditState> {
        self.pending_edit
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
    }

    fn merged_args(&self, args: &serde_json::Value) -> Result<MergedEditArgs, ToolError> {
        let source_args = resolve_new_text_source(args, EDIT_TOOL)?;
        let supplied = supplied_edit_texts(&source_args, EDIT_TOOL)?;
        let pending = self.pending_snapshot();
        let object = source_args
            .as_object()
            .ok_or_else(|| invalid_args(EDIT_TOOL, "args must be a JSON object"))?;
        let has_semantic_target = object.contains_key("location")
            || object.contains_key("directory_id")
            || object.contains_key("directory")
            || object.contains_key("filename");
        let mut merged = object.clone();
        let target = if has_semantic_target {
            Some(resolve_edit_target(
                &source_args,
                &self.user_file.environment,
                EDIT_TOOL,
            )?)
        } else if !object.contains_key("path") {
            let Some(pending) = pending.as_ref() else {
                return Ok(MergedEditArgs {
                    args: source_args,
                    target: None,
                    explicit_target: false,
                    supplied_old_text: supplied.0,
                    supplied_new_text: supplied.1,
                });
            };
            merged.insert(
                "location".to_string(),
                Value::String(pending.target.directory.json_key().to_string()),
            );
            merged.insert(
                "filename".to_string(),
                Value::String(pending.target.relative_path.to_string_lossy().into_owned()),
            );
            Some(pending.target.clone())
        } else {
            None
        };

        let same_target = target
            .as_ref()
            .zip(pending.as_ref())
            .is_some_and(|(target, pending)| target == &pending.target);
        if same_target {
            if !merged.contains_key("old_text") {
                if let Some(old_text) = pending.as_ref().and_then(|state| state.old_text.clone()) {
                    merged.insert("old_text".to_string(), Value::String(old_text));
                }
            }
            if !merged.contains_key("new_text") {
                if let Some(new_text) = pending.as_ref().and_then(|state| state.new_text.clone()) {
                    merged.insert("new_text".to_string(), Value::String(new_text));
                }
            }
        }

        Ok(MergedEditArgs {
            args: Value::Object(merged),
            target,
            explicit_target: has_semantic_target || object.contains_key("path"),
            supplied_old_text: supplied.0,
            supplied_new_text: supplied.1,
        })
    }

    fn remember_edit_attempt(
        &self,
        target: Option<ResolvedEditTarget>,
        supplied_old_text: Option<String>,
        supplied_new_text: Option<String>,
    ) {
        if let Some(target) = target {
            if let Ok(mut guard) = self.pending_edit.lock() {
                if guard
                    .as_ref()
                    .is_some_and(|pending| pending.target == target)
                {
                    if let Some(pending) = guard.as_mut() {
                        if supplied_old_text.is_some() {
                            pending.old_text = supplied_old_text;
                        }
                        if supplied_new_text.is_some() {
                            pending.new_text = supplied_new_text;
                        }
                    }
                } else {
                    *guard = Some(PendingEditState {
                        target,
                        old_text: supplied_old_text,
                        new_text: supplied_new_text,
                    });
                }
            }
        }
    }

    fn clear_pending_edit(&self) {
        if let Ok(mut guard) = self.pending_edit.lock() {
            *guard = None;
        }
    }

    fn discard_if_explicit_target_changed(&self, merged: &MergedEditArgs) {
        if !merged.explicit_target {
            return;
        }
        if let Ok(mut guard) = self.pending_edit.lock() {
            let same_target = merged
                .target
                .as_ref()
                .zip(guard.as_ref())
                .is_some_and(|(target, pending)| target == &pending.target);
            if !same_target {
                *guard = None;
            }
        }
    }

    fn keep_pending_after_error(error: &ToolError) -> bool {
        match error {
            ToolError::RetryRequired { .. } => true,
            ToolError::Failed { message, .. } => message.contains("replacement block occurs"),
            _ => false,
        }
    }
}

#[async_trait::async_trait]
impl Tool for EditTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new(EDIT_TOOL),
            description: "Edit one exact text block inside an existing file. Use location plus filename for Desktop/Documents/etc., or path for an explicit absolute path. Target, old_text, and new_text may be completed across same-turn retries; the host preserves valid pending values. A UTF-8 file with exactly one non-empty line can be edited without first reading it. For date/time replacements, new_text_source may be current_date, current_time, or current_datetime. Exact-match safety, capability tickets, symlink checks, and atomic mutation remain enforced by the filesystem broker.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "location": {
                        "type": "string",
                        "enum": USER_DIRECTORY_ENUM,
                        "description": "Semantic operating-system user directory identifier. Use with filename, or omit filename only when the location contains exactly one direct file; do not combine with path."
                    },
                    "filename": {
                        "type": "string",
                        "description": "Existing file name such as note.txt, or a relative subpath such as notes/note.txt. Use with location, or omit only when that location contains exactly one direct file; do not combine with path."
                    },
                    "path": {
                        "type": "string",
                        "description": "Absolute host-native path of the existing file. Use instead of location+filename for arbitrary paths; do not combine with location or filename. A relative value is accepted only as a compatibility alias for a unique existing configured user-directory filename."
                    },
                    "old_text": {
                        "type": "string",
                        "description": "Exact current text to replace. It must occur exactly once in the file. Read the file first when unknown."
                    },
                    "new_text": {
                        "type": "string",
                        "description": "Replacement text."
                    },
                    "new_text_source": {
                        "type": "string",
                        "enum": ["current_date", "current_time", "current_datetime"],
                        "description": "Resolve replacement text from the native local clock. Use instead of new_text for basic date/time edits."
                    }
                }
            }),
            effects: vec![tool_core::ToolEffect::FilesystemWrite],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        let merged = match self.merged_args(args) {
            Ok(merged) => merged,
            Err(_) => {
                self.clear_pending_edit();
                return None;
            }
        };
        self.discard_if_explicit_target_changed(&merged);
        let normalized =
            normalize_unified_edit_args(&merged.args, &self.user_file.environment, EDIT_TOOL)
                .ok()?;
        match edit_target_style(&normalized, EDIT_TOOL).ok()? {
            EditTargetStyle::UserFile => self.user_file.required_capability(&normalized),
            EditTargetStyle::ExplicitPath => self.file.required_capability(&normalized),
        }
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let merged = match self.merged_args(&args) {
            Ok(merged) => merged,
            Err(error) => {
                self.clear_pending_edit();
                return Err(error);
            }
        };
        let normalized =
            match normalize_unified_edit_args(&merged.args, &self.user_file.environment, EDIT_TOOL)
            {
                Ok(normalized) => normalized,
                Err(error) => {
                    self.clear_pending_edit();
                    return Err(error);
                }
            };
        let style = match edit_target_style(&normalized, EDIT_TOOL) {
            Ok(style) => style,
            Err(error) => {
                self.clear_pending_edit();
                return Err(error);
            }
        };
        match style {
            EditTargetStyle::UserFile => {
                let target = match resolve_edit_target(
                    &normalized,
                    &self.user_file.environment,
                    EDIT_TOOL,
                ) {
                    Ok(target) => target,
                    Err(error) => {
                        self.clear_pending_edit();
                        return Err(error);
                    }
                };
                self.remember_edit_attempt(
                    Some(target.clone()),
                    merged.supplied_old_text,
                    merged.supplied_new_text,
                );
                let (old_text, new_text) = match parse_unified_old_new(&normalized, &target.path) {
                    Ok(values) => values,
                    Err(error) => return Err(error),
                };
                let mut delegated = normalized;
                if let Some(object) = delegated.as_object_mut() {
                    object.insert("old_text".to_string(), Value::String(old_text));
                    object.insert("new_text".to_string(), Value::String(new_text));
                }
                match self.user_file.invoke(ctx, delegated).await {
                    Ok(output) => {
                        self.clear_pending_edit();
                        Ok(output)
                    }
                    Err(error) => {
                        if !Self::keep_pending_after_error(&error) {
                            self.clear_pending_edit();
                        }
                        Err(error)
                    }
                }
            }
            EditTargetStyle::ExplicitPath => {
                self.clear_pending_edit();
                let target = match resolve_edit_file_path(
                    &normalized,
                    &self.file.environment,
                    EDIT_FILE_TOOL,
                    EDIT_USER_FILE_TOOL,
                ) {
                    Ok(target) => target,
                    Err(error) => return Err(error),
                };
                let (old_text, new_text) = parse_unified_old_new(&normalized, &target.path)?;
                let mut delegated = normalized;
                if let Some(object) = delegated.as_object_mut() {
                    object.insert("old_text".to_string(), Value::String(old_text));
                    object.insert("new_text".to_string(), Value::String(new_text));
                }
                self.file.invoke(ctx, delegated).await
            }
        }
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
    async fn edit_user_file_rejects_missing_old_text_without_overwriting() {
        let home = std::env::temp_dir().join(format!(
            "utsuwa-edit-user-file-whole-{}",
            std::process::id()
        ));
        let (_home, desktop) = edit_harness(&home);
        let target = desktop.join("note.txt");
        std::fs::write(&target, "old contents").unwrap();
        let tool = EditUserFileTool::new(
            tool_filesystem::FilesystemLimits::default(),
            environment(&home, &desktop),
        );
        let error = tool
            .invoke(
                ticketed_context(&target),
                serde_json::json!({
                    "location": "desktop",
                    "filename": "note.txt",
                    "new_text": "complete replacement",
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::InvalidArgs { .. }), "{error:?}");
        assert!(error.to_string().contains("filesystem.read"));
        assert!(error.to_string().contains("Updated date"));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old contents");
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
        assert!(
            matches!(missing, ToolError::RetryRequired { .. }),
            "{missing:?}"
        );
        assert!(missing.to_string().contains("old_text_mismatch"));
        assert!(missing.to_string().contains("filesystem.read"));
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
            matches!(ambiguous, ToolError::RetryRequired { .. }),
            "{ambiguous:?}"
        );
        assert!(ambiguous.to_string().contains("old_text_mismatch"));
        let placeholder = tool
            .invoke(
                ticketed_context(&target),
                serde_json::json!({
                    "location": "desktop",
                    "filename": "note.txt",
                    "old_text": "alpha beta alpha",
                    "new_text": "Updated date",
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(placeholder, ToolError::InvalidArgs { .. }));
        assert!(placeholder.to_string().contains("system.time"));
        // All failed attempts leave the file untouched.
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
        let tool = EditFileTool::new(
            tool_filesystem::FilesystemLimits::default(),
            environment(&dir, &dir.join("Escritorio")),
        );
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
        let home =
            std::env::temp_dir().join(format!("utsuwa-edit-file-relative-{}", std::process::id()));
        let tool = EditFileTool::new(
            tool_filesystem::FilesystemLimits::default(),
            environment(&home, &home.join("Escritorio")),
        );
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
        let tool = AppendFileTool::new(
            tool_filesystem::FilesystemLimits::default(),
            environment(&dir, &dir.join("Escritorio")),
        );
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

    fn stale_home(name: &str) -> (PathBuf, PathBuf) {
        let home = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let desktop = home.join("Escritorio");
        std::fs::create_dir_all(&desktop).unwrap();
        (home, desktop)
    }

    #[tokio::test]
    async fn edit_file_normalizes_a_stale_conventional_desktop_path() {
        let (home, desktop) = stale_home("utsuwa-edit-remap");
        let target = desktop.join("note.txt");
        std::fs::write(&target, "old content here").unwrap();
        // The conventional directory is never created.
        let stale = home.join("Desktop").join("note.txt");
        let tool = EditFileTool::new(
            tool_filesystem::FilesystemLimits::default(),
            environment(&home, &desktop),
        );
        let args = serde_json::json!({
            "path": stale.to_string_lossy(),
            "old_text": "old",
            "new_text": "new",
        });
        // The ticket must scope the normalized target, not the stale path.
        let requirement = tool
            .required_capability(&args)
            .expect("the normalized target needs filesystem write");
        assert_eq!(requirement.capability, Capability::FilesystemWrite);
        assert_eq!(
            requirement.resource,
            Resource::Path(target.canonicalize().unwrap_or(target.clone()))
        );
        let output = tool.invoke(ticketed_context(&target), args).await.unwrap();
        assert_eq!(output.content["path"], target.to_string_lossy().as_ref());
        assert_eq!(output.content["updated"], true);
        assert_eq!(
            output.content["normalized_from"],
            stale.to_string_lossy().as_ref()
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "new content here"
        );
        assert!(!home.join("Desktop").exists());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn edit_file_keeps_an_explicit_path_when_both_locations_exist() {
        let (home, desktop) = stale_home("utsuwa-edit-ambiguous");
        let conventional_dir = home.join("Desktop");
        std::fs::create_dir_all(&conventional_dir).unwrap();
        let conventional = conventional_dir.join("note.txt");
        std::fs::write(&conventional, "conventional old").unwrap();
        let configured = desktop.join("note.txt");
        std::fs::write(&configured, "configured old").unwrap();
        let tool = EditFileTool::new(
            tool_filesystem::FilesystemLimits::default(),
            environment(&home, &desktop),
        );
        let output = tool
            .invoke(
                ticketed_context(&conventional),
                serde_json::json!({
                    "path": conventional.to_string_lossy(),
                    "old_text": "old",
                    "new_text": "new",
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            output.content["path"],
            conventional.to_string_lossy().as_ref()
        );
        assert!(output.content.get("normalized_from").is_none());
        assert_eq!(
            std::fs::read_to_string(&conventional).unwrap(),
            "conventional new"
        );
        assert_eq!(
            std::fs::read_to_string(&configured).unwrap(),
            "configured old"
        );
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn edit_file_reports_a_structured_error_when_nothing_exists() {
        let (home, desktop) = stale_home("utsuwa-edit-missing");
        let stale = home.join("Desktop").join("absent.txt");
        let tool = EditFileTool::new(
            tool_filesystem::FilesystemLimits::default(),
            environment(&home, &desktop),
        );
        let error = tool
            .invoke(
                ToolContext::new(Principal::Agent(AgentId::new("edit-missing"))),
                serde_json::json!({
                    "path": stale.to_string_lossy(),
                    "old_text": "old",
                    "new_text": "new",
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::Failed { .. }), "{error:?}");
        let message = error.to_string();
        assert!(message.contains("file_not_found"), "{message}");
        assert!(
            message.contains(&stale.to_string_lossy().into_owned()),
            "{message}"
        );
        assert!(
            message.contains(&desktop.to_string_lossy().into_owned()),
            "{message}"
        );
        assert!(message.contains("filesystem.edit_user_file"), "{message}");
        // No capability is minted for a call that cannot resolve a target.
        assert!(tool
            .required_capability(&serde_json::json!({
                "path": stale.to_string_lossy(),
                "old_text": "old",
                "new_text": "new",
            }))
            .is_none());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn edit_reports_read_retry_guidance_for_unknown_old_text() {
        let (home, desktop) = stale_home("utsuwa-edit-retry");
        let target = desktop.join("note.txt");
        std::fs::write(&target, "exact current contents").unwrap();
        let tool = EditFileTool::new(
            tool_filesystem::FilesystemLimits::default(),
            environment(&home, &desktop),
        );
        let error = tool
            .invoke(
                ticketed_context(&target),
                serde_json::json!({
                    "path": target.to_string_lossy(),
                    "old_text": "guessed text",
                    "new_text": "new",
                }),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, ToolError::RetryRequired { .. }),
            "{error:?}"
        );
        let message = error.to_string();
        assert!(message.contains("old_text_mismatch"), "{message}");
        assert!(message.contains("filesystem.read"), "{message}");
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "exact current contents"
        );
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn edit_rejects_missing_old_text_without_overwriting() {
        let (home, desktop) = stale_home("utsuwa-edit-missing-old");
        let target = desktop.join("note.txt");
        std::fs::write(&target, "exact current contents").unwrap();
        let tool = EditFileTool::new(
            tool_filesystem::FilesystemLimits::default(),
            environment(&home, &desktop),
        );
        let metadata = tool.metadata();
        let required = metadata.input_schema["required"]
            .as_array()
            .expect("edit schema required must be an array");
        assert!(required.iter().any(|field| field == "old_text"));

        // Missing old_text must never turn a partial edit into a whole-file overwrite.
        let missing = tool
            .invoke(
                ticketed_context(&target),
                serde_json::json!({
                    "path": target.to_string_lossy(),
                    "new_text": "new",
                }),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(missing, ToolError::InvalidArgs { .. }),
            "{missing:?}"
        );
        assert!(missing.to_string().contains("filesystem.read"));
        assert!(!missing.to_string().contains("exact current contents"));
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "exact current contents"
        );

        // Empty old_text would match everywhere, so it stays rejected with guidance.
        let empty = tool
            .invoke(
                ticketed_context(&target),
                serde_json::json!({
                    "path": target.to_string_lossy(),
                    "old_text": "",
                    "new_text": "new",
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(empty, ToolError::InvalidArgs { .. }), "{empty:?}");
        assert!(empty.to_string().contains("filesystem.read"), "{empty:?}");
        // Missing new_text names the fix as well.
        let no_new = tool
            .invoke(
                ticketed_context(&target),
                serde_json::json!({
                    "path": target.to_string_lossy(),
                    "old_text": "exact current contents",
                }),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(no_new, ToolError::InvalidArgs { .. }),
            "{no_new:?}"
        );
        assert!(no_new.to_string().contains("new_text"), "{no_new:?}");
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "exact current contents"
        );
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn edit_file_does_not_remap_nested_or_foreign_lookalikes() {
        let (home, desktop) = stale_home("utsuwa-edit-lookalike");
        // Nested lookalike: the parent is not directly $HOME/Desktop.
        let nested_dir = home.join("projects").join("Desktop");
        std::fs::create_dir_all(&nested_dir).unwrap();
        let nested = nested_dir.join("note.txt");
        std::fs::write(&nested, "nested old").unwrap();
        let tool = EditFileTool::new(
            tool_filesystem::FilesystemLimits::default(),
            environment(&home, &desktop),
        );
        tool.invoke(
            ticketed_context(&nested),
            serde_json::json!({
                "path": nested.to_string_lossy(),
                "old_text": "old",
                "new_text": "new",
            }),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&nested).unwrap(), "nested new");

        // Foreign root: an explicit path outside $HOME is never rewritten.
        let foreign_root =
            std::env::temp_dir().join(format!("utsuwa-edit-foreign-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&foreign_root);
        let foreign_dir = foreign_root.join("Desktop");
        std::fs::create_dir_all(&foreign_dir).unwrap();
        let foreign = foreign_dir.join("note.txt");
        std::fs::write(&foreign, "foreign old").unwrap();
        tool.invoke(
            ticketed_context(&foreign),
            serde_json::json!({
                "path": foreign.to_string_lossy(),
                "old_text": "old",
                "new_text": "new",
            }),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&foreign).unwrap(), "foreign new");
        std::fs::remove_dir_all(&home).unwrap();
        std::fs::remove_dir_all(&foreign_root).unwrap();
    }

    #[tokio::test]
    async fn append_file_normalizes_a_stale_conventional_path() {
        let (home, desktop) = stale_home("utsuwa-append-remap");
        let target = desktop.join("note.txt");
        std::fs::write(&target, "line one\n").unwrap();
        let stale = home.join("Desktop").join("note.txt");
        let tool = AppendFileTool::new(
            tool_filesystem::FilesystemLimits::default(),
            environment(&home, &desktop),
        );
        let requirement = tool
            .required_capability(&serde_json::json!({
                "path": stale.to_string_lossy(),
                "content": "line two\n",
            }))
            .expect("the normalized target needs filesystem write");
        assert_eq!(
            requirement.resource,
            Resource::Path(target.canonicalize().unwrap_or(target.clone()))
        );
        let output = tool
            .invoke(
                ticketed_context(&target),
                serde_json::json!({
                    "path": stale.to_string_lossy(),
                    "content": "line two\n",
                }),
            )
            .await
            .unwrap();
        assert_eq!(output.content["appended"], true);
        assert_eq!(
            output.content["normalized_from"],
            stale.to_string_lossy().as_ref()
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "line one\nline two\n"
        );
        assert!(!home.join("Desktop").exists());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn append_file_keeps_explicit_path_when_conventional_parent_exists() {
        let (home, desktop) = stale_home("utsuwa-append-ambiguous");
        let conventional_dir = home.join("Desktop");
        std::fs::create_dir_all(&conventional_dir).unwrap();
        let conventional = conventional_dir.join("note.txt");
        std::fs::write(&conventional, "conventional\n").unwrap();
        let configured = desktop.join("note.txt");
        std::fs::write(&configured, "configured\n").unwrap();
        let tool = AppendFileTool::new(
            tool_filesystem::FilesystemLimits::default(),
            environment(&home, &desktop),
        );
        tool.invoke(
            ticketed_context(&conventional),
            serde_json::json!({
                "path": conventional.to_string_lossy(),
                "content": "more\n",
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&conventional).unwrap(),
            "conventional\nmore\n"
        );
        assert_eq!(
            std::fs::read_to_string(&configured).unwrap(),
            "configured\n"
        );
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn host_aware_write_normalizes_a_stale_conventional_path() {
        let (home, desktop) = stale_home("utsuwa-write-remap");
        let stale = home.join("Desktop").join("fresh.txt");
        let candidate = desktop.join("fresh.txt");
        let tool = HostAwareWriteTool::new(
            tool_filesystem::FilesystemLimits::default(),
            environment(&home, &desktop),
        );
        let requirement = tool
            .required_capability(&serde_json::json!({
                "path": stale.to_string_lossy(),
                "content": "hello",
            }))
            .expect("the normalized target needs filesystem write");
        // The missing file resolves against its nearest existing ancestor.
        assert_eq!(
            requirement.resource,
            Resource::Path(desktop.canonicalize().unwrap().join("fresh.txt"))
        );
        let output = tool
            .invoke(
                ticketed_context(&candidate),
                serde_json::json!({
                    "path": stale.to_string_lossy(),
                    "content": "hello",
                }),
            )
            .await
            .unwrap();
        assert_eq!(output.content["path"], candidate.to_string_lossy().as_ref());
        assert_eq!(
            output.content["normalized_from"],
            stale.to_string_lossy().as_ref()
        );
        assert_eq!(std::fs::read_to_string(&candidate).unwrap(), "hello");
        assert!(!home.join("Desktop").exists());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn host_aware_write_keeps_explicit_path_when_both_directories_exist() {
        let (home, desktop) = stale_home("utsuwa-write-ambiguous");
        let conventional_dir = home.join("Desktop");
        std::fs::create_dir_all(&conventional_dir).unwrap();
        let conventional = conventional_dir.join("fresh.txt");
        let configured = desktop.join("fresh.txt");
        let tool = HostAwareWriteTool::new(
            tool_filesystem::FilesystemLimits::default(),
            environment(&home, &desktop),
        );
        tool.invoke(
            ticketed_context(&conventional),
            serde_json::json!({
                "path": conventional.to_string_lossy(),
                "content": "explicit",
            }),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&conventional).unwrap(), "explicit");
        assert!(!configured.exists());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn unified_edit_routes_location_style_to_the_configured_directory() {
        let (home, desktop) = stale_home("utsuwa-edit-unified-location");
        let target = desktop.join("note.txt");
        std::fs::write(&target, "hello").unwrap();
        let tool = EditTool::new(
            tool_filesystem::FilesystemLimits::default(),
            environment(&home, &desktop),
        );
        let output = tool
            .invoke(
                ticketed_context(&target),
                serde_json::json!({
                    "location": "desktop",
                    "filename": "note.txt",
                    "old_text": "hello",
                    "new_text": "hello world",
                }),
            )
            .await
            .unwrap();
        assert_eq!(output.content["path"], target.to_string_lossy().as_ref());
        assert_eq!(output.content["updated"], true);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello world");
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn unified_edit_infers_the_only_non_empty_line_without_reading() {
        let (home, desktop) = stale_home("utsuwa-edit-single-line-inference");
        let target = desktop.join("note.txt");
        std::fs::write(&target, "2026-09-08\n").unwrap();
        let tool = EditTool::new(
            tool_filesystem::FilesystemLimits::default(),
            environment(&home, &desktop),
        );
        let schema = tool.metadata().input_schema;
        assert!(schema.get("required").is_none());
        assert_eq!(
            schema["properties"]["new_text_source"]["enum"],
            serde_json::json!(["current_date", "current_time", "current_datetime"])
        );
        let output = tool
            .invoke(
                ticketed_context(&target),
                serde_json::json!({
                    "location": "desktop",
                    "filename": "note.txt",
                    "new_text": "2026-09-09",
                }),
            )
            .await
            .unwrap();
        assert_eq!(output.content["updated"], true);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "2026-09-09\n");
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn unified_edit_merges_new_text_across_a_multiline_retry() {
        let (home, desktop) = stale_home("utsuwa-edit-merge-new");
        let target = desktop.join("note.txt");
        std::fs::write(&target, "Version 1\nDetails\n").unwrap();
        let tool = EditTool::new(
            tool_filesystem::FilesystemLimits::default(),
            environment(&home, &desktop),
        );
        let first = tool
            .invoke(
                ticketed_context(&target),
                serde_json::json!({
                    "location": "desktop",
                    "filename": "note.txt",
                    "new_text": "Version 2",
                }),
            )
            .await
            .unwrap_err();
        match &first {
            ToolError::RetryRequired { recovery, .. } => {
                assert_eq!(recovery["error"], "old_text_required");
                assert_eq!(recovery["preserved"], serde_json::json!(["new_text"]));
                assert_eq!(recovery["next_tool"], "filesystem.read");
            }
            other => panic!("expected structured retry, got {other:?}"),
        }
        assert!(!first.to_string().contains("Version 1"));

        let retry = serde_json::json!({ "old_text": "Version 1" });
        let requirement = tool
            .required_capability(&retry)
            .expect("pending target must authorize the retry");
        assert_eq!(
            requirement.resource,
            Resource::Path(target.canonicalize().unwrap_or(target.clone()))
        );
        tool.invoke(ticketed_context(&target), retry).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "Version 2\nDetails\n"
        );
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn unified_edit_preserves_old_text_for_a_reverse_partial_retry() {
        let (home, desktop) = stale_home("utsuwa-edit-merge-old");
        let target = desktop.join("note.txt");
        std::fs::write(&target, "Version 1\nDetails\n").unwrap();
        let tool = EditTool::new(
            tool_filesystem::FilesystemLimits::default(),
            environment(&home, &desktop),
        );
        let first = tool
            .invoke(
                ticketed_context(&target),
                serde_json::json!({
                    "location": "desktop",
                    "filename": "note.txt",
                    "old_text": "Version 1",
                }),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(first, ToolError::RetryRequired { .. }),
            "{first:?}"
        );
        tool.invoke(
            ticketed_context(&target),
            serde_json::json!({ "new_text": "Version 2" }),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "Version 2\nDetails\n"
        );
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn unified_edit_resets_pending_values_when_the_target_changes() {
        let (home, desktop) = stale_home("utsuwa-edit-target-reset");
        let documents = home.join("Documentos");
        std::fs::create_dir_all(&documents).unwrap();
        let note = desktop.join("note.txt");
        let report = documents.join("report.txt");
        std::fs::write(&note, "Note 1\nDetails\n").unwrap();
        std::fs::write(&report, "Report 1\nDetails\n").unwrap();
        let mut host = environment(&home, &desktop);
        host.user_dirs.documents = Some(documents.clone());
        let tool = EditTool::new(tool_filesystem::FilesystemLimits::default(), host);

        let first = tool
            .invoke(
                ticketed_context(&note),
                serde_json::json!({
                    "location": "desktop",
                    "filename": "note.txt",
                    "new_text": "Note 2",
                }),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(first, ToolError::RetryRequired { .. }),
            "{first:?}"
        );

        let changed_target = tool
            .invoke(
                ticketed_context(&report),
                serde_json::json!({
                    "location": "documents",
                    "filename": "report.txt",
                    "old_text": "Report 1",
                }),
            )
            .await
            .unwrap_err();
        match changed_target {
            ToolError::RetryRequired { recovery, .. } => {
                assert_eq!(recovery["target"], report.to_string_lossy().as_ref());
                assert_eq!(recovery["preserved"], serde_json::json!(["old_text"]));
            }
            other => panic!("expected report retry, got {other:?}"),
        }

        tool.invoke(
            ticketed_context(&report),
            serde_json::json!({ "new_text": "Report 2" }),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&note).unwrap(), "Note 1\nDetails\n");
        assert_eq!(
            std::fs::read_to_string(&report).unwrap(),
            "Report 2\nDetails\n"
        );
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn unified_edit_resolves_new_text_from_the_native_clock() {
        let (home, desktop) = stale_home("utsuwa-edit-text-source");
        let target = desktop.join("note.txt");
        std::fs::write(&target, "Version 1\nDetails\n").unwrap();
        let tool = EditTool::new(
            tool_filesystem::FilesystemLimits::default(),
            environment(&home, &desktop),
        );
        let expected = chrono::Local::now().format("%Y-%m-%d").to_string();
        tool.invoke(
            ticketed_context(&target),
            serde_json::json!({
                "location": "desktop",
                "filename": "note.txt",
                "old_text": "Version 1",
                "new_text_source": "current_date",
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            format!("{expected}\nDetails\n")
        );
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn unified_edit_resolves_a_unique_bare_filename_to_desktop() {
        let (home, desktop) = stale_home("utsuwa-edit-bare-filename");
        let target = desktop.join("note.txt");
        std::fs::write(&target, "old date").unwrap();
        let tool = EditTool::new(
            tool_filesystem::FilesystemLimits::default(),
            environment(&home, &desktop),
        );
        let output = tool
            .invoke(
                ticketed_context(&target),
                serde_json::json!({
                    "filename": "note.txt",
                    "old_text": "old date",
                    "new_text": "2026-09-09",
                }),
            )
            .await
            .unwrap();
        assert_eq!(output.content["path"], target.to_string_lossy().as_ref());
        assert_eq!(output.content["location"], "desktop");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "2026-09-09");
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn unified_edit_resolves_a_relative_path_alias_to_desktop() {
        let (home, desktop) = stale_home("utsuwa-edit-relative-path-alias");
        let target = desktop.join("note.txt");
        std::fs::write(&target, "old date").unwrap();
        let tool = EditTool::new(
            tool_filesystem::FilesystemLimits::default(),
            environment(&home, &desktop),
        );
        let requirement = tool
            .required_capability(&serde_json::json!({
                "path": "note.txt",
                "old_text": "old date",
                "new_text": "2026-09-09",
            }))
            .expect("the relative compatibility target needs filesystem write");
        assert_eq!(
            requirement.resource,
            Resource::Path(target.canonicalize().unwrap_or(target.clone()))
        );
        let output = tool
            .invoke(
                ticketed_context(&target),
                serde_json::json!({
                    "path": "note.txt",
                    "old_text": "old date",
                    "new_text": "2026-09-09",
                }),
            )
            .await
            .unwrap();
        assert_eq!(output.content["path"], target.to_string_lossy().as_ref());
        assert_eq!(output.content["location"], "desktop");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "2026-09-09");
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn unified_edit_reuses_the_last_target_for_a_targetless_retry() {
        let (home, desktop) = stale_home("utsuwa-edit-targetless-retry");
        let target = desktop.join("note.txt");
        std::fs::write(&target, "Updated date\nDetails").unwrap();
        let tool = EditTool::new(
            tool_filesystem::FilesystemLimits::default(),
            environment(&home, &desktop),
        );
        let first = tool
            .invoke(
                ticketed_context(&target),
                serde_json::json!({
                    "location": "desktop",
                    "filename": "note.txt",
                    "new_text": "2026-09-09",
                }),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(first, ToolError::RetryRequired { .. }),
            "{first:?}"
        );

        let retry_args = serde_json::json!({
            "old_text": "Updated date",
            "new_text": "2026-09-09",
        });
        let requirement = tool
            .required_capability(&retry_args)
            .expect("the remembered target needs filesystem write");
        assert_eq!(
            requirement.resource,
            Resource::Path(target.canonicalize().unwrap_or(target.clone()))
        );
        let output = tool
            .invoke(ticketed_context(&target), retry_args)
            .await
            .unwrap();
        assert_eq!(output.content["path"], target.to_string_lossy().as_ref());
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "2026-09-09\nDetails"
        );
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn unified_edit_routes_path_style_through_normalization() {
        let (home, desktop) = stale_home("utsuwa-edit-unified-path");
        let target = desktop.join("note.txt");
        std::fs::write(&target, "old value").unwrap();
        let stale = home.join("Desktop").join("note.txt");
        let tool = EditTool::new(
            tool_filesystem::FilesystemLimits::default(),
            environment(&home, &desktop),
        );
        let requirement = tool
            .required_capability(&serde_json::json!({
                "path": stale.to_string_lossy(),
                "old_text": "old",
                "new_text": "new",
            }))
            .expect("the normalized target needs filesystem write");
        assert_eq!(
            requirement.resource,
            Resource::Path(target.canonicalize().unwrap_or(target.clone()))
        );
        tool.invoke(
            ticketed_context(&target),
            serde_json::json!({
                "path": stale.to_string_lossy(),
                "old_text": "old",
                "new_text": "new",
            }),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new value");
        assert!(!home.join("Desktop").exists());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn unified_edit_rejects_mixed_or_missing_target_styles() {
        let (home, desktop) = stale_home("utsuwa-edit-unified-styles");
        let target = desktop.join("note.txt");
        std::fs::write(&target, "hello").unwrap();
        let tool = EditTool::new(
            tool_filesystem::FilesystemLimits::default(),
            environment(&home, &desktop),
        );
        // A redundant matching path is safe and keeps older/model-generated
        // calls from failing before the edit reaches the filesystem broker.
        tool.invoke(
            ticketed_context(&target),
            serde_json::json!({
                "location": "desktop",
                "filename": "note.txt",
                "path": target.to_string_lossy(),
                "old_text": "hello",
                "new_text": "hi",
            }),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hi");
        std::fs::write(&target, "hello").unwrap();

        tool.invoke(
            ticketed_context(&target),
            serde_json::json!({
                "location": "desktop",
                "filename": "note.txt",
                "path": "note.txt",
                "old_text": "hello",
                "new_text": "hi",
            }),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hi");
        std::fs::write(&target, "hello").unwrap();

        let error = tool
            .invoke(
                ticketed_context(&target),
                serde_json::json!({
                    "location": "desktop",
                    "filename": "note.txt",
                    "path": home.join("other.txt").to_string_lossy(),
                    "old_text": "hello",
                    "new_text": "hi",
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::InvalidArgs { .. }), "{error:?}");
        assert!(error.to_string().contains("same file"));

        // A location-only call can infer the sole direct file in the
        // configured directory, which is useful for small-model retries.
        tool.invoke(
            ticketed_context(&target),
            serde_json::json!({
                "location": "desktop",
                "old_text": "hello",
                "new_text": "hi",
            }),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hi");
        std::fs::write(&target, "hello").unwrap();

        let error = tool
            .invoke(
                ticketed_context(&target),
                serde_json::json!({
                    "old_text": "hello",
                    "new_text": "hi",
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::InvalidArgs { .. }), "{error:?}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello");
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
        assert!(enriched.to_string().contains("filesystem.create_user_file"));
    }
}
