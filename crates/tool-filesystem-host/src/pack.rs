//! Host filesystem tool group: the filesystem plugin's declared
//! capabilities plus host-resolved user-directory writes and lookup,
//! all behind policy + tickets.
//!
//! This is the former builtin filesystem surface from the agent runtime,
//! moved behind the [`tool_sdk::ToolPack`] interface so the runtime composes it without knowing individual tools. The model-facing
//! profile pre-filters here; the catalog snapshot re-applies it
//! centrally (idempotent during migration).

use crate::append::AppendFileTool;
use crate::append_user::AppendUserFileTool;
use crate::create::CreateUserFileTool;
use crate::edit::EditTool;
use crate::edit_file::EditFileTool;
use crate::edit_user_file::EditUserFileTool;
use crate::read::HostAwarePathTool;
use crate::replace::ReplaceUserFileTool;
use crate::resolve::ResolveUserDirectoryTool;
use crate::user_write::UserDirectoryWriteTool;
use crate::write::HostAwareWriteTool;
use file_target::ConversationFileContext;
use host_core::HostEnvironment;
use std::sync::{Arc, Mutex};
use tool_sdk::{ToolLoadContext, ToolPack};

/// Static host-filesystem tool group, parameterized by the per-turn host
/// environment snapshot and the conversational file context.
pub struct HostFilesystemPack {
    pub environment: HostEnvironment,
    pub file_context: Option<Arc<Mutex<ConversationFileContext>>>,
}

impl HostFilesystemPack {
    pub fn new(
        environment: HostEnvironment,
        file_context: Option<Arc<Mutex<ConversationFileContext>>>,
    ) -> Self {
        Self {
            environment,
            file_context,
        }
    }
}

impl ToolPack for HostFilesystemPack {
    fn id(&self) -> &'static str {
        "builtin.filesystem.host"
    }

    fn tools(&self, ctx: &ToolLoadContext) -> Vec<Arc<dyn tool_core::Tool>> {
        let host_environment = self.environment.clone();
        let file_context = self.file_context.clone();
        let mut fs_plugins = tool_filesystem::plugin::FsPluginRegistry::new();
        fs_plugins.register(tool_filesystem::plugin::FsPlugin::local());
        let selected_fs = fs_plugins.select();
        let mut tools: Vec<Arc<dyn tool_core::Tool>> = selected_fs
            .as_ref()
            .map(tool_filesystem::plugin::tools_for_plugin)
            .unwrap_or_default();
        if let Some(plugin) = selected_fs {
            if plugin.supports(tool_filesystem::plugin::FsCapability::Read) {
                if let Some(index) = tools
                    .iter()
                    .position(|tool| tool.metadata().id.0 == "filesystem.read")
                {
                    tools[index] = Arc::new(
                        HostAwarePathTool::read(plugin.limits.clone(), host_environment.clone())
                            .with_active_context(file_context.clone()),
                    );
                }
            }
            if plugin.supports(tool_filesystem::plugin::FsCapability::Stat) {
                if let Some(index) = tools
                    .iter()
                    .position(|tool| tool.metadata().id.0 == "filesystem.stat")
                {
                    tools[index] = Arc::new(
                        HostAwarePathTool::stat(host_environment.clone())
                            .with_active_context(file_context.clone()),
                    );
                }
            }
            if plugin.supports(tool_filesystem::plugin::FsCapability::ReadRange) {
                if let Some(index) = tools
                    .iter()
                    .position(|tool| tool.metadata().id.0 == "filesystem.read_range")
                {
                    tools[index] = Arc::new(
                        HostAwarePathTool::read_range(
                            plugin.limits.clone(),
                            host_environment.clone(),
                        )
                        .with_active_context(file_context.clone()),
                    );
                }
            }
            if plugin.supports(tool_filesystem::plugin::FsCapability::Patch) {
                if let Some(index) = tools
                    .iter()
                    .position(|tool| tool.metadata().id.0 == "filesystem.patch")
                {
                    tools[index] = Arc::new(
                        HostAwarePathTool::patch(plugin.limits.clone(), host_environment.clone())
                            .with_active_context(file_context.clone()),
                    );
                }
            }
            if plugin.supports(tool_filesystem::plugin::FsCapability::Patch) {
                tools.push(Arc::new(EditUserFileTool::new(
                    plugin.limits.clone(),
                    host_environment.clone(),
                )));
                tools.push(Arc::new(EditFileTool::new(
                    plugin.limits.clone(),
                    host_environment.clone(),
                )));
                tools.push(Arc::new(EditTool::new_with_context(
                    plugin.limits.clone(),
                    host_environment.clone(),
                    file_context,
                )));
            }
            if plugin.supports(tool_filesystem::plugin::FsCapability::Write) {
                if let Some(index) = tools
                    .iter()
                    .position(|tool| tool.metadata().id.0 == "filesystem.write")
                {
                    tools[index] = Arc::new(HostAwareWriteTool::new(
                        plugin.limits.clone(),
                        host_environment.clone(),
                    ));
                }
                tools.push(Arc::new(CreateUserFileTool::new(
                    plugin.limits.clone(),
                    host_environment.clone(),
                )));
                tools.push(Arc::new(UserDirectoryWriteTool::new(
                    plugin.limits.clone(),
                    host_environment.clone(),
                )));
                tools.push(Arc::new(ReplaceUserFileTool::new(
                    plugin.limits.clone(),
                    host_environment.clone(),
                )));
                tools.push(Arc::new(AppendUserFileTool::new(
                    plugin.limits.clone(),
                    host_environment.clone(),
                )));
                tools.push(Arc::new(AppendFileTool::new(
                    plugin.limits.clone(),
                    host_environment.clone(),
                )));
            }
        }
        tools.push(Arc::new(ResolveUserDirectoryTool::new(
            host_environment.clone(),
        )));
        let profile = ctx.profile.policy();
        tools
            .into_iter()
            .filter(|tool| profile.visible(&tool.metadata().id.0))
            .collect()
    }
}
