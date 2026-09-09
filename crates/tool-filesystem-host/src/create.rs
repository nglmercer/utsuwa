//! Semantic user-directory file creation.

use crate::common::{resolve_create_user_file_args, CREATE_USER_FILE_TOOL, USER_DIRECTORY_ENUM};
use crate::edit_args::tag_user_file_output;
use host_core::HostEnvironment;
use serde_json::Value;
use tool_core::{CapabilityRequirement, Tool, ToolContext, ToolError, ToolMetadata, ToolOutput};
use tool_filesystem::WriteTool;

/// Preferred small-model interface for files in OS-configured user
/// directories. It resolves the semantic location and safe filename in the
/// native host, then delegates to the existing filesystem write broker.
pub(crate) struct CreateUserFileTool {
    pub(crate) inner: WriteTool,
    pub(crate) environment: HostEnvironment,
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
    pub(crate) fn with_environment(
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
        tag_user_file_output(
            &mut output,
            resolved.directory,
            &resolved.resolved_directory,
            &resolved.relative_path,
            &self.environment,
        );
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
