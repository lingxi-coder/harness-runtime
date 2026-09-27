//! Pure attempt journal fold. The session owner persists intent/receipt and
//! resulting vector atomically before publishing these staged values.
//! This module grants no dispatch authority and performs no I/O.

use crate::{
    CostStateVector, ModelPricing, ModelRef, ModelUsage, NonTokenBillableUnit, TokenClass, Usage,
};
use protocol::SessionId;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use thiserror::Error;

/// Shared immutable contract; persisted variant names remain unchanged.
pub use platform_api::ModelAttemptBillingMode as AttemptBillingMode;

/// Shared platform stage metadata; it grants no dispatch authority.
pub use platform_api::ModelAttemptStage as AttemptStage;

/// Original output generation retained for restart reconciliation, never the
/// currently selected turn. Live admission stamps this from its bound account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptOutputScope {
    /// Captured turn or command-only generation.
    pub generation_id: protocol::MessageId,
    /// Immutable ceiling of that generation.
    pub max_output_tokens: Option<u64>,
}

/// Compact validated output contribution for startup hydration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttemptOutputRevision {
    /// Receipt revision, one or its one allowed correction.
    pub revision: u64,
    /// Exact, conservative unknown, or proven not sent.
    pub disposition: AttemptDisposition,
    /// Output occupancy, distinct from known token counters.
    pub output: u64,
}

/// A recovered attempt's original output account and deduplication markers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptOutputRecovery {
    /// Original generation; never install it as the current turn implicitly.
    pub scope: AttemptOutputScope,
    /// Stable physical attempt identity.
    pub attempt_id: String,
    /// First accepted receipt, retained for identical retry recognition.
    pub first: AttemptOutputRevision,
    /// Latest accepted replacement.
    pub current: AttemptOutputRevision,
}

/// Pinned normalized usage reachability, not a source of implicit free rates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttemptUsageContract {
    /// Input/output/cache-read/reasoning only; cache creation is unreachable.
    StandardDisjointTokensV1,
    /// Conservative six-bucket contract, also used by pre-contract intents.
    #[default]
    AnthropicCacheTtlV1,
}

/// Host-captured immutable wire authorization. No bearer tokens are persisted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptIntent {
    /// Settlement contract version: one uses fixed pinned token rates; two
    /// requires an SDK token quote and treats pinned token rates as bounds.
    /// Older readers must reject version two rather than misprice its bounds.
    pub schema_version: u32,
    /// Canonical session authority.
    pub session_id: SessionId,
    /// Older journals may omit this; they must never charge a new turn during
    /// recovery. Every newly admitted bound attempt receives an explicit scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_scope: Option<AttemptOutputScope>,
    /// Stable unique physical-send identity.
    pub attempt_id: String,
    /// Owning Fusion run identity.
    pub run_id: String,
    /// Stable logical call identity across explicit retries.
    pub logical_call_id: String,
    /// One-based physical-send ordinal within this logical call.
    pub wire_ordinal: u64,
    /// Owning stage.
    pub stage: AttemptStage,
    /// Required for panels, absent for analyst/synthesis.
    pub panel_slot: Option<u32>,
    /// Exact selected host profile.
    pub profile_id: String,
    /// Normalized selected provider/model.
    pub model: ModelRef,
    /// Captured routing configuration revision.
    pub route_revision: u64,
    /// Integer rates, effective date and original price provenance.
    /// Rates already include speed/subscription overrides; never re-resolve.
    pub pricing: ModelPricing,
    /// Token rates are admission bounds; settlement requires the SDK's actual
    /// per-attempt token quote. Older journals keep fixed-rate settlement.
    #[serde(default)]
    pub token_pricing_requires_quote: bool,
    /// Finite authorized monetary occupancy in nano-USD.
    pub authorized_nano_usd: u64,
    /// Captured prepared-request input basis, not observed token counters.
    pub authorized_input_tokens: u64,
    /// Finite authorized output occupancy, not an observed output counter.
    pub authorized_output_tokens: u64,
    /// Run-level choice, immutable across every intent for this run.
    pub billing_mode: AttemptBillingMode,
    /// Immutable supported usage buckets. Old records retain strict validation.
    #[serde(default)]
    pub usage_contract: AttemptUsageContract,
}

/// Meaning of an observation. Unknown counters are lower bounds, not a claim
/// that unobserved token classes were zero or that remote billing is exact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttemptDisposition {
    /// Dispatch may have occurred; preserve conservative occupancy.
    Unknown,
    /// Actual complete provider observation retained by the transport owner.
    Exact,
    /// Host proved no model request was dispatched.
    ProvenNotSent,
    /// Dispatch was marked, but no provider response was ever accepted: the
    /// transport failed, or the request was refused before any usage report.
    /// Distinct from `Unknown`, which means a response arrived with incomplete
    /// usage. Neither one licenses charging the authorization as if spent.
    NoProviderResponse,
}

/// An immutable revision of one attempt's actual observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptReceipt {
    /// Exact intent session.
    pub session_id: SessionId,
    /// Exact intent physical-send identity.
    pub attempt_id: String,
    /// First receipt is one; the only correction is the next revision.
    pub revision: u64,
    /// Prior Unknown revision replaced by an Exact observation.
    pub replaces_revision: Option<u64>,
    /// Truthful observation provenance.
    pub disposition: AttemptDisposition,
    /// SDK-estimated USD token cost, excluding host-owned non-token charges.
    /// Only Exact observations for quote-requiring intents may carry this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_quote_nano_usd: Option<u64>,
    /// Actual known, disjoint normalized billable buckets. In particular,
    /// output excludes reasoning_output; adapters must normalize overlapping
    /// provider counters before recording. For Unknown, zero means no known
    /// count and must not be interpreted as a complete provider usage report.
    pub usage: Usage,
    /// Provider's explicit cache-read request counter.
    pub cache_read_input_tokens: u64,
    /// Provider's explicit cache-creation request counter (may include TTL tiers).
    pub cache_creation_input_tokens: u64,
    /// Observed API duration.
    pub api_duration_ms: u64,
    /// Non-retry portion of observed API duration.
    pub api_duration_without_retries_ms: u64,
}

/// Additive accounting attributable to exactly one current receipt.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptContribution {
    /// Known usage, never manufactured from a monetary quote.
    pub usage: Usage,
    /// Explicit cache-read counter, independent of normalized token classes.
    pub cache_read_input_tokens: u64,
    /// Explicit cache-creation counter, independent of normalized token classes.
    pub cache_creation_input_tokens: u64,
    /// Known cost under the pinned settlement contract: fixed rates or an
    /// actual SDK token quote plus host-owned non-token charges.
    pub nano_usd: u64,
    /// Authorized-but-unaccounted remainder for an attempt whose usage report
    /// was incomplete. Disclosed beside the realized total, never inside it,
    /// and still counted against the session halt so an unpriced run cannot
    /// escape its ceiling.
    pub unverified_nano_usd: u64,
    /// One for a dispatched/uncertain request, zero for proven-not-sent.
    pub request_count: u64,
    /// Conservative or actual output occupancy, separate from token usage.
    pub output_occupancy: u64,
    /// Number of incomplete attempts contributing to this rollup.
    pub unknown_count: u64,
    /// Observed API duration.
    pub api_duration_ms: u64,
    /// Observed API duration excluding retries.
    pub api_duration_without_retries_ms: u64,
}

/// Original fold acknowledgment, reused verbatim for a matching old revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptFoldAck {
    /// Cost revision originally assigned by the fold.
    pub cost_revision: u64,
    /// Completion-order marker the owner should publish alongside this vector.
    pub last_usage_revision: Option<u64>,
    /// Current attempt contribution at this receipt revision.
    pub contribution: AttemptContribution,
}

/// Failed folds leave both ledger and supplied state unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AttemptFoldError {
    /// An immutable identity, mode, revision or observation disagreed.
    #[error("invalid attempt transition: {0}")]
    Invalid(&'static str),
    /// No intent exists for this receipt.
    #[error("receipt has no matching intent")]
    MissingIntent,
    /// An additive field overflowed or could not remove its prior contribution.
    #[error("attempt arithmetic overflow or underflow")]
    Arithmetic,
    /// An actually used billable class has no pinned rate.
    #[error("attempt usage has no pinned price")]
    MissingPrice,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ReceiptEntry {
    receipt: AttemptReceipt,
    ack: AttemptFoldAck,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct AttemptEntry {
    intent: AttemptIntent,
    receipts: BTreeMap<u64, ReceiptEntry>,
    completion_revision: Option<u64>,
}

/// Read-only prepared receipt transition. The journal owner can serialize its
/// result before publishing it, without cloning the session's attempt history.
/// Dropping this value has no effect on the ledger or cost projection.
#[derive(Debug)]
pub struct PreparedAttemptFold {
    receipt: AttemptReceipt,
    expected_entry: AttemptEntry,
    expected_rollup: Option<AttemptContribution>,
    base_state: CostStateVector,
    next_state: CostStateVector,
    next_rollup: Option<AttemptContribution>,
    completion_revision: Option<u64>,
    ack: AttemptFoldAck,
    duplicate: bool,
}

impl PreparedAttemptFold {
    /// State to include atomically with the receipt in its journal event.
    #[must_use]
    pub fn state(&self) -> &CostStateVector {
        &self.next_state
    }

    /// Original or newly assigned receipt acknowledgment.
    #[must_use]
    pub fn ack(&self) -> &AttemptFoldAck {
        &self.ack
    }

    /// True when no journal append or projection mutation is required.
    #[must_use]
    pub fn is_duplicate(&self) -> bool {
        self.duplicate
    }
}

/// Session-owned attempt history, separate from ordinary full cost vectors.
/// Reconstruct by replaying intents/receipts through the same checked methods;
/// do not trust deserialization as validation or as a parallel cost authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptLedger {
    session_id: SessionId,
    attempts: BTreeMap<String, AttemptEntry>,
    run_modes: BTreeMap<String, AttemptBillingMode>,
    /// Derived identity index; rebuilt by normal intent replay.
    wire_identities: BTreeSet<(String, String, u64)>,
    /// Derived current contributions; never independently persisted.
    model_contributions: HashMap<ModelRef, AttemptContribution>,
}

impl AttemptLedger {
    /// Recover compact output contributions after intent recovery has settled
    /// every incomplete send. Legacy unbound intents cannot name a new turn.
    pub fn output_recovery(&self) -> Vec<AttemptOutputRecovery> {
        self.attempts
            .values()
            .filter_map(|entry| {
                let scope = entry.intent.output_scope.clone()?;
                let (_, first) = entry.receipts.first_key_value()?;
                let (_, current) = entry.receipts.last_key_value()?;
                let revision = |entry: &ReceiptEntry| AttemptOutputRevision {
                    revision: entry.receipt.revision,
                    disposition: entry.receipt.disposition,
                    output: entry.ack.contribution.output_occupancy,
                };
                Some(AttemptOutputRecovery {
                    scope,
                    attempt_id: entry.intent.attempt_id.clone(),
                    first: revision(first),
                    current: revision(current),
                })
            })
            .collect()
    }

    /// Empty fold authority for one canonical session.
    #[must_use]
    pub fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            attempts: BTreeMap::new(),
            run_modes: BTreeMap::new(),
            wire_identities: BTreeSet::new(),
            model_contributions: HashMap::new(),
        }
    }

    /// Accept an immutable intent without touching a cost revision. Returns
    /// false for an exact retry and true for a newly accepted intent.
    pub fn record_intent(&mut self, intent: AttemptIntent) -> Result<bool, AttemptFoldError> {
        if !self.check_intent(&intent)? {
            return Ok(false);
        }
        let wire_identity = (
            intent.run_id.clone(),
            intent.logical_call_id.clone(),
            intent.wire_ordinal,
        );
        self.run_modes
            .insert(intent.run_id.clone(), intent.billing_mode);
        self.wire_identities.insert(wire_identity);
        self.attempts.insert(
            intent.attempt_id.clone(),
            AttemptEntry {
                intent,
                receipts: BTreeMap::new(),
                completion_revision: None,
            },
        );
        Ok(true)
    }

    /// Validate before writing an intent to the journal, without changing any
    /// state. Returns false for an identical already-recorded intent.
    pub fn check_intent(&self, intent: &AttemptIntent) -> Result<bool, AttemptFoldError> {
        validate_intent(intent, self.session_id)?;
        if let Some(old) = self.attempts.get(&intent.attempt_id) {
            return if &old.intent == intent {
                Ok(false)
            } else {
                Err(AttemptFoldError::Invalid("conflicting intent"))
            };
        }
        if self
            .run_modes
            .get(&intent.run_id)
            .is_some_and(|mode| *mode != intent.billing_mode)
        {
            return Err(AttemptFoldError::Invalid("run billing mode changed"));
        }
        let wire_identity = (
            intent.run_id.clone(),
            intent.logical_call_id.clone(),
            intent.wire_ordinal,
        );
        if self.wire_identities.contains(&wire_identity) {
            return Err(AttemptFoldError::Invalid("wire ordinal reused"));
        }
        Ok(true)
    }

    /// Look up the canonical immutable intent.
    #[must_use]
    pub fn intent(&self, id: &str) -> Option<&AttemptIntent> {
        self.attempts.get(id).map(|entry| &entry.intent)
    }

    /// Most recent persisted receipt and its original acknowledgment.
    #[must_use]
    pub fn latest_receipt(&self, id: &str) -> Option<(&AttemptReceipt, &AttemptFoldAck)> {
        self.attempts
            .get(id)?
            .receipts
            .last_key_value()
            .map(|(_, entry)| (&entry.receipt, &entry.ack))
    }

    /// Stable recovery scan order; legacy intents are never auto-converted.
    #[must_use]
    pub fn pending_intent_ids(&self) -> Vec<&str> {
        self.attempts
            .iter()
            .filter(|(_, entry)| {
                entry.receipts.is_empty()
                    && entry.intent.billing_mode == AttemptBillingMode::MeteredAttempts
            })
            .map(|(id, _)| id.as_str())
            .collect()
    }

    /// Deterministic incomplete receipt for an intent without a receipt.
    /// Recovery must persist this before admitting dependent paid work. It
    /// cannot tell whether the remote service billed, and never reruns it.
    pub fn recovery_receipt(&self, id: &str) -> Result<Option<AttemptReceipt>, AttemptFoldError> {
        let entry = self
            .attempts
            .get(id)
            .ok_or(AttemptFoldError::MissingIntent)?;
        if entry.intent.billing_mode != AttemptBillingMode::MeteredAttempts {
            return Err(AttemptFoldError::Invalid(
                "legacy run cannot use attempt recovery",
            ));
        }
        Ok(entry.receipts.is_empty().then(|| AttemptReceipt {
            session_id: self.session_id,
            attempt_id: id.to_string(),
            revision: 1,
            replaces_revision: None,
            disposition: AttemptDisposition::Unknown,
            token_quote_nano_usd: None,
            usage: Usage::default(),
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            api_duration_ms: 0,
            api_duration_without_retries_ms: 0,
        }))
    }

    /// Stage one receipt into the supplied vector. `last_usage_revision` is
    /// the session owner's authoritative completion marker INCLUDING ordinary
    /// model responses. Corrections retain their first completion position.
    /// The owner must persist ledger event, vector and marker as one mutation.
    /// An old duplicate returns its original acknowledgment without changing
    /// either input; the caller must not republish its old completion marker.
    pub fn fold_receipt(
        &mut self,
        state: &mut CostStateVector,
        receipt: AttemptReceipt,
        last_usage_revision: Option<u64>,
    ) -> Result<AttemptFoldAck, AttemptFoldError> {
        if state.last_usage_revision != last_usage_revision {
            return Err(AttemptFoldError::Invalid("completion marker mismatch"));
        }
        let prepared = self.prepare_receipt(state, receipt)?;
        self.commit_receipt(state, prepared)
    }

    /// Compute a checked receipt transition without publishing it. The caller
    /// must append its event durably before commit, serializing other mutations
    /// for that session through the existing coordinator queue.
    pub fn prepare_receipt(
        &self,
        state: &CostStateVector,
        receipt: AttemptReceipt,
    ) -> Result<PreparedAttemptFold, AttemptFoldError> {
        if state.session_id != self.session_id || receipt.session_id != self.session_id {
            return Err(AttemptFoldError::Invalid("session mismatch"));
        }
        let last_usage_revision = state.last_usage_revision;
        if last_usage_revision.is_some_and(|revision| revision > state.cost_revision) {
            return Err(AttemptFoldError::Invalid("future completion marker"));
        }
        let entry = self
            .attempts
            .get(&receipt.attempt_id)
            .ok_or(AttemptFoldError::MissingIntent)?;
        if entry.intent.billing_mode != AttemptBillingMode::MeteredAttempts {
            return Err(AttemptFoldError::Invalid(
                "legacy run cannot charge attempts",
            ));
        }
        if let Some(old) = entry.receipts.get(&receipt.revision) {
            return if old.receipt == receipt {
                Ok(PreparedAttemptFold {
                    ack: old.ack.clone(),
                    receipt,
                    expected_entry: entry.clone(),
                    expected_rollup: None,
                    base_state: state.clone(),
                    next_state: state.clone(),
                    next_rollup: None,
                    completion_revision: entry.completion_revision,
                    duplicate: true,
                })
            } else {
                Err(AttemptFoldError::Invalid("conflicting receipt revision"))
            };
        }
        let previous = entry.receipts.last_key_value().map(|(_, value)| value);
        match previous {
            None if receipt.revision == 1 && receipt.replaces_revision.is_none() => {}
            Some(old)
                if old.receipt.disposition == AttemptDisposition::Unknown
                    && receipt.disposition == AttemptDisposition::Exact
                    && old.receipt.revision.checked_add(1) == Some(receipt.revision)
                    && receipt.replaces_revision == Some(old.receipt.revision) => {}
            _ => {
                return Err(AttemptFoldError::Invalid(
                    "receipt gap or absorbing disposition",
                ))
            }
        }
        let contribution = receipt_contribution(&entry.intent, &receipt)?;
        let old = previous
            .map(|old| old.ack.contribution.clone())
            .unwrap_or_default();
        let mut rollup = self
            .model_contributions
            .get(&entry.intent.model)
            .cloned()
            .unwrap_or_default();
        replace_contribution(&mut rollup, &old, &contribution)?;
        let mut next = state.clone();
        replace_vector(&mut next, &entry.intent.model, &old, &contribution)?;
        next.cost_revision = state
            .cost_revision
            .checked_add(1)
            .ok_or(AttemptFoldError::Arithmetic)?;
        let completion_revision = entry.completion_revision.unwrap_or(next.cost_revision);
        let mut next_last = last_usage_revision;
        if contribution.request_count != 0
            && last_usage_revision.is_none_or(|latest| completion_revision >= latest)
        {
            next.last_usage = Some(contribution.usage);
            next.last_cache_read_input_tokens = contribution.cache_read_input_tokens;
            next.last_cache_creation_input_tokens = contribution.cache_creation_input_tokens;
            next_last = Some(completion_revision);
        }
        let ack = AttemptFoldAck {
            cost_revision: next.cost_revision,
            last_usage_revision: next_last,
            contribution,
        };
        next.last_usage_revision = next_last;
        Ok(PreparedAttemptFold {
            receipt,
            expected_entry: entry.clone(),
            expected_rollup: self.model_contributions.get(&entry.intent.model).cloned(),
            base_state: state.clone(),
            next_state: next,
            next_rollup: Some(rollup),
            completion_revision: Some(completion_revision),
            ack,
            duplicate: false,
        })
    }

    /// Publish one prepared, durable transition. A stale preparation is rejected
    /// without changing state; no caller may overwrite a newer cost vector.
    pub fn commit_receipt(
        &mut self,
        state: &mut CostStateVector,
        prepared: PreparedAttemptFold,
    ) -> Result<AttemptFoldAck, AttemptFoldError> {
        if state.session_id != self.session_id || prepared.receipt.session_id != self.session_id {
            return Err(AttemptFoldError::Invalid("session mismatch"));
        }
        let current = self
            .attempts
            .get(&prepared.receipt.attempt_id)
            .ok_or(AttemptFoldError::MissingIntent)?;
        if prepared.duplicate {
            let original = current
                .receipts
                .get(&prepared.receipt.revision)
                .ok_or(AttemptFoldError::MissingIntent)?;
            if original.receipt != prepared.receipt {
                return Err(AttemptFoldError::Invalid("conflicting prepared duplicate"));
            }
            return Ok(original.ack.clone());
        }
        if *state != prepared.base_state
            || current != &prepared.expected_entry
            || self.model_contributions.get(&current.intent.model)
                != prepared.expected_rollup.as_ref()
        {
            return Err(AttemptFoldError::Invalid("stale prepared receipt"));
        }
        let entry = self
            .attempts
            .get_mut(&prepared.receipt.attempt_id)
            .expect("validated intent exists");
        entry.completion_revision = prepared.completion_revision;
        entry.receipts.insert(
            prepared.receipt.revision,
            ReceiptEntry {
                receipt: prepared.receipt,
                ack: prepared.ack.clone(),
            },
        );
        self.model_contributions.insert(
            entry.intent.model.clone(),
            prepared.next_rollup.expect("nonduplicate rollup"),
        );
        *state = prepared.next_state;
        Ok(prepared.ack)
    }

    /// Current per-model attempt contributions, retaining request count and
    /// incomplete occupancy even though ordinary ModelUsage has no such fields.
    #[must_use]
    pub fn model_rollups(&self) -> HashMap<ModelRef, AttemptContribution> {
        self.model_contributions.clone()
    }
}

fn validate_intent(intent: &AttemptIntent, session_id: SessionId) -> Result<(), AttemptFoldError> {
    if !matches!(
        (intent.schema_version, intent.token_pricing_requires_quote),
        (1, false) | (2, true)
    ) {
        return Err(AttemptFoldError::Invalid(
            "unsupported intent schema or token pricing contract",
        ));
    }
    if intent.session_id != session_id
        || intent.wire_ordinal == 0
        || [
            &intent.attempt_id,
            &intent.run_id,
            &intent.logical_call_id,
            &intent.profile_id,
            &intent.model.model,
        ]
        .iter()
        .any(|id| id.trim().is_empty())
        || intent.model != intent.pricing.model_ref
        || (intent.stage == AttemptStage::Panel) != intent.panel_slot.is_some()
    {
        return Err(AttemptFoldError::Invalid(
            "invalid intent identity or route",
        ));
    }
    // Explicit zero rates represent free/subscription classes. Missing rates
    // are not permission to dispatch an unpriced registered attempt.
    for class in [
        TokenClass::Input,
        TokenClass::Output,
        TokenClass::CacheWrite,
        TokenClass::CacheRead,
        TokenClass::ReasoningOutput,
        TokenClass::CacheWrite1h,
    ] {
        if intent.usage_contract == AttemptUsageContract::StandardDisjointTokensV1
            && matches!(class, TokenClass::CacheWrite | TokenClass::CacheWrite1h)
        {
            continue;
        }
        if !intent.pricing.token_rates.contains_key(&class) {
            return Err(AttemptFoldError::MissingPrice);
        }
    }
    Ok(())
}

/// Checked calculation using only the already pinned price; no speed/catalog
/// resolution occurs here. Missing nonzero classes and overflow fail closed.
pub fn calculate_pinned_attempt_cost(
    usage: &Usage,
    pricing: &ModelPricing,
) -> Result<u64, AttemptFoldError> {
    calculate_pinned_token_cost(usage, pricing)?
        .checked_add(calculate_pinned_non_token_cost(usage, pricing)?)
        .ok_or(AttemptFoldError::Arithmetic)
}

fn calculate_pinned_token_cost(
    usage: &Usage,
    pricing: &ModelPricing,
) -> Result<u64, AttemptFoldError> {
    let mut total = 0_u64;
    for class in [
        TokenClass::Input,
        TokenClass::Output,
        TokenClass::CacheWrite,
        TokenClass::CacheRead,
        TokenClass::ReasoningOutput,
        TokenClass::CacheWrite1h,
    ] {
        let count = usage.tokens_for(class);
        if count != 0 {
            let rate = pricing
                .token_rates
                .get(&class)
                .ok_or(AttemptFoldError::MissingPrice)?;
            total = total
                .checked_add(
                    count
                        .checked_mul(rate.nano_usd_per_token)
                        .ok_or(AttemptFoldError::Arithmetic)?,
                )
                .ok_or(AttemptFoldError::Arithmetic)?;
        }
    }
    Ok(total)
}

fn calculate_pinned_non_token_cost(
    usage: &Usage,
    pricing: &ModelPricing,
) -> Result<u64, AttemptFoldError> {
    let search = usage
        .server_tool_use
        .map_or(0, |tools| tools.web_search_requests);
    if search != 0 {
        let rate = pricing
            .non_token_rates_nano_usd
            .get(&NonTokenBillableUnit::WebSearchRequest)
            .ok_or(AttemptFoldError::MissingPrice)?;
        return u64::from(search)
            .checked_mul(*rate)
            .ok_or(AttemptFoldError::Arithmetic);
    }
    Ok(0)
}

fn receipt_contribution(
    intent: &AttemptIntent,
    receipt: &AttemptReceipt,
) -> Result<AttemptContribution, AttemptFoldError> {
    if intent.usage_contract == AttemptUsageContract::StandardDisjointTokensV1
        && (receipt.usage.tokens.cache_write != 0
            || receipt.usage.tokens.cache_write_1h != 0
            || receipt.cache_creation_input_tokens != 0)
    {
        return Err(AttemptFoldError::Invalid(
            "receipt carries cache creation outside pinned usage contract",
        ));
    }
    if receipt.api_duration_without_retries_ms > receipt.api_duration_ms {
        return Err(AttemptFoldError::Invalid(
            "non-retry duration exceeds duration",
        ));
    }
    if receipt.token_quote_nano_usd.is_some()
        && (!intent.token_pricing_requires_quote
            || receipt.disposition != AttemptDisposition::Exact)
    {
        return Err(AttemptFoldError::Invalid(
            "token quote outside pinned settlement contract",
        ));
    }
    if intent.token_pricing_requires_quote
        && receipt.disposition == AttemptDisposition::Exact
        && receipt.token_quote_nano_usd.is_none()
    {
        return Err(AttemptFoldError::Invalid(
            "exact receipt requires an actual token quote",
        ));
    }
    if matches!(
        receipt.disposition,
        AttemptDisposition::ProvenNotSent | AttemptDisposition::NoProviderResponse
    ) {
        if receipt.usage != Usage::default()
            || receipt.cache_read_input_tokens != 0
            || receipt.cache_creation_input_tokens != 0
        {
            return Err(AttemptFoldError::Invalid("not-sent receipt carries usage"));
        }
        if receipt.disposition == AttemptDisposition::ProvenNotSent {
            if receipt.api_duration_ms != 0 {
                return Err(AttemptFoldError::Invalid("not-sent receipt carries usage"));
            }
            return Ok(AttemptContribution::default());
        }
        // A physical send was attempted and nothing came back. No output
        // tokens can exist, so occupancy stays zero, but the attempt is real
        // and its whole authorization is unaccounted for.
        return Ok(AttemptContribution {
            unverified_nano_usd: intent.authorized_nano_usd,
            request_count: 1,
            unknown_count: 1,
            api_duration_ms: receipt.api_duration_ms,
            api_duration_without_retries_ms: receipt.api_duration_without_retries_ms,
            ..AttemptContribution::default()
        });
    }
    let exact_cost = if intent.token_pricing_requires_quote {
        // The pinned token rates are only a conservative authorization bound.
        // Partial observations retain their real counters without presenting
        // that bound as money actually spent.
        let token_cost = if let Some(quote) = receipt.token_quote_nano_usd {
            let bound = calculate_pinned_token_cost(&receipt.usage, &intent.pricing)?;
            if quote > bound {
                return Err(AttemptFoldError::Invalid(
                    "actual token quote exceeds pinned token bound",
                ));
            }
            quote
        } else {
            0
        };
        token_cost
            .checked_add(calculate_pinned_non_token_cost(
                &receipt.usage,
                &intent.pricing,
            )?)
            .ok_or(AttemptFoldError::Arithmetic)?
    } else {
        calculate_pinned_attempt_cost(&receipt.usage, &intent.pricing)?
    };
    let output_tokens = receipt
        .usage
        .tokens
        .output
        .checked_add(receipt.usage.tokens.reasoning_output)
        .ok_or(AttemptFoldError::Arithmetic)?;
    let unknown = receipt.disposition == AttemptDisposition::Unknown;
    Ok(AttemptContribution {
        usage: receipt.usage,
        cache_read_input_tokens: receipt.cache_read_input_tokens,
        cache_creation_input_tokens: receipt.cache_creation_input_tokens,
        // Only what the provider actually reported reaches the realized total.
        // An authorization is a ceiling on what the run may spend, never
        // evidence that it was spent, so an incomplete usage report discloses
        // the unverified remainder on its own channel instead of inflating the
        // number `/cost` shows.
        nano_usd: exact_cost,
        unverified_nano_usd: if unknown {
            intent.authorized_nano_usd.saturating_sub(exact_cost)
        } else {
            0
        },
        request_count: 1,
        output_occupancy: if unknown {
            output_tokens.max(intent.authorized_output_tokens)
        } else {
            output_tokens
        },
        unknown_count: u64::from(unknown),
        api_duration_ms: receipt.api_duration_ms,
        api_duration_without_retries_ms: receipt.api_duration_without_retries_ms,
    })
}

fn replace(value: u64, old: u64, new: u64) -> Result<u64, AttemptFoldError> {
    value
        .checked_sub(old)
        .and_then(|base| base.checked_add(new))
        .ok_or(AttemptFoldError::Arithmetic)
}

fn replace_usage(value: &mut Usage, old: &Usage, new: &Usage) -> Result<(), AttemptFoldError> {
    macro_rules! token {
        ($field:ident) => {
            value.tokens.$field =
                replace(value.tokens.$field, old.tokens.$field, new.tokens.$field)?;
        };
    }
    token!(input);
    token!(output);
    token!(cache_write);
    token!(cache_read);
    token!(reasoning_output);
    token!(cache_write_1h);
    let search = replace(
        u64::from(value.server_tool_use.map_or(0, |x| x.web_search_requests)),
        u64::from(old.server_tool_use.map_or(0, |x| x.web_search_requests)),
        u64::from(new.server_tool_use.map_or(0, |x| x.web_search_requests)),
    )?;
    let search = u32::try_from(search).map_err(|_| AttemptFoldError::Arithmetic)?;
    if value.server_tool_use.is_some() || new.server_tool_use.is_some() {
        value.server_tool_use = Some(crate::ServerToolUsage {
            web_search_requests: search,
        });
    }
    Ok(())
}

fn replace_contribution(
    value: &mut AttemptContribution,
    old: &AttemptContribution,
    new: &AttemptContribution,
) -> Result<(), AttemptFoldError> {
    replace_usage(&mut value.usage, &old.usage, &new.usage)?;
    macro_rules! counter {
        ($field:ident) => {
            value.$field = replace(value.$field, old.$field, new.$field)?;
        };
    }
    counter!(nano_usd);
    counter!(unverified_nano_usd);
    counter!(request_count);
    counter!(output_occupancy);
    counter!(unknown_count);
    counter!(cache_read_input_tokens);
    counter!(cache_creation_input_tokens);
    counter!(api_duration_ms);
    counter!(api_duration_without_retries_ms);
    Ok(())
}

fn replace_vector(
    state: &mut CostStateVector,
    model: &ModelRef,
    old: &AttemptContribution,
    new: &AttemptContribution,
) -> Result<(), AttemptFoldError> {
    state.total_nano_usd = replace(state.total_nano_usd, old.nano_usd, new.nano_usd)?;
    state.unverified_nano_usd = replace(
        state.unverified_nano_usd,
        old.unverified_nano_usd,
        new.unverified_nano_usd,
    )?;
    state.total_api_duration_ms = replace(
        state.total_api_duration_ms,
        old.api_duration_ms,
        new.api_duration_ms,
    )?;
    state.total_api_duration_without_retries_ms = replace(
        state.total_api_duration_without_retries_ms,
        old.api_duration_without_retries_ms,
        new.api_duration_without_retries_ms,
    )?;
    state.total_web_search_requests = u32::try_from(replace(
        u64::from(state.total_web_search_requests),
        u64::from(
            old.usage
                .server_tool_use
                .map_or(0, |x| x.web_search_requests),
        ),
        u64::from(
            new.usage
                .server_tool_use
                .map_or(0, |x| x.web_search_requests),
        ),
    )?)
    .map_err(|_| AttemptFoldError::Arithmetic)?;
    let positions = state
        .per_model_usage
        .iter()
        .enumerate()
        .filter_map(|(index, usage)| (&usage.model_ref == model).then_some(index))
        .collect::<Vec<_>>();
    if positions.len() > 1 {
        return Err(AttemptFoldError::Invalid("duplicate model vector rows"));
    }
    if old.request_count == 0 && new.request_count == 0 {
        return Ok(());
    }
    let index = if let Some(index) = positions.first() {
        *index
    } else {
        if old.request_count != 0 {
            return Err(AttemptFoldError::Invalid(
                "missing previous model contribution",
            ));
        }
        state.per_model_usage.push(ModelUsage {
            model_ref: model.clone(),
            usage: Usage::default(),
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            cost_nano_usd: 0,
        });
        state.per_model_usage.len() - 1
    };
    let row = &mut state.per_model_usage[index];
    replace_usage(&mut row.usage, &old.usage, &new.usage)?;
    row.cost_nano_usd = replace(row.cost_nano_usd, old.nano_usd, new.nano_usd)?;
    row.cache_read_input_tokens = replace(
        row.cache_read_input_tokens,
        old.cache_read_input_tokens,
        new.cache_read_input_tokens,
    )?;
    row.cache_creation_input_tokens = replace(
        row.cache_creation_input_tokens,
        old.cache_creation_input_tokens,
        new.cache_creation_input_tokens,
    )?;
    // Every attempt is explicitly priced. Preserve legacy unpriced membership:
    // an unrelated ordinary unpriced response cannot be erased by this fold.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CostState, MoneyPerToken, PricingSource, ProviderId, ServerToolUsage, TokenUsage};

    fn intent(session_id: SessionId, id: &str) -> AttemptIntent {
        let model = ModelRef {
            provider: ProviderId::Anthropic,
            model: "pinned-model".into(),
        };
        let pricing = ModelPricing {
            model_ref: model.clone(),
            token_rates: [
                TokenClass::Input,
                TokenClass::Output,
                TokenClass::CacheWrite,
                TokenClass::CacheRead,
                TokenClass::ReasoningOutput,
                TokenClass::CacheWrite1h,
            ]
            .into_iter()
            .map(|class| {
                (
                    class,
                    MoneyPerToken {
                        nano_usd_per_token: 2,
                    },
                )
            })
            .collect(),
            non_token_rates_nano_usd: [(NonTokenBillableUnit::WebSearchRequest, 3)]
                .into_iter()
                .collect(),
            effective_from: None,
            source: PricingSource::BuiltInReference {
                provider: ProviderId::Anthropic,
            },
        };
        AttemptIntent {
            schema_version: 1,
            session_id,
            output_scope: None,
            attempt_id: id.into(),
            run_id: "run".into(),
            logical_call_id: id.into(),
            wire_ordinal: 1,
            stage: AttemptStage::Panel,
            panel_slot: Some(0),
            profile_id: "profile".into(),
            model,
            route_revision: 42,
            pricing,
            token_pricing_requires_quote: false,
            authorized_nano_usd: 1000,
            authorized_input_tokens: 100,
            authorized_output_tokens: 200,
            billing_mode: AttemptBillingMode::MeteredAttempts,
            usage_contract: AttemptUsageContract::AnthropicCacheTtlV1,
        }
    }

    fn fixture() -> (AttemptLedger, CostStateVector, AttemptIntent) {
        let sid = SessionId::new();
        let state = CostStateVector::from(&CostState {
            session_id: sid,
            ..CostState::default()
        });
        (AttemptLedger::new(sid), state, intent(sid, "attempt-a"))
    }

    fn receipt(
        intent: &AttemptIntent,
        disposition: AttemptDisposition,
        count: u64,
    ) -> AttemptReceipt {
        AttemptReceipt {
            session_id: intent.session_id,
            attempt_id: intent.attempt_id.clone(),
            revision: 1,
            replaces_revision: None,
            disposition,
            token_quote_nano_usd: None,
            usage: Usage {
                tokens: TokenUsage {
                    input: count,
                    output: count,
                    cache_write: count,
                    cache_read: count,
                    reasoning_output: count,
                    cache_write_1h: count,
                },
                server_tool_use: Some(ServerToolUsage {
                    web_search_requests: u32::try_from(count).unwrap(),
                }),
                speed: None,
            },
            cache_read_input_tokens: count * 2,
            cache_creation_input_tokens: count * 3,
            api_duration_ms: count * 10,
            api_duration_without_retries_ms: count * 5,
        }
    }

    fn correction(intent: &AttemptIntent, count: u64) -> AttemptReceipt {
        AttemptReceipt {
            revision: 2,
            replaces_revision: Some(1),
            ..receipt(intent, AttemptDisposition::Exact, count)
        }
    }

    #[test]
    fn actual_token_quote_corrects_unknown_and_replays_from_serialized_records() {
        let (mut ledger, mut state, mut intent) = fixture();
        let initial_state = state.clone();
        intent.schema_version = 2;
        intent.token_pricing_requires_quote = true;
        ledger.record_intent(intent.clone()).unwrap();
        let unknown = receipt(&intent, AttemptDisposition::Unknown, 2);
        let first = ledger
            .fold_receipt(&mut state, unknown.clone(), None)
            .unwrap();
        assert_eq!(first.contribution.nano_usd, 6, "only known host tools");
        assert_eq!(first.contribution.unverified_nano_usd, 994);
        assert_eq!(first.contribution.usage, unknown.usage);
        assert_eq!(first.contribution.output_occupancy, 200);

        let mut exact = correction(&intent, 4);
        exact.token_quote_nano_usd = Some(24);
        let last = ledger
            .fold_receipt(&mut state, exact.clone(), first.last_usage_revision)
            .unwrap();
        assert_eq!(last.contribution.nano_usd, 36, "actual tokens plus tools");
        assert_eq!(last.contribution.unverified_nano_usd, 0);
        assert_eq!(last.contribution.usage, exact.usage);
        assert_eq!(last.contribution.output_occupancy, 8);
        assert_eq!(last.contribution.unknown_count, 0);
        assert_eq!(state.total_nano_usd, 36);
        assert_eq!(state.unverified_nano_usd, 0);
        assert_eq!(state.last_usage, Some(exact.usage));
        assert_eq!(ledger.model_rollups()[&intent.model].request_count, 1);

        let mut replay = AttemptLedger::new(intent.session_id);
        let mut replay_state = initial_state;
        let encoded = serde_json::to_vec(&intent).unwrap();
        replay
            .record_intent(serde_json::from_slice(&encoded).unwrap())
            .unwrap();
        for observed in [&unknown, &exact] {
            let encoded = serde_json::to_vec(observed).unwrap();
            let decoded: AttemptReceipt = serde_json::from_slice(&encoded).unwrap();
            assert_eq!(&decoded, observed);
            let marker = replay_state.last_usage_revision;
            replay
                .fold_receipt(&mut replay_state, decoded, marker)
                .unwrap();
        }
        assert_eq!(replay_state, state);
        assert_eq!(replay, ledger);
        assert_eq!(
            replay
                .fold_receipt(&mut replay_state, unknown, last.last_usage_revision)
                .unwrap(),
            first
        );
        assert_eq!(replay_state, state);
    }

    #[test]
    fn old_journal_without_token_quote_contract_keeps_fixed_rate_settlement() {
        let (mut ledger, mut state, intent) = fixture();
        let mut encoded = serde_json::to_value(&intent).unwrap();
        encoded
            .as_object_mut()
            .unwrap()
            .remove("token_pricing_requires_quote");
        let decoded: AttemptIntent = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, intent);
        ledger.record_intent(decoded).unwrap();
        let observed = receipt(&intent, AttemptDisposition::Exact, 1);
        let encoded = serde_json::to_value(&observed).unwrap();
        assert!(encoded.get("token_quote_nano_usd").is_none());
        let decoded: AttemptReceipt = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, observed);
        let ack = ledger.fold_receipt(&mut state, decoded, None).unwrap();
        assert_eq!(ack.contribution.nano_usd, 15);
        let before = ledger.clone();
        let mut changed = intent;
        changed.schema_version = 2;
        changed.token_pricing_requires_quote = true;
        assert!(ledger.record_intent(changed).is_err());
        assert_eq!(ledger, before);
    }

    #[test]
    fn intent_schema_versions_pin_the_token_settlement_contract() {
        for schema_version in [0, 1, 2, 3] {
            for requires_quote in [false, true] {
                let (mut ledger, _, mut intent) = fixture();
                intent.schema_version = schema_version;
                intent.token_pricing_requires_quote = requires_quote;
                let before = ledger.clone();
                let result = ledger.record_intent(intent);
                if matches!((schema_version, requires_quote), (1, false) | (2, true)) {
                    assert_eq!(result, Ok(true));
                } else {
                    assert_eq!(
                        result,
                        Err(AttemptFoldError::Invalid(
                            "unsupported intent schema or token pricing contract"
                        ))
                    );
                    assert_eq!(ledger, before);
                }
            }
        }
    }

    #[test]
    fn invalid_token_quotes_leave_ledger_and_vector_unchanged() {
        for (requires_quote, disposition, quote) in [
            (true, AttemptDisposition::Exact, None),
            (true, AttemptDisposition::Unknown, Some(0)),
            (true, AttemptDisposition::ProvenNotSent, Some(0)),
            (true, AttemptDisposition::NoProviderResponse, Some(0)),
            (false, AttemptDisposition::Exact, Some(0)),
            // The host tool charge is three; it must not increase the token
            // bound of twelve or make this excessive quote acceptable.
            (true, AttemptDisposition::Exact, Some(13)),
        ] {
            let (mut ledger, mut state, mut intent) = fixture();
            intent.schema_version = if requires_quote { 2 } else { 1 };
            intent.token_pricing_requires_quote = requires_quote;
            ledger.record_intent(intent.clone()).unwrap();
            let mut observed = receipt(&intent, disposition, 1);
            if matches!(
                disposition,
                AttemptDisposition::ProvenNotSent | AttemptDisposition::NoProviderResponse
            ) {
                observed = ledger
                    .recovery_receipt(&intent.attempt_id)
                    .unwrap()
                    .unwrap();
                observed.disposition = disposition;
            }
            observed.token_quote_nano_usd = quote;
            let before = (ledger.clone(), state.clone());
            assert!(matches!(
                ledger.fold_receipt(&mut state, observed, None),
                Err(AttemptFoldError::Invalid(_))
            ));
            assert_eq!((ledger, state), before);
        }
        for quote in [0, 12] {
            let (mut ledger, mut state, mut intent) = fixture();
            intent.schema_version = 2;
            intent.token_pricing_requires_quote = true;
            ledger.record_intent(intent.clone()).unwrap();
            let mut observed = receipt(&intent, AttemptDisposition::Exact, 1);
            observed.token_quote_nano_usd = Some(quote);
            let ack = ledger.fold_receipt(&mut state, observed, None).unwrap();
            assert_eq!(ack.contribution.nano_usd, quote + 3);
        }
    }

    #[test]
    fn quoted_token_bounds_and_host_tool_arithmetic_fail_atomically() {
        for overflow in 0..3 {
            let (mut ledger, mut state, mut intent) = fixture();
            intent.schema_version = 2;
            intent.token_pricing_requires_quote = true;
            let mut observed = receipt(&intent, AttemptDisposition::Exact, 0);
            observed.token_quote_nano_usd = Some(0);
            match overflow {
                0 => observed.usage.tokens.input = u64::MAX,
                1 => {
                    intent
                        .pricing
                        .non_token_rates_nano_usd
                        .insert(NonTokenBillableUnit::WebSearchRequest, u64::MAX);
                    observed.usage.server_tool_use = Some(ServerToolUsage {
                        web_search_requests: 2,
                    });
                }
                _ => {
                    intent.pricing.token_rates.insert(
                        TokenClass::Input,
                        MoneyPerToken {
                            nano_usd_per_token: u64::MAX - 1,
                        },
                    );
                    observed.usage.tokens.input = 1;
                    observed.token_quote_nano_usd = Some(u64::MAX - 1);
                    observed.usage.server_tool_use = Some(ServerToolUsage {
                        web_search_requests: 1,
                    });
                }
            }
            ledger.record_intent(intent).unwrap();
            let before = (ledger.clone(), state.clone());
            assert_eq!(
                ledger.fold_receipt(&mut state, observed, None),
                Err(AttemptFoldError::Arithmetic)
            );
            assert_eq!((ledger, state), before);
        }
    }

    #[test]
    fn unknown_quote_contract_preserves_tokens_without_pricing_them_at_the_bound() {
        let (mut ledger, mut state, mut intent) = fixture();
        intent.schema_version = 2;
        intent.token_pricing_requires_quote = true;
        ledger.record_intent(intent.clone()).unwrap();
        let mut observed = receipt(&intent, AttemptDisposition::Unknown, 0);
        observed.usage.tokens.input = u64::MAX;
        observed.usage.server_tool_use = Some(ServerToolUsage {
            web_search_requests: 1,
        });
        let ack = ledger.fold_receipt(&mut state, observed, None).unwrap();
        assert_eq!(ack.contribution.usage.tokens.input, u64::MAX);
        assert_eq!(ack.contribution.nano_usd, 3);
        assert_eq!(ack.contribution.unverified_nano_usd, 997);
    }

    #[test]
    fn usage_contract_standard_prices_and_unreachable_receipts_are_checked() {
        let (mut ledger, mut state, mut intent) = fixture();
        intent.usage_contract = AttemptUsageContract::StandardDisjointTokensV1;
        intent.pricing.token_rates.remove(&TokenClass::CacheWrite);
        intent.pricing.token_rates.remove(&TokenClass::CacheWrite1h);
        let mut missing = intent.clone();
        missing
            .pricing
            .token_rates
            .remove(&TokenClass::ReasoningOutput);
        assert_eq!(
            ledger.record_intent(missing),
            Err(AttemptFoldError::MissingPrice)
        );
        ledger.record_intent(intent.clone()).unwrap();
        let before = (ledger.clone(), state.clone());
        for disposition in [
            AttemptDisposition::Unknown,
            AttemptDisposition::Exact,
            AttemptDisposition::ProvenNotSent,
        ] {
            for bucket in 0..3 {
                let mut bad = receipt(&intent, disposition, 0);
                match bucket {
                    0 => bad.usage.tokens.cache_write = 1,
                    1 => bad.usage.tokens.cache_write_1h = 1,
                    _ => bad.cache_creation_input_tokens = 1,
                }
                assert!(ledger.fold_receipt(&mut state, bad, None).is_err());
                assert_eq!((ledger.clone(), state.clone()), before);
            }
        }
        let mut exact = receipt(&intent, AttemptDisposition::Exact, 1);
        exact.usage.tokens.cache_write = 0;
        exact.usage.tokens.cache_write_1h = 0;
        exact.cache_creation_input_tokens = 0;
        ledger.fold_receipt(&mut state, exact, None).unwrap();
    }

    #[test]
    fn usage_contract_is_immutable_and_old_json_replays_conservatively() {
        let (mut ledger, mut state, intent) = fixture();
        let mut json = serde_json::to_value(&intent).unwrap();
        json.as_object_mut().unwrap().remove("usage_contract");
        let old: AttemptIntent = serde_json::from_value(json).unwrap();
        assert_eq!(old, intent);
        ledger.record_intent(old.clone()).unwrap();
        assert!(!ledger.record_intent(intent.clone()).unwrap());
        let before = ledger.clone();
        let mut changed = intent.clone();
        changed.usage_contract = AttemptUsageContract::StandardDisjointTokensV1;
        assert!(ledger.record_intent(changed).is_err());
        assert_eq!(ledger, before);
        let observed = receipt(&old, AttemptDisposition::Exact, 1);
        let ack = ledger
            .fold_receipt(&mut state, observed.clone(), None)
            .unwrap();
        assert_eq!(
            ledger
                .fold_receipt(&mut state, observed, ack.last_usage_revision)
                .unwrap(),
            ack
        );
        let mut missing = old;
        missing
            .pricing
            .token_rates
            .remove(&TokenClass::CacheWrite1h);
        assert_eq!(
            validate_intent(&missing, missing.session_id),
            Err(AttemptFoldError::MissingPrice)
        );
    }

    #[test]
    fn intent_and_receipt_identity_conflicts_are_atomic() {
        let (mut ledger, mut state, intent) = fixture();
        let observed = receipt(&intent, AttemptDisposition::Exact, 1);
        assert_eq!(
            ledger.fold_receipt(&mut state, observed.clone(), None),
            Err(AttemptFoldError::MissingIntent)
        );
        assert!(ledger.record_intent(intent.clone()).unwrap());
        assert!(!ledger.record_intent(intent.clone()).unwrap());
        assert_eq!(state.cost_revision, 0);
        let original = ledger.clone();
        let mut conflict = intent.clone();
        conflict.route_revision += 1;
        assert!(ledger.record_intent(conflict).is_err());
        let mut mode = intent.clone();
        mode.attempt_id = "new".into();
        mode.logical_call_id = "new".into();
        mode.billing_mode = AttemptBillingMode::LegacyAggregate;
        assert!(ledger.record_intent(mode).is_err());
        assert_eq!(ledger, original);
        let ack = ledger
            .fold_receipt(&mut state, observed.clone(), None)
            .unwrap();
        let original = (ledger.clone(), state.clone());
        let mut conflict = observed.clone();
        conflict.api_duration_ms += 1;
        assert!(ledger
            .fold_receipt(&mut state, conflict, ack.last_usage_revision)
            .is_err());
        assert_eq!((ledger.clone(), state.clone()), original);
        state.cost_revision += 10; // unrelated later owner mutation
        let newer = state.clone();
        assert_eq!(
            ledger
                .fold_receipt(&mut state, observed, ack.last_usage_revision)
                .unwrap(),
            ack
        );
        assert_eq!(state, newer);
    }

    #[test]
    fn prepared_receipt_is_inert_until_commit_and_rejects_stale_publication() {
        let (mut ledger, mut state, intent) = fixture();
        let empty = ledger.clone();
        assert!(ledger.check_intent(&intent).unwrap());
        assert_eq!(ledger, empty);
        ledger.record_intent(intent.clone()).unwrap();
        let observed = receipt(&intent, AttemptDisposition::Exact, 1);
        let before = (ledger.clone(), state.clone());
        let abandoned = ledger.prepare_receipt(&state, observed.clone()).unwrap();
        assert!(!abandoned.is_duplicate());
        assert_eq!(abandoned.state().cost_revision, 1);
        assert_eq!((ledger.clone(), state.clone()), before);
        drop(abandoned);
        assert_eq!((ledger.clone(), state.clone()), before);

        let first = ledger.prepare_receipt(&state, observed.clone()).unwrap();
        let stale = ledger.prepare_receipt(&state, observed.clone()).unwrap();
        let ack = ledger.commit_receipt(&mut state, first).unwrap();
        assert_eq!(state.last_usage_revision, ack.last_usage_revision);
        let committed = (ledger.clone(), state.clone());
        assert!(ledger.commit_receipt(&mut state, stale).is_err());
        assert_eq!((ledger.clone(), state.clone()), committed);

        let duplicate = ledger.prepare_receipt(&state, observed).unwrap();
        assert!(duplicate.is_duplicate());
        state.cost_revision += 1; // an unrelated durable administrative mutation
        let newer = state.clone();
        assert_eq!(ledger.commit_receipt(&mut state, duplicate).unwrap(), ack);
        assert_eq!(state, newer);
    }

    #[test]
    fn exact_replacement_corrects_every_counter_once_and_keeps_one_request() {
        let (mut ledger, mut state, intent) = fixture();
        ledger.record_intent(intent.clone()).unwrap();
        state.total_tool_duration_ms = 33;
        state.total_lines_added = 12;
        state.total_lines_removed = 4;
        state.external_nano_usd = 50;
        state.legacy_opening_balance_nano_usd = 20;
        state.total_nano_usd = 70;
        state.unpriced_models.push(ModelRef {
            provider: ProviderId::OpenAI,
            model: "legacy-unpriced".into(),
        });
        let initial = ledger
            .fold_receipt(
                &mut state,
                receipt(&intent, AttemptDisposition::Unknown, 8),
                None,
            )
            .unwrap();
        // 6 token classes * 8 tokens * 2 nano + 8 searches * 3 nano + 70 seeded.
        assert_eq!(state.total_nano_usd, 190);
        assert_eq!(state.unverified_nano_usd, 1000 - 120);
        let exact = correction(&intent, 2);
        let ack = ledger
            .fold_receipt(&mut state, exact.clone(), initial.last_usage_revision)
            .unwrap();
        assert_eq!(state.total_nano_usd, 100); // 6 token classes * 2 * 2 + 2 searches * 3 + 70
        assert_eq!(
            state.unverified_nano_usd, 0,
            "an exact correction retires the unverified remainder"
        );
        assert_eq!(state.total_web_search_requests, 2);
        assert_eq!(state.total_api_duration_ms, 20);
        assert_eq!(state.total_api_duration_without_retries_ms, 10);
        assert_eq!(state.total_tool_duration_ms, 33);
        assert_eq!(
            (state.total_lines_added, state.total_lines_removed),
            (12, 4)
        );
        assert_eq!(
            (
                state.external_nano_usd,
                state.legacy_opening_balance_nano_usd
            ),
            (50, 20)
        );
        assert_eq!(state.unpriced_models.len(), 1);
        let model = &state.per_model_usage[0];
        assert_eq!(model.usage, exact.usage);
        assert_eq!(model.cost_nano_usd, 30);
        assert_eq!(
            (
                model.cache_read_input_tokens,
                model.cache_creation_input_tokens
            ),
            (4, 6)
        );
        assert_eq!(state.last_usage, Some(exact.usage));
        assert_eq!(
            (
                state.last_cache_read_input_tokens,
                state.last_cache_creation_input_tokens
            ),
            (4, 6)
        );
        assert_eq!(ack.last_usage_revision, initial.last_usage_revision);
        let rollups = ledger.model_rollups();
        assert_eq!(rollups[&intent.model].request_count, 1);
        assert_eq!(rollups[&intent.model].unknown_count, 0);
        assert_eq!(rollups[&intent.model].output_occupancy, 4);
        let snapshot = state.clone();
        assert_eq!(
            ledger
                .fold_receipt(&mut state, exact, ack.last_usage_revision)
                .unwrap(),
            ack
        );
        assert_eq!(snapshot, state);
    }

    #[test]
    fn older_correction_does_not_replace_newer_attempt_or_ordinary_last_usage() {
        for ordinary in [false, true] {
            let (mut ledger, mut state, intent_a) = fixture();
            ledger.record_intent(intent_a.clone()).unwrap();
            ledger
                .fold_receipt(
                    &mut state,
                    receipt(&intent_a, AttemptDisposition::Unknown, 5),
                    None,
                )
                .unwrap();
            let newest = if ordinary {
                state.cost_revision += 1;
                state.last_usage = Some(Usage::default());
                state.last_usage_revision = Some(state.cost_revision);
                state.last_cache_read_input_tokens = 91;
                state.last_cache_creation_input_tokens = 92;
                Some(state.cost_revision)
            } else {
                let intent_b = intent(intent_a.session_id, "attempt-b");
                ledger.record_intent(intent_b.clone()).unwrap();
                ledger
                    .fold_receipt(
                        &mut state,
                        receipt(&intent_b, AttemptDisposition::Exact, 3),
                        Some(1),
                    )
                    .unwrap()
                    .last_usage_revision
            };
            let last = (
                state.last_usage,
                state.last_cache_read_input_tokens,
                state.last_cache_creation_input_tokens,
            );
            let ack = ledger
                .fold_receipt(&mut state, correction(&intent_a, 1), newest)
                .unwrap();
            assert_eq!(ack.last_usage_revision, newest);
            assert_eq!(
                (
                    state.last_usage,
                    state.last_cache_read_input_tokens,
                    state.last_cache_creation_input_tokens
                ),
                last
            );
        }
    }

    #[test]
    fn gaps_downgrades_and_final_dispositions_are_rejected() {
        let (mut ledger, mut state, intent) = fixture();
        ledger.record_intent(intent.clone()).unwrap();
        let mut skipped = receipt(&intent, AttemptDisposition::Exact, 1);
        skipped.revision = 2;
        assert!(ledger.fold_receipt(&mut state, skipped, None).is_err());
        ledger
            .fold_receipt(
                &mut state,
                receipt(&intent, AttemptDisposition::Unknown, 1),
                None,
            )
            .unwrap();
        let mut skipped = correction(&intent, 1);
        skipped.revision = 3;
        assert!(ledger.fold_receipt(&mut state, skipped, Some(1)).is_err());
        let mut wrong = correction(&intent, 1);
        wrong.disposition = AttemptDisposition::ProvenNotSent;
        assert!(ledger.fold_receipt(&mut state, wrong, Some(1)).is_err());
        ledger
            .fold_receipt(&mut state, correction(&intent, 1), Some(1))
            .unwrap();
        let mut changed = correction(&intent, 2);
        changed.revision = 3;
        changed.replaces_revision = Some(2);
        assert!(ledger.fold_receipt(&mut state, changed, Some(1)).is_err());
    }

    #[test]
    fn unknown_receipt_never_charges_the_authorized_ceiling() {
        // A dispatched attempt whose usage came back incomplete is charged for
        // what was observed. The authorization was a ceiling on what the run
        // was allowed to spend, never evidence that it spent it.
        let (mut ledger, mut state, intent) = fixture();
        assert_eq!(intent.authorized_nano_usd, 1000);
        ledger.record_intent(intent.clone()).unwrap();
        let ack = ledger
            .fold_receipt(
                &mut state,
                receipt(&intent, AttemptDisposition::Unknown, 1),
                None,
            )
            .unwrap();
        // 6 token classes x 1 token x 2 nano + 1 web search x 3 nano.
        assert_eq!(
            ack.contribution.nano_usd, 15,
            "only observed usage reaches the realized total"
        );
        assert_eq!(state.total_nano_usd, 15);
        // The ceiling is not lost, only moved off the realized total.
        assert_eq!(ack.contribution.unverified_nano_usd, 1000 - 15);
        assert_eq!(state.unverified_nano_usd, 985);

        // A complete correction retires the unverified remainder entirely.
        let ack = ledger
            .fold_receipt(&mut state, correction(&intent, 1), Some(1))
            .unwrap();
        assert_eq!(ack.contribution.unverified_nano_usd, 0);
        assert_eq!(state.unverified_nano_usd, 0);
        assert_eq!(state.total_nano_usd, 15);
    }

    #[test]
    fn an_attempt_with_no_provider_response_contributes_nothing() {
        // The transport failed after the dispatch marker. Nothing was
        // observed, so nothing is charged and nothing is guessed.
        let (mut ledger, mut state, intent) = fixture();
        ledger.record_intent(intent.clone()).unwrap();
        let mut receipt = receipt(&intent, AttemptDisposition::NoProviderResponse, 0);
        receipt.usage = Usage::default();
        receipt.cache_read_input_tokens = 0;
        receipt.cache_creation_input_tokens = 0;
        receipt.api_duration_ms = 0;
        receipt.api_duration_without_retries_ms = 0;
        let ack = ledger.fold_receipt(&mut state, receipt, None).unwrap();
        assert_eq!(ack.contribution.nano_usd, 0, "nothing was reported");
        assert_eq!(
            ack.contribution.output_occupancy, 0,
            "no response means no output tokens exist to hold"
        );
        // The send was still attempted, so it is one request and one
        // incomplete attempt, with its whole authorization unaccounted for.
        assert_eq!(ack.contribution.request_count, 1);
        assert_eq!(ack.contribution.unknown_count, 1);
        assert_eq!(ack.contribution.unverified_nano_usd, 1000);
        assert_eq!(state.total_nano_usd, 0);
        assert_eq!(state.unverified_nano_usd, 1000);
    }

    #[test]
    fn recovery_unknown_keeps_occupancy_without_inventing_usage_and_not_sent_is_zero() {
        let (mut ledger, mut state, intent_a) = fixture();
        ledger.record_intent(intent_a.clone()).unwrap();
        let recovered = ledger
            .recovery_receipt(&intent_a.attempt_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            Some(recovered.clone()),
            ledger.recovery_receipt(&intent_a.attempt_id).unwrap()
        );
        let ack = ledger.fold_receipt(&mut state, recovered, None).unwrap();
        assert_eq!(ack.contribution.usage, Usage::default());
        // A recovered attempt reports no usage, so it costs nothing realized.
        // The whole authorization stays visible as unverified rather than
        // being charged as if the provider had billed it.
        assert_eq!(ack.contribution.nano_usd, 0);
        assert_eq!(ack.contribution.unverified_nano_usd, 1000);
        // Output occupancy stays conservative: it guards publication rights,
        // not money.
        assert_eq!(ack.contribution.output_occupancy, 200);
        assert_eq!(ack.contribution.request_count, 1);
        assert_eq!(ack.contribution.unknown_count, 1);
        assert!(ledger
            .recovery_receipt(&intent_a.attempt_id)
            .unwrap()
            .is_none());
        let intent_b = intent(intent_a.session_id, "attempt-b");
        ledger.record_intent(intent_b.clone()).unwrap();
        let mut not_sent = ledger
            .recovery_receipt(&intent_b.attempt_id)
            .unwrap()
            .unwrap();
        not_sent.disposition = AttemptDisposition::ProvenNotSent;
        let before = state.clone();
        let ack = ledger.fold_receipt(&mut state, not_sent, Some(1)).unwrap();
        assert_eq!(ack.contribution, AttemptContribution::default());
        assert_eq!(state.total_nano_usd, before.total_nano_usd);
        assert_eq!(state.last_usage, before.last_usage);
        assert_eq!(ledger.model_rollups()[&intent_a.model].request_count, 1);
        assert!(ledger
            .fold_receipt(&mut state, correction(&intent_b, 1), Some(1))
            .is_err());
    }

    #[test]
    fn checked_underflow_overflow_leave_ledger_and_vector_unchanged() {
        for corrupt_model in [false, true] {
            let (mut ledger, mut state, intent) = fixture();
            ledger.record_intent(intent.clone()).unwrap();
            ledger
                .fold_receipt(
                    &mut state,
                    receipt(&intent, AttemptDisposition::Unknown, 4),
                    None,
                )
                .unwrap();
            if corrupt_model {
                state.per_model_usage[0].usage.tokens.cache_write_1h = 0;
            } else {
                state.total_nano_usd = 0;
            }
            let before = (ledger.clone(), state.clone());
            assert!(ledger
                .fold_receipt(&mut state, correction(&intent, 1), Some(1))
                .is_err());
            assert_eq!((ledger, state), before);
        }
        for revision_overflow in [false, true] {
            let (mut ledger, mut state, intent) = fixture();
            ledger.record_intent(intent.clone()).unwrap();
            if revision_overflow {
                state.cost_revision = u64::MAX;
            } else {
                state.total_nano_usd = u64::MAX;
            }
            let before = (ledger.clone(), state.clone());
            assert_eq!(
                ledger.fold_receipt(
                    &mut state,
                    receipt(&intent, AttemptDisposition::Exact, 1),
                    None
                ),
                Err(AttemptFoldError::Arithmetic)
            );
            assert_eq!((ledger, state), before);
        }
    }

    #[test]
    fn pinned_effective_rates_ignore_fast_flag_and_later_price_edits() {
        let (mut ledger, mut state, mut intent) = fixture();
        intent.model.model = "claude-opus-4-6".into();
        intent.pricing.model_ref = intent.model.clone();
        ledger.record_intent(intent.clone()).unwrap();
        let mut observed = receipt(&intent, AttemptDisposition::Exact, 1);
        observed.usage.speed = Some(crate::ApiSpeed::Fast);
        intent
            .pricing
            .token_rates
            .get_mut(&TokenClass::Input)
            .unwrap()
            .nano_usd_per_token = 999;
        assert!(ledger.record_intent(intent.clone()).is_err());
        let ack = ledger.fold_receipt(&mut state, observed, None).unwrap();
        assert_eq!(ack.contribution.nano_usd, 15);
        let mut usage = Usage::default();
        usage.tokens.input = u64::MAX;
        assert_eq!(
            calculate_pinned_attempt_cost(&usage, &intent.pricing),
            Err(AttemptFoldError::Arithmetic)
        );
    }

    #[test]
    fn legacy_mode_has_no_implicit_metered_receipts() {
        let (mut ledger, mut state, mut intent) = fixture();
        intent.billing_mode = AttemptBillingMode::LegacyAggregate;
        ledger.record_intent(intent.clone()).unwrap();
        let before = (ledger.clone(), state.clone());
        assert!(ledger.recovery_receipt(&intent.attempt_id).is_err());
        assert!(ledger
            .fold_receipt(
                &mut state,
                receipt(&intent, AttemptDisposition::Exact, 1),
                None
            )
            .is_err());
        assert_eq!((ledger, state), before);
    }

    #[test]
    fn server_counter_and_output_occupancy_overflow_are_atomic() {
        let (mut ledger, mut state, intent_a) = fixture();
        ledger.record_intent(intent_a.clone()).unwrap();
        state.total_web_search_requests = u32::MAX;
        let before = (ledger.clone(), state.clone());
        assert_eq!(
            ledger.fold_receipt(
                &mut state,
                receipt(&intent_a, AttemptDisposition::Exact, 1),
                None
            ),
            Err(AttemptFoldError::Arithmetic)
        );
        assert_eq!((ledger.clone(), state.clone()), before);
        state.total_web_search_requests = 0;
        let recovery = ledger
            .recovery_receipt(&intent_a.attempt_id)
            .unwrap()
            .unwrap();
        ledger.fold_receipt(&mut state, recovery, None).unwrap();
        let mut intent_b = intent(intent_a.session_id, "attempt-b");
        intent_b.authorized_output_tokens = u64::MAX;
        ledger.record_intent(intent_b.clone()).unwrap();
        let recovery = ledger
            .recovery_receipt(&intent_b.attempt_id)
            .unwrap()
            .unwrap();
        let before = (ledger.clone(), state.clone());
        assert_eq!(
            ledger.fold_receipt(&mut state, recovery, Some(1)),
            Err(AttemptFoldError::Arithmetic)
        );
        assert_eq!((ledger, state), before);
    }

    #[test]
    fn disjoint_visible_and_reasoning_output_share_checked_occupancy() {
        for disposition in [AttemptDisposition::Exact, AttemptDisposition::Unknown] {
            let (mut ledger, mut state, mut intent) = fixture();
            intent.authorized_output_tokens = 80;
            ledger.record_intent(intent.clone()).unwrap();
            let mut observed = receipt(&intent, disposition, 0);
            observed.usage.tokens.output = 40;
            observed.usage.tokens.reasoning_output = 60;
            let ack = ledger.fold_receipt(&mut state, observed, None).unwrap();
            assert_eq!(ack.contribution.output_occupancy, 100);
        }
        let (mut ledger, mut state, mut intent) = fixture();
        // Explicit free pricing isolates output-sum overflow from money overflow.
        for rate in intent.pricing.token_rates.values_mut() {
            rate.nano_usd_per_token = 0;
        }
        ledger.record_intent(intent.clone()).unwrap();
        let mut observed = receipt(&intent, AttemptDisposition::Exact, 0);
        observed.usage.tokens.output = u64::MAX;
        observed.usage.tokens.reasoning_output = 1;
        let before = (ledger.clone(), state.clone());
        assert_eq!(
            ledger.fold_receipt(&mut state, observed, None),
            Err(AttemptFoldError::Arithmetic)
        );
        assert_eq!((ledger, state), before);
    }

    #[test]
    fn intent_and_receipt_round_trip_and_wrong_session_are_checked() {
        let (mut ledger, mut state, intent) = fixture();
        let decoded: AttemptIntent =
            serde_json::from_str(&serde_json::to_string(&intent).unwrap()).unwrap();
        assert_eq!(decoded, intent);
        ledger.record_intent(decoded).unwrap();
        let observed = receipt(&intent, AttemptDisposition::Unknown, 1);
        let decoded: AttemptReceipt =
            serde_json::from_str(&serde_json::to_string(&observed).unwrap()).unwrap();
        assert_eq!(observed, decoded);
        let before = (ledger.clone(), state.clone());
        let mut wrong = decoded;
        wrong.session_id = SessionId::new();
        assert!(ledger.fold_receipt(&mut state, wrong, None).is_err());
        assert_eq!((ledger, state), before);
    }

    #[test]
    fn derived_indexes_match_history_after_insert_correction_failure_and_replay() {
        fn assert_indexes(ledger: &AttemptLedger) {
            let mut models = HashMap::new();
            let mut identities = BTreeSet::new();
            for entry in ledger.attempts.values() {
                identities.insert((
                    entry.intent.run_id.clone(),
                    entry.intent.logical_call_id.clone(),
                    entry.intent.wire_ordinal,
                ));
                if let Some((_, latest)) = entry.receipts.last_key_value() {
                    replace_contribution(
                        models.entry(entry.intent.model.clone()).or_default(),
                        &AttemptContribution::default(),
                        &latest.ack.contribution,
                    )
                    .unwrap();
                }
            }
            assert_eq!(ledger.wire_identities, identities);
            assert_eq!(ledger.model_rollups(), models);
        }
        let (mut ledger, mut state, intent_a) = fixture();
        let initial_state = state.clone();
        let mut intent_b = intent(intent_a.session_id, "attempt-b");
        intent_b.model.model = "second-model".into();
        intent_b.pricing.model_ref = intent_b.model.clone();
        for intent in [&intent_a, &intent_b] {
            ledger.record_intent(intent.clone()).unwrap();
            assert_indexes(&ledger);
        }
        let mut duplicate_wire = intent_a.clone();
        duplicate_wire.attempt_id = "different-attempt-same-wire".into();
        let before = ledger.clone();
        assert!(ledger.record_intent(duplicate_wire).is_err());
        assert_eq!(ledger, before);
        let events = [
            receipt(&intent_a, AttemptDisposition::Unknown, 4),
            receipt(&intent_b, AttemptDisposition::Exact, 2),
            correction(&intent_a, 1),
        ];
        let mut marker = None;
        for event in &events {
            marker = ledger
                .fold_receipt(&mut state, event.clone(), marker)
                .unwrap()
                .last_usage_revision;
            assert_indexes(&ledger);
        }
        let before = (ledger.clone(), state.clone());
        let mut conflict = events[2].clone();
        conflict.usage.tokens.input += 1;
        assert!(ledger.fold_receipt(&mut state, conflict, marker).is_err());
        assert_eq!((ledger.clone(), state.clone()), before);
        ledger
            .fold_receipt(&mut state, events[0].clone(), marker)
            .unwrap();
        assert_eq!((ledger.clone(), state.clone()), before);
        let mut replayed = AttemptLedger::new(intent_a.session_id);
        let mut replayed_state = initial_state;
        replayed.record_intent(intent_a).unwrap();
        replayed.record_intent(intent_b).unwrap();
        let mut marker = None;
        for event in events {
            marker = replayed
                .fold_receipt(&mut replayed_state, event, marker)
                .unwrap()
                .last_usage_revision;
        }
        assert_indexes(&replayed);
        assert_eq!(replayed, ledger);
        assert_eq!(replayed_state, state);
    }
}
