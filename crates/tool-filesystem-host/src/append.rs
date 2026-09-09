//! Append text at an explicit absolute path.

use crate::common::{
    filesystem_error, invalid_args, is_configured_directory_alias, lexical_absolute_path,
    looks_like_windows_absolute_path, normalize_filename, retryable_target_args, APPEND_FILE_TOOL,
    APPEND_USER_FILE_TOOL, EDIT_USER_FILE_TOOL,
};
use crate::edit_args::{
    normalize_special_user_path, path_only_args, required_string_field,
    resolve_edit_target_with_purpose, tag_normalized_from, NormalizedSpecialPath,
    ResolvedEditTarget, SpecialPathPurpose,
};
use crate::replace::append_content_for;
use file_target::{FileResolver, ResolvedFileTarget, TargetPurpose};
use host_core::HostEnvironment;
use serde_json::Value;
use std::path::Path;
use tool_core::{CapabilityRequirement, Tool, ToolContext, ToolError, ToolMetadata, ToolOutput};
use tool_filesystem::WriteTool;

/// Append-only interface for a file at an explicit absolute host-native
/// path. Host-aware like [`EditFileTool`]: a stale conventional
/// special-directory path is normalized before authorization. Reads,
/// appends, and delegates the write to the existing broker.
pub(crate) struct AppendFileTool {
    pub(crate) inner: WriteTool,
    pub(crate) limits: tool_filesystem::FilesystemLimits,
    pub(crate) environment: HostEnvironment,
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
pub(crate) fn resolve_append_file_path(
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
        tag_normalized_from(
            &mut output,
            &normalized,
            &self.environment,
            TargetPurpose::Create,
        );
        if let Some(object) = output.content.as_object_mut() {
            object.insert("appended".to_string(), Value::Bool(true));
        }
        Ok(output)
    }
}

/// Which target style a unified [`EditTool`] call uses. Exactly one style
/// is allowed per call so the model never has to guess precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditTargetStyle {
    /// `location` + `filename`: resolved through `HostEnvironment`.
    UserFile,
    /// `path`: explicit absolute path with stale conventional
    /// special-directory compatibility.
    ExplicitPath,
}

/// Some small models copy the host's `resolved_directory` metadata into the
/// edit `path` field while also sending `location` + `filename`. Treat that
/// value as a directory hint only when it is exactly the selected configured
/// root (or its conventional, localized alias). The actual file target is
/// still resolved and capability-checked from the semantic target.
pub(crate) fn is_selected_directory_alias(
    path: &Path,
    target: &ResolvedEditTarget,
    environment: &HostEnvironment,
) -> bool {
    is_configured_directory_alias(path, Some(target.directory), environment)
}

/// Keep older callers that put a bare filename in `path` working, but never
/// reinterpret an arbitrary path. A relative value is converted to the
/// semantic filename form and then resolved only if it uniquely identifies an
/// existing configured user-directory file.
pub(crate) fn normalize_unified_edit_args(
    args: &serde_json::Value,
    environment: &HostEnvironment,
    tool: &str,
    purpose: TargetPurpose,
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
        let semantic_target =
            resolve_edit_target_with_purpose(&semantic_args, environment, tool, purpose)?;
        let path_matches = if Path::new(&path).is_absolute() {
            if is_selected_directory_alias(Path::new(&path), &semantic_target, environment) {
                true
            } else {
                let normalized = normalize_special_user_path(
                    Path::new(&path),
                    environment,
                    tool,
                    if matches!(purpose, TargetPurpose::Create) {
                        SpecialPathPurpose::Write
                    } else {
                        SpecialPathPurpose::ExistingFile
                    },
                    EDIT_USER_FILE_TOOL,
                )?;
                lexical_absolute_path(&normalized.path)
                    == lexical_absolute_path(&semantic_target.path)
            }
        } else if looks_like_windows_absolute_path(&path) {
            false
        } else {
            let (_, normalized) =
                normalize_filename(&path, &semantic_target.resolved_directory, tool)?;
            lexical_absolute_path(&normalized) == lexical_absolute_path(&semantic_target.path)
        };
        if !path_matches {
            return Err(retryable_target_args(
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

pub(crate) fn edit_target_from_descriptor(
    resolved: &ResolvedFileTarget,
    environment: &HostEnvironment,
) -> Option<ResolvedEditTarget> {
    let directory = resolved.directory?;
    let relative_path = resolved.relative_path.clone()?;
    let resolved_directory = environment.user_dirs.get(directory)?.to_path_buf();
    Some(ResolvedEditTarget {
        directory,
        resolved_directory,
        relative_path,
        path: resolved.absolute_path.clone(),
        file_ref: resolved.file_ref.clone(),
    })
}

/// Resolve an explicit path far enough to retain it as pending edit state
/// when it belongs to a semantic user directory. Arbitrary allowed paths
/// remain delegated to the explicit-path broker and are never converted into
/// a user-directory capability.
pub(crate) fn resolve_edit_path_target(
    args: &serde_json::Value,
    environment: &HostEnvironment,
    tool: &str,
    purpose: TargetPurpose,
) -> Result<Option<ResolvedEditTarget>, ToolError> {
    let Some(path) = args.get("path").and_then(Value::as_str) else {
        return Ok(None);
    };
    if !Path::new(path).is_absolute() {
        return Ok(None);
    }
    let normalized = normalize_special_user_path(
        Path::new(path),
        environment,
        tool,
        if matches!(purpose, TargetPurpose::Create) {
            SpecialPathPurpose::Write
        } else {
            SpecialPathPurpose::ExistingFile
        },
        EDIT_USER_FILE_TOOL,
    )?;
    let resolved = FileResolver::new(environment.clone())
        .descriptor_for_absolute(&normalized.path, purpose)
        .map_err(|error| filesystem_error(tool, error))?;
    Ok(edit_target_from_descriptor(&resolved, environment))
}

pub(crate) fn edit_target_style(
    args: &serde_json::Value,
    tool: &str,
) -> Result<EditTargetStyle, ToolError> {
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
