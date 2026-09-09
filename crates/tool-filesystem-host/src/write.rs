//! Host-aware write tool over the filesystem broker.

use crate::common::{host_file_metadata, normalize_host_file_args, tag_file_output};
use file_target::TargetPurpose;
use host_core::{HostEnvironment, UserDirectory};
use serde_json::Value;
use std::path::Path;
use tool_core::{CapabilityRequirement, Tool, ToolContext, ToolError, ToolMetadata, ToolOutput};
use tool_filesystem::WriteTool;

/// The raw path write tool, with host-aware recovery guidance for the common
/// stale `~/Desktop` mistake. Its actual invocation remains the filesystem
/// broker's normal `WriteTool` implementation.
pub(crate) struct HostAwareWriteTool {
    pub(crate) inner: WriteTool,
    pub(crate) environment: HostEnvironment,
}

impl HostAwareWriteTool {
    pub(crate) fn new(
        limits: tool_filesystem::FilesystemLimits,
        environment: HostEnvironment,
    ) -> Self {
        Self {
            inner: WriteTool { limits },
            environment,
        }
    }

    /// Normalize a compatible target before the ticket is minted, so the
    /// ticket always scopes the final path. A file_ref or semantic target is
    /// resolved by the same host resolver as all other filesystem tools.
    fn normalized_write_args(
        &self,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, ToolError> {
        normalize_host_file_args(
            args,
            &self.environment,
            TargetPurpose::Create,
            "filesystem.write",
            None,
        )
    }
}

pub(crate) fn enrich_write_error(error: ToolError, environment: &HostEnvironment) -> ToolError {
    let ToolError::Failed { tool, message } = error else {
        return error;
    };
    let Some(missing_parent) = message.strip_prefix("parent directory does not exist: ") else {
        return ToolError::Failed { tool, message };
    };
    let Some(home) = environment.home.as_deref() else {
        return ToolError::Failed { tool, message };
    };
    let missing_parent = Path::new(missing_parent);
    let suggestion = UserDirectory::ALL.into_iter().find_map(|directory| {
        let configured = environment.user_dirs.get(directory)?;
        let conventional = home.join(directory.conventional_name());
        (missing_parent == conventional && configured != conventional)
            .then(|| (directory, configured.to_path_buf()))
    });
    let Some((directory, configured)) = suggestion else {
        return ToolError::Failed { tool, message };
    };
    ToolError::Failed {
        tool,
        message: format!(
            "{message}. The host-configured {} directory is {}. Use filesystem.create_user_file with location='{}', or retry using that exact resolved path",
            directory.prompt_label(),
            configured.display(),
            directory.json_key(),
        ),
    }
}

#[async_trait::async_trait]
impl Tool for HostAwareWriteTool {
    fn metadata(&self) -> ToolMetadata {
        let metadata = host_file_metadata(self.inner.metadata(), &["content"]);
        let mut metadata = metadata;
        metadata.description.push_str(
            " Prefer filesystem.create_user_file for OS-configured Desktop, Documents, Downloads, Pictures, Music, Videos, Public, or Templates directories; use filesystem.write only for arbitrary explicit paths.",
        );
        metadata
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        let forwarded = self.normalized_write_args(args).ok()?;
        self.inner.required_capability(&forwarded)
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let original_path = args.get("path").and_then(Value::as_str).map(str::to_owned);
        let args = self.normalized_write_args(&args)?;
        let normalized_path = args.get("path").and_then(Value::as_str).map(str::to_owned);
        let mut output = self
            .inner
            .invoke(ctx, args)
            .await
            .map_err(|error| enrich_write_error(error, &self.environment))?;
        if original_path.is_some() && original_path != normalized_path {
            if let Some(object) = output.content.as_object_mut() {
                if let Some(original_path) = original_path {
                    object.insert("normalized_from".to_string(), Value::String(original_path));
                }
            }
        }
        if let Some(path) = output
            .content
            .get("path")
            .and_then(Value::as_str)
            .map(str::to_owned)
        {
            tag_file_output(
                &mut output,
                Path::new(&path),
                &self.environment,
                TargetPurpose::Create,
            );
        }
        Ok(output)
    }
}
