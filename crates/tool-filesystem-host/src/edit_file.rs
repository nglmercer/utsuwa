//! Explicit-path edit tool (legacy shape).

use crate::common::{invalid_args, EDIT_FILE_TOOL, EDIT_USER_FILE_TOOL};
use crate::edit_args::{
    normalize_special_user_path, parse_old_new, patch_args_for, path_only_args,
    required_string_field, tag_normalized_from, with_read_retry_guidance, NormalizedSpecialPath,
    SpecialPathPurpose,
};
use file_target::TargetPurpose;
use host_core::HostEnvironment;
use serde_json::Value;
use std::path::Path;
use tool_core::{CapabilityRequirement, Tool, ToolContext, ToolError, ToolMetadata, ToolOutput};
use tool_filesystem::PatchTool;

/// Partial-edit interface for an existing file at an explicit absolute path.
/// Host-aware: a stale conventional special-directory path
/// (`$HOME/Desktop/...`) is normalized to the OS-configured location before
/// any capability ticket is minted, so the ticket always scopes the final
/// resolved path. Delegates the mutation to the existing filesystem patch
/// broker.
pub(crate) struct EditFileTool {
    pub(crate) inner: PatchTool,
    pub(crate) environment: HostEnvironment,
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
pub(crate) fn resolve_edit_file_path(
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
        tag_normalized_from(
            &mut output,
            &normalized,
            &self.environment,
            TargetPurpose::Existing,
        );
        if let Some(object) = output.content.as_object_mut() {
            object.insert("updated".to_string(), Value::Bool(true));
        }
        Ok(output)
    }
}
