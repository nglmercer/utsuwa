//! Shared errors, metadata rewriting, and user-target resolution.

use crate::edit_args::{
    normalize_legacy_directory_shape, normalize_special_user_path, SpecialPathPurpose,
};
use file_target::{
    ConversationFileContext, FileRef, FileResolver, FileTarget, FileTargetError,
    FilesystemErrorCode, ResolvedFileTarget, TargetPurpose,
};
use host_core::{HostEnvironment, UserDirectory};
use serde_json::{Map, Value};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use tool_core::{ToolError, ToolMetadata, ToolOutput};

pub(crate) const USER_DIRECTORY_ENUM: [&str; 8] = [
    "desktop",
    "documents",
    "downloads",
    "pictures",
    "music",
    "videos",
    "public_share",
    "templates",
];

pub(crate) const CREATE_USER_FILE_TOOL: &str = "filesystem.create_user_file";
pub(crate) const WRITE_USER_FILE_TOOL: &str = "filesystem.write_user_file";
pub(crate) const EDIT_USER_FILE_TOOL: &str = "filesystem.edit_user_file";
pub(crate) const EDIT_FILE_TOOL: &str = "filesystem.edit_file";
pub(crate) const EDIT_TOOL: &str = "filesystem.edit";
pub(crate) const REPLACE_USER_FILE_TOOL: &str = "filesystem.replace_user_file";
pub(crate) const APPEND_USER_FILE_TOOL: &str = "filesystem.append_user_file";
pub(crate) const APPEND_FILE_TOOL: &str = "filesystem.append_file";
pub(crate) const LIST_TOOL: &str = "filesystem.list";
pub(crate) const DEFAULT_USER_FILENAME: &str = "note.txt";

pub(crate) fn invalid_args(tool: &str, message: impl Into<String>) -> ToolError {
    ToolError::InvalidArgs {
        tool: tool.to_string(),
        message: message.into(),
    }
}

pub(crate) fn failed(tool: &str, message: impl Into<String>) -> ToolError {
    ToolError::Failed {
        tool: tool.to_string(),
        message: message.into(),
    }
}

pub(crate) fn filesystem_error(tool: &str, error: FileTargetError) -> ToolError {
    let mut details = serde_json::Map::new();
    if let Some(received) = error.received {
        details.insert("received".to_string(), Value::String(received));
    }
    if let Some(target) = error.suggested_target {
        details.insert(
            "suggested_target".to_string(),
            serde_json::to_value(target).unwrap_or(Value::Null),
        );
    }
    ToolError::Filesystem {
        tool: tool.to_string(),
        code: serde_json::to_string(&error.code)
            .unwrap_or_else(|_| "io_failure".to_string())
            .trim_matches('"')
            .to_string(),
        retryable: error.retryable,
        message: error.message,
        details: Value::Object(details),
    }
}

pub(crate) fn file_target_descriptor(resolved: &ResolvedFileTarget) -> Value {
    let mut file = serde_json::Map::new();
    file.insert(
        "file_ref".to_string(),
        Value::String(resolved.file_ref.to_string()),
    );
    if let Some(directory) = resolved.directory {
        file.insert(
            "directory".to_string(),
            Value::String(directory.json_key().to_string()),
        );
    }
    if let Some(relative_path) = &resolved.relative_path {
        file.insert(
            "relative_path".to_string(),
            Value::String(relative_path.to_string_lossy().into_owned()),
        );
    }
    file.insert(
        "display_path".to_string(),
        Value::String(resolved.display_path.clone()),
    );
    Value::Object(file)
}

pub(crate) fn tag_file_output(
    output: &mut ToolOutput,
    path: &Path,
    environment: &HostEnvironment,
    purpose: TargetPurpose,
) {
    let resolver = FileResolver::new(environment.clone());
    let resolved = match resolver.descriptor_for_absolute(path, purpose) {
        Ok(resolved) => resolved,
        // Directory outputs (listings) tag the directory itself; file
        // tools never reach this fallback with a directory because their
        // normalization already rejected it.
        Err(error) if error.code == FilesystemErrorCode::TargetIsDirectory => {
            match resolver.descriptor_for_directory(path) {
                Ok(resolved) => resolved,
                Err(_) => return,
            }
        }
        Err(_) => return,
    };
    if let Some(object) = output.content.as_object_mut() {
        object.insert(
            "file_ref".to_string(),
            Value::String(resolved.file_ref.to_string()),
        );
        object.insert(
            "display_path".to_string(),
            Value::String(resolved.display_path.clone()),
        );
        if let Some(directory) = resolved.directory {
            object.insert(
                "directory".to_string(),
                Value::String(directory.json_key().to_string()),
            );
        }
        if let Some(relative_path) = &resolved.relative_path {
            object.insert(
                "relative_path".to_string(),
                Value::String(relative_path.to_string_lossy().into_owned()),
            );
        }
        object.insert("file".to_string(), file_target_descriptor(&resolved));
    }
}

pub(crate) fn host_file_metadata(
    mut metadata: ToolMetadata,
    target_required: &[&str],
) -> ToolMetadata {
    if let Some(schema) = metadata.input_schema.as_object_mut() {
        if let Some(properties) = schema
            .entry("properties")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
        {
            properties.insert(
                "file_ref".to_string(),
                serde_json::json!({
                    "type": "string",
                    "description": "Stable reference returned by a previous successful filesystem operation. Prefer this for follow-up requests."
                }),
            );
            properties.insert(
                "target".to_string(),
                serde_json::json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "directory": { "type": "string", "enum": USER_DIRECTORY_ENUM },
                        "relative_path": { "type": "string" }
                    },
                    "required": ["directory", "relative_path"]
                }),
            );
        }
        schema.insert(
            "required".to_string(),
            Value::Array(
                target_required
                    .iter()
                    .map(|field| Value::String((*field).to_string()))
                    .collect(),
            ),
        );
    }
    metadata.description.push_str(
        " Prefer file_ref or target.directory + target.relative_path; absolute path remains a compatibility fallback and is still capability-checked.",
    );
    metadata
}

/// A small model may copy the configured directory root into `path` while
/// also sending a semantic target. Treat that exact root (or its conventional
/// host-local alias) as redundant metadata, never as the file target. The
/// selected semantic target remains the source of truth and still goes
/// through the resolver and capability broker.
pub(crate) fn is_configured_directory_alias(
    path: &Path,
    directory: Option<UserDirectory>,
    environment: &HostEnvironment,
) -> bool {
    let Some(directory) = directory else {
        return false;
    };
    let Some(configured) = environment.user_dirs.get(directory) else {
        return false;
    };
    let same_canonical_root = path
        .canonicalize()
        .ok()
        .zip(configured.canonicalize().ok())
        .is_some_and(|(path, configured)| path == configured);
    if same_canonical_root {
        return true;
    }
    environment.home.as_deref().is_some_and(|home| {
        lexical_absolute_path(path)
            == lexical_absolute_path(&home.join(directory.conventional_name()))
    })
}

pub(crate) fn active_file_from_context(
    active_context: Option<&Arc<Mutex<ConversationFileContext>>>,
) -> Option<FileRef> {
    active_context
        .and_then(|context| context.lock().ok())
        .and_then(|context| context.active_file.clone())
}

pub(crate) fn retryable_target_args(tool: &str, message: impl Into<String>) -> ToolError {
    ToolError::RetryRequired {
        tool: tool.to_string(),
        message: "Filesystem target needs correction".to_string(),
        recovery: serde_json::json!({
            "error": "invalid_file_target",
            "reason": message.into(),
            "target_fields": ["file_ref", "target", "location", "filename", "path"],
            "next_tool": tool,
        }),
    }
}

pub(crate) fn normalize_host_file_args(
    args: &serde_json::Value,
    environment: &HostEnvironment,
    purpose: TargetPurpose,
    tool: &str,
    active_context: Option<&Arc<Mutex<ConversationFileContext>>>,
) -> Result<serde_json::Value, ToolError> {
    let object = args
        .as_object()
        .ok_or_else(|| invalid_args(tool, "args must be a JSON object"))?;
    let has_file_ref = object.contains_key("file_ref");
    let has_target = object.contains_key("target");
    let has_path = object.contains_key("path");
    let selector_count = has_file_ref as u8 + has_target as u8 + has_path as u8;
    tracing::debug!(
        tool = %tool,
        has_file_ref,
        has_target,
        has_path,
        "filesystem target styles received"
    );
    if selector_count == 0 {
        if let Some(file_ref) = active_file_from_context(active_context) {
            let resolver = FileResolver::new(environment.clone());
            let resolved = resolver
                .resolve_ref(&file_ref, purpose)
                .map_err(|error| filesystem_error(tool, error))?;
            let mut normalized = object.clone();
            normalized.insert(
                "path".to_string(),
                Value::String(resolved.absolute_path.to_string_lossy().into_owned()),
            );
            tracing::debug!(
                tool = %tool,
                file_ref = %resolved.file_ref,
                path = %resolved.absolute_path.display(),
                "filesystem target resolved from active file context"
            );
            return Ok(Value::Object(normalized));
        }
        return Err(retryable_target_args(
            tool,
            "missing file_ref, target, or absolute path target",
        ));
    }
    let resolver = FileResolver::new(environment.clone());
    let mut normalized = object.clone();

    // Resolve every supplied selector before choosing the canonical path.
    // Small models frequently echo both a stable reference and a display
    // path; equivalent selectors are safe to collapse, while mismatches are
    // still rejected.
    let mut candidates = Vec::<(&str, ResolvedFileTarget)>::new();
    if has_file_ref {
        let raw = object
            .get("file_ref")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_args(tool, "'file_ref' must be a string"))?;
        let resolved =
            FileRef::parse(raw).and_then(|file_ref| resolver.resolve_ref(&file_ref, purpose));
        match resolved {
            Ok(resolved) => candidates.push(("file_ref", resolved)),
            Err(error) if error.code == FilesystemErrorCode::InvalidFileRef => {
                // A malformed reference identifies no file, so it cannot
                // disagree with sibling selectors: ignore it and resolve
                // the rest. Small models frequently echo a bare filename
                // here while also sending the real path.
                if has_target || has_path {
                    tracing::debug!(
                        tool = %tool,
                        received = %raw,
                        "ignored malformed file_ref; resolving sibling selectors"
                    );
                } else if let Some(active) = active_file_from_context(active_context) {
                    // A tiny model can copy or truncate the active reference.
                    // The malformed value identifies no file, so the only safe
                    // recovery is the already successful active target. Resolve
                    // it again so current permissions and symlink checks still
                    // apply; never use this fallback for a stale or valid but
                    // mismatched reference.
                    let resolved = resolver
                        .resolve_ref(&active, purpose)
                        .map_err(|error| filesystem_error(tool, error))?;
                    tracing::debug!(
                        tool = %tool,
                        active_file_ref = %resolved.file_ref,
                        "recovered malformed file_ref from active file context"
                    );
                    candidates.push(("active_file", resolved));
                } else {
                    return Err(filesystem_error(tool, error));
                }
            }
            Err(error) => return Err(filesystem_error(tool, error)),
        }
    }
    if has_target {
        let target = object
            .get("target")
            .and_then(Value::as_object)
            .ok_or_else(|| invalid_args(tool, "'target' must be an object"))?;
        let directory = target
            .get("directory")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                invalid_args(tool, "target.directory must be a semantic directory id")
            })?;
        let directory = parse_directory_value(directory, environment, tool)?;
        let relative_path = target
            .get("relative_path")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_args(tool, "target.relative_path must be a string"))?;
        let resolved = resolver
            .resolve_target(
                &FileTarget {
                    directory,
                    relative_path: PathBuf::from(relative_path),
                },
                purpose,
            )
            .map_err(|error| filesystem_error(tool, error))?;
        candidates.push(("target", resolved));
    }
    if has_path {
        let path = object
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_args(tool, "'path' must be a string"))?;
        if !Path::new(path).is_absolute() {
            return Err(retryable_target_args(tool, "path must be absolute"));
        }
        let path = Path::new(path);
        let is_directory_hint = candidates.first().is_some_and(|(_, target)| {
            is_configured_directory_alias(path, target.directory, environment)
        });
        if is_directory_hint {
            tracing::debug!(
                tool = %tool,
                path = %path.display(),
                "ignored redundant configured-directory path hint"
            );
        } else {
            let normalized_special = normalize_special_user_path(
                path,
                environment,
                tool,
                if matches!(purpose, TargetPurpose::Existing) {
                    SpecialPathPurpose::ExistingFile
                } else {
                    SpecialPathPurpose::Write
                },
                "filesystem.create_user_file",
            )?;
            let resolved = resolver.descriptor_for_absolute(&normalized_special.path, purpose);
            let resolved = match resolved {
                Ok(resolved) => resolved,
                // `filesystem.list` names the directory itself: an absolute
                // directory path resolves to a directory descriptor instead
                // of failing. Every other tool keeps the file-oriented
                // error, so reads and edits on directories do not change.
                Err(error)
                    if error.code == FilesystemErrorCode::TargetIsDirectory
                        && tool == LIST_TOOL =>
                {
                    resolver
                        .descriptor_for_directory(&normalized_special.path)
                        .map_err(|error| filesystem_error(tool, error))?
                }
                Err(error) => return Err(filesystem_error(tool, error)),
            };
            candidates.push(("path", resolved));
        }
    }

    let Some((_, canonical)) = candidates.first() else {
        return Err(retryable_target_args(
            tool,
            "missing file_ref, target, or absolute path target",
        ));
    };
    if candidates
        .iter()
        .any(|(_, candidate)| candidate.absolute_path != canonical.absolute_path)
    {
        tracing::debug!(
            tool = %tool,
            candidates = ?candidates
                .iter()
                .map(|(kind, candidate)| (*kind, candidate.absolute_path.display().to_string()))
                .collect::<Vec<_>>(),
            "filesystem target styles resolved to different paths"
        );
        return Err(retryable_target_args(
            tool,
            "target references identify different files; provide one target or equivalent file_ref, target, and path values",
        ));
    }
    normalized.remove("file_ref");
    normalized.remove("target");
    normalized.insert(
        "path".to_string(),
        Value::String(canonical.absolute_path.to_string_lossy().into_owned()),
    );
    tracing::debug!(
        tool = %tool,
        file_ref = %canonical.file_ref,
        path = %canonical.absolute_path.display(),
        "filesystem target normalized"
    );
    Ok(Value::Object(normalized))
}
pub(crate) fn invalid_user_directory(
    tool: &str,
    received: &str,
    environment: &HostEnvironment,
) -> ToolError {
    let mut available_directories = Map::new();
    for directory in UserDirectory::ALL {
        if let Some(path) = environment.user_dirs.get(directory) {
            available_directories.insert(
                directory.json_key().to_string(),
                Value::String(path.to_string_lossy().into_owned()),
            );
        }
    }
    ToolError::Filesystem {
        tool: tool.to_string(),
        code: "invalid_directory".to_string(),
        retryable: true,
        message: "The directory must be a semantic allowed-directory id, not an arbitrary path."
            .to_string(),
        details: serde_json::json!({
            "received": received,
            "valid_directory_ids": USER_DIRECTORY_ENUM,
            "available_directories": available_directories,
        }),
    }
}

pub(crate) fn configured_directory_for_path(
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

pub(crate) fn configured_directory_for_basename(
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

pub(crate) fn parse_directory_value(
    value: &str,
    environment: &HostEnvironment,
    tool: &str,
) -> Result<UserDirectory, ToolError> {
    UserDirectory::from_json_key(value)
        .or_else(|| configured_directory_for_path(value, environment))
        .or_else(|| configured_directory_for_basename(value, environment))
        .ok_or_else(|| invalid_user_directory(tool, value, environment))
}

pub(crate) fn parse_directory(
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
pub(crate) fn parse_location(
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

pub(crate) fn reject_unsafe_relative_path(relative_path: &Path) -> bool {
    relative_path.components().any(|component| {
        matches!(
            component,
            Component::Prefix(_) | Component::RootDir | Component::ParentDir
        )
    }) || relative_path.file_name().is_none()
}

pub(crate) struct ResolvedUserFile {
    pub(crate) directory: UserDirectory,
    pub(crate) resolved_directory: PathBuf,
    pub(crate) relative_path: PathBuf,
    pub(crate) generated_filename: bool,
    pub(crate) write_args: serde_json::Value,
}

pub(crate) fn write_args_for_path(object: &Map<String, Value>, path: &Path) -> serde_json::Value {
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

pub(crate) fn normalize_user_target_args(
    args: &serde_json::Value,
    environment: &HostEnvironment,
    tool: &str,
    purpose: TargetPurpose,
) -> Result<serde_json::Value, ToolError> {
    let args = normalize_legacy_directory_shape(args, environment, tool)?;
    let Some(object) = args.as_object() else {
        return Ok(args);
    };
    let has_file_ref = object.contains_key("file_ref");
    let has_target = object.contains_key("target");
    if has_file_ref && has_target {
        return Err(invalid_args(
            tool,
            "provide exactly one target: file_ref or target",
        ));
    }
    if !has_file_ref && !has_target {
        return Ok(args);
    }
    let resolver = FileResolver::new(environment.clone());
    let mut normalized = object.clone();
    let resolved = if has_file_ref {
        let raw = object
            .get("file_ref")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_args(tool, "'file_ref' must be a string"))?;
        let file_ref = FileRef::parse(raw).map_err(|error| filesystem_error(tool, error))?;
        resolver
            .resolve_ref(&file_ref, purpose)
            .map_err(|error| filesystem_error(tool, error))?
    } else {
        let target = object
            .get("target")
            .and_then(Value::as_object)
            .ok_or_else(|| invalid_args(tool, "'target' must be an object"))?;
        let directory = target
            .get("directory")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                invalid_args(tool, "target.directory must be a semantic directory id")
            })?;
        let directory = parse_directory_value(directory, environment, tool)?;
        let relative_path = target
            .get("relative_path")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_args(tool, "target.relative_path must be a string"))?;
        resolver
            .resolve_target(
                &FileTarget {
                    directory,
                    relative_path: PathBuf::from(relative_path),
                },
                purpose,
            )
            .map_err(|error| filesystem_error(tool, error))?
    };
    let (Some(directory), Some(relative_path)) = (resolved.directory, resolved.relative_path)
    else {
        return Err(invalid_args(
            tool,
            "this user-directory tool requires a semantic file_ref or target",
        ));
    };
    for field in [
        "file_ref",
        "target",
        "location",
        "directory_id",
        "directory",
        "user_directory",
    ] {
        normalized.remove(field);
    }
    normalized.insert(
        "location".to_string(),
        Value::String(directory.json_key().to_string()),
    );
    normalized.insert(
        "filename".to_string(),
        Value::String(relative_path.to_string_lossy().into_owned()),
    );
    Ok(Value::Object(normalized))
}

pub(crate) fn resolve_user_file_args(
    args: &serde_json::Value,
    environment: &HostEnvironment,
    tool: &str,
) -> Result<ResolvedUserFile, ToolError> {
    let args = normalize_user_target_args(args, environment, tool, TargetPurpose::Create)?;
    let object = args
        .as_object()
        .ok_or_else(|| invalid_args(tool, "args must be a JSON object"))?;
    let directory = parse_directory(&args, environment, tool)?;
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
    let resolved = FileResolver::new(environment.clone())
        .resolve_target(
            &FileTarget {
                directory,
                relative_path: relative_path.to_path_buf(),
            },
            TargetPurpose::Create,
        )
        .map_err(|error| filesystem_error(tool, error))?;
    let relative_path = resolved
        .relative_path
        .unwrap_or(relative_path.to_path_buf());
    let resolved_directory = base.to_path_buf();
    let path = resolved.absolute_path;

    Ok(ResolvedUserFile {
        directory,
        resolved_directory,
        relative_path: relative_path.to_path_buf(),
        generated_filename: false,
        write_args: write_args_for_path(object, &path),
    })
}

pub(crate) fn lexical_absolute_path(path: &Path) -> Option<PathBuf> {
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

pub(crate) fn looks_like_windows_absolute_path(value: &str) -> bool {
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
pub(crate) fn normalize_filename(
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

pub(crate) fn optional_filename(
    object: &Map<String, Value>,
    tool: &str,
) -> Result<(String, bool), ToolError> {
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

pub(crate) fn resolve_create_user_file_args(
    args: &serde_json::Value,
    environment: &HostEnvironment,
    tool: &str,
) -> Result<ResolvedUserFile, ToolError> {
    let args = normalize_user_target_args(args, environment, tool, TargetPurpose::Create)?;
    let object = args
        .as_object()
        .ok_or_else(|| invalid_args(tool, "args must be a JSON object"))?;
    let directory = parse_location(&args, environment, tool)?;
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
    let (relative_path, _) = normalize_filename(&filename, base, tool)?;
    let resolved = FileResolver::new(environment.clone())
        .resolve_target(
            &FileTarget {
                directory,
                relative_path: relative_path.clone(),
            },
            TargetPurpose::Create,
        )
        .map_err(|error| filesystem_error(tool, error))?;
    let relative_path = resolved.relative_path.unwrap_or(relative_path);
    let path = resolved.absolute_path;
    Ok(ResolvedUserFile {
        directory,
        resolved_directory: base.to_path_buf(),
        relative_path,
        generated_filename,
        write_args: write_args_for_path(object, &path),
    })
}
