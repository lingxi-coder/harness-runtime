//! `/cost` data API: `CostTracker::summary() -> CostSummary { session, day, month, by_model }`.
//!
//! M5 will route this through the `/cost` slash command; M3-05 only provides
//! the data shape and the read-side aggregation.
//!
//! See spec §1 non-goals (line 68) — M3-05 provides the data stub; the actual
//! slash-command UI ships in M5.
//!
//! ## Time-bucketing in M3-05
//!
//! M3-05's `CostTracker` does not yet persist time-series data; it tracks a
//! single cumulative total. So `day` and `month` report **the current
//! cumulative session total** under a **correctly-labeled** day / month
//! bucket. M5 will extend the tracker with windowed buckets when wiring
//! `/cost --day` and `/cost --month` flags; until then, both buckets equal
//! the session total.
//!
//! The labels (`YYYY-MM-DD` and `YYYY-MM`) ARE correct as of `Utc::now()`.

#![forbid(unsafe_code)]

use crate::tracker::{CostState, CostTracker};
use crate::ModelRef;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Aggregate of one tracker's cost state, returned by [`CostTracker::summary`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CostSummary {
    /// Session-scope totals.
    pub session: SessionCostSummary,
    /// Day-scope totals (currently equals session total — see module docs).
    pub day: PeriodCostSummary,
    /// Month-scope totals (currently equals session total — see module docs).
    pub month: PeriodCostSummary,
    /// Per-model breakdown.
    pub by_model: HashMap<ModelRef, ModelCostSummary>,
}

/// Session-scope rollup.
///
/// `lingxi_core::host::CostSnapshot` (M5-02) mirrors the three primary fields
/// (`session_id`, `total_nano_usd`, `total_tokens`) without depending on
/// `lingxi-cost`, so traits-tier consumers can publish costs without
/// pulling in pricing. The two types convert via
/// `From<&SessionCostSummary> for CostSnapshot`, wired in `lingxi-cost`
/// behind a `traits` feature in a later plan; for M5-02 the mapping is
/// the orchestrator's responsibility.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionCostSummary {
    /// The session this summary is for.
    pub session_id: lingxi_core::types::SessionId,
    /// Cumulative cost across all models in nano-USD.
    pub total_nano_usd: u64,
    /// Cumulative total tokens (input + output across all models).
    pub total_tokens: u64,
    /// Subset of `total_nano_usd` recorded via
    /// [`crate::tracker::CostTracker::record_external_cost`] rather than
    /// [`crate::tracker::CostTracker::record_api_response_v2`] — spend
    /// (currently: Fusion panel/analyst/synthesizer settlement) that is real
    /// and already included in `total_nano_usd`, but has no per-model
    /// `ModelRef` to attribute inside `by_model` (a Fusion run prices
    /// several models' usage into one already-computed total at the
    /// reservation-commit seam). On a session whose money was never touched
    /// by the legacy compatibility restore, `by_model`'s totals plus this
    /// field and `legacy_opening_balance_nano_usd` should reconcile to
    /// `total_nano_usd`; a residual gap beyond those fields is an actual bug.
    ///
    /// NOTE: this is the [`CostTracker::summary`] projection, which has no
    /// production caller today — the live `/usage`/`/cost` rendering path is
    /// `ConversationModel::snapshot_cost_real` /
    /// `lingxi_core::host::CostSnapshot`, which does not yet read this field (see
    /// [`crate::tracker::CostTracker::record_external_cost`] doc comment).
    pub external_nano_usd: u64,
    /// Pre-V1 totals-only opening balance included in `total_nano_usd` but not
    /// attributable to any new model row or Fusion aggregate.
    #[serde(default)]
    pub legacy_opening_balance_nano_usd: u64,
}

/// Period-scope rollup (day or month).
///
/// `label` is a calendar bucket (`"YYYY-MM-DD"` for day, `"YYYY-MM"` for
/// month, both UTC); `total_nano_usd` is the cumulative cost in that bucket.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PeriodCostSummary {
    /// Calendar label of the bucket (e.g. `"2026-05-23"` or `"2026-05"`).
    pub label: String,
    /// Cumulative cost across all models in nano-USD within this bucket.
    pub total_nano_usd: u64,
}

/// Per-model entry of [`CostSummary::by_model`].
///
/// `Default` is intentionally not derived: `ModelRef` has no `Default` impl,
/// and no current callsite constructs a default per-model summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCostSummary {
    /// Which model this slice is for.
    pub model_ref: ModelRef,
    /// Cumulative cost for this model in nano-USD.
    pub total_nano_usd: u64,
    /// Cumulative input tokens for this model.
    pub input_tokens: u64,
    /// Cumulative output tokens for this model.
    pub output_tokens: u64,
    /// Cumulative tokens read from the prompt cache.
    pub cache_read_input_tokens: u64,
    /// Cumulative tokens written into the prompt cache.
    pub cache_creation_input_tokens: u64,
}

impl CostTracker {
    /// Snapshot the current state into a [`CostSummary`] for the `/cost`
    /// slash command (M5 will route this through the slash command).
    ///
    /// `day` and `month` use UTC date bucketing as of the call. Until M5
    /// extends the tracker with time-windowed buckets, both amounts equal
    /// the cumulative session total.
    pub async fn summary(&self) -> CostSummary {
        let state: CostState = self.snapshot().await;
        let now = Utc::now();
        let day_label = now.format("%Y-%m-%d").to_string();
        let month_label = now.format("%Y-%m").to_string();

        let mut total_tokens = 0u64;
        let mut by_model = HashMap::new();
        for (mr, mu) in &state.per_model_usage {
            total_tokens = total_tokens
                .saturating_add(mu.usage.tokens.input)
                .saturating_add(mu.usage.tokens.output);
            by_model.insert(
                mr.clone(),
                ModelCostSummary {
                    model_ref: mr.clone(),
                    total_nano_usd: mu.cost_nano_usd,
                    input_tokens: mu.usage.tokens.input,
                    output_tokens: mu.usage.tokens.output,
                    cache_read_input_tokens: mu.cache_read_input_tokens,
                    cache_creation_input_tokens: mu.cache_creation_input_tokens,
                },
            );
        }

        CostSummary {
            session: SessionCostSummary {
                session_id: state.session_id,
                total_nano_usd: state.total_nano_usd,
                total_tokens,
                external_nano_usd: state.external_nano_usd,
                legacy_opening_balance_nano_usd: state.legacy_opening_balance_nano_usd,
            },
            day: PeriodCostSummary {
                label: day_label,
                total_nano_usd: state.total_nano_usd,
            },
            month: PeriodCostSummary {
                label: month_label,
                total_nano_usd: state.total_nano_usd,
            },
            by_model,
        }
    }
}
