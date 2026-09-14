//! Native system, clipboard, application, and document tools.
//!
//! Three packs share one crate because they share one dependency profile
//! (OS inspection without new runtimes):
//!
//! - [`system::SystemToolPack`] — `system.*` read-only host facts with
//!   secret redaction.
//! - [`clipboard::ClipboardToolPack`] — explicit `clipboard.*` tools; reads
//!   never enter model context implicitly.
//! - [`application::ApplicationToolPack`] — `application.*` process-based
//!   management with validated identities (no shell, no argument strings).
//! - [`document::DocumentToolPack`] — `document.*`/`image.*`/`pdf.*`
//!   inspection over already-authorized files.

pub mod application;
pub mod clipboard;
pub mod document;
pub mod system;

pub use application::ApplicationToolPack;
pub use clipboard::ClipboardToolPack;
pub use document::{pdftoppm_available, DocumentToolPack};
pub use system::SystemToolPack;
