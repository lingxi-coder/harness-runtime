//! Fusion multi-model deliberation orchestrator.
//!
//! This crate implements the Fusion state machine. Callers depend on
//! [`platform_api::FusionExecutor`]; composition roots inject a
//! [`FusionOrchestrator`]. There is no public Agent / slash / workflow entry
//! here — those land in later PRs.

#![forbid(unsafe_code)]
// Documentation debt, not a decision that docs do not matter: this crate had
// 2 undocumented public item(s) when `missing_docs` was measured across the
// workspace (2026-09-16). The lint stays `warn` at the workspace level so a NEW
// crate still inherits the requirement; this allow is scoped here so the debt
// is visible per crate and can be repaid one crate at a time by deleting this
// line.
#![allow(missing_docs)]
// Dead code kept visible, not swept: this crate had 4 item(s) rustc could
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

mod analyst;
mod attempts;
mod budget;
mod citations;
mod config;
mod decision;
mod model_resolver;
mod orchestrator;
mod packing;
mod panel;
mod progress;
mod snapshot;
mod synthesizer;

pub use attempts::{
    FusionAttemptFinalizer, FusionAttemptLivePolicy, FusionAttemptRegistrar,
    FusionAttemptRegistration, FusionAttemptSettlement, FusionAttemptSettlementError,
    FusionAttemptSummary, FusionPanelAttemptFence, RegisteredFusionAttempts,
};
pub use budget::{CapturedPriceBook, FusionPriceBook, FusionQuote, ModelRates};
pub use config::{FusionCompletionPolicy, FusionConfigSource, FusionRuntimeConfig};
pub use model_resolver::{CatalogModel, ModelLimits, ModelSource, ResolvedPanel, ResolvedSet};
pub use orchestrator::FusionOrchestrator;
pub use snapshot::{CatalogRevision, CatalogSnapshot, FusionRuntimeSnapshot};

#[cfg(test)]
mod orchestrator_test;
