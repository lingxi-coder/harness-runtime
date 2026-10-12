//! Native worker-state metadata carrier for the proactivity selector.

use crate::types::utf16_json::Utf16JsonProjection;
use serde::{Deserialize, Serialize};

/// Raw Native `sessionState` metadata returned by or restored from an
/// authoritative worker-state host. This is not a JSONL transcript format.
///
/// Native stores opt-in under `internal.proactivity_opt_in` and the mirrored
/// level under `external.proactivity`. The projections preserve absent versus
/// explicit null fields; callers must opt in only for a strict boolean `true`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeProactivitySessionMetadata {
    /// Native `sessionState.internal`, when present in worker state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub internal: Option<Utf16JsonProjection>,
    /// Native `sessionState.external`, when present in worker state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external: Option<Utf16JsonProjection>,
}

/// Public state query for a host's current proactivity control surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeProactivityStatusSnapshot {
    /// Whether the session has an explicit Native opt-in.
    pub opted_in: bool,
    /// Whether the current host gates permit an explicit opt-in.
    pub opt_in_available: bool,
    /// Whether the live selector is currently enabled.
    pub selector_gate: bool,
    /// Whether the selector has been enabled in this session before.
    pub selector_gate_ever_on: bool,
    /// Whether model-facing attachments are enabled for this session.
    pub attachments_enabled: bool,
    /// Current legal proactivity level.
    pub level: String,
}

impl Default for NativeProactivityStatusSnapshot {
    fn default() -> Self {
        Self {
            opted_in: false,
            opt_in_available: false,
            selector_gate: false,
            selector_gate_ever_on: false,
            attachments_enabled: false,
            level: "default".to_owned(),
        }
    }
}

/// Successful response from the current Native `set_proactivity_level` API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeProactivityControlResponse {
    /// Current legal level after the update.
    pub level: String,
    /// Permission mode selected by the live policy gate.
    pub permission_mode: String,
}
