//! `tengu_cost_*` event schemas — 9 events. M3-05 emits 2; M3-06 locks all 9.
//!
//! Spec §7 line 730-745. The two M3-05-emitted events
//! (`tengu_cost_budget_warning`, `tengu_cost_budget_exceeded`) byte-match the
//! wire shape emitted on the budget-threshold path. The remaining 7 schemas
//! are M4-staged but locked here so M4 can emit without bumping the schema tree.
//!
//! Strict-parity note: the former `tengu_cost_recorded` event (per-API-call
//! cost record) was PORT-ONLY — 0 hits in claude-code 2.1.195 — and was
//! dropped. The per-request success telemetry is now `tengu_api_success`
//! (emitted by `cost::events::emit_api_success`), which IS present in 2.1.195.

use crate::pii::Verified;
use serde::{Deserialize, Serialize};

/// `tengu_cost_budget_warning` — 80% threshold crossed. M3-05 emitter.
pub const BUDGET_WARNING: &str = "tengu_cost_budget_warning";
/// `tengu_cost_budget_exceeded` — 100% threshold crossed. M3-05 emitter.
pub const BUDGET_EXCEEDED: &str = "tengu_cost_budget_exceeded";
/// `tengu_cost_summary_requested` — `/cost` slash-command invocation.
pub const SUMMARY_REQUESTED: &str = "tengu_cost_summary_requested";
/// `tengu_cost_summary_generated` — `/cost` summary payload produced.
pub const SUMMARY_GENERATED: &str = "tengu_cost_summary_generated";
/// `tengu_cost_unknown_model` — a model lacked a pricing-table entry.
pub const UNKNOWN_MODEL: &str = "tengu_cost_unknown_model";
/// `tengu_cost_batch_discount_applied` — M4-only: `/v1/messages/batches` discount.
pub const BATCH_DISCOUNT_APPLIED: &str = "tengu_cost_batch_discount_applied";
/// `tengu_cost_pricing_table_refreshed` — pricing table reloaded.
pub const PRICING_TABLE_REFRESHED: &str = "tengu_cost_pricing_table_refreshed";
/// `tengu_cost_session_total_updated` — per-session aggregate moved.
pub const SESSION_TOTAL_UPDATED: &str = "tengu_cost_session_total_updated";
/// `tengu_cost_persistence_failed` — cost-state flush to disk failed.
pub const PERSISTENCE_FAILED: &str = "tengu_cost_persistence_failed";

pub(crate) const NAMES: &[&str] = &[
    BUDGET_WARNING,
    BUDGET_EXCEEDED,
    SUMMARY_REQUESTED,
    SUMMARY_GENERATED,
    UNKNOWN_MODEL,
    BATCH_DISCOUNT_APPLIED,
    PRICING_TABLE_REFRESHED,
    SESSION_TOTAL_UPDATED,
    PERSISTENCE_FAILED,
];

/// Payload for [`BUDGET_WARNING`] — fired once when usage crosses 80%.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetWarningPayload {
    /// Nano-USD limit.
    pub limit_usd: u64,
    /// Nano-USD current.
    pub current_usd: u64,
    /// Basis-points percentage (8000 = 80%).
    pub percent_bps: u64,
}

/// Payload for [`BUDGET_EXCEEDED`] — fired once when usage crosses 100%.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetExceededPayload {
    /// Nano-USD limit.
    pub limit_usd: u64,
    /// Nano-USD current.
    pub current_usd: u64,
}

/// Payload for [`SUMMARY_REQUESTED`] — `/cost` invocation entered.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SummaryRequestedPayload {
    /// Session identifier the summary was requested for.
    pub session_id: Verified,
}

/// Payload for [`SUMMARY_GENERATED`] — `/cost` payload built and rendered.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SummaryGeneratedPayload {
    /// Session identifier the summary was generated for.
    pub session_id: Verified,
    /// Nano-USD total for this session.
    pub session_total_usd: u64,
    /// Nano-USD total for the calendar day (rolling).
    pub day_total_usd: u64,
    /// Nano-USD total for the calendar month (rolling).
    pub month_total_usd: u64,
    /// Distinct models billed in this summary window.
    pub models_billed: u32,
}

/// Payload for [`UNKNOWN_MODEL`] — pricing-table miss, fell back to default.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnknownModelPayload {
    /// Model identifier that lacked a pricing-table entry.
    pub model: Verified,
}

/// M4-only schema; M3 never emits. Locked here so M4 can plug in without bumping schemas.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchDiscountAppliedPayload {
    /// Model identifier on the discounted batch.
    pub model: Verified,
    /// Nano-USD before the batch discount.
    pub original_cost_usd: u64,
    /// Nano-USD after the batch discount.
    pub discounted_cost_usd: u64,
    /// Basis-points discount applied (5000 = 50%).
    pub discount_bps: u64,
}

/// Payload for [`PRICING_TABLE_REFRESHED`] — pricing data reloaded.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PricingTableRefreshedPayload {
    /// Distinct model entries now resident in the table.
    pub entries: u32,
}

/// Payload for [`SESSION_TOTAL_UPDATED`] — per-session aggregate moved.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTotalUpdatedPayload {
    /// Session identifier whose total was updated.
    pub session_id: Verified,
    /// New nano-USD running total for this session.
    pub total_usd: u64,
}

/// Payload for [`PERSISTENCE_FAILED`] — cost-state flush to disk failed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistenceFailedPayload {
    /// Session identifier whose state failed to persist.
    pub session_id: Verified,
    /// Whitelisted error kind (no raw IO details).
    pub error: Verified,
}
