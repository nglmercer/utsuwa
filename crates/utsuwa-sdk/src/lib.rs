//! Stable public extension API for Utsuwa tools.
//!
//! This crate is a **facade, not an implementation layer**: it only
//! re-exports the contracts that survived the runtime refactor. Internal
//! tool packs depend on these APIs directly; external extension authors
//! depend on this crate so internal refactors behind the facade do not
//! break them.
//!
//! Start here:
//!
//! - [`Tool`] / [`TypedTool`]: implement tools (typed form preferred).
//! - [`ToolPack`] / [`ToolSource`]: publish groups of tools.
//! - [`ToolCatalog`]: compose sources into a per-turn [`ToolRegistry`].
//! - [`Capability`] / [`CapabilityRequirement`]: declare required authority.
//! - [`FileTarget`] / [`HostEnvironment`]: build host-aware filesystem tools.

pub use capability_core::{
    AgentId, Capability, CapabilityRequest, CapabilityTicket, InvocationId, PluginId, Principal,
    Resource, ServerId, ToolId,
};
pub use file_target::{
    ConversationFileContext, FileRef, FileResolver, FileTarget, FileTargetError,
    FilesystemErrorCode, ResolvedFileTarget, TargetPurpose,
};
pub use host_core::{HostEnvironment, UserDirectories, UserDirectory};
pub use tool_core::{
    CapabilityRequirement, MutationEvidence, Tool, ToolContext, ToolEffect, ToolError,
    ToolMetadata, ToolOutput, ToolRegistry,
};
pub use tool_sdk::{
    collect_tools, ClosureSource, FullToolPolicy, SimpleToolPolicy, ToolCatalog, ToolLoadContext,
    ToolPack, ToolProfile, ToolProvenance, ToolSource, ToolSourceError, ToolSourceId,
    ToolVisibilityPolicy, TypedTool, TypedToolAdapter,
};
