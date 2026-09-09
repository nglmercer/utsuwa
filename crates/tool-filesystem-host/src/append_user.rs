//! Append text in user directories.

use crate::common::{invalid_args, APPEND_USER_FILE_TOOL, USER_DIRECTORY_ENUM};
use crate::edit_args::{
    path_only_args, required_string_field, resolve_edit_target_with_purpose, tag_user_file_output,
};
use crate::replace::append_content_for;
use file_target::TargetPurpose;
use host_core::HostEnvironment;
use serde_json::Value;
use tool_core::{CapabilityRequirement, Tool, ToolContext, ToolError, ToolMetadata, ToolOutput};
use tool_filesystem::WriteTool;

/// Append-only interface for files in OS-configured user directories. The
/// model never reproduces the whole file — the host reads, appends, and
/// writes atomically through the existing broker.
pub(crate) struct AppendUserFileTool {
    pub(crate) inner: WriteTool,
    pub(crate) limits: tool_filesystem::FilesystemLimits,
    pub(crate) environment: HostEnvironment,
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
        let resolved = resolve_edit_target_with_purpose(
            args,
            &self.environment,
            APPEND_USER_FILE_TOOL,
            TargetPurpose::Create,
        )
        .ok()?;
        self.inner
            .required_capability(&path_only_args(&resolved.path))
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let resolved = resolve_edit_target_with_purpose(
            &args,
            &self.environment,
            APPEND_USER_FILE_TOOL,
            TargetPurpose::Create,
        )?;
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
            &self.environment,
        );
        if let Some(object) = output.content.as_object_mut() {
            object.insert("appended".to_string(), Value::Bool(true));
        }
        Ok(output)
    }
}
