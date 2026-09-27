//! Plugin lifecycle — eight mutually-exclusive states.
//!
//! Transitions are driven by [`crate::manager::PluginManager`]:
//!
//! ```text
//!  Declared ──fetch──▶ Fetching ──manifest──▶ Fetched ──load──▶ Loaded ──disable ok──▶ Disabled
//!     │                    │                    │                 │
//!     │                    ▼                    ▼                 ▼
//!     └──blocklist──▶  Blocked            Failed       DisablingFailed ──retry fail──▶ DisablingFailed
//!                                                              │
//!                                                              └────retry ok──────▶ Disabled
//! ```
//!
//! See spec §15.2.

use crate::manifest::PluginManifest;
use crate::source::PluginSource;
use std::path::PathBuf;
use std::time::SystemTime;

/// Lifecycle state for one plugin.
///
/// A plugin always carries a [`PluginSource`] (the original install request);
/// once a manifest has been parsed, later states also carry the
/// [`PluginManifest`] and the install directory.
#[derive(Debug, Clone)]
pub enum PluginState {
    /// Install was requested but not yet started.
    Declared {
        /// Source declared by the install caller.
        source: PluginSource,
    },
    /// Fetch is in-flight (git clone / marketplace download / mcpb unzip).
    Fetching {
        /// Source being fetched.
        source: PluginSource,
        /// When the fetch attempt began.
        started_at: SystemTime,
    },
    /// Fetch completed; manifest has been parsed but the plugin is not yet
    /// loaded into the engine's registries.
    Fetched {
        /// Parsed manifest.
        manifest: PluginManifest,
        /// Install directory on disk.
        install_dir: PathBuf,
    },
    /// Plugin is loaded and active — components are materialised into the
    /// engine registries.
    Loaded {
        /// Parsed manifest.
        manifest: PluginManifest,
        /// Install directory on disk.
        install_dir: PathBuf,
        /// When the plugin became `Loaded`.
        loaded_at: SystemTime,
    },
    /// Disable/unload started from `Loaded`, but MCP teardown failed before the
    /// plugin could be fully removed from all registries. The remaining MCP
    /// names stay tracked so a later `disable()`/reload pass can retry.
    DisablingFailed {
        /// Parsed manifest.
        manifest: PluginManifest,
        /// Install directory on disk.
        install_dir: PathBuf,
        /// Diagnostic message from the failed unload attempt.
        error: String,
    },
    /// Plugin is installed but explicitly disabled — components are not in
    /// any registry.
    Disabled {
        /// Parsed manifest.
        manifest: PluginManifest,
        /// Install directory on disk.
        install_dir: PathBuf,
    },
    /// Install or load failed.
    Failed {
        /// Source that failed.
        source: PluginSource,
        /// Diagnostic message.
        error: String,
    },
    /// Plugin was matched by the blocklist (static or remote) and rejected
    /// before any registry mutation.
    Blocked {
        /// Source that was blocked.
        source: PluginSource,
        /// Reason returned by the blocklist (e.g. `"static blocklist"`).
        reason: String,
    },
}
