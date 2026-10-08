//! Inline chat visualizations.
//!
//! An assistant publishes an HTML fragment through the `Visualization` tool
//! and places it with a reference line in its reply. This crate owns every
//! piece the runtime and the three hosts share:
//!
//! - [`reference`] / [`splitter`]: the reference grammar and the one parser
//!   that splits live deltas and replayed text identically;
//! - [`store`]: per-root-session revisions and compare-and-swap widget state;
//! - [`document`] / [`assets`]: the sandboxed document, its CSP, and the
//!   embedded shell, runtime and libraries (vendored from upstream Codex
//!   `ea27864f` plus D3);
//! - [`service`]: mount tokens and the scheme-handler router hosts call;
//! - [`checks`]: publish-time fragment checks;
//! - [`context`]: the "continue analysis" block;
//! - [`skill`]: the `visualize` skill text.

#![forbid(unsafe_code)]

pub mod assets;
pub mod checks;
pub mod context;
pub mod document;
pub mod reference;
pub mod service;
pub mod skill;
pub mod splitter;
pub mod store;

pub use reference::{VisualizationId, VisualizationRef, REFERENCE_PREFIX};
pub use service::{HostResponse, MountRequest, MountStateWrite, MountTicket, VisualizationService};
pub use splitter::{project_text, ReferenceSplitter, Segment, SplitEvent};
pub use store::{Publisher, StoreError, VisualizationStore};

/// Tool name the agent calls to publish.
pub const TOOL_NAME: &str = "Visualization";

/// Host capability that turns the tool and the skill on.
pub const CAPABILITY: &str = "inline_visualization";
