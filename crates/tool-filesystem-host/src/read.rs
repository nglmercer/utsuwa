//! Host-aware read/stat/range/patch/list tools over the filesystem broker.

use crate::common::{host_file_metadata, normalize_host_file_args, tag_file_output};
use file_target::{ConversationFileContext, TargetPurpose};
use host_core::HostEnvironment;
use serde_json::Value;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tool_core::{CapabilityRequirement, Tool, ToolContext, ToolError, ToolMetadata, ToolOutput};
use tool_filesystem::{ListTool, PatchTool, ReadRangeTool, ReadTool, StatTool};

/// Host adapter for absolute-path filesystem tools. It accepts the canonical
/// `file_ref`/`target` contract, normalizes compatible absolute paths, then
/// delegates the capability-bearing operation to the original broker tool.
pub(crate) struct HostAwarePathTool {
    pub(crate) inner: Arc<dyn Tool>,
    pub(crate) environment: HostEnvironment,
    pub(crate) purpose: TargetPurpose,
    pub(crate) active_context: Option<Arc<Mutex<ConversationFileContext>>>,
}

impl HostAwarePathTool {
    pub(crate) fn read(
        limits: tool_filesystem::FilesystemLimits,
        environment: HostEnvironment,
    ) -> Self {
        Self {
            inner: Arc::new(ReadTool { limits }),
            environment,
            purpose: TargetPurpose::Existing,
            active_context: None,
        }
    }

    pub(crate) fn stat(environment: HostEnvironment) -> Self {
        Self {
            inner: Arc::new(StatTool),
            environment,
            purpose: TargetPurpose::Existing,
            active_context: None,
        }
    }

    pub(crate) fn read_range(
        limits: tool_filesystem::FilesystemLimits,
        environment: HostEnvironment,
    ) -> Self {
        Self {
            inner: Arc::new(ReadRangeTool { limits }),
            environment,
            purpose: TargetPurpose::Existing,
            active_context: None,
        }
    }

    pub(crate) fn patch(
        limits: tool_filesystem::FilesystemLimits,
        environment: HostEnvironment,
    ) -> Self {
        Self {
            inner: Arc::new(PatchTool { limits }),
            environment,
            purpose: TargetPurpose::Existing,
            active_context: None,
        }
    }

    /// Directory listing with the same `file_ref`/`target` contract as the
    /// file tools, so models never have to guess localized absolute paths
    /// (e.g. `/home/u/Desktop` vs `/home/u/Escritorio`) to explore.
    pub(crate) fn list(
        limits: tool_filesystem::FilesystemLimits,
        environment: HostEnvironment,
    ) -> Self {
        Self {
            inner: Arc::new(ListTool { limits }),
            environment,
            purpose: TargetPurpose::Existing,
            active_context: None,
        }
    }

    pub(crate) fn with_active_context(
        mut self,
        active_context: Option<Arc<Mutex<ConversationFileContext>>>,
    ) -> Self {
        self.active_context = active_context;
        self
    }
}

#[async_trait::async_trait]
impl Tool for HostAwarePathTool {
    fn metadata(&self) -> ToolMetadata {
        let required = if self.inner.metadata().id.0 == "filesystem.read_range" {
            &["offset", "length"][..]
        } else if self.inner.metadata().id.0 == "filesystem.patch" {
            &["replacements"][..]
        } else {
            &[][..]
        };
        host_file_metadata(self.inner.metadata(), required)
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        let normalized = normalize_host_file_args(
            args,
            &self.environment,
            self.purpose,
            &self.inner.metadata().id.0,
            self.active_context.as_ref(),
        )
        .ok()?;
        self.inner.required_capability(&normalized)
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let normalized = normalize_host_file_args(
            &args,
            &self.environment,
            self.purpose,
            &self.inner.metadata().id.0,
            self.active_context.as_ref(),
        )?;
        let mut output = self.inner.invoke(ctx, normalized).await?;
        let path = output
            .content
            .get("path")
            .and_then(Value::as_str)
            .or_else(|| args.get("path").and_then(Value::as_str))
            .map(str::to_owned);
        if let Some(path) = path {
            tag_file_output(
                &mut output,
                Path::new(&path),
                &self.environment,
                self.purpose,
            );
        }
        Ok(output)
    }
}
