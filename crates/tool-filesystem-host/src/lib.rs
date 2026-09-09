//! Model-friendly semantic filesystem tools for host-configured user
//! directories.
//!
//! The low-level capability-checked broker lives in `tool-filesystem`.
//! This crate is the host-facing layer: it accepts the canonical
//! `file_ref`/`target` contract (plus compatibility absolute paths),
//! resolves names through [`file_target`] against [`host_core`]
//! environment facts, and delegates the capability-bearing operation to
//! the original broker tool.
//!
//! The public surface is [`HostFilesystemPack`]; the per-operation
//! modules are crate-internal.

pub mod append;
pub mod append_user;
pub mod common;
pub mod create;
pub mod edit;
pub mod edit_args;
pub mod edit_file;
pub mod edit_user_file;
pub mod pack;
pub mod read;
pub mod replace;
pub mod resolve;
pub mod user_write;
pub mod write;

pub use pack::HostFilesystemPack;

#[cfg(test)]
mod tests;
