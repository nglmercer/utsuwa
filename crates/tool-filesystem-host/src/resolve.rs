//! Semantic user-directory lookup tool.

use crate::common::{failed, parse_directory, USER_DIRECTORY_ENUM};
use host_core::HostEnvironment;
use tool_core::{Tool, ToolContext, ToolError, ToolMetadata, ToolOutput};

/// Resolve one validated special directory without exposing an OS mutation
/// API or requiring a capability ticket.
pub(crate) struct ResolveUserDirectoryTool {
    pub(crate) environment: HostEnvironment,
}

impl ResolveUserDirectoryTool {
    pub(crate) fn new(environment: HostEnvironment) -> Self {
        Self { environment }
    }

    #[cfg(test)]
    pub(crate) fn with_environment(environment: HostEnvironment) -> Self {
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
