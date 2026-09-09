//! Whole-file replacement in user directories.

use crate::common::{failed, invalid_args, REPLACE_USER_FILE_TOOL, USER_DIRECTORY_ENUM};
use crate::edit_args::{
    path_only_args, required_string_field, resolve_edit_target_with_purpose, tag_user_file_output,
};
use file_target::TargetPurpose;
use host_core::HostEnvironment;
use serde_json::Value;
use std::path::Path;
use tool_core::{CapabilityRequirement, Tool, ToolContext, ToolError, ToolMetadata, ToolOutput};
use tool_filesystem::WriteTool;

/// Whole-file replacement for an existing file in an OS-configured user
/// directory. Delegates to the existing filesystem write broker; the target
/// must already exist so a replace call can never silently create a file —
/// use filesystem.create_user_file to create one.
pub(crate) struct ReplaceUserFileTool {
    pub(crate) inner: WriteTool,
    pub(crate) environment: HostEnvironment,
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
        let resolved = resolve_edit_target_with_purpose(
            args,
            &self.environment,
            REPLACE_USER_FILE_TOOL,
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
            REPLACE_USER_FILE_TOOL,
            TargetPurpose::Create,
        )?;
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
            &self.environment,
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
pub(crate) fn append_content_for(
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
