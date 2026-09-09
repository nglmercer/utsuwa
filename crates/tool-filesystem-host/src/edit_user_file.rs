//! Semantic user-directory edit tool (legacy shape).

use crate::common::{EDIT_USER_FILE_TOOL, USER_DIRECTORY_ENUM};
use crate::edit_args::{
    parse_old_new, patch_args_for, path_only_args, resolve_edit_target, tag_user_file_output,
    with_read_retry_guidance,
};
use host_core::HostEnvironment;
use serde_json::Value;
use tool_core::{CapabilityRequirement, Tool, ToolContext, ToolError, ToolMetadata, ToolOutput};
use tool_filesystem::PatchTool;

/// Small-model partial-edit interface for files in OS-configured user
/// directories. Resolves the semantic location in the native host, converts
/// the simple arguments into `PatchTool` arguments and delegates the actual
/// mutation to the existing filesystem patch broker — ticket validation,
/// exact-match safety, and atomic writes stay intact.
pub(crate) struct EditUserFileTool {
    pub(crate) inner: PatchTool,
    pub(crate) environment: HostEnvironment,
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
            &self.environment,
        );
        if let Some(object) = output.content.as_object_mut() {
            object.insert("updated".to_string(), Value::Bool(true));
        }
        Ok(output)
    }
}
