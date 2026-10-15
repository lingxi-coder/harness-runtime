//! Built-in local apps.
//!
//! An assistant builds a small app as ONE self-contained HTML document and
//! publishes it with the `LocalApp` tool. The app joins the device's app
//! library, outlives the conversation, and opens full screen from the card
//! under the tool call or from the host's Apps page. This crate owns what the
//! runtime and the hosts share:
//!
//! - [`store`]: the app library under `<config-home>/local-apps/`;
//! - [`checks`]: publish-time document checks;
//! - [`service`]: the response a host's WebView request handler returns;
//! - [`skill`]: the `local-app` skill text.
//!
//! Apps that need a framework or a build step come from the external
//! `local-app-builder` plugin, which publishes its single-file build through
//! the same tool. A visualization of the conversation can be saved as an app
//! too: the tool renders its fragment into a document and the app starts with
//! the visualization's confirmed state.

#![forbid(unsafe_code)]

pub mod checks;
pub mod service;
pub mod skill;
pub mod store;

pub use service::{HostResponse, LocalAppService, BOOT_PLACEHOLDER};
pub use store::{
    AppSummary, LocalAppId, LocalAppStore, NewRevision, Published, SourceFormat, StoreError,
    VisualizationOrigin,
};

/// Tool name the agent calls to publish, list and read apps.
pub const TOOL_NAME: &str = "LocalApp";
