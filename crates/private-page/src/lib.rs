//! Private pages that stay on this computer.
//!
//! An assistant publishes an HTML fragment through the `Page` tool. The page
//! is stored under the config home and opened inside LingXi. Nothing is
//! uploaded and there is no public URL. This is not an inline visualization.

#![forbid(unsafe_code)]

pub mod checks;
pub mod document;
pub mod reference;
pub mod store;

pub use reference::{parse_directive, PageId, PageRef, REFERENCE_PREFIX};
pub use store::{PageStore, Published, StoreError};

/// Tool name the agent calls to publish a private page.
pub const TOOL_NAME: &str = "Page";

/// Host capability that turns the tool on.
pub const CAPABILITY: &str = "private_pages";
