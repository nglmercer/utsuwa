//! Unified `filesystem.edit` tool.

use crate::append::{
    edit_target_style, normalize_unified_edit_args, resolve_edit_path_target, AppendFileTool,
    EditTargetStyle,
};
use crate::append_user::AppendUserFileTool;
use crate::common::{
    failed, filesystem_error, invalid_args, EDIT_FILE_TOOL, EDIT_TOOL, EDIT_USER_FILE_TOOL,
    USER_DIRECTORY_ENUM,
};
use crate::edit_args::{
    edit_operation_kind, log_edit_argument_shape, next_line_append_content,
    normalize_clock_replacement_to_append, normalize_structured_edit_args, parse_unified_old_new,
    resolve_edit_target_with_purpose, resolve_new_text_source, supplied_edit_texts,
    EditOperationKind, MergedEditArgs, PendingEditState, ResolvedEditTarget,
};
use crate::edit_file::{resolve_edit_file_path, EditFileTool};
use crate::edit_user_file::EditUserFileTool;
use file_target::{ConversationFileContext, FileRef, FileResolver, TargetPurpose};
use host_core::HostEnvironment;
use serde_json::Value;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tool_core::{CapabilityRequirement, Tool, ToolContext, ToolError, ToolMetadata, ToolOutput};

/// The single model-facing edit tool: one interface for both the semantic
/// user-directory style (`location` + `filename`) and the explicit absolute
/// path style (`path`). It routes to [`EditUserFileTool`] or
/// [`EditFileTool`], so stale-path normalization, read-retry guidance,
/// ticket validation, exact-match safety, and atomic writes are all
/// inherited rather than reimplemented.
pub(crate) struct EditTool {
    pub(crate) user_file: EditUserFileTool,
    pub(crate) file: EditFileTool,
    pub(crate) append_user_file: AppendUserFileTool,
    pub(crate) append_file: AppendFileTool,
    pub(crate) pending_edit: Arc<Mutex<Option<PendingEditState>>>,
    pub(crate) active_context: Option<Arc<Mutex<ConversationFileContext>>>,
}

impl EditTool {
    #[cfg(test)]
    pub(crate) fn new(
        limits: tool_filesystem::FilesystemLimits,
        environment: HostEnvironment,
    ) -> Self {
        Self::new_with_context(limits, environment, None)
    }

    pub(crate) fn new_with_context(
        limits: tool_filesystem::FilesystemLimits,
        environment: HostEnvironment,
        active_context: Option<Arc<Mutex<ConversationFileContext>>>,
    ) -> Self {
        Self {
            user_file: EditUserFileTool::new(limits.clone(), environment.clone()),
            file: EditFileTool::new(limits.clone(), environment.clone()),
            append_user_file: AppendUserFileTool::new(limits.clone(), environment.clone()),
            append_file: AppendFileTool::new(limits, environment),
            pending_edit: Arc::new(Mutex::new(None)),
            active_context,
        }
    }

    fn pending_snapshot(&self) -> Option<PendingEditState> {
        self.pending_edit
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
    }

    fn merged_args(&self, args: &serde_json::Value) -> Result<MergedEditArgs, ToolError> {
        log_edit_argument_shape(args);
        let structured_args =
            normalize_structured_edit_args(args, &self.user_file.environment, EDIT_TOOL)?;
        let source_args = resolve_new_text_source(&structured_args, EDIT_TOOL)?;
        let source_args = normalize_clock_replacement_to_append(&source_args);
        let supplied = supplied_edit_texts(&source_args, EDIT_TOOL)?;
        let pending = self.pending_snapshot();
        let object = source_args
            .as_object()
            .ok_or_else(|| invalid_args(EDIT_TOOL, "args must be a JSON object"))?;
        let has_semantic_target = object.contains_key("location")
            || object.contains_key("directory_id")
            || object.contains_key("directory")
            || object.contains_key("filename");
        let target_purpose = if edit_operation_kind(&source_args) == EditOperationKind::Append {
            TargetPurpose::Create
        } else {
            TargetPurpose::Existing
        };
        let mut merged = object.clone();
        let target = if has_semantic_target {
            Some(resolve_edit_target_with_purpose(
                &source_args,
                &self.user_file.environment,
                EDIT_TOOL,
                target_purpose,
            )?)
        } else if object.contains_key("path") {
            resolve_edit_path_target(
                &source_args,
                &self.user_file.environment,
                EDIT_TOOL,
                target_purpose,
            )?
        } else if !object.contains_key("path") {
            if let Some(pending) = pending.as_ref() {
                merged.insert(
                    "location".to_string(),
                    Value::String(pending.target.directory.json_key().to_string()),
                );
                merged.insert(
                    "filename".to_string(),
                    Value::String(pending.target.relative_path.to_string_lossy().into_owned()),
                );
                Some(pending.target.clone())
            } else if let Some(active) = self.active_file()? {
                let resolved = FileResolver::new(self.user_file.environment.clone())
                    .resolve_ref(&active, target_purpose)
                    .map_err(|error| filesystem_error(EDIT_TOOL, error))?;
                if let (Some(directory), Some(relative_path)) =
                    (resolved.directory, resolved.relative_path)
                {
                    merged.insert(
                        "location".to_string(),
                        Value::String(directory.json_key().to_string()),
                    );
                    merged.insert(
                        "filename".to_string(),
                        Value::String(relative_path.to_string_lossy().into_owned()),
                    );
                    Some(ResolvedEditTarget {
                        directory,
                        resolved_directory: self
                            .user_file
                            .environment
                            .user_dirs
                            .get(directory)
                            .map(Path::to_path_buf)
                            .unwrap_or_default(),
                        relative_path,
                        path: resolved.absolute_path,
                        file_ref: resolved.file_ref,
                    })
                } else {
                    merged.insert(
                        "path".to_string(),
                        Value::String(resolved.absolute_path.to_string_lossy().into_owned()),
                    );
                    None
                }
            } else {
                return Ok(MergedEditArgs {
                    args: source_args,
                    target: None,
                    explicit_target: false,
                    supplied_old_text: supplied.0,
                    supplied_new_text: supplied.1,
                });
            }
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

    fn active_file(&self) -> Result<Option<FileRef>, ToolError> {
        let Some(context) = &self.active_context else {
            return Ok(None);
        };
        context
            .lock()
            .map(|context| context.active_file.clone())
            .map_err(|_| failed(EDIT_TOOL, "active file context is unavailable"))
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
            ToolError::Filesystem {
                retryable: true, ..
            } => true,
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
            description: "Edit or append to an existing file. Prefer file_ref from a previous successful filesystem result; otherwise use target.directory (a semantic id such as desktop) plus target.relative_path. The host also accepts legacy location+filename or an absolute path as a safe compatibility fallback. Use operation.type=replace with old_text/new_text, or operation.type=append with text. Target and edit values may be completed across same-turn retries; valid pending values are preserved. Exact-match safety, capability tickets, canonical path checks, symlink checks, and atomic mutation remain enforced by the filesystem broker.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "file_ref": {
                        "type": "string",
                        "description": "Stable reference returned by a previous successful filesystem operation. Prefer this for follow-up edits; do not reconstruct it from display_path."
                    },
                    "target": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "directory": {
                                "type": "string",
                                "enum": USER_DIRECTORY_ENUM,
                                "description": "Semantic allowed-directory id such as desktop or documents. Never pass a complete file path here."
                            },
                            "relative_path": {
                                "type": "string",
                                "description": "Path to the file relative to directory, such as note.txt or projects/demo/config.toml."
                            }
                        },
                        "required": ["directory", "relative_path"]
                    },
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
                    },
                    "operation": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "type": {
                                "type": "string",
                                "enum": ["replace", "append"]
                            },
                            "old_text": { "type": "string" },
                            "new_text": { "type": "string" },
                            "new_text_source": {
                                "type": "string",
                                "enum": ["current_date", "current_time", "current_datetime"]
                            },
                            "text": { "type": "string" }
                        },
                        "required": ["type"]
                    }
                }
            }),
            effects: vec![tool_core::ToolEffect::FilesystemWrite],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        let merged = match self.merged_args(args) {
            Ok(merged) => merged,
            Err(_) => return None,
        };
        self.discard_if_explicit_target_changed(&merged);
        // Preflight runs before invoke. Remember the model's valid partial
        // values here as well, otherwise a capability lookup can erase the
        // only state available for the following retry.
        self.remember_edit_attempt(
            merged.target.clone(),
            merged.supplied_old_text.clone(),
            merged.supplied_new_text.clone(),
        );
        let normalized = normalize_unified_edit_args(
            &merged.args,
            &self.user_file.environment,
            EDIT_TOOL,
            if edit_operation_kind(&merged.args) == EditOperationKind::Append {
                TargetPurpose::Create
            } else {
                TargetPurpose::Existing
            },
        )
        .ok()?;
        match (
            edit_operation_kind(&normalized),
            edit_target_style(&normalized, EDIT_TOOL).ok()?,
        ) {
            (EditOperationKind::Replace, EditTargetStyle::UserFile) => {
                self.user_file.required_capability(&normalized)
            }
            (EditOperationKind::Replace, EditTargetStyle::ExplicitPath) => {
                self.file.required_capability(&normalized)
            }
            (EditOperationKind::Append, EditTargetStyle::UserFile) => {
                self.append_user_file.required_capability(&normalized)
            }
            (EditOperationKind::Append, EditTargetStyle::ExplicitPath) => {
                self.append_file.required_capability(&normalized)
            }
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
                if !Self::keep_pending_after_error(&error) {
                    self.clear_pending_edit();
                }
                return Err(error);
            }
        };
        self.discard_if_explicit_target_changed(&merged);
        self.remember_edit_attempt(
            merged.target.clone(),
            merged.supplied_old_text.clone(),
            merged.supplied_new_text.clone(),
        );
        let normalized = match normalize_unified_edit_args(
            &merged.args,
            &self.user_file.environment,
            EDIT_TOOL,
            if edit_operation_kind(&merged.args) == EditOperationKind::Append {
                TargetPurpose::Create
            } else {
                TargetPurpose::Existing
            },
        ) {
            Ok(normalized) => normalized,
            Err(error) => {
                if !Self::keep_pending_after_error(&error) {
                    self.clear_pending_edit();
                }
                return Err(error);
            }
        };
        let style = match edit_target_style(&normalized, EDIT_TOOL) {
            Ok(style) => style,
            Err(error) => {
                if !Self::keep_pending_after_error(&error) {
                    self.clear_pending_edit();
                }
                return Err(error);
            }
        };
        match style {
            EditTargetStyle::UserFile => {
                let target = match resolve_edit_target_with_purpose(
                    &normalized,
                    &self.user_file.environment,
                    EDIT_TOOL,
                    if edit_operation_kind(&normalized) == EditOperationKind::Append {
                        TargetPurpose::Create
                    } else {
                        TargetPurpose::Existing
                    },
                ) {
                    Ok(target) => target,
                    Err(error) => {
                        if !Self::keep_pending_after_error(&error) {
                            self.clear_pending_edit();
                        }
                        return Err(error);
                    }
                };
                if edit_operation_kind(&normalized) == EditOperationKind::Append {
                    let mut delegated = normalized;
                    if let Some(object) = delegated.as_object_mut() {
                        let auto_next_line = object
                            .remove("_auto_next_line")
                            .and_then(|value| value.as_bool())
                            .unwrap_or(false);
                        let content = object.remove("content").ok_or_else(|| {
                            invalid_args(EDIT_TOOL, "append operation requires text")
                        })?;
                        let content = content.as_str().map(str::to_owned).ok_or_else(|| {
                            invalid_args(EDIT_TOOL, "append text must be a string")
                        })?;
                        object.insert(
                            "content".to_string(),
                            Value::String(if auto_next_line {
                                next_line_append_content(&target.path, content)
                            } else {
                                content
                            }),
                        );
                    }
                    return match self.append_user_file.invoke(ctx, delegated).await {
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
                    };
                }
                let (old_text, new_text) = match parse_unified_old_new(&normalized, &target.path) {
                    Ok(values) => values,
                    Err(error) => {
                        if !Self::keep_pending_after_error(&error) {
                            self.clear_pending_edit();
                        }
                        return Err(error);
                    }
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
                let target = match resolve_edit_file_path(
                    &normalized,
                    &self.file.environment,
                    EDIT_FILE_TOOL,
                    EDIT_USER_FILE_TOOL,
                ) {
                    Ok(target) => target,
                    Err(error) => {
                        if !Self::keep_pending_after_error(&error) {
                            self.clear_pending_edit();
                        }
                        return Err(error);
                    }
                };
                if edit_operation_kind(&normalized) == EditOperationKind::Append {
                    let mut delegated = normalized;
                    if let Some(object) = delegated.as_object_mut() {
                        let auto_next_line = object
                            .remove("_auto_next_line")
                            .and_then(|value| value.as_bool())
                            .unwrap_or(false);
                        let content = object.remove("content").ok_or_else(|| {
                            invalid_args(EDIT_TOOL, "append operation requires text")
                        })?;
                        let content = content.as_str().map(str::to_owned).ok_or_else(|| {
                            invalid_args(EDIT_TOOL, "append text must be a string")
                        })?;
                        object.insert(
                            "content".to_string(),
                            Value::String(if auto_next_line {
                                next_line_append_content(&target.path, content)
                            } else {
                                content
                            }),
                        );
                    }
                    return self.append_file.invoke(ctx, delegated).await;
                }
                let (old_text, new_text) = match parse_unified_old_new(&normalized, &target.path) {
                    Ok(values) => values,
                    Err(error) => {
                        if !Self::keep_pending_after_error(&error) {
                            self.clear_pending_edit();
                        }
                        return Err(error);
                    }
                };
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
