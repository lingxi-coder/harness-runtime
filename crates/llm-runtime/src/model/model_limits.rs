//! Process-global per-model token-limit registry.
//!
//! ## Why this exists
//!
//! [`context_window_for_model`](super::context_window::context_window_for_model)
//! and [`max_output_tokens_for_model`](super::context_window::max_output_tokens_for_model)
//! are free functions taking only `model: &str` (and `betas`). They are called
//! from low-level crates (`compaction`) and the CLI with no access to the
//! resolved provider profile, so there is no parameter to thread per-model
//! limits through.
//!
//! claude-code is single-provider, so its hardcoded `200k`/`32k` Claude tables
//! are always correct there. In LingXi the SAME functions run for OpenAI,
//! Gemini, DeepSeek, GLM, etc., and those Claude defaults are simply wrong
//! (e.g. `gpt-5.4` has a 1,050,000-token window, `deepseek-chat` 384,000 max
//! output). The real per-model limits exist — `models.dev` slices carry
//! model metadata — but were
//! parsed and discarded.
//!
//! This registry is the bridge: at catalog-assembly time the real limits are
//! registered keyed by every id a caller might pass (request id, display name,
//! aliases); the window/output functions consult it for **non-Claude** models
//! before falling back to the byte-faithful Claude tables. Claude-family ids
//! (incl. Bedrock/Vertex-hosted Claude) deliberately bypass the registry so the
//! 200k / `[1m]`-beta / canonical max-output behavior stays byte-identical to
//! claude-code.

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

/// Real per-model token limits sourced from the provider catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelLimits {
    /// Context-window size in tokens.
    pub context_window: u64,
    /// Maximum output tokens.
    pub max_output_tokens: u64,
}

fn registry() -> &'static RwLock<HashMap<String, ModelLimits>> {
    static REG: OnceLock<RwLock<HashMap<String, ModelLimits>>> = OnceLock::new();
    REG.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Register `limits` under `model_id`.
///
/// The same bare id can appear in several provider slices (e.g. `gpt-4.1` ships
/// via both OpenAI and GitHub Copilot, with different per-provider caps). Since
/// the window/output functions only receive the bare id — not the resolved
/// route — registration is **order-independent**: when an id is already present
/// we keep the most generous (max) of each field. Under-reporting a window
/// silently truncates most of the usable context; over-reporting at worst
/// defers compaction slightly and is recoverable, so max is the safe direction.
pub fn register(model_id: &str, limits: ModelLimits) {
    if model_id.is_empty() {
        return;
    }
    if let Ok(mut guard) = registry().write() {
        guard
            .entry(model_id.to_string())
            .and_modify(|existing| {
                existing.context_window = existing.context_window.max(limits.context_window);
                existing.max_output_tokens =
                    existing.max_output_tokens.max(limits.max_output_tokens);
            })
            .or_insert(limits);
    }
}

/// Look up the real limits for `model_id`, or `None` when unregistered.
#[must_use]
pub fn lookup(model_id: &str) -> Option<ModelLimits> {
    registry()
        .read()
        .ok()
        .and_then(|guard| guard.get(model_id).copied())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_then_lookup_roundtrips() {
        let id = "test-model-limits-roundtrip-xyz";
        assert_eq!(lookup(id), None);
        register(
            id,
            ModelLimits {
                context_window: 1_050_000,
                max_output_tokens: 128_000,
            },
        );
        let got = lookup(id).expect("registered");
        assert_eq!(got.context_window, 1_050_000);
        assert_eq!(got.max_output_tokens, 128_000);
    }

    #[test]
    fn empty_id_is_ignored() {
        register(
            "",
            ModelLimits {
                context_window: 1,
                max_output_tokens: 1,
            },
        );
        assert_eq!(lookup(""), None);
    }
}
