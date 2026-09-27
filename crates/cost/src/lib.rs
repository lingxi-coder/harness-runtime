//! Cost subsystem: pricing catalog, token/non-token usage aggregation, a
//! saturating-arithmetic cost calculator, an in-process cost tracker, a
//! budget enforcer, and (M3-05) cost-event emission + budget alarms +
//! `/cost` data API.
//!
//! See design spec §7 (Cost), the M1 plan for the M1 scope, and the M3
//! engine-completion design (`docs/superpowers/specs/2026-05-23-m3-engine-completion-design.md`)
//! for the M3-05 extensions.
#![forbid(unsafe_code)]
// Documentation debt, not a decision that docs do not matter: this crate had
// 7 undocumented public item(s) when `missing_docs` was measured across the
// workspace (2026-09-16). The lint stays `warn` at the workspace level so a NEW
// crate still inherits the requirement; this allow is scoped here so the debt
// is visible per crate and can be repaid one crate at a time by deleting this
// line.
#![allow(missing_docs)]
// Dead code kept visible, not swept: this crate had 3 item(s) rustc could
// reach from nothing when the workspace was measured (2026-09-16). The lint
// stays `warn` at the workspace level so a NEW crate still inherits it; this
// allow is scoped here so the count is per crate and repayable by deleting this
// line. This is the category where "named, computed, never wired" hides — some
// of these read like features that were built and never connected. Each wants a
// decision (delete, or wire), not a blanket deletion.
// ⚠️ The count above is ONE macOS, lib-target measurement. It is not a list of
// deletable items — see docs/HANDOFF-dead-code-adjudication-2026-09-17.md,
// which records two near-misses where it said "dead" about live code.
#![allow(dead_code)]

pub mod attempt;
pub mod budget;
pub mod calculator;
pub mod events;
pub mod handle;
pub mod persistence;
pub mod pricing;
pub mod prompt_cache_ledger;
pub mod render;
pub mod summary;
pub mod token_usage_replay;
pub mod tracker;
pub mod usage;

pub use attempt::{
    calculate_pinned_attempt_cost, AttemptBillingMode, AttemptContribution, AttemptDisposition,
    AttemptFoldAck, AttemptFoldError, AttemptIntent, AttemptLedger, AttemptOutputRecovery,
    AttemptOutputRevision, AttemptOutputScope, AttemptReceipt, AttemptStage, AttemptUsageContract,
    PreparedAttemptFold,
};
pub use budget::{
    BudgetCheckResult, BudgetConfig, BudgetEnforcer, BudgetExceedPolicy,
    BUDGET_EXCEEDED_THRESHOLD_BPS, BUDGET_WARNING_THRESHOLD_BPS,
};
pub use calculator::CostCalculator;
pub use events::{emit_api_success, ApiSuccessFields, EVENT_NAME_API_SUCCESS};
pub use persistence::{
    AttemptPersistAck, AttemptPersistMutation, AttemptPersistPermit, AttemptPersistRequest,
    CostDurabilityGate, CostHydration, CostHydrator, CostMutationId, CostMutationRecord,
    CostMutationSource, CostPersistAck, CostPersistError, CostPersistPermit, CostPersistRequest,
    CostPersistResult, CostPersistence, CostStateVector,
};
pub use pricing::{
    nano_usd_to_dollars_format, CostError, ModelPricing, ModelRef, MoneyPerToken,
    NonTokenBillableUnit, PricingCatalog, PricingResolution, PricingSource, ProviderId, TokenClass,
    BATCH_DISCOUNT_BPS,
};
pub use summary::{CostSummary, ModelCostSummary, PeriodCostSummary, SessionCostSummary};
pub use token_usage_replay::{
    latest_token_usage_turn_id, latest_token_usage_turn_id_from_events, ReplayTurn,
    ReplayTurnStatus,
};
pub use tracker::{
    CostAttemptReceipt, CostAttemptSettlement, CostModelResponse, CostResponseObservation,
    CostResponseReceipt, CostResponseSettlement, CostSessionScope, CostState, CostTracker,
    ModelUsage, PreparedCostSession, RetainedCostResponse,
};
pub use usage::{ApiSpeed, ServerToolUsage, TokenUsage, Usage};
