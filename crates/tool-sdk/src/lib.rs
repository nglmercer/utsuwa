//! Extension/runtime SDK for Utsuwa tools (refactor step 2).
//!
//! `tool-core` remains the runtime ABI: [`Tool`], [`ToolRegistry`],
//! [`ToolContext`], [`ToolOutput`]. This crate adds the composition layer
//! the host uses to *build* per-turn registries without knowing every tool:
//!
//! ```text
//! ToolPack (sync, builtin groups) ─┐
//!                                   ├─► ToolSource ─► ToolCatalog::snapshot ─► ToolRegistry
//! ToolSource (async, MCP/plugins) ──┘
//! ```
//!
//! Rules:
//! - The catalog rebuilds an immutable snapshot per turn. `ToolRegistry`
//!   stays a plain store; it never becomes a globally locked mutable hub.
//! - Provenance ([`ToolProvenance`]) is attached at the catalog layer, so
//!   the ~95 existing `Tool::metadata` implementations keep compiling
//!   untouched. Native `source`/`version`/`tags` fields on `ToolMetadata`
//!   arrive later, after a literal-migration codemod.
//! - [`TypedTool`] is opt-in. Existing `Tool` impls keep working forever;
//!   new tools should prefer the typed form (argument structs become the
//!   schema source of truth instead of hand-maintained JSON Schema).
//!
//! Dependency direction: `tool-sdk` depends on `tool-core` and
//! `capability-core`. Nothing under `tool-*` may depend on `app-host`.

mod profile;
mod source;
mod typed;

pub use profile::{FullToolPolicy, SimpleToolPolicy, ToolProfile, ToolVisibilityPolicy};
pub use source::{
    collect_tools, ClosureSource, ToolCatalog, ToolLoadContext, ToolPack, ToolProvenance,
    ToolSource, ToolSourceError, ToolSourceId,
};
pub use typed::{TypedTool, TypedToolAdapter};
