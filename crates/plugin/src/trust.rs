//! Plugin trust classification.
//!
//! Three levels — `AdminTrusted` (built-in / official marketplace),
//! `UserTrusted` (third-party marketplace / `.mcpb` bundle), and
//! `Untrusted` (git / local-path checkouts). The default for each source
//! is described by [`default_trust_for_source`]; a host can override the
//! level explicitly at install time.
//!
//! See spec A7: git and local-path plugins default to `Untrusted`.

use crate::source::PluginSource;
use serde::{Deserialize, Serialize};

/// Trust level applied to plugin-supplied components.
///
/// The trust level decides whether high-privilege manifest fields (hooks
/// that escalate, MCP servers contributing tools, etc.) require an extra
/// approval prompt before the registry will materialise them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PluginTrustLevel {
    /// Engine-built-in or hosted on the official Anthropic marketplace.
    AdminTrusted,
    /// Installed from a third-party marketplace or `.mcpb` bundle with
    /// the user's explicit consent.
    UserTrusted,
    /// Anything not vouched for at install time — git/local checkouts.
    Untrusted,
}

/// Default trust level for `source` (see spec A7).
#[must_use]
pub fn default_trust_for_source(src: &PluginSource) -> PluginTrustLevel {
    match src {
        PluginSource::BuiltIn | PluginSource::OfficialMarketplace { .. } => {
            PluginTrustLevel::AdminTrusted
        }
        PluginSource::Marketplace { .. } | PluginSource::Mcpb { .. } => {
            PluginTrustLevel::UserTrusted
        }
        PluginSource::Git { .. } | PluginSource::LocalPath { .. } => PluginTrustLevel::Untrusted,
    }
}
