//! Claude.ai subscription limits tracker.
//!
//! See spec §30.4. The server returns rate-limit hints in `x-claudeai-*`
//! headers on every API call; the tracker accumulates the most recent values
//! so UI surfaces can render quota warnings without an extra round-trip.

#![allow(clippy::unwrap_used)]

use lingxi_llm_client::auth::oauth::anthropic::parse_claudeai_quota_headers;
pub use lingxi_llm_client::auth::oauth::anthropic::SubscriptionType;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::SystemTime;

/// Snapshot of subscription state and rolling-window usage.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClaudeAiLimitsState {
    /// Latest known subscription tier (if reported).
    pub subscription_type: Option<SubscriptionType>,
    /// Messages used in the current rolling window.
    pub message_count_window: u32,
    /// Message cap for the current rolling window.
    pub message_limit_window: u32,
    /// When the current rolling window resets (server clock).
    pub window_resets_at: Option<SystemTime>,
    /// Overage usage in USD beyond the included quota.
    pub extra_usage_dollars: f64,
    /// Last time this struct was updated from a server response.
    pub last_updated: Option<SystemTime>,
}

/// Thread-safe in-memory store for the latest [`ClaudeAiLimitsState`].
///
/// Updates are folded in from `x-claudeai-*` HTTP headers via
/// [`Self::update_from_headers`]; callers read the most recent values
/// via [`Self::snapshot`].
pub struct ClaudeAiLimitsTracker {
    state: Mutex<ClaudeAiLimitsState>,
}

impl ClaudeAiLimitsTracker {
    /// Construct an empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(ClaudeAiLimitsState::default()),
        }
    }

    /// Fold a response's `x-claudeai-*` headers into the current state.
    ///
    /// Headers absent from the map leave the corresponding field unchanged.
    pub fn update_from_headers(&self, headers: &HashMap<String, String>) {
        let mut s = self.state.lock().unwrap();
        let quota = parse_claudeai_quota_headers(headers);
        if let Some(c) = quota.message_count_window {
            s.message_count_window = c;
        }
        if let Some(l) = quota.message_limit_window {
            s.message_limit_window = l;
        }
        s.last_updated = Some(SystemTime::now());
    }

    /// Return a cloned snapshot of the current state.
    #[must_use]
    pub fn snapshot(&self) -> ClaudeAiLimitsState {
        self.state.lock().unwrap().clone()
    }
}

impl Default for ClaudeAiLimitsTracker {
    fn default() -> Self {
        Self::new()
    }
}
