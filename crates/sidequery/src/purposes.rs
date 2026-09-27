//! Telemetry / COGS tag for side queries and forked agents.
//!
//! Every side query (one-shot LLM call) and forked agent (full subagent loop
//! rooted at the parent's cache-safe prompt prefix) is tagged with one of
//! these variants so cost rollups can attribute spend per purpose.

use serde::{Deserialize, Serialize};

/// Tagging the purpose of a side query/forked agent in telemetry COGS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum QuerySource {
    /// §6.3 memory selector chooses which Tier 3 files surface in this turn.
    MemorySelector,
    /// §7.4 permission explainer renders a free-text rationale for a denial.
    PermissionExplainer,
    /// §6.5 session search / RAG over prior sessions.
    SessionSearch,
    /// Lightweight intent / classification side query.
    Classifier,
    /// §13.6 autocompactor (forked).
    Compaction,
    /// §6.5 post-session memory extraction (forked).
    SessionMemoryExtraction,
    /// §10.7 supervisor / mediator (forked).
    Supervisor,
    /// Prompt suggestion / autocomplete side query.
    PromptSuggestion,
    /// Post-turn summary / digest (forked).
    PostTurnSummary,
    /// §9 skill execution helper (forked).
    SkillExecution,
    /// `WebFetch`'s secondary "apply" call: process fetched markdown with the
    /// caller's prompt via a small-fast model (claude-code `web_fetch_apply`).
    WebFetchApply,
    /// Delegated image analysis performed before a non-vision main-model turn.
    VisionDelegation,
    /// Fusion panel subagent / provider call.
    FusionPanel,
    /// Fusion analyst strict JSON side query.
    FusionAnalyst,
    /// Fusion synthesizer side query (merge path only).
    FusionSynthesizer,
    /// Caller-supplied label for purposes not enumerated above.
    Custom(String),
}

impl QuerySource {
    /// Stable telemetry label for this side-query purpose.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::MemorySelector => "memory_selector",
            Self::PermissionExplainer => "permission_explainer",
            Self::SessionSearch => "session_search",
            Self::Classifier => "classifier",
            Self::Compaction => "compaction",
            Self::SessionMemoryExtraction => "session_memory_extraction",
            Self::Supervisor => "supervisor",
            Self::PromptSuggestion => "prompt_suggestion",
            Self::PostTurnSummary => "post_turn_summary",
            Self::SkillExecution => "skill_execution",
            Self::WebFetchApply => "web_fetch_apply",
            Self::VisionDelegation => "vision_delegation",
            Self::FusionPanel => "fusion_panel",
            Self::FusionAnalyst => "fusion_analyst",
            Self::FusionSynthesizer => "fusion_synthesizer",
            Self::Custom(value) => value.as_str(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::QuerySource;

    #[test]
    fn web_fetch_apply_roundtrips() {
        let q = QuerySource::WebFetchApply;
        let json = serde_json::to_string(&q).unwrap();
        assert_eq!(json, "\"WebFetchApply\"");
        let back: QuerySource = serde_json::from_str(&json).unwrap();
        assert_eq!(back, QuerySource::WebFetchApply);
    }

    #[test]
    fn vision_delegation_roundtrips() {
        let q = QuerySource::VisionDelegation;
        let json = serde_json::to_string(&q).unwrap();
        assert_eq!(json, "\"VisionDelegation\"");
        let back: QuerySource = serde_json::from_str(&json).unwrap();
        assert_eq!(back, QuerySource::VisionDelegation);
    }

    #[test]
    fn fusion_sources_roundtrip_and_label() {
        for (q, label, json) in [
            (QuerySource::FusionPanel, "fusion_panel", "\"FusionPanel\""),
            (
                QuerySource::FusionAnalyst,
                "fusion_analyst",
                "\"FusionAnalyst\"",
            ),
            (
                QuerySource::FusionSynthesizer,
                "fusion_synthesizer",
                "\"FusionSynthesizer\"",
            ),
        ] {
            assert_eq!(q.as_str(), label);
            let encoded = serde_json::to_string(&q).unwrap();
            assert_eq!(encoded, json);
            let back: QuerySource = serde_json::from_str(&encoded).unwrap();
            assert_eq!(back, q);
        }
    }
}
