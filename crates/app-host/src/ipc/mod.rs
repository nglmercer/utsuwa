//! Closed, typed IPC dispatch, split by domain.
//!
//! [`IpcMethod`] is intentionally a closed protocol: the dispatcher matches
//! its variants explicitly (see `dispatcher.rs`). The domain modules only
//! organize the implementation — IPC is not dynamically extensible.

pub mod activity;
pub mod agent;
pub mod audio;
pub mod dispatcher;
pub mod permissions;
pub mod plugins;
pub mod providers;
pub mod settings;

pub use dispatcher::{emit_script, Dispatcher};
