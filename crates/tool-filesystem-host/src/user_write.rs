//! Semantic user-directory write tool.

use crate::common::{resolve_user_file_args, USER_DIRECTORY_ENUM, WRITE_USER_FILE_TOOL};
use crate::edit_args::tag_user_file_output;
use host_core::HostEnvironment;
use tool_core::{CapabilityRequirement, Tool, ToolContext, ToolError, ToolMetadata, ToolOutput};
use tool_filesystem::WriteTool;

/// Write inside a validated, OS-configured user directory while preserving
/// the existing filesystem write broker and its exact capability scope.
pub(crate) struct UserDirectoryWriteTool {
    pub(crate) inner: WriteTool,
    pub(crate) environment: HostEnvironment,
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
    pub(crate) fn with_environment(
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
        tag_user_file_output(
            &mut output,
            resolved.directory,
            &resolved.resolved_directory,
            &resolved.relative_path,
            &self.environment,
        );
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
