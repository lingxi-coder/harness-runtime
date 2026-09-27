//! Where a plugin came from.
//!
//! Plugins enter the engine through one of six sources. The source drives
//! both the default trust level (see [`crate::trust::default_trust_for_source`])
//! and the fetch / install code path (Plan 16 implements the actual fetches).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Origin of a plugin install.
///
/// Each variant captures the minimum information needed to re-fetch the
/// plugin during a future install or re-validation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PluginSource {
    /// Built-in plugin compiled into the engine.
    BuiltIn,
    /// Plugin pulled from the official Anthropic-hosted marketplace.
    OfficialMarketplace {
        /// Canonical plugin name in the marketplace.
        name: String,
    },
    /// Plugin from a self-hosted / third-party marketplace.
    Marketplace {
        /// Marketplace endpoint URL.
        url: String,
        /// Canonical plugin name in the marketplace.
        name: String,
    },
    /// Plugin checked out from a Git repository.
    Git {
        /// Repository URL.
        url: String,
        /// Branch, tag, or commit-ish to check out.
        #[serde(rename = "ref")]
        ref_: String,
    },
    /// Plugin sourced from a path on the local filesystem.
    LocalPath {
        /// Absolute path to the plugin install directory.
        path: PathBuf,
    },
    /// Plugin shipped as an `.mcpb` bundle (zip archive).
    Mcpb {
        /// Path to the `.mcpb` file on disk.
        path: PathBuf,
        /// Content-hash of the bundle (for tamper detection).
        hash: String,
    },
}
