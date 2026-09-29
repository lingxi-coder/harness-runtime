//! Fusion — fifth run mode: multi-model deliberation DTOs and executor trait.
//!
//! Callers (Agent tool, `/fusion`, workflow) depend only on this module. The
//! concrete orchestrator lives in the `fusion` crate so `platform-api` stays a
//! leaf. Side-query clients and settings snapshots are injected by the
//! composition root into that orchestrator, not onto this trait.
//!
//! Fusion panels are ordinary hidden subagents: they inherit the parent
//! session's budget and cancellation handles. Their built-in definition uses
//! an explicit read-only tool allow-list, so deliberation cannot mutate the
//! parent workspace even when the parent session permits writes.

use crate::budget::BudgetEnforcerHandle;
use crate::subagent_spawn::SubagentInheritance;
use async_trait::async_trait;
use futures_core::future::BoxFuture;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::mpsc::Sender;
use tokio::sync::watch;
use tokio::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// Wire / persistence schema version for Fusion DTOs.
pub const FUSION_SCHEMA_VERSION: u16 = 1;

/// Default analyst dimensions. Accuracy is omitted: the analyst has no tools
/// and cannot independently verify world facts.
pub const DEFAULT_FUSION_DIMENSIONS: [&str; 5] = [
    "evidence_quality",
    "coverage",
    "reasoning",
    "safety",
    "actionability",
];

/// Human-readable rubric anchors for [`DEFAULT_FUSION_DIMENSIONS`], same
/// order. The analyst system prompt renders these so the judge model scores
/// against a shared meaning instead of guessing from the bare dimension name
/// (F003). A caller-supplied custom dimension list has no built-in
/// description; the analyst scores those by their plain meaning.
pub const DEFAULT_FUSION_DIMENSION_DESCRIPTIONS: [&str; 5] = [
    "evidence_quality: how well the report's claims are grounded in cited evidence (files, URLs, commands actually consulted) rather than unsupported assertion",
    "coverage: how much of the task's scope the report actually addresses",
    "reasoning: how sound and internally consistent the report's chain of reasoning is",
    "safety: whether the report avoids introducing risk (destructive actions, security issues, unverified claims stated as fact)",
    "actionability: how directly the report's answer can be acted on without further clarification",
];

/// Minimum and maximum panel sizes.
pub const FUSION_MIN_PANEL: u8 = 2;
/// OpenRouter-aligned panel cap.
pub const FUSION_MAX_PANEL: u8 = 8;

/// Slots in the Fusion panel sub-pool. Sized to the largest admissible group
/// so a single run is never refused for want of capacity, and kept separate
/// from the ordinary subagent pool so a queued group cannot delay or refuse a
/// user's own Agent call. Desktop already ran a second pool for teammates for
/// the same reason.
pub const FUSION_PANEL_POOL_CAP: usize = FUSION_MAX_PANEL as usize;

/// Hidden panel subagent type. Resolved like `fork` (catalog cannot shadow it)
/// and never appears in the Agent listing.
pub const FUSION_PANEL_TYPE: &str = "fusion-panel";

/// Hard ceiling for workflow `fusion()` calls in a single workflow run.
/// [`FusionExecutor::workflow_fusion_call_cap`] may return a lower value (a
/// host/settings override), never higher — every caller clamps to this.
/// `tasks::handlers::local_workflow` and this crate's own default
/// [`FusionExecutor::workflow_fusion_call_cap`] read this constant directly.
/// Settings validation (`core/src/settings/schema.rs`) and
/// `fusion::config`'s literal default do NOT yet read it — they still
/// hardcode `20` — so until those are consolidated onto this constant, do
/// not treat this as the single source of truth; changing this value alone
/// will not move the other two.
pub const FUSION_WORKFLOW_CALL_CAP_HARD_LIMIT: u32 = 20;

/// Which surface started this Fusion run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FusionOrigin {
    /// `Agent` tool with `subagent_type: "fusion"`.
    Agent,
    /// User slash `/fusion`.
    Slash,
    /// Workflow `fusion()`.
    Workflow,
}

/// Built-in panel selection preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FusionPreset {
    /// Highest-quality eligible models (default).
    Quality,
    /// Latency-homogeneous lighter models.
    Fast,
}

/// Parse a preset from its wire string (`"quality"` / `"fast"`). The single
/// implementation every caller (Agent tool, `/fusion`, workflow `fusion()`)
/// parses a caller-supplied preset string through, so the accepted spelling
/// and the rejection message stay identical across entrypoints.
impl std::str::FromStr for FusionPreset {
    type Err = FusionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "quality" => Ok(Self::Quality),
            "fast" => Ok(Self::Fast),
            other => Err(FusionError::InvalidRequest(format!(
                "fusion preset `{other}` must be quality or fast"
            ))),
        }
    }
}

/// Which of Fusion's three model roles a configured route fills.
///
/// Fusion runs three different kinds of call and they have genuinely different
/// requirements, which is why they are configured separately rather than as one
/// "fusion model" list: panels answer the prompt independently, the analyst must
/// emit constrained JSON to score them, and the synthesizer writes the final
/// answer the user reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FusionModelRole {
    /// The panel roster, in priority order.
    Panels,
    /// The judge that scores the panel reports.
    Analyst,
    /// The merge model that writes the final answer.
    Synthesizer,
}

impl FusionModelRole {
    /// Every role, in the order the setup wizard asks for them.
    pub const ALL: [Self; 3] = [Self::Panels, Self::Analyst, Self::Synthesizer];

    /// The `settings.json` key that configures this role.
    #[must_use]
    pub const fn setting_key(self) -> &'static str {
        match self {
            Self::Panels => "fusion.panelModels",
            Self::Analyst => "fusion.analystModel",
            Self::Synthesizer => "fusion.synthesizerModel",
        }
    }

    /// Short human label for pickers and error text.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Panels => "panel models",
            Self::Analyst => "analyst model",
            Self::Synthesizer => "synthesizer model",
        }
    }

    /// One line explaining what this role does, shown above its picker.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Panels => {
                "Answer the prompt independently, in parallel. Pick 2-8; different \
                 model families disagree more usefully than two sizes of one family."
            }
            Self::Analyst => {
                "Reads every panel report and scores it. Must support structured \
                 output (JSON schema) on its provider."
            }
            Self::Synthesizer => {
                "Merges the analysis into the answer you read. Usually the model \
                 you already talk to."
            }
        }
    }
}

/// One explicitly configured Fusion route: the provider profile that owns the
/// model plus that profile's wire model id.
///
/// The settings-file mirror of this type is `FusionModelSelectionJson` in
/// `lingxi_core::settings::schema` (the two crates cannot depend on each other;
/// `fusion::config`'s `the_settings_reader_and_the_setup_writer_agree` pins the
/// key spellings together).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FusionModelChoice {
    /// Provider profile name.
    pub profile: String,
    /// Wire model id as that profile spells it.
    pub model: String,
}

impl FusionModelChoice {
    /// Build a choice from borrowed halves.
    #[must_use]
    pub fn new(profile: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            profile: profile.into(),
            model: model.into(),
        }
    }

    /// `profile/model`, the spelling used in pickers, logs and error text.
    #[must_use]
    pub fn route(&self) -> String {
        format!("{}/{}", self.profile, self.model)
    }
}

impl std::fmt::Display for FusionModelChoice {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}/{}", self.profile, self.model)
    }
}

/// One explicit panel / analyst model reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FusionModelRef {
    /// Provider profile name. `None` = resolve `model` in the request's scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Wire / display model id.
    pub model: String,
}

/// Parse one `--models` / `models[]` entry (`"profile:model"` or a bare
/// `"model"`) into a [`FusionModelRef`]. The single implementation the Agent
/// tool and `/fusion` both parse caller-supplied model strings through, so a
/// malformed entry (e.g. `"openai:"` — a colon with an empty model) is
/// rejected identically from either entrypoint instead of one silently
/// treating the whole literal as a bare model id.
///
/// # Errors
///
/// Returns [`FusionError::InvalidRequest`] for an empty entry or one with a
/// `:` but an empty profile or model half.
pub fn parse_fusion_model_ref(item: &str) -> Result<FusionModelRef, FusionError> {
    let item = item.trim();
    if item.is_empty() {
        return Err(FusionError::InvalidRequest(
            "fusion models entries must be non-empty".into(),
        ));
    }
    match item.split_once(':') {
        Some((profile, model)) if !profile.is_empty() && !model.is_empty() => Ok(FusionModelRef {
            profile: Some(profile.to_string()),
            model: model.to_string(),
        }),
        Some(_) => Err(FusionError::InvalidRequest(format!(
            "invalid fusion models entry `{item}`"
        ))),
        None => Ok(FusionModelRef {
            profile: None,
            model: item.to_string(),
        }),
    }
}

/// Parse a full `models` list from raw entry strings. See
/// [`parse_fusion_model_ref`] for the per-entry grammar.
///
/// # Errors
///
/// Returns the first entry's [`FusionError::InvalidRequest`].
pub fn parse_fusion_models(raw: &[String]) -> Result<Vec<FusionModelRef>, FusionError> {
    raw.iter()
        .map(|item| parse_fusion_model_ref(item))
        .collect()
}

/// Per-request Fusion input. Unknown fields are rejected (caller-facing).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FusionRequest {
    /// Schema version. Missing values deserialize as [`FUSION_SCHEMA_VERSION`].
    #[serde(default = "fusion_schema_version")]
    pub schema_version: u16,
    /// Which entrypoint constructed this request.
    pub origin: FusionOrigin,
    /// Task prompt. Must be non-empty after trim.
    pub prompt: String,
    /// Panel selection preset. Ignored when [`Self::models`] is `Some`.
    pub preset: FusionPreset,
    /// Explicit panel models. When set, must contain at least two distinct refs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<FusionModelRef>>,
    /// Scoring dimensions (1..=12, `snake_case`, caller order after dedup).
    pub dimensions: Vec<String>,
    /// Continue into analysis when some panels fail but the min is met.
    pub partial_ok: bool,
    /// Override panel count / explicit-list cap. Clamped to settings max.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_panel: Option<u8>,
    /// Whether this run may leave the parent provider/profile.
    pub cross_provider: bool,
    /// Parent provider profile name.
    pub parent_profile: String,
    /// Parent wire model id.
    pub parent_model: String,
    /// Workflow run id when origin is [`FusionOrigin::Workflow`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_run_id: Option<String>,
}

const fn fusion_schema_version() -> u16 {
    FUSION_SCHEMA_VERSION
}

/// Session handles the parent already holds. Side-query clients and settings
/// stay on the orchestrator (they are not `platform-api` types).
#[derive(Clone)]
pub struct FusionInheritance {
    /// Parent tool invoker + budget Arc (recursion-lock / COGS aggregation).
    pub subagent: SubagentInheritance,
    /// Cancellation for the whole Fusion run.
    pub cancel: CancellationToken,
    /// Effective end-to-end timeout captured for this run before it is
    /// activated. `None` preserves legacy behavior for callers that do not
    /// expose a pre-run runtime snapshot.
    pub effective_timeout_ms: Option<u64>,
    /// Original workflow output account. This trusted, non-serialized
    /// capability must not follow a newer turn in the same session.
    pub output_scope: Option<crate::WorkflowOutputScope>,
}

impl FusionInheritance {
    /// Build inheritance from a subagent bundle and a cancel token.
    #[must_use]
    pub fn new(subagent: SubagentInheritance, cancel: CancellationToken) -> Self {
        Self {
            subagent,
            cancel,
            effective_timeout_ms: None,
            output_scope: None,
        }
    }

    /// Attach the effective timeout captured for this run.
    #[must_use]
    pub fn with_effective_timeout_ms(mut self, timeout_ms: Option<u64>) -> Self {
        self.effective_timeout_ms = timeout_ms;
        self
    }

    /// Carry the workflow's already-captured account without re-resolving it.
    #[must_use]
    pub fn with_output_scope(mut self, scope: Option<crate::WorkflowOutputScope>) -> Self {
        self.output_scope = scope;
        self
    }

    /// Budget handle inherited from the parent.
    #[must_use]
    pub fn budget(&self) -> Arc<dyn BudgetEnforcerHandle> {
        Arc::clone(&self.subagent.budget)
    }
}

/// Validated identity minted for one Fusion computation.
///
/// The wire format is intentionally kept opaque: current production ids are
/// `fu_` followed by a compact UUID, while callers and persisted reports only
/// need a stable, validated string. This avoids coupling platform-api to the
/// orchestrator's id generator while still rejecting accidental empty or
/// cross-run ids at the boundary.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct FusionRunId(String);

impl FusionRunId {
    /// Mint a production-compatible id.
    #[must_use]
    pub fn generated() -> Self {
        Self(format!("fu_{}", uuid::Uuid::new_v4().simple()))
    }

    /// Validate and retain an existing id.
    pub fn parse(value: impl Into<String>) -> Result<Self, FusionError> {
        let value = value.into();
        let valid = value.strip_prefix("fu_").is_some_and(|suffix| {
            suffix.len() == 32 && suffix.bytes().all(|b| b.is_ascii_hexdigit())
        });
        if valid {
            Ok(Self(value))
        } else {
            Err(FusionError::InvalidRequest(
                "fusion run id must be `fu_` followed by 32 hexadecimal characters".into(),
            ))
        }
    }

    /// Borrow the wire spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FusionRunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for FusionRunId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

/// Trusted identity shared by every Fusion entrypoint and terminal outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FusionRunIdentity {
    /// Stable `fu_…` run id.
    pub run_id: FusionRunId,
    /// Trusted originating session. `None` is retained for legacy/unit callers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<protocol::SessionId>,
    /// Entry surface that started this computation.
    pub origin: FusionOrigin,
    /// Opaque host operation id (Agent invocation, task id, or workflow run).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_operation_id: Option<String>,
}

impl FusionRunIdentity {
    /// Construct an identity after validating the run id and request origin.
    pub fn new(
        run_id: FusionRunId,
        session_id: Option<protocol::SessionId>,
        origin: FusionOrigin,
        parent_operation_id: Option<String>,
    ) -> Self {
        Self {
            run_id,
            session_id,
            origin,
            parent_operation_id,
        }
    }
}

/// Immutable request/inheritance handoff consumed by `prepare`.
#[derive(Clone)]
pub struct FusionSubmission {
    /// Caller request DTO. It carries what to run, never who owns it: the
    /// session is `identity.session_id` and nothing else.
    pub request: FusionRequest,
    /// Parent handles captured by the caller.
    pub inherit: FusionInheritance,
    /// Trusted identity supplied by the host.
    pub identity: FusionRunIdentity,
}

impl FusionSubmission {
    /// Construct and reject a request whose legacy session disagrees with the
    /// trusted identity.
    pub fn new(
        request: FusionRequest,
        inherit: FusionInheritance,
        identity: FusionRunIdentity,
    ) -> Result<Self, FusionError> {
        if request.origin != identity.origin {
            return Err(FusionError::InvalidRequest(
                "fusion origin does not match the trusted run identity".into(),
            ));
        }
        if let Some(parent_operation_id) = identity.parent_operation_id.as_deref() {
            if parent_operation_id.trim().is_empty() {
                return Err(FusionError::InvalidRequest(
                    "fusion parent operation id must be non-empty".into(),
                ));
            }
        }
        if request.origin == FusionOrigin::Workflow
            && request
                .workflow_run_id
                .as_deref()
                .is_none_or(|run_id| run_id.trim().is_empty())
        {
            return Err(FusionError::InvalidRequest(
                "workflow fusion request must carry a non-empty workflow run id".into(),
            ));
        }
        if request.origin != FusionOrigin::Workflow && request.workflow_run_id.is_some() {
            return Err(FusionError::InvalidRequest(
                "non-workflow fusion request cannot carry a workflow run id".into(),
            ));
        }
        if request.origin == FusionOrigin::Workflow {
            let parent_operation = identity.parent_operation_id.as_deref().ok_or_else(|| {
                FusionError::InvalidRequest(
                    "workflow fusion identity must carry a trusted parent operation".into(),
                )
            })?;
            if request.workflow_run_id.as_deref() != Some(parent_operation) {
                return Err(FusionError::InvalidRequest(
                    "fusion workflow run does not match the trusted parent operation".into(),
                ));
            }
        }
        Ok(Self {
            request,
            inherit,
            identity,
        })
    }
}

/// Summary available to task registries before activation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FusionPreparedSummary {
    /// Immutable run identity.
    pub identity: FusionRunIdentity,
    /// Captured outer duration in milliseconds.
    pub duration_ms: u64,
    /// Exact prepared panel count when route resolution could determine it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planned_panels: Option<u8>,
}

/// Timestamp captured by the host's activation callback, not by a scheduled
/// worker after it eventually gets polled.
#[derive(Debug, Clone, Copy)]
pub struct FusionActivation {
    /// Monotonic activation time.
    pub activated_at: Instant,
}

impl FusionActivation {
    /// Capture the current monotonic time.
    #[must_use]
    pub fn now() -> Self {
        Self {
            activated_at: Instant::now(),
        }
    }
}

/// Durable attempt settlement is independent of computation and publication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum FusionAttemptSettlementStatus {
    /// Registered producers/finalizers have not finished yet.
    Pending,
    /// Every accepted attempt has completed durable settlement.
    Settled,
    /// Computation may still have an answer, but accounting is unavailable.
    Failed {
        /// Safe diagnostic, excluding prompts, credentials and provider bodies.
        reason: String,
    },
}

/// Reliable run facts. Progress events are a lossy UI projection and never
/// replace this recorder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FusionRunFacts {
    /// Absent for legacy aggregate runners. A computed answer does not imply
    /// that accepted physical attempts have settled successfully.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_settlement: Option<FusionAttemptSettlementStatus>,
    /// Number of routes resolved before any provider call, when known.
    #[serde(default)]
    pub resolved_panels: Option<u8>,
    /// Number of child tasks allocated by the spawner, when known.
    #[serde(default)]
    pub allocated_panels: Option<u8>,
    /// Number of panel dispatches that reached a provider-capable spawner,
    /// when known.
    #[serde(default)]
    pub dispatched_panels: Option<u8>,
    /// Provider/model attempts started, including attempts without usage,
    /// when known.
    #[serde(default)]
    pub attempts: Option<u32>,
    /// Best-known aggregate usage. `None` means the legacy runner did not
    /// expose enough information to claim that usage was zero.
    #[serde(default)]
    pub usage: Option<FusionUsage>,
    /// True when a dispatched attempt has an incomplete/estimated figure.
    pub usage_incomplete: bool,
    /// Profiles confirmed to have received prompt data.
    #[serde(default)]
    pub confirmed_egress: Vec<String>,
    /// Profiles that may have received data before the run lost certainty.
    #[serde(default)]
    pub possible_egress: Vec<String>,
    /// Final stage timings known to the recorder.
    pub timing: FusionTiming,
}

/// Synchronized owner of reliable Fusion facts.
#[derive(Clone, Default)]
pub struct FusionRunFactsRecorder(Arc<std::sync::Mutex<FusionRunFacts>>);

impl FusionRunFactsRecorder {
    /// Record the host-owned attempt finalizer status without changing the
    /// computation result or the independent transcript publication receipt.
    pub fn set_attempt_settlement(&self, status: FusionAttemptSettlementStatus) {
        let mut facts = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(
            facts.attempt_settlement,
            Some(FusionAttemptSettlementStatus::Failed { .. })
        ) || (matches!(
            facts.attempt_settlement,
            Some(FusionAttemptSettlementStatus::Settled)
        ) && matches!(status, FusionAttemptSettlementStatus::Pending))
        {
            return;
        }
        if matches!(status, FusionAttemptSettlementStatus::Failed { .. }) {
            facts.usage_incomplete = true;
        }
        facts.attempt_settlement = Some(status);
    }

    /// Snapshot facts without exposing the lock to callers.
    #[must_use]
    pub fn snapshot(&self) -> FusionRunFacts {
        let mut facts = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        facts.confirmed_egress.sort();
        facts.confirmed_egress.dedup();
        facts.possible_egress.sort();
        facts.possible_egress.dedup();
        facts
    }

    /// Replace the aggregate usage with the latest authoritative rollup.
    pub fn replace_usage(&self, usage: FusionUsage, incomplete: bool) {
        let mut facts = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        facts.usage = Some(usage);
        facts.usage_incomplete = incomplete
            || matches!(
                facts.attempt_settlement,
                Some(FusionAttemptSettlementStatus::Failed { .. })
            );
    }

    /// Record a terminal/preflight state that provably made no provider call.
    /// This is deliberately explicit: absence of legacy facts remains
    /// `None`, not an invented zero.
    pub fn set_known_zero(&self) {
        let mut facts = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        facts.allocated_panels = Some(0);
        facts.dispatched_panels = Some(0);
        facts.attempts = Some(0);
        facts.usage = Some(FusionUsage::default());
        facts.usage_incomplete = matches!(
            facts.attempt_settlement,
            Some(FusionAttemptSettlementStatus::Failed { .. })
        );
        facts.confirmed_egress.clear();
        facts.possible_egress.clear();
    }

    /// Latch a resolved panel count.
    pub fn set_resolved_panels(&self, count: u8) {
        let mut facts = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        facts.resolved_panels = Some(facts.resolved_panels.unwrap_or_default().max(count));
    }

    /// Latch an allocated panel count.
    pub fn set_allocated_panels(&self, count: u8) {
        let mut facts = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        facts.allocated_panels = Some(facts.allocated_panels.unwrap_or_default().max(count));
    }

    /// Latch a dispatched panel count.
    pub fn set_dispatched_panels(&self, count: u8) {
        let mut facts = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        facts.dispatched_panels = Some(facts.dispatched_panels.unwrap_or_default().max(count));
    }

    /// Add model/provider attempts.
    pub fn add_attempts(&self, attempts: u32) {
        let mut facts = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        facts.attempts = Some(facts.attempts.unwrap_or_default().saturating_add(attempts));
    }

    /// Latch a known attempt count without turning a later partial snapshot
    /// into a second copy of the same attempts.
    pub fn set_attempts(&self, attempts: u32) {
        let mut facts = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        facts.attempts = Some(facts.attempts.unwrap_or_default().max(attempts));
    }

    /// Replace the attempt count with the latest exact value, or `None` when
    /// work may have reached a provider but the current boundary cannot prove
    /// how many wire attempts started.
    pub fn replace_attempts(&self, attempts: Option<u32>) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .attempts = attempts;
    }

    /// A provider-capable boundary was reached without an exact wire-attempt
    /// receipt. Preserve known usage while withdrawing a provisional zero.
    pub fn mark_attempts_unknown(&self) {
        let mut facts = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        facts.attempts = None;
        facts.usage_incomplete = true;
    }

    /// Publish the activation-time monetary hold while preserving any usage
    /// already observed at the same boundary.
    pub fn set_reserved_max_nano_usd(&self, reserved_nano_usd: u64) {
        let mut facts = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        facts
            .usage
            .get_or_insert_with(FusionUsage::default)
            .reserved_max_nano_usd = reserved_nano_usd;
    }

    /// Merge a confirmed egress profile into the facts.
    pub fn add_confirmed_egress(&self, profile: impl Into<String>) {
        let profile = profile.into();
        let mut facts = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !facts.confirmed_egress.contains(&profile) {
            facts.confirmed_egress.push(profile);
        }
    }

    /// Merge a possible egress profile into the facts.
    pub fn add_possible_egress(&self, profile: impl Into<String>) {
        let profile = profile.into();
        let mut facts = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !facts.possible_egress.contains(&profile) {
            facts.possible_egress.push(profile);
        }
    }

    /// Replace the current conservative egress set. This lets a spawner
    /// rejection retire a profile that was only provisional while the call
    /// was in flight, without erasing separately confirmed egress.
    pub fn replace_possible_egress(&self, mut profiles: Vec<String>) {
        profiles.sort();
        profiles.dedup();
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .possible_egress = profiles;
    }

    /// Replace stage timings.
    pub fn set_timing(&self, timing: FusionTiming) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .timing = timing;
    }

    /// Replace provisional stage egress with authoritative attempt facts.
    pub fn replace_egress(&self, mut confirmed: Vec<String>, mut possible: Vec<String>) {
        confirmed.sort();
        confirmed.dedup();
        possible.sort();
        possible.dedup();
        possible.retain(|profile| confirmed.binary_search(profile).is_err());
        let mut facts = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        facts.confirmed_egress = confirmed;
        facts.possible_egress = possible;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FusionControlPhase {
    Prepared,
    Running,
    Finalizing,
    Terminal,
}

#[derive(Debug)]
struct FusionControlState {
    phase: FusionControlPhase,
    cancel_claimed: bool,
    activated_at: Option<Instant>,
    deadline: Option<Instant>,
    outcome: Option<Arc<FusionRunOutcome>>,
}

/// Shared cooperative cancellation/deadline/terminal authority.
#[derive(Clone)]
pub struct FusionRunControl {
    identity: FusionRunIdentity,
    billing_mode: crate::ModelAttemptBillingMode,
    duration: Duration,
    cancel: CancellationToken,
    facts: FusionRunFactsRecorder,
    state: Arc<std::sync::Mutex<FusionControlState>>,
    terminal_tx: watch::Sender<Option<Arc<FusionRunOutcome>>>,
}

impl FusionRunControl {
    /// Construct a prepared control object. The deadline starts only when the
    /// host supplies the activation timestamp.
    #[must_use]
    pub fn new(
        identity: FusionRunIdentity,
        duration_ms: u64,
        cancel: CancellationToken,
        facts: FusionRunFactsRecorder,
    ) -> Self {
        Self::new_with_billing_mode(
            identity,
            duration_ms,
            cancel,
            facts,
            crate::ModelAttemptBillingMode::LegacyAggregate,
        )
    }

    /// Capture the host's accounting contract before activation. Select
    /// metered mode only when every paid stage uses registered attempts;
    /// serialized request metadata must never choose this value.
    #[must_use]
    pub fn new_with_billing_mode(
        identity: FusionRunIdentity,
        duration_ms: u64,
        cancel: CancellationToken,
        facts: FusionRunFactsRecorder,
        billing_mode: crate::ModelAttemptBillingMode,
    ) -> Self {
        if billing_mode == crate::ModelAttemptBillingMode::MeteredAttempts {
            facts.set_attempt_settlement(FusionAttemptSettlementStatus::Pending);
        }
        let (terminal_tx, _terminal_rx) = watch::channel(None);
        Self {
            identity,
            billing_mode,
            duration: Duration::from_millis(duration_ms),
            cancel,
            facts,
            state: Arc::new(std::sync::Mutex::new(FusionControlState {
                phase: FusionControlPhase::Prepared,
                cancel_claimed: false,
                activated_at: None,
                deadline: None,
                outcome: None,
            })),
            terminal_tx,
        }
    }

    /// Identity carried by this control.
    #[must_use]
    pub fn identity(&self) -> &FusionRunIdentity {
        &self.identity
    }

    /// Immutable host-selected accounting contract, shared by every terminal path.
    #[must_use]
    pub fn billing_mode(&self) -> crate::ModelAttemptBillingMode {
        self.billing_mode
    }

    /// Shared facts recorder.
    #[must_use]
    pub fn facts(&self) -> FusionRunFactsRecorder {
        self.facts.clone()
    }

    /// Cancellation token shared with the parent/worker.
    #[must_use]
    pub fn cancel(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Captured outer duration used by the supervisor.
    #[must_use]
    pub fn duration(&self) -> Duration {
        self.duration
    }

    /// Activate once. The timestamp is captured by the host handoff.
    pub fn activate_at(&self, activated_at: Instant) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.phase != FusionControlPhase::Prepared {
            return false;
        }
        if self.cancel.is_cancelled() {
            state.phase = FusionControlPhase::Terminal;
            state.cancel_claimed = true;
            return false;
        }
        let Some(deadline) = activated_at.checked_add(self.duration) else {
            state.phase = FusionControlPhase::Terminal;
            return false;
        };
        state.phase = FusionControlPhase::Running;
        state.activated_at = Some(activated_at);
        state.deadline = Some(deadline);
        true
    }

    /// Remaining duration from the single captured deadline.
    #[must_use]
    pub fn remaining(&self) -> Duration {
        let deadline = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .deadline;
        deadline.map_or(Duration::ZERO, |deadline| {
            deadline.saturating_duration_since(Instant::now())
        })
    }

    /// Absolute deadline captured at activation, if the run is active.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .deadline
    }

    /// Begin irreversible finalization. Exactly one owner wins.
    pub fn begin_finalizing(&self) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.phase != FusionControlPhase::Running {
            return false;
        }
        state.phase = FusionControlPhase::Finalizing;
        true
    }

    /// Atomically claim cancellation while the run is still cancellable.
    ///
    /// Once finalization has begun, the natural result owns settlement and
    /// cancellation must not replace it. Callers should only tear down their
    /// task projection when this method returns `true`.
    pub fn request_cancel(&self) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !matches!(
            state.phase,
            FusionControlPhase::Prepared | FusionControlPhase::Running
        ) {
            return false;
        }
        state.phase = FusionControlPhase::Terminal;
        state.cancel_claimed = true;
        // Cancel while the phase lock is still held. A concurrent finalizer
        // can only observe Terminal after the token is already signalled.
        self.cancel.cancel();
        true
    }

    /// Atomically choose the terminal result owned by the common supervisor.
    ///
    /// A successful natural result crosses the irreversible `Finalizing`
    /// boundary; an error owns `Terminal` directly. If cancellation already
    /// won either claim, its result is authoritative. Keeping the phase read,
    /// winner selection, and transition under one lock prevents a cancellation
    /// claim from landing between a stale read and a failed natural claim.
    fn claim_supervisor_result(
        &self,
        natural: Result<FusionResult, FusionError>,
    ) -> Result<FusionResult, FusionError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.cancel_claimed {
            return Err(FusionError::Cancelled);
        }
        match state.phase {
            FusionControlPhase::Prepared => {
                // An unpolled activation/failed preparation owns a no-dispatch
                // terminal before its asynchronous recorder is scheduled.
                state.phase = FusionControlPhase::Terminal;
            }
            FusionControlPhase::Running => {
                state.phase = if natural.is_ok() {
                    FusionControlPhase::Finalizing
                } else {
                    FusionControlPhase::Terminal
                };
            }
            // The Fusion orchestrator may cross Finalizing before it returns
            // while it commits settlement. Preserve that natural winner.
            FusionControlPhase::Finalizing | FusionControlPhase::Terminal => {}
        }
        natural
    }

    /// Claim an early terminal state while the run is still prepared/running.
    ///
    /// Finalization has a separate claim method so cancellation cannot steal a
    /// natural result after the supervisor crossed its irreversible boundary.
    pub fn claim_terminal(&self) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !matches!(
            state.phase,
            FusionControlPhase::Prepared | FusionControlPhase::Running
        ) {
            return false;
        }
        state.phase = FusionControlPhase::Terminal;
        true
    }

    /// Claim the terminal state after [`Self::begin_finalizing`] won.
    pub fn claim_finalizing(&self) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.phase != FusionControlPhase::Finalizing {
            return false;
        }
        state.phase = FusionControlPhase::Terminal;
        true
    }

    /// Whether a terminal owner already exists.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .phase
            == FusionControlPhase::Terminal
    }

    /// Whether the run crossed its irreversible finalization boundary.
    #[must_use]
    pub fn is_finalizing(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .phase
            == FusionControlPhase::Finalizing
    }

    /// Return the sealed outcome once the owned supervisor has finished all
    /// accounting and terminal callbacks. A terminal phase alone is not
    /// sufficient: cancellation may claim it while settlement is still
    /// draining.
    #[must_use]
    pub fn terminal_outcome(&self) -> Option<FusionRunOutcome> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .outcome
            .as_deref()
            .cloned()
    }

    /// Wait for the terminal envelope, not merely a terminal phase claim.
    /// This is the synchronization point for quota and task projections that
    /// must observe the final allocation/cost facts.
    pub async fn wait_terminal(&self) -> FusionRunOutcome {
        let mut terminal_rx = self.terminal_tx.subscribe();
        loop {
            if let Some(outcome) = self.terminal_outcome() {
                return outcome;
            }
            // The sender is retained by `self`, so closure is unreachable.
            let _ = terminal_rx.changed().await;
        }
    }

    fn publish_terminal(&self, result: Result<FusionResult, FusionError>) -> FusionRunOutcome {
        self.publish_terminal_with_receipt(result, FusionPublicationReceipt::not_required())
    }

    fn publish_terminal_with_receipt(
        &self,
        result: Result<FusionResult, FusionError>,
        publication: FusionPublicationReceipt,
    ) -> FusionRunOutcome {
        let candidate = self.terminal_candidate(result);
        self.publish_terminal_candidate(candidate, publication)
    }

    fn terminal_candidate(&self, result: Result<FusionResult, FusionError>) -> FusionRunOutcome {
        if self.billing_mode == crate::ModelAttemptBillingMode::MeteredAttempts {
            let activated = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .activated_at
                .is_some();
            let mut facts = self
                .facts
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if matches!(
                facts.attempt_settlement,
                None | Some(FusionAttemptSettlementStatus::Pending)
            ) {
                facts.attempt_settlement = Some(if activated {
                    facts.usage_incomplete = true;
                    FusionAttemptSettlementStatus::Failed {
                        reason: "registered attempt settlement did not complete".into(),
                    }
                } else {
                    // An unactivated prepared closure cannot acquire holds or send.
                    FusionAttemptSettlementStatus::Settled
                });
            }
        }
        FusionRunOutcome::from_control(self, result)
    }

    fn publish_terminal_candidate(
        &self,
        mut candidate: FusionRunOutcome,
        publication: FusionPublicationReceipt,
    ) -> FusionRunOutcome {
        candidate.publication = publication;
        let candidate = Arc::new(candidate);
        let outcome = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(outcome) = state.outcome.as_ref() {
                return outcome.as_ref().clone();
            }
            state.phase = FusionControlPhase::Terminal;
            state.outcome = Some(Arc::clone(&candidate));
            candidate
        };
        self.terminal_tx.send_replace(Some(Arc::clone(&outcome)));
        outcome.as_ref().clone()
    }
}

/// One terminal Fusion envelope, including reliable identity and facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FusionRunOutcome {
    billing_mode: crate::ModelAttemptBillingMode,
    /// Immutable identity.
    pub identity: FusionRunIdentity,
    /// Legacy computation result or failure.
    pub result: Result<FusionResult, FusionError>,
    /// Reliable facts snapshot.
    pub facts: FusionRunFacts,
    /// Durable terminal/publication receipt. The computation result remains
    /// authoritative even when this receipt reports a storage failure.
    pub publication: FusionPublicationReceipt,
}

impl FusionRunOutcome {
    /// Construct a terminal outcome from a control object.
    #[must_use]
    pub fn from_control(
        control: &FusionRunControl,
        result: Result<FusionResult, FusionError>,
    ) -> Self {
        Self {
            billing_mode: control.billing_mode,
            identity: control.identity.clone(),
            result,
            facts: control.facts.snapshot(),
            publication: FusionPublicationReceipt::not_required(),
        }
    }

    /// Whether attempt receipts or the legacy aggregate own accounting.
    #[must_use]
    pub fn billing_mode(&self) -> crate::ModelAttemptBillingMode {
        self.billing_mode
    }
}

type PreparedRunner = Box<
    dyn FnOnce(
            FusionActivation,
            Option<Sender<FusionProgress>>,
        ) -> BoxFuture<'static, FusionRunOutcome>
        + Send,
>;

/// Host-trusted target that permits a terminal run to enqueue a parent-session
/// Slash publication. Agent and Workflow entrypoints deliberately do not
/// receive this capability, even when they carry a session identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FusionSlashPublicationTarget {
    /// Canonical parent session selected by the trusted task/bridge input.
    pub session_id: protocol::SessionId,
}

/// Common durable terminal recorder. The recorder is invoked by the owned
/// [`PreparedFusionRun`] supervisor before its control watch is sealed, so all
/// origins and terminal paths share one persistence boundary.
#[async_trait]
pub trait FusionRunRecorder: Send + Sync {
    /// Record one immutable terminal outcome and, when `slash_target` is
    /// present, atomically retain its trusted parent-session outbox item.
    async fn record_terminal(
        &self,
        outcome: FusionRunOutcome,
        slash_target: Option<FusionSlashPublicationTarget>,
    ) -> FusionPublicationReceipt;
}

/// Pure host factory for a recorder pinned to one already-hydrated session
/// authority. Entrypoints call this during preparation, so a hot A→B switch
/// cannot leave a future run holding A's recorder. Returning `None` means the
/// trusted session is not currently mounted and the caller must fail closed or
/// use its explicitly configured legacy adapter.
pub trait FusionRunRecorderFactory: Send + Sync {
    /// Resolve a recorder without I/O or permit acquisition.
    fn recorder_for(&self, session_id: protocol::SessionId) -> Option<Arc<dyn FusionRunRecorder>>;
}

/// Capability attached by a host after preparation. Attaching it performs no
/// I/O and reserves no permits; the owned supervisor consumes it only when the
/// run reaches a terminal boundary.
#[derive(Clone)]
pub struct FusionTerminalCapability {
    recorder: Arc<dyn FusionRunRecorder>,
    slash_target: Option<FusionSlashPublicationTarget>,
}

impl FusionTerminalCapability {
    /// Attach an all-origin recorder without granting parent publication.
    #[must_use]
    pub fn new(recorder: Arc<dyn FusionRunRecorder>) -> Self {
        Self {
            recorder,
            slash_target: None,
        }
    }

    /// Grant the host-trusted Slash parent publication capability.
    #[must_use]
    pub fn with_slash_target(mut self, target: FusionSlashPublicationTarget) -> Self {
        self.slash_target = Some(target);
        self
    }

    async fn record(&self, outcome: FusionRunOutcome) -> FusionPublicationReceipt {
        self.recorder
            .record_terminal(outcome, self.slash_target)
            .await
    }
}

/// Poll a prepared runner behind a panic boundary inside the owned supervisor.
struct CatchPanicFuture<F> {
    inner: F,
}

impl<F> std::future::Future for CatchPanicFuture<F>
where
    F: std::future::Future + Unpin,
{
    type Output = Result<F::Output, ()>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let poll = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            std::pin::Pin::new(&mut self.inner).poll(cx)
        }));
        match poll {
            Ok(std::task::Poll::Ready(output)) => std::task::Poll::Ready(Ok(output)),
            Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
            Err(_) => std::task::Poll::Ready(Err(())),
        }
    }
}

/// Opaque one-shot prepared execution. It is deliberately non-Clone so a
/// prepared route cannot be accidentally dispatched twice.
pub struct PreparedFusionRun {
    summary: FusionPreparedSummary,
    control: FusionRunControl,
    runner: Option<PreparedRunner>,
    unactivated_error: FusionError,
    terminal_capability: Option<FusionTerminalCapability>,
}

impl PreparedFusionRun {
    /// Build a prepared run around a private runner closure.
    pub fn new<F, Fut>(summary: FusionPreparedSummary, control: FusionRunControl, runner: F) -> Self
    where
        F: FnOnce(FusionActivation, Option<Sender<FusionProgress>>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = FusionRunOutcome> + Send + 'static,
    {
        Self {
            summary,
            control,
            runner: Some(Box::new(move |activation, progress| {
                Box::pin(runner(activation, progress))
            })),
            unactivated_error: FusionError::Cancelled,
            terminal_capability: None,
        }
    }

    /// Attach the host-owned terminal recorder after pure preparation.
    #[must_use]
    pub fn with_terminal_capability(mut self, capability: FusionTerminalCapability) -> Self {
        self.terminal_capability = Some(capability);
        self
    }

    /// Build an inert terminal run for pre-activation preparation failures.
    #[must_use]
    pub fn failed(
        summary: FusionPreparedSummary,
        control: FusionRunControl,
        error: FusionError,
    ) -> Self {
        control.facts().set_known_zero();
        let mut prepared = Self::new(summary, control.clone(), {
            let error = error.clone();
            move |_activation, _progress| {
                let outcome = FusionRunOutcome::from_control(&control, Err(error));
                async move { outcome }
            }
        });
        prepared.unactivated_error = error;
        prepared
    }

    /// Summary safe to publish before activation.
    #[must_use]
    pub fn summary(&self) -> &FusionPreparedSummary {
        &self.summary
    }

    /// Shared control used by the host supervisor.
    #[must_use]
    pub fn control(&self) -> FusionRunControl {
        self.control.clone()
    }

    /// Activate exactly once with the host-captured timestamp.
    pub async fn activate(
        mut self,
        activation: FusionActivation,
        progress: Option<Sender<FusionProgress>>,
    ) -> FusionRunOutcome {
        if !self.control.activate_at(activation.activated_at) {
            let error = if self.control.cancel().is_cancelled() {
                FusionError::Cancelled
            } else {
                FusionError::Internal
            };
            self.control.facts().set_known_zero();
            self.runner.take();
            return self.spawn_detached_terminal(Err(error)).await;
        }
        let Some(runner) = self.runner.take() else {
            self.control.facts().set_known_zero();
            return self
                .spawn_detached_terminal(Err(FusionError::Internal))
                .await;
        };
        let supervisor_control = self.control.clone();
        let terminal_capability = self.terminal_capability.clone();
        let spawn = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tokio::spawn(async move {
                // Constructing the runner future happens inside this async
                // block, so both a synchronous closure panic and a later poll
                // panic are caught by the same boundary.
                let guarded: BoxFuture<'static, FusionRunOutcome> =
                    Box::pin(async move { runner(activation, progress).await });
                let outcome = CatchPanicFuture { inner: guarded }.await;
                let runner_result = match outcome {
                    Ok(outcome)
                        if &outcome.identity == supervisor_control.identity()
                            && outcome.result.as_ref().map_or(true, |result| {
                                result.run_id == supervisor_control.identity().run_id.as_str()
                            }) =>
                    {
                        outcome.result
                    }
                    Ok(_) | Err(()) => Err(FusionError::Internal),
                };
                // A runner supplied by a legacy/fake executor may ignore the
                // cooperative token. Resolve the natural/cancel winner and
                // cross the finalization boundary in one atomic transition.
                let result = supervisor_control.claim_supervisor_result(runner_result);
                let candidate = supervisor_control.terminal_candidate(result.clone());
                let publication =
                    Self::record_terminal_candidate(candidate.clone(), terminal_capability).await;
                supervisor_control.publish_terminal_candidate(candidate, publication);
            })
        }));
        if spawn.is_err() {
            self.control.facts().set_known_zero();
            return self
                .spawn_detached_terminal(Err(FusionError::Internal))
                .await;
        }
        self.control.wait_terminal().await
    }

    async fn spawn_detached_terminal(
        self,
        result: Result<FusionResult, FusionError>,
    ) -> FusionRunOutcome {
        let control = self.control.clone();
        let result = control.claim_supervisor_result(result);
        let capability = self.terminal_capability.clone();
        let fallback_control = control.clone();
        let fallback_result = result.clone();
        let spawn = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tokio::spawn(async move {
                Self::seal_terminal(control, result, capability).await;
            })
        }));
        if spawn.is_err() {
            return fallback_control.publish_terminal_with_receipt(
                fallback_result,
                FusionPublicationReceipt::storage_failure(
                    "terminal supervisor could not be scheduled",
                ),
            );
        }
        fallback_control.wait_terminal().await
    }

    async fn record_terminal_candidate(
        candidate: FusionRunOutcome,
        capability: Option<FusionTerminalCapability>,
    ) -> FusionPublicationReceipt {
        let Some(capability) = capability else {
            return FusionPublicationReceipt::not_required();
        };
        let recorder = capability.clone();
        let future: BoxFuture<'static, FusionPublicationReceipt> =
            Box::pin(async move { recorder.record(candidate).await });
        CatchPanicFuture { inner: future }
            .await
            .unwrap_or_else(|_| {
                FusionPublicationReceipt::storage_failure("terminal recorder panicked")
            })
    }

    async fn seal_terminal(
        control: FusionRunControl,
        result: Result<FusionResult, FusionError>,
        capability: Option<FusionTerminalCapability>,
    ) -> FusionRunOutcome {
        let candidate = control.terminal_candidate(result.clone());
        let publication = Self::record_terminal_candidate(candidate.clone(), capability).await;
        control.publish_terminal_candidate(candidate, publication)
    }
}

impl Drop for PreparedFusionRun {
    fn drop(&mut self) {
        if self.runner.is_none() {
            return;
        }
        // No activation poll can have crossed a provider boundary while the
        // one-shot runner is still armed. Seal an exact-zero outcome so a
        // host guard waiting to release quota cannot hang when the prepared
        // value (or an entirely unpolled `activate` future) is abandoned.
        self.control.facts().set_known_zero();
        let control = self.control.clone();
        let result = control.claim_supervisor_result(Err(self.unactivated_error.clone()));
        let Some(capability) = self.terminal_capability.clone() else {
            control.publish_terminal_with_receipt(result, FusionPublicationReceipt::not_required());
            return;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            self.control.publish_terminal_with_receipt(
                result,
                FusionPublicationReceipt::storage_failure(
                    "terminal recorder requires an async runtime",
                ),
            );
            return;
        };
        handle.spawn(async move {
            PreparedFusionRun::seal_terminal(control, result, Some(capability)).await;
        });
    }
}

/// Claim inside a panel report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelClaim {
    /// Claim text.
    pub statement: String,
    /// Evidence ids in the same report.
    #[serde(default)]
    pub evidence_refs: Vec<String>,
    /// 0..=100.
    pub confidence: u8,
}

/// Kind of supporting evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    /// A workspace file or host-located workspace search result.
    File,
    /// A fetched URL.
    Url,
    /// A shell command the panel ran.
    Command,
}

/// One evidence item cited by claims.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelEvidence {
    /// Report-local unique id.
    pub id: String,
    /// Evidence kind.
    pub kind: EvidenceKind,
    /// Path, URL, command string, or exact host-minted `lingxi-search:` locator
    /// identifying captured search output (not a read of all matching files).
    pub locator: String,
    /// Optional excerpt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
}

/// Risk severity on a panel report or analyst contradiction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskSeverity {
    /// Low impact.
    Low,
    /// Medium impact.
    Medium,
    /// High impact.
    High,
    /// Must not be averaged away.
    Critical,
}

/// One risk called out by a panel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelRisk {
    /// Severity.
    pub severity: RiskSeverity,
    /// Description.
    pub description: String,
}

/// Structured panel output. Host-validated after the runner schema check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelReport {
    /// Schema version.
    #[serde(default = "fusion_schema_version")]
    pub schema_version: u16,
    /// Short summary.
    pub summary: String,
    /// Proposed final answer / patch / plan.
    pub candidate_answer: String,
    /// Claims.
    #[serde(default)]
    pub claims: Vec<PanelClaim>,
    /// Evidence items.
    #[serde(default)]
    pub evidence: Vec<PanelEvidence>,
    /// Explicit assumptions.
    #[serde(default)]
    pub assumptions: Vec<String>,
    /// Risks.
    #[serde(default)]
    pub risks: Vec<PanelRisk>,
    /// Questions the panel could not resolve.
    #[serde(default)]
    pub unresolved_questions: Vec<String>,
}

/// Where two (or more) panels disagree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FusionContradiction {
    /// How severe the disagreement is.
    pub severity: RiskSeverity,
    /// Topic label.
    pub topic: String,
    /// Per-panel positions (anonymous ids).
    pub positions: Vec<PanelPosition>,
}

/// One panel's stance on a contradiction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelPosition {
    /// Anonymous panel id (`P1`, …).
    pub panel_id: String,
    /// Stance text.
    pub position: String,
}

/// Insight unique to one panel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FusionUniqueInsight {
    /// Anonymous panel id.
    pub panel_id: String,
    /// Insight text.
    pub insight: String,
}

/// Analyst recommendation. The host interpreter may override Merge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FusionRecommendation {
    /// Adopt one panel's candidate answer.
    Pick {
        /// Anonymous panel id.
        panel_id: String,
        /// Why this panel won.
        reason: String,
    },
    /// Ask the parent model to synthesize.
    Merge {
        /// Why a merge is justified.
        reason: String,
    },
    /// Do not auto-conclude.
    NeedsParent {
        /// Why the parent must decide.
        reason: String,
    },
}

/// Structured analyst output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FusionAnalysis {
    /// Schema version.
    #[serde(default = "fusion_schema_version")]
    pub schema_version: u16,
    /// Points most panels agreed on.
    #[serde(default)]
    pub consensus: Vec<String>,
    /// Direct conflicts.
    #[serde(default)]
    pub contradictions: Vec<FusionContradiction>,
    /// Insights only some panels had.
    #[serde(default)]
    pub unique_insights: Vec<FusionUniqueInsight>,
    /// Gaps none of the panels covered.
    #[serde(default)]
    pub coverage_gaps: Vec<String>,
    /// `panel_id → dimension → 0..=100`.
    #[serde(default)]
    pub scores: BTreeMap<String, BTreeMap<String, u8>>,
    /// Analyst confidence 0..=100.
    pub confidence: u8,
    /// Analyst recommendation (host may rewrite Merge → `NeedsParent`).
    pub recommendation: FusionRecommendation,
}

/// Terminal status of a Fusion run that produced usable material.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FusionStatus {
    /// A final answer was produced (pick or merge).
    Completed,
    /// Material exists but the caller should route it back to the parent
    /// model/user for judgment instead of treating it as a final answer.
    NeedsParent,
}

/// Why Fusion returned [`FusionStatus::NeedsParent`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FusionNeedsParentReason {
    /// Analyst asked for parent judgment.
    AnalystRequested {
        /// Reason text.
        reason: String,
    },
    /// Analyst JSON failed to decode / validate after all retries.
    AnalysisParseFailed,
    /// Analyst call failed for a reason other than a decode failure — a
    /// timeout, a transport/4xx/5xx error, or (post-panel) an analyst whose
    /// route turned out not to support structured output. `category` is a
    /// sanitized category string (never a raw provider body), e.g. `"timeout"`
    /// or `"structured_output_unsupported"`.
    AnalysisFailed {
        /// Sanitized failure category.
        category: String,
    },
    /// Unresolved critical contradiction blocked merge.
    CriticalContradiction,
    /// Merge confidence below the host threshold.
    LowConfidence,
    /// Synthesizer call failed.
    SynthesisFailed,
    /// Synthesizer timed out.
    SynthesisTimedOut,
}

/// Decision recorded on [`FusionResult`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FusionDecision {
    /// Sanitized candidate of this panel was returned.
    Picked {
        /// Anonymous panel id.
        panel_id: String,
    },
    /// Parent model synthesized a merged answer.
    Merged,
    /// Structured needs-parent outcome. The result stays `Ok(...)`; callers can
    /// surface the compact material without pretending Fusion reached closure.
    NeedsParent {
        /// Machine-readable reason.
        reason: FusionNeedsParentReason,
    },
}

/// Panel terminal status in the compact result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelRunStatus {
    /// Produced a valid `PanelReport`.
    Completed,
    /// Failed (protocol, provider, or tool error).
    Failed,
    /// Idle or total timeout.
    TimedOut,
    /// Cancelled with the parent run.
    Cancelled,
}

/// Compact per-panel outcome. Does not carry the raw [`PanelReport`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelOutcome {
    /// Anonymous id (`P1`, …).
    pub panel_id: String,
    /// Terminal status.
    pub status: PanelRunStatus,
    /// Wall-clock duration.
    pub duration_ms: u64,
    /// Sanitized error category. Never a raw provider body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_category: Option<String>,
    /// Sanitized, length-capped one-line detail of the source error (G011).
    /// Additive: absent on older serialized results, and `None` when there
    /// was nothing beyond [`Self::error_category`] to attach. Never a raw
    /// provider body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_detail: Option<String>,
    /// Cumulative usage for this panel when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<FusionUsage>,
}

/// THE SINGLE SOURCE OF TRUTH for "this panel provably never became a
/// subagent, so it provably made no provider call and must not be charged
/// against the session's lifetime spawn quota or disclosed as egress".
///
/// The two [`PanelOutcome::error_category`] values that prove it:
/// - `"spawn"` — the spawner rejected the panel before allocating a child.
/// - `"not_dispatched"` — the slot was cancelled before its task ever called
///   the spawner, or aborted while parked INSIDE a spawner call that had not
///   yet allocated a child (fusion's `PanelDispatch` distinguishes "entered
///   the spawner call" from "the pool handed us a child" for exactly this).
///
/// Every other category describes a panel for which a subagent provably
/// existed and which may therefore have been billed.
///
/// This predicate lives HERE, beside the field it reads, because it is
/// consumed from two crates that cannot see each other: `fusion` produces the
/// categories, and `tool-agent` decides the spawn-quota release from them.
/// Round-6 blocking B2 was precisely those two crates drifting apart — a new
/// value was added on the fusion side while the tool-agent side still
/// compared against `"spawn"` alone, silently burning one lifetime spawn slot
/// per such panel. **Add any new never-dispatched category here and only
/// here**; both crates route through this function.
pub fn panel_never_dispatched(category: Option<&str>) -> bool {
    matches!(category, Some("spawn" | "not_dispatched"))
}

/// Aggregated Fusion usage / cost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FusionUsage {
    /// Billable input tokens.
    pub input_tokens: u64,
    /// Billable output tokens.
    pub output_tokens: u64,
    /// Reasoning tokens.
    pub reasoning_tokens: u64,
    /// Cache-read tokens.
    pub cache_read_tokens: u64,
    /// Cache-write tokens.
    pub cache_write_tokens: u64,
    /// Realized cost in nano-USD.
    pub realized_nano_usd: u64,
    /// Reserved maximum in nano-USD. This is the cost guardrail, not a bill.
    pub reserved_max_nano_usd: u64,
    /// True when any component used an estimated fallback.
    pub estimated: bool,
    /// Provider HTTP/API requests started.
    pub provider_requests: u32,
}

/// Stage timings in milliseconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FusionTiming {
    /// End-to-end.
    pub total_ms: u64,
    /// Panel fan-out (wall clock, not sum).
    pub panels_ms: u64,
    /// Analyst call.
    pub analyst_ms: u64,
    /// Synthesizer call (0 on pick).
    pub synthesizer_ms: u64,
}

/// Persisted / returned Fusion result. Unknown optional fields are ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FusionResult {
    /// Schema version.
    #[serde(default = "fusion_schema_version")]
    pub schema_version: u16,
    /// `fu_` + ulid. Distinct from the `LocalFusion` task id (`f` + 8 base36).
    pub run_id: String,
    /// Completed vs needs-parent.
    pub status: FusionStatus,
    /// Decision.
    pub decision: FusionDecision,
    /// Sanitized final text (or a deterministic `NeedsParent` summary).
    pub final_text: String,
    /// Analyst output when analysis ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analysis: Option<FusionAnalysis>,
    /// Compact panel outcomes.
    #[serde(default)]
    pub panels: Vec<PanelOutcome>,
    /// Aggregated usage.
    #[serde(default)]
    pub usage: FusionUsage,
    /// Timings.
    #[serde(default)]
    pub timing: FusionTiming,
    /// Provider profiles that received prompt data. Cross-provider runs may
    /// include profiles beyond the parent session's provider.
    #[serde(default)]
    pub egress_profiles: Vec<String>,
}

/// Publication state for the sanitized result produced by a Fusion run.
///
/// Publication is deliberately independent from [`FusionStatus`]. A run can
/// have a perfectly usable answer while its parent-session append is still in
/// flight or has failed. In particular, `Queued` means that a durable outbox
/// accepted the item; an in-memory hand-off must not use that state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FusionPublicationStatus {
    /// No parent-session publication is required for this run.
    NotRequired,
    /// Publication has been requested but has not reached a terminal state.
    #[default]
    Pending,
    /// A durable outbox accepted the result; delivery may happen later.
    Queued,
    /// The result was appended to the parent session.
    Published,
    /// An outbox was expected but rejected the item.
    OutboxFailed,
    /// The result could not be durably stored.
    StorageFailure,
}

impl FusionPublicationStatus {
    /// Whether this state is final for a publication attempt.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Pending)
    }

    /// Whether this state provides a durable publication outcome suitable for
    /// a successful one-shot CLI exit.
    #[must_use]
    pub const fn is_success(self) -> bool {
        matches!(self, Self::Queued | Self::Published)
    }
}

/// Typed acknowledgement returned by a [`FusionCompletionSink`].
///
/// The optional error is sanitized, host-owned context. It is retained on the
/// task state so callers can report a storage/outbox failure without dropping
/// the already-computed answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FusionPublicationReceipt {
    /// Result of the publication attempt.
    pub status: FusionPublicationStatus,
    /// Short failure detail, when the status is `OutboxFailed` or
    /// `StorageFailure`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Default for FusionPublicationReceipt {
    fn default() -> Self {
        Self::pending()
    }
}

impl FusionPublicationReceipt {
    /// Construct a pending receipt.
    #[must_use]
    pub const fn pending() -> Self {
        Self {
            status: FusionPublicationStatus::Pending,
            error: None,
        }
    }

    /// Construct a no-publication-required receipt.
    #[must_use]
    pub const fn not_required() -> Self {
        Self {
            status: FusionPublicationStatus::NotRequired,
            error: None,
        }
    }

    /// Construct a durable-queue acknowledgement.
    #[must_use]
    pub const fn queued() -> Self {
        Self {
            status: FusionPublicationStatus::Queued,
            error: None,
        }
    }

    /// Construct a successful append acknowledgement.
    #[must_use]
    pub const fn published() -> Self {
        Self {
            status: FusionPublicationStatus::Published,
            error: None,
        }
    }

    /// Construct an outbox failure receipt.
    #[must_use]
    pub fn outbox_failed(error: impl Into<String>) -> Self {
        Self {
            status: FusionPublicationStatus::OutboxFailed,
            error: Some(error.into()),
        }
    }

    /// Construct a storage failure receipt.
    #[must_use]
    pub fn storage_failure(error: impl Into<String>) -> Self {
        Self {
            status: FusionPublicationStatus::StorageFailure,
            error: Some(error.into()),
        }
    }

    /// Whether the receipt proves that the append landed.
    #[must_use]
    pub const fn is_published(&self) -> bool {
        matches!(self.status, FusionPublicationStatus::Published)
    }
}

/// Durable parent-session delivery item shared by the platform terminal
/// contract and app-tier session coordinator. The payload is already
/// sanitized and carries a deterministic message identity, so replay never
/// reruns Fusion or mints a new parent-dependent message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableFusionOutboxRecord {
    /// Stable delivery identity, normally derived from `(session, run)`.
    pub delivery_id: String,
    /// Destination session.
    pub session_id: protocol::SessionId,
    /// Deterministic transcript message UUID.
    pub message_uuid: String,
    /// Sanitized transcript payload.
    pub payload: serde_json::Value,
    /// Monotonic retry generation. A long-lived dead letter never wraps back
    /// onto an earlier event.
    pub attempt: u64,
    /// Inclusive last attempt in this durable local retry cycle.
    pub retry_cycle_end: u64,
    /// Last durable delivery receipt.
    pub receipt: FusionPublicationReceipt,
}

impl DurableFusionOutboxRecord {
    /// Return the next retry generation without wrapping its durable identity.
    #[must_use]
    pub const fn checked_next_attempt(&self) -> Option<u64> {
        self.attempt.checked_add(1)
    }
}

/// Durable terminal projection shared by Slash, Agent, and Workflow. The
/// optional outbox item is part of this same event, so terminal computation
/// and trusted Slash publication are acknowledged together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableFusionTerminalRecord {
    /// Stable event/run identity.
    pub event_id: String,
    /// Immutable run identity.
    pub identity: FusionRunIdentity,
    /// Computation result or terminal error.
    pub result: Result<FusionResult, FusionError>,
    /// Reliable accounting facts.
    pub facts: FusionRunFacts,
    /// Publication projection at terminal-record time.
    pub publication: FusionPublicationReceipt,
    /// Optional trusted Slash outbox item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outbox: Option<DurableFusionOutboxRecord>,
}

/// Compatibility alias used by callers that describe the field as a state
/// rather than a status. Keep both spellings available while the durable
/// outbox work remains a later package.
pub type FusionPublicationState = FusionPublicationStatus;

/// Progress stage names shared by Agent / slash / TUI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum FusionStage {
    /// Resolving the panel set.
    ResolvingModels,
    /// Reserving budget.
    ReservingBudget,
    /// Panels running.
    RunningPanels {
        /// Completed count.
        completed: u8,
        /// Started count.
        total: u8,
    },
    /// [round-3 review, findings 11/19] `panel::run_panels` has dispatched
    /// every panel task to the spawner — real provider calls are (or were,
    /// if they failed immediately) in flight — but none has reached a
    /// terminal outcome yet. Distinct from `RunningPanels { completed: 0,
    /// .. }`, which `run_panel_stage` emits BEFORE `run_panels` is even
    /// called (zero panel tasks exist yet): a consumer that needs to know
    /// "did a panel genuinely spawn" (e.g. `tools/agent`'s spawn-reservation
    /// accounting) cannot tell the two `completed: 0`-shaped moments apart
    /// without this separate signal.
    PanelsDispatched {
        /// Panel count dispatched.
        total: u8,
    },
    /// Analyst running.
    Analyzing,
    /// Pick path (no synth).
    Selecting,
    /// Merge synthesizer running.
    Synthesizing,
    /// Terminal success.
    Completed,
    /// Terminal needs-parent.
    NeedsParent,
    /// Terminal failure.
    Failed,
    /// Terminal cancel.
    Cancelled,
}

impl FusionStage {
    /// Fixed, human-readable label shared by every progress surface (Agent
    /// tool forwarder, `/fusion` task DTO, workflow bridge) per design §7's
    /// copy — the ONE place that copy is spelled, so every entrypoint that
    /// renders `FusionStage` renders the SAME words (F005).
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::ResolvingModels => "Resolving models".to_string(),
            Self::ReservingBudget => "Reserving budget".to_string(),
            Self::RunningPanels { completed, total } => {
                format!("Running panels {completed}/{total}")
            }
            // Deliberately the SAME text `RunningPanels { completed: 0, .. }`
            // renders — this is a distinct SIGNAL for consumers that need to
            // tell "about to spawn" from "genuinely dispatched" apart, not a
            // distinct user-visible progress state (F005).
            Self::PanelsDispatched { total } => format!("Running panels 0/{total}"),
            Self::Analyzing => "Analyzing reports".to_string(),
            Self::Selecting => "Selecting answer".to_string(),
            Self::Synthesizing => "Synthesizing answer".to_string(),
            Self::Completed => "Completed".to_string(),
            Self::NeedsParent => "Needs parent".to_string(),
            Self::Failed => "Failed".to_string(),
            Self::Cancelled => "Cancelled".to_string(),
        }
    }
}

/// One progress event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FusionProgress {
    /// Stage.
    pub stage: FusionStage,
    /// Anonymous panel id when the event is panel-scoped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panel_id: Option<String>,
    /// Short human line.
    pub message: String,
    /// Finding [12]: this run's realized output-token spend so far, when
    /// this event is emitted at a point the orchestrator has already priced
    /// and committed real usage (today: only the `check_panel_bar` failure
    /// path in `run_inner`, right before it returns `Err`). `None` on every
    /// other progress event — a caller that tracks a running token budget
    /// (e.g. `tasks::handlers::local_workflow`'s `fusion()` bridge arm) can
    /// charge this amount even when the overall call ends in `Err`, instead
    /// of treating an errored call as having spent nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub realized_output_tokens: Option<u64>,
    /// [Round-3 review B2, reworked] The provider profiles the request was
    /// ACTUALLY dispatched to — never the merely-intended/resolved set.
    /// `FusionOrchestrator::run_inner` latches this only once
    /// `run_panel_stage` returns, from panels that are NOT
    /// [`panel_never_dispatched`] (i.e. panels that made a real provider call
    /// — see `dispatched_egress_profiles`; as of round-5 item 8 and round-6
    /// B1 that excludes both `"spawn"` AND `"not_dispatched"`, not `"spawn"`
    /// alone), and folds in the analyst's profile
    /// only once the analyst call is actually issued
    /// (`run_analyst_call`) — never before dispatch happened. `None` on
    /// every progress event emitted before panel dispatch completes
    /// (including every preflight refusal, and a panel-bar failure where
    /// every panel was rejected pre-allocation, both of which guarantee
    /// zero provider calls) and on ordinary non-terminal progress events
    /// that carry no new information here. This is the privacy-relevant
    /// counterpart to `realized_output_tokens`: `tasks::handlers::local_fusion`
    /// latches the LAST `Some` value seen and uses it to fill
    /// `<egress-profiles>` in a failure's task notification instead of
    /// silently reporting no egress for a run that really sent the
    /// prompt to these providers — and, symmetrically, never claims egress
    /// to a provider the run never actually reached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress_profiles: Option<Vec<String>>,
    /// [Round-12 finding [3]] How many panels the SPAWNER has provably
    /// allocated a child for so far — `fusion::panel::PanelDispatch`'s
    /// `allocated` flags, set from `SubagentObservation::Allocated`.
    ///
    /// This is a strictly different question from every `total` on
    /// [`FusionStage`], which is the RESOLVED panel count: a panel the
    /// spawner rejects pre-allocation (`error_category: "spawn"`) is counted
    /// in `total` and is not counted here. The distinction is what
    /// `tools/agent`'s spawn-reservation accounting needs — its `Ok` arm
    /// filters those panels out of the charge via
    /// `fusion_panels_that_reached_the_spawner`, and without this field its
    /// `Err`/drop paths had no way to apply the same filter, so two
    /// terminations of an identical dispatch charged
    /// `CLAUDE_CODE_MAX_SUBAGENTS_PER_SESSION` differently.
    ///
    /// Monotonically non-decreasing across a run's events, so a consumer may
    /// take the max over everything it sees. `None` means "this emitter
    /// publishes no allocation figure" — never "zero allocated" — so a
    /// consumer must fall back to the resolved total rather than treat it as
    /// evidence of nothing spawning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panels_allocated: Option<u8>,
}

/// Fusion failure. Preflight variants guarantee zero provider calls.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FusionError {
    /// Agent/workflow gated off.
    #[error("fusion is disabled")]
    Disabled,
    /// Mobile / unsupported host.
    #[error("fusion is unavailable on this platform")]
    UnavailableOnPlatform,
    /// Settings failed validation.
    #[error("invalid fusion configuration: {0}")]
    InvalidConfiguration(String),
    /// Request failed validation.
    #[error("invalid fusion request: {0}")]
    InvalidRequest(String),
    /// Fewer than two usable models. Carries enough to point at the setting
    /// that would fix it (F011): credentials, `fusion.allowedProfiles`, or
    /// `cross_provider`.
    #[error(
        "too few fusion models: {eligible} eligible, {required} required (parent profile `{parent_profile}`, same_provider_only={same_provider_only}); check provider credentials, fusion.allowedProfiles, or pass cross_provider: true"
    )]
    TooFewModels {
        /// Models that passed hint/allowlist/provider filtering.
        eligible: usize,
        /// Minimum panel count that triggered this error.
        required: u8,
        /// Whether the request was restricted to the parent's provider.
        same_provider_only: bool,
        /// Requesting session's parent provider profile.
        parent_profile: String,
    },
    /// One or more model roles have not been configured. Fusion deliberately
    /// does not choose models on the operator's behalf: which models a run
    /// spends money on is a decision that belongs in the settings file, where
    /// it is visible and reviewable, not in a checked-in ranking table that can
    /// change under a working configuration.
    #[error(
        "fusion has no models configured for: {}; run `/fusion setup` or set {} in settings.json",
        .missing.iter().map(|role| role.label()).collect::<Vec<_>>().join(", "),
        .missing.iter().map(|role| role.setting_key()).collect::<Vec<_>>().join(", ")
    )]
    NotConfigured {
        /// Roles with no usable configuration, in `FusionModelRole::ALL` order.
        missing: Vec<FusionModelRole>,
    },
    /// Explicit `models` list is unusable.
    #[error("invalid custom fusion models: {0}")]
    InvalidCustomModels(String),
    /// Cross-provider was requested but not allowed.
    #[error("cross-provider fusion is not allowed")]
    CrossProviderDenied,
    /// No judge-eligible model with strict JSON schema. Same data shape as
    /// [`FusionError::TooFewModels`] (F011).
    #[error(
        "no fusion analyst model is available: {eligible} eligible, {required} required (parent profile `{parent_profile}`, same_provider_only={same_provider_only}); check provider credentials, fusion.allowedProfiles, or pass cross_provider: true"
    )]
    NoJudgeModel {
        /// Judge-eligible models that passed allowlist/provider filtering.
        eligible: usize,
        /// Minimum judge count (always 1) that triggered this error.
        required: u8,
        /// Whether the request was restricted to the parent's provider.
        same_provider_only: bool,
        /// Requesting session's parent provider profile.
        parent_profile: String,
    },
    /// Analyst route cannot emit constrained JSON.
    #[error("structured output is unsupported for the fusion analyst")]
    StructuredOutputUnsupported,
    /// Session has a max budget but reservation is unimplemented.
    #[error("fusion budget reservation is unavailable")]
    BudgetReservationUnavailable,
    /// Reservation would exceed the session cap.
    #[error("fusion budget exceeded")]
    BudgetExceeded,
    /// Session spawn cap cannot admit the panel batch.
    #[error("fusion spawn limit exceeded")]
    SpawnLimitExceeded,
    /// Whole-group capacity admission failed before any panel was spawned.
    #[error("fusion panel admission rejected: {0}")]
    PanelAdmissionRejected(String),
    /// Every panel failed.
    #[error("all fusion panels failed")]
    AllPanelsFailed,
    /// [round-3 review, finding 12] Every panel failed via a pre-allocation
    /// spawner rejection (e.g. `SubagentSpawnError::PoolFull`, or an
    /// unresolvable panel agent definition) — a distinct, STRONGER shape
    /// than [`Self::AllPanelsFailed`], which can also cover panels that made
    /// a real (unrecovered) provider call. Every panel here is guaranteed to
    /// have made ZERO provider calls, so — unlike the general
    /// `AllPanelsFailed` — this variant is preflight: it must not keep a
    /// caller's up-front spawn-slot reservation charged for subagents that
    /// never existed.
    #[error("all fusion panels failed before dispatch")]
    AllPanelsFailedPreflight,
    /// Successful panels below minSuccessfulPanels.
    #[error("fusion did not meet the minimum successful panel count")]
    MinPanelsNotMet,
    /// `partial_ok=false` and at least one panel was not successful.
    #[error("fusion panel set is incomplete")]
    PanelSetIncomplete,
    /// Total timeout with zero successful panels.
    #[error("fusion timed out before any panel completed")]
    TimedOutEmpty,
    /// Caller cancelled.
    #[error("fusion cancelled")]
    Cancelled,
    /// Internal error. User-facing text stays short; correlation is elsewhere.
    #[error("internal fusion error")]
    Internal,
}

/// Durable representation of [`FusionError`].
///
/// Serde's internally tagged enums cannot contain newtype variants. The public
/// error keeps string payloads for ergonomic callers, while this wire form
/// stores them as named `message` fields so terminal records remain durable.
#[derive(Serialize, Deserialize)]
#[serde(tag = "error", rename_all = "snake_case")]
enum FusionErrorWire {
    Disabled,
    UnavailableOnPlatform,
    InvalidConfiguration {
        message: String,
    },
    InvalidRequest {
        message: String,
    },
    TooFewModels {
        eligible: usize,
        required: u8,
        same_provider_only: bool,
        parent_profile: String,
    },
    NotConfigured {
        missing: Vec<FusionModelRole>,
    },
    InvalidCustomModels {
        message: String,
    },
    CrossProviderDenied,
    NoJudgeModel {
        eligible: usize,
        required: u8,
        same_provider_only: bool,
        parent_profile: String,
    },
    StructuredOutputUnsupported,
    BudgetReservationUnavailable,
    BudgetExceeded,
    SpawnLimitExceeded,
    PanelAdmissionRejected {
        message: String,
    },
    AllPanelsFailed,
    AllPanelsFailedPreflight,
    MinPanelsNotMet,
    PanelSetIncomplete,
    TimedOutEmpty,
    Cancelled,
    Internal,
}

impl From<FusionError> for FusionErrorWire {
    fn from(error: FusionError) -> Self {
        match error {
            FusionError::Disabled => Self::Disabled,
            FusionError::UnavailableOnPlatform => Self::UnavailableOnPlatform,
            FusionError::InvalidConfiguration(message) => Self::InvalidConfiguration { message },
            FusionError::InvalidRequest(message) => Self::InvalidRequest { message },
            FusionError::TooFewModels {
                eligible,
                required,
                same_provider_only,
                parent_profile,
            } => Self::TooFewModels {
                eligible,
                required,
                same_provider_only,
                parent_profile,
            },
            FusionError::NotConfigured { missing } => Self::NotConfigured { missing },
            FusionError::InvalidCustomModels(message) => Self::InvalidCustomModels { message },
            FusionError::CrossProviderDenied => Self::CrossProviderDenied,
            FusionError::NoJudgeModel {
                eligible,
                required,
                same_provider_only,
                parent_profile,
            } => Self::NoJudgeModel {
                eligible,
                required,
                same_provider_only,
                parent_profile,
            },
            FusionError::StructuredOutputUnsupported => Self::StructuredOutputUnsupported,
            FusionError::BudgetReservationUnavailable => Self::BudgetReservationUnavailable,
            FusionError::BudgetExceeded => Self::BudgetExceeded,
            FusionError::SpawnLimitExceeded => Self::SpawnLimitExceeded,
            FusionError::PanelAdmissionRejected(message) => {
                Self::PanelAdmissionRejected { message }
            }
            FusionError::AllPanelsFailed => Self::AllPanelsFailed,
            FusionError::AllPanelsFailedPreflight => Self::AllPanelsFailedPreflight,
            FusionError::MinPanelsNotMet => Self::MinPanelsNotMet,
            FusionError::PanelSetIncomplete => Self::PanelSetIncomplete,
            FusionError::TimedOutEmpty => Self::TimedOutEmpty,
            FusionError::Cancelled => Self::Cancelled,
            FusionError::Internal => Self::Internal,
        }
    }
}

impl From<FusionErrorWire> for FusionError {
    fn from(error: FusionErrorWire) -> Self {
        match error {
            FusionErrorWire::Disabled => Self::Disabled,
            FusionErrorWire::UnavailableOnPlatform => Self::UnavailableOnPlatform,
            FusionErrorWire::InvalidConfiguration { message } => {
                Self::InvalidConfiguration(message)
            }
            FusionErrorWire::InvalidRequest { message } => Self::InvalidRequest(message),
            FusionErrorWire::TooFewModels {
                eligible,
                required,
                same_provider_only,
                parent_profile,
            } => Self::TooFewModels {
                eligible,
                required,
                same_provider_only,
                parent_profile,
            },
            FusionErrorWire::NotConfigured { missing } => Self::NotConfigured { missing },
            FusionErrorWire::InvalidCustomModels { message } => Self::InvalidCustomModels(message),
            FusionErrorWire::CrossProviderDenied => Self::CrossProviderDenied,
            FusionErrorWire::NoJudgeModel {
                eligible,
                required,
                same_provider_only,
                parent_profile,
            } => Self::NoJudgeModel {
                eligible,
                required,
                same_provider_only,
                parent_profile,
            },
            FusionErrorWire::StructuredOutputUnsupported => Self::StructuredOutputUnsupported,
            FusionErrorWire::BudgetReservationUnavailable => Self::BudgetReservationUnavailable,
            FusionErrorWire::BudgetExceeded => Self::BudgetExceeded,
            FusionErrorWire::SpawnLimitExceeded => Self::SpawnLimitExceeded,
            FusionErrorWire::PanelAdmissionRejected { message } => {
                Self::PanelAdmissionRejected(message)
            }
            FusionErrorWire::AllPanelsFailed => Self::AllPanelsFailed,
            FusionErrorWire::AllPanelsFailedPreflight => Self::AllPanelsFailedPreflight,
            FusionErrorWire::MinPanelsNotMet => Self::MinPanelsNotMet,
            FusionErrorWire::PanelSetIncomplete => Self::PanelSetIncomplete,
            FusionErrorWire::TimedOutEmpty => Self::TimedOutEmpty,
            FusionErrorWire::Cancelled => Self::Cancelled,
            FusionErrorWire::Internal => Self::Internal,
        }
    }
}

impl Serialize for FusionError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        FusionErrorWire::from(self.clone()).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for FusionError {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(FusionErrorWire::deserialize(deserializer)?.into())
    }
}

impl FusionError {
    /// Whether this error's contract proves that no provider request could
    /// have started. Callers must not infer zero from every error: timeout,
    /// cancellation, and internal failures deliberately remain unknown.
    #[must_use]
    pub const fn guarantees_zero_provider_calls(&self) -> bool {
        matches!(
            self,
            Self::Disabled
                | Self::UnavailableOnPlatform
                | Self::InvalidConfiguration(_)
                | Self::InvalidRequest(_)
                | Self::TooFewModels { .. }
                | Self::NotConfigured { .. }
                | Self::InvalidCustomModels(_)
                | Self::CrossProviderDenied
                | Self::NoJudgeModel { .. }
                | Self::StructuredOutputUnsupported
                | Self::BudgetReservationUnavailable
                | Self::BudgetExceeded
                | Self::SpawnLimitExceeded
                | Self::PanelAdmissionRejected(_)
                | Self::AllPanelsFailedPreflight
        )
    }
}

/// Checked-in quality / latency / cost hints for automatic panel selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct FusionModelHints {
    /// Participate in automatic quality/fast presets.
    #[serde(default)]
    pub eligible: bool,
    /// Higher is better. Only compared among eligible models.
    #[serde(default)]
    pub quality_rank: u16,
    /// Latency class.
    #[serde(default)]
    pub latency_class: FusionLatencyClass,
    /// Cost class.
    #[serde(default)]
    pub cost_class: FusionCostClass,
    /// May serve as the analyst (still requires structured output).
    #[serde(default)]
    pub judge_eligible: bool,
}

/// Coarse latency band. Ord is slowest-last so Fast sorts before Slow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FusionLatencyClass {
    /// Fastest band.
    Instant,
    /// Fast agentic turns.
    Fast,
    /// Default.
    #[default]
    Standard,
    /// Slow / high-reasoning.
    Slow,
}

/// Coarse cost band.
///
/// Deliberately does NOT derive `Ord`/`PartialOrd` (F011/G001-adjacent review
/// finding): declaration order `Low < Medium < High < Subscription < Unknown`
/// contradicts this type's own "cheaper-first" intent — a $0-marginal
/// subscription route would lose a tie-break to a per-token `High` route.
/// Use [`FusionCostClass::rank`] for cheapest-first comparisons instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FusionCostClass {
    /// Subscription (dollar reserve may be 0) — cheapest by construction.
    Subscription,
    /// Cheapest token-billed band.
    Low,
    /// Mid.
    #[default]
    Medium,
    /// Expensive token pricing.
    High,
    /// Unknown pricing.
    Unknown,
}

impl FusionCostClass {
    /// Explicit cheapest-first rank. Lower sorts first. `Subscription` is 0
    /// (cheapest) regardless of declaration/serde order.
    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            Self::Subscription => 0,
            Self::Low => 1,
            Self::Medium => 2,
            Self::High => 3,
            Self::Unknown => 4,
        }
    }
}

impl PartialOrd for FusionCostClass {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for FusionCostClass {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.rank().cmp(&other.rank())
    }
}

/// Snapshot the Agent tool needs to list / authorize Fusion without depending
/// on the `fusion` crate.
// A flat capability snapshot mirroring `FusionRuntimeConfig`'s independent
// toggles; not a state machine, so enum-izing the bools would only add
// indirection at the tool boundary.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FusionAgentSurface {
    /// When true, `fusion` is in the Agent listing and `subagent_type: "fusion"`
    /// is accepted.
    pub enabled: bool,
    /// Agent may request `cross_provider: true`.
    pub allow_cross_provider: bool,
    /// Default preset when the tool input omits `preset`.
    pub default_preset: FusionPreset,
    /// Default `partial_ok` when omitted.
    pub default_partial_ok: bool,
    /// Quality preset panel count.
    pub quality_panel_count: u8,
    /// Fast preset panel count.
    pub fast_panel_count: u8,
    /// Hard panel cap.
    pub max_panel: u8,
    /// `/fusion` default when neither `--same-provider` nor `--cross-provider`
    /// is passed. `true` allows prompt data to leave the parent provider.
    pub slash_cross_provider_default: bool,
}

impl Default for FusionAgentSurface {
    fn default() -> Self {
        Self {
            enabled: false,
            allow_cross_provider: false,
            default_preset: FusionPreset::Quality,
            default_partial_ok: true,
            quality_panel_count: 3,
            fast_panel_count: 2,
            max_panel: FUSION_MAX_PANEL,
            slash_cross_provider_default: true,
        }
    }
}

/// Build a prepared run around a single async body that produces the whole
/// result at once.
///
/// Preparation for such an executor has nothing to resolve, so the body is
/// simply deferred until activation and its result is folded into the run's
/// facts: usage, timing, attempt count and confirmed egress on success, and a
/// proven-zero marker for an error that guarantees no provider was reached.
/// The prepared identity is authoritative, so a body that mints its own run id
/// does not get to keep it.
///
/// This exists for test doubles and other executors with no routing of their
/// own. A production executor resolves a route and owns its own supervisor,
/// and builds its [`PreparedFusionRun`] directly.
pub fn prepared_from_oneshot<F, Fut>(
    submission: FusionSubmission,
    effective_timeout_ms: Option<u64>,
    run: F,
) -> Result<PreparedFusionRun, FusionError>
where
    F: FnOnce(FusionRequest, FusionInheritance, Option<Sender<FusionProgress>>) -> Fut
        + Send
        + 'static,
    Fut: std::future::Future<Output = Result<FusionResult, FusionError>> + Send + 'static,
{
    let FusionSubmission {
        request,
        inherit,
        identity,
    } = FusionSubmission::new(submission.request, submission.inherit, submission.identity)?;
    let duration_ms = inherit
        .effective_timeout_ms
        .or(effective_timeout_ms)
        .unwrap_or_default();
    let summary = FusionPreparedSummary {
        identity: identity.clone(),
        duration_ms,
        planned_panels: None,
    };
    let control = FusionRunControl::new(
        identity.clone(),
        duration_ms,
        inherit.cancel.clone(),
        FusionRunFactsRecorder::default(),
    );
    let facts_control = control.clone();
    Ok(PreparedFusionRun::new(
        summary,
        control,
        move |_activation, progress| {
            let facts_control = facts_control.clone();
            async move {
                let mut result = run(request, inherit, progress).await;
                if let Ok(result) = &mut result {
                    result.run_id = identity.run_id.to_string();
                    let facts = facts_control.facts();
                    facts.replace_usage(result.usage.clone(), result.usage.estimated);
                    facts.set_timing(result.timing.clone());
                    facts.set_attempts(result.usage.provider_requests);
                    for profile in &result.egress_profiles {
                        facts.add_confirmed_egress(profile.clone());
                    }
                } else if result
                    .as_ref()
                    .err()
                    .is_some_and(FusionError::guarantees_zero_provider_calls)
                {
                    facts_control.facts().set_known_zero();
                }
                FusionRunOutcome::from_control(&facts_control, result)
            }
        },
    ))
}

/// Executor implemented by the `fusion` crate and injected at the composition root.
#[async_trait]
pub trait FusionExecutor: Send + Sync + 'static {
    /// Prepare one immutable route/config/identity handoff.
    ///
    /// This is the only entrypoint. Preparation is pure: it resolves the
    /// route and mints the run's control and facts without dispatching, and
    /// the returned handle is what actually starts work. A test double with
    /// nothing to prepare can build its handle with
    /// [`prepared_from_oneshot`].
    ///
    /// [`FusionStatus::NeedsParent`] is carried as a completed outcome, not
    /// as an error.
    fn prepare(
        self: Arc<Self>,
        submission: FusionSubmission,
    ) -> Result<PreparedFusionRun, FusionError>;

    /// Return the effective end-to-end timeout for a newly spawned run, in
    /// milliseconds, when the host can expose one without starting work.
    ///
    /// Task/CLI surfaces snapshot this value before publishing a
    /// `local_fusion` row so print mode can wait for the configured run rather
    /// than duplicating the Fusion default. `None` keeps lightweight/test
    /// executors source-compatible; hosts that return `Some` must use the
    /// same value for the run's outer deadline (including any internal grace
    /// handling) or leave it `None` until that runtime snapshot is available.
    fn effective_timeout_ms(&self) -> Option<u64> {
        None
    }

    /// Trusted per-workflow Fusion concurrency, sampled at the batch boundary.
    /// Values above one require metered prepared runs whose every physical
    /// request atomically reserves money and the original output account before
    /// dispatch. A scope or billing-mode label alone is not this capability.
    /// Legacy and unqualified hosts remain sequential; callers hard-cap at two.
    fn workflow_batch_concurrency(&self) -> usize {
        1
    }

    /// Agent listing / intercept gate. Default is disabled (inert).
    fn agent_surface(&self) -> FusionAgentSurface {
        FusionAgentSurface::default()
    }

    /// A boot-time (or otherwise pinned) failure that makes every `run()`
    /// call fail identically, checked BEFORE the `agent_surface().enabled`
    /// gate (F008). Lets a composition root that rejected an invalid
    /// `fusion.*` value (see `RejectedFusionExecutor`) surface the real
    /// [`FusionError::InvalidConfiguration`] through the Agent tool and the
    /// workflow bridge, instead of both falling back to `enabled: false`'s
    /// generic "not found"/`Disabled` message. Default `None` — an executor
    /// that never pins a rejection is unaffected.
    fn preflight_error(&self) -> Option<FusionError> {
        None
    }

    /// Resolve the provider profile for a parent model.
    ///
    /// The default accepts only an explicit non-empty profile. Production
    /// executors may fall back to their live model catalog for resumed sessions
    /// whose persisted selection contains only a bare model id.
    fn resolve_parent_profile(
        &self,
        _parent_model: &str,
        explicit_profile: Option<&str>,
    ) -> Option<String> {
        explicit_profile
            .map(str::trim)
            .filter(|profile| !profile.is_empty())
            .map(str::to_string)
    }

    /// Workflow-global `fusion()` call cap for one workflow run.
    ///
    /// Hosts may return a lower value, but callers must still enforce the
    /// global hard ceiling of 20.
    fn workflow_fusion_call_cap(&self) -> u32 {
        FUSION_WORKFLOW_CALL_CAP_HARD_LIMIT
    }
}

/// Parent-conversation sink for a finished Fusion run.
///
/// Failures here must not rewrite the Fusion task's terminal status.
#[async_trait]
pub trait FusionCompletionSink: Send + Sync {
    /// Publish one sanitized Fusion result. Implementations must be idempotent
    /// on `(conversation_id, run_id)` and return a truthful receipt. In
    /// particular, a sink must return [`FusionPublicationStatus::Published`]
    /// only after the append is durable; failures must not be represented as
    /// an empty successful response.
    async fn publish(
        &self,
        conversation_id: &str,
        result: &FusionResult,
    ) -> FusionPublicationReceipt;
}

/// Test / unwired sink.
pub struct NoopFusionCompletionSink;

#[async_trait]
impl FusionCompletionSink for NoopFusionCompletionSink {
    async fn publish(
        &self,
        _conversation_id: &str,
        _result: &FusionResult,
    ) -> FusionPublicationReceipt {
        // A no-op sink is useful in standalone/unit-test hosts, but it never
        // proves that a parent transcript was written.
        FusionPublicationReceipt::not_required()
    }
}

/// Normalize and validate dimension names.
///
/// # Errors
///
/// Returns [`FusionError::InvalidRequest`] when the list is empty, longer than
/// 12, not unique `snake_case`, or uses a reserved identity-like name.
pub fn normalize_dimensions(raw: Vec<String>) -> Result<Vec<String>, FusionError> {
    if raw.is_empty() {
        return Ok(DEFAULT_FUSION_DIMENSIONS
            .iter()
            .map(|s| (*s).to_string())
            .collect());
    }
    if raw.len() > 12 {
        return Err(FusionError::InvalidRequest(
            "dimensions must have at most 12 entries".into(),
        ));
    }
    let mut out = Vec::with_capacity(raw.len());
    for dim in raw {
        if !is_snake_case_dimension(&dim) {
            return Err(FusionError::InvalidRequest(format!(
                "dimension `{dim}` must be lowercase snake_case"
            )));
        }
        if is_reserved_dimension(&dim) {
            return Err(FusionError::InvalidRequest(format!(
                "dimension `{dim}` cannot be a provider, model, or panel identity"
            )));
        }
        if !out.iter().any(|existing| existing == &dim) {
            out.push(dim);
        }
    }
    if out.is_empty() {
        return Err(FusionError::InvalidRequest(
            "dimensions must have at least 1 entry".into(),
        ));
    }
    Ok(out)
}

fn is_snake_case_dimension(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    let mut prev_underscore = false;
    for c in chars {
        if c == '_' {
            if prev_underscore {
                return false;
            }
            prev_underscore = true;
            continue;
        }
        prev_underscore = false;
        if !c.is_ascii_lowercase() && !c.is_ascii_digit() {
            return false;
        }
    }
    !prev_underscore
}

fn is_reserved_dimension(s: &str) -> bool {
    matches!(
        s,
        "provider" | "model" | "profile" | "panel_id" | "panel" | "identity"
    ) || (s.starts_with('p')
        && s.len() > 1
        && s.as_bytes()[1].is_ascii_digit()
        && s.bytes().skip(1).all(|b| b.is_ascii_digit()))
}

/// Host-side `PanelReport` checks (dangling refs, ranges, uniqueness).
///
/// # Errors
///
/// Returns [`FusionError::InvalidRequest`] describing the first protocol fault.
pub fn validate_panel_report(report: &PanelReport) -> Result<(), FusionError> {
    if report.confidence_out_of_range() {
        return Err(FusionError::InvalidRequest(
            "panel claim confidence must be 0..=100".into(),
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    for ev in &report.evidence {
        if !seen.insert(ev.id.as_str()) {
            return Err(FusionError::InvalidRequest(format!(
                "duplicate evidence id `{}`",
                ev.id
            )));
        }
    }
    for claim in &report.claims {
        if claim.confidence > 100 {
            return Err(FusionError::InvalidRequest(
                "panel claim confidence must be 0..=100".into(),
            ));
        }
        for id in &claim.evidence_refs {
            if !seen.contains(id.as_str()) {
                return Err(FusionError::InvalidRequest(format!(
                    "claim evidence_ref `{id}` does not exist"
                )));
            }
        }
    }
    Ok(())
}

impl PanelReport {
    fn confidence_out_of_range(&self) -> bool {
        self.claims.iter().any(|c| c.confidence > 100)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ProbeTerminalRecorder {
        calls: AtomicUsize,
        seen: std::sync::Mutex<
            Option<
                tokio::sync::oneshot::Sender<(
                    FusionRunOutcome,
                    Option<FusionSlashPublicationTarget>,
                )>,
            >,
        >,
        release: Option<Arc<tokio::sync::Semaphore>>,
        panic_after_observation: bool,
        receipt: FusionPublicationReceipt,
    }

    impl ProbeTerminalRecorder {
        fn new(
            blocked: bool,
            panic_after_observation: bool,
            receipt: FusionPublicationReceipt,
        ) -> (
            Arc<Self>,
            tokio::sync::oneshot::Receiver<(
                FusionRunOutcome,
                Option<FusionSlashPublicationTarget>,
            )>,
            Option<Arc<tokio::sync::Semaphore>>,
        ) {
            let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
            let release = blocked.then(|| Arc::new(tokio::sync::Semaphore::new(0)));
            (
                Arc::new(Self {
                    calls: AtomicUsize::new(0),
                    seen: std::sync::Mutex::new(Some(seen_tx)),
                    release: release.clone(),
                    panic_after_observation,
                    receipt,
                }),
                seen_rx,
                release,
            )
        }
    }

    #[async_trait]
    impl FusionRunRecorder for ProbeTerminalRecorder {
        async fn record_terminal(
            &self,
            outcome: FusionRunOutcome,
            slash_target: Option<FusionSlashPublicationTarget>,
        ) -> FusionPublicationReceipt {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(seen) = self
                .seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                let _ = seen.send((outcome, slash_target));
            }
            assert!(!self.panic_after_observation, "terminal recorder panic");
            if let Some(release) = self.release.as_ref() {
                release
                    .acquire()
                    .await
                    .expect("test terminal recorder semaphore remains open")
                    .forget();
            }
            self.receipt.clone()
        }
    }

    fn terminal_test_control(
        origin: FusionOrigin,
    ) -> (protocol::SessionId, FusionRunControl, FusionPreparedSummary) {
        let session_id = protocol::SessionId::new();
        let identity = FusionRunIdentity::new(
            FusionRunId::generated(),
            Some(session_id),
            origin,
            (origin == FusionOrigin::Workflow).then(|| "workflow-test".to_string()),
        );
        let control = FusionRunControl::new(
            identity.clone(),
            1_000,
            CancellationToken::new(),
            FusionRunFactsRecorder::default(),
        );
        let summary = FusionPreparedSummary {
            identity,
            duration_ms: 1_000,
            planned_panels: Some(1),
        };
        (session_id, control, summary)
    }

    fn terminal_test_result(run_id: &FusionRunId) -> FusionResult {
        FusionResult {
            schema_version: FUSION_SCHEMA_VERSION,
            run_id: run_id.to_string(),
            status: FusionStatus::Completed,
            decision: FusionDecision::Merged,
            final_text: "sealed".into(),
            analysis: None,
            panels: Vec::new(),
            usage: FusionUsage::default(),
            timing: FusionTiming::default(),
            egress_profiles: Vec::new(),
        }
    }

    fn terminal_test_capability(
        recorder: Arc<ProbeTerminalRecorder>,
        origin: FusionOrigin,
        session_id: protocol::SessionId,
    ) -> FusionTerminalCapability {
        let capability = FusionTerminalCapability::new(recorder);
        if origin == FusionOrigin::Slash {
            capability.with_slash_target(FusionSlashPublicationTarget { session_id })
        } else {
            capability
        }
    }

    /// F011: `Subscription` (a $0-marginal route) must rank cheapest,
    /// contradicting the OLD derived-`Ord` declaration order
    /// `Low < Medium < High < Subscription < Unknown`, which sorted a
    /// per-token `High` route ahead of a free subscription tie-break.
    #[test]
    fn cost_class_rank_is_cheaper_first_with_subscription_at_zero() {
        assert_eq!(FusionCostClass::Subscription.rank(), 0);
        assert!(FusionCostClass::Subscription.rank() < FusionCostClass::Low.rank());
        assert!(FusionCostClass::Low.rank() < FusionCostClass::Medium.rank());
        assert!(FusionCostClass::Medium.rank() < FusionCostClass::High.rank());
        assert!(FusionCostClass::High.rank() < FusionCostClass::Unknown.rank());
        // `Ord` must agree with `rank()` (it's implemented via rank()) — this
        // is the actual regression guard for any `.cmp()`/`.sort_by()` call
        // site that still relies on derived-looking `Ord` semantics.
        assert!(FusionCostClass::Subscription < FusionCostClass::High);
    }

    #[test]
    fn schema_version_defaults_on_request() {
        let json = serde_json::json!({
            "origin": "agent",
            "prompt": "review this",
            "preset": "quality",
            "dimensions": ["coverage"],
            "partial_ok": true,
            "cross_provider": false,
            "parent_profile": "anthropic",
            "parent_model": "claude-sonnet-5"
        });
        let req: FusionRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.schema_version, FUSION_SCHEMA_VERSION);
    }

    #[test]
    fn request_rejects_unknown_fields() {
        let json = serde_json::json!({
            "origin": "agent",
            "prompt": "x",
            "preset": "fast",
            "dimensions": ["coverage"],
            "partial_ok": true,
            "cross_provider": false,
            "parent_profile": "anthropic",
            "parent_model": "claude-sonnet-5",
            "unexpected": true
        });
        let err = serde_json::from_value::<FusionRequest>(json).unwrap_err();
        assert!(err.to_string().contains("unexpected"));
    }

    #[test]
    fn result_ignores_unknown_optional_fields() {
        let json = serde_json::json!({
            "schema_version": 1,
            "run_id": "fu_test",
            "status": "completed",
            "decision": {"type": "merged"},
            "final_text": "ok",
            "future_field": {"nested": 1}
        });
        let result: FusionResult = serde_json::from_value(json).unwrap();
        assert_eq!(result.final_text, "ok");
        assert_eq!(result.status, FusionStatus::Completed);
    }

    #[test]
    fn publication_receipt_roundtrips_and_only_published_is_durable() {
        let receipts = [
            (FusionPublicationReceipt::not_required(), "not_required"),
            (FusionPublicationReceipt::pending(), "pending"),
            (FusionPublicationReceipt::queued(), "queued"),
            (FusionPublicationReceipt::published(), "published"),
            (
                FusionPublicationReceipt::outbox_failed("queue unavailable"),
                "outbox_failed",
            ),
            (
                FusionPublicationReceipt::storage_failure("append failed"),
                "storage_failure",
            ),
        ];
        for (receipt, wire_status) in receipts {
            let json = serde_json::to_value(&receipt).unwrap();
            assert_eq!(json["status"], serde_json::json!(wire_status));
            let back: FusionPublicationReceipt = serde_json::from_value(json).unwrap();
            assert_eq!(back, receipt);
            assert_eq!(
                back.is_published(),
                back.status == FusionPublicationStatus::Published
            );
        }
    }

    #[tokio::test]
    async fn noop_completion_sink_never_claims_published() {
        let result: FusionResult = serde_json::from_value(serde_json::json!({
            "run_id": "fu_noop",
            "status": "completed",
            "decision": {"type": "merged"},
            "final_text": "answer"
        }))
        .unwrap();
        let receipt = NoopFusionCompletionSink
            .publish("conversation", &result)
            .await;
        assert_eq!(receipt.status, FusionPublicationStatus::NotRequired);
        assert!(!receipt.is_published());
    }

    /// G011: `PanelOutcome::error_detail` is additive — a result serialized
    /// before this field existed must still deserialize (defaulting to
    /// `None`), and a populated value must round-trip byte-exact.
    #[test]
    fn panel_outcome_error_detail_is_additive_and_round_trips() {
        let pre_existing_json = serde_json::json!({
            "panel_id": "P1",
            "status": "failed",
            "duration_ms": 12,
            "error_category": "provider"
        });
        let outcome: PanelOutcome = serde_json::from_value(pre_existing_json).unwrap();
        assert_eq!(outcome.error_detail, None);

        let with_detail = PanelOutcome {
            panel_id: "P1".into(),
            status: PanelRunStatus::Failed,
            duration_ms: 12,
            error_category: Some("provider".into()),
            error_detail: Some("rate limited by upstream".into()),
            usage: None,
        };
        let json = serde_json::to_value(&with_detail).unwrap();
        assert_eq!(
            json.get("error_detail").and_then(|v| v.as_str()),
            Some("rate limited by upstream")
        );
        let back: PanelOutcome = serde_json::from_value(json).unwrap();
        assert_eq!(back, with_detail);
    }

    #[test]
    fn preset_from_str_accepts_the_two_wire_spellings_and_rejects_others() {
        assert_eq!(
            "quality".parse::<FusionPreset>().unwrap(),
            FusionPreset::Quality
        );
        assert_eq!("fast".parse::<FusionPreset>().unwrap(), FusionPreset::Fast);
        let err = "sloppy".parse::<FusionPreset>().unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid fusion request: fusion preset `sloppy` must be quality or fast"
        );
    }

    #[test]
    fn parse_fusion_model_ref_rejects_a_colon_with_an_empty_model() {
        // The Agent-tool bug this closes: `split_once(':')` on "openai:" used
        // to fall through to `(None, "openai:")`, silently treating the whole
        // literal (including the trailing colon) as a bare model id instead
        // of rejecting the malformed `profile:` entry.
        let err = parse_fusion_model_ref("openai:").unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid fusion request: invalid fusion models entry `openai:`"
        );
    }

    #[test]
    fn parse_fusion_model_ref_accepts_profile_colon_model_and_bare_model() {
        assert_eq!(
            parse_fusion_model_ref("anthropic:claude-sonnet-5").unwrap(),
            FusionModelRef {
                profile: Some("anthropic".into()),
                model: "claude-sonnet-5".into(),
            }
        );
        assert_eq!(
            parse_fusion_model_ref("claude-sonnet-5").unwrap(),
            FusionModelRef {
                profile: None,
                model: "claude-sonnet-5".into(),
            }
        );
    }

    #[test]
    fn parse_fusion_models_stops_at_the_first_bad_entry() {
        let err = parse_fusion_models(&["anthropic:opus".to_string(), "openai:".to_string()])
            .unwrap_err();
        assert!(err.to_string().contains("invalid fusion models entry"));
    }

    #[test]
    fn workflow_fusion_call_cap_default_matches_the_single_hard_limit_constant() {
        struct DefaultCapExecutor;
        #[async_trait::async_trait]
        impl FusionExecutor for DefaultCapExecutor {
            fn prepare(
                self: ::std::sync::Arc<Self>,
                submission: crate::FusionSubmission,
            ) -> Result<crate::PreparedFusionRun, crate::FusionError> {
                let timeout = self.effective_timeout_ms();
                crate::prepared_from_oneshot(
                    submission,
                    timeout,
                    move |_request, _inherit, _progress| async move { unimplemented!() },
                )
            }
        }
        assert_eq!(
            DefaultCapExecutor.workflow_fusion_call_cap(),
            FUSION_WORKFLOW_CALL_CAP_HARD_LIMIT
        );
    }

    #[test]
    fn recommendation_roundtrips() {
        let rec = FusionRecommendation::Pick {
            panel_id: "P2".into(),
            reason: "stronger evidence".into(),
        };
        let json = serde_json::to_value(&rec).unwrap();
        let back: FusionRecommendation = serde_json::from_value(json).unwrap();
        assert_eq!(rec, back);
    }

    #[test]
    fn empty_dimensions_become_defaults() {
        let dims = normalize_dimensions(Vec::new()).unwrap();
        assert_eq!(
            dims,
            DEFAULT_FUSION_DIMENSIONS
                .iter()
                .map(|s| (*s).to_string())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn dimensions_dedup_preserve_order() {
        let dims = normalize_dimensions(vec![
            "coverage".into(),
            "reasoning".into(),
            "coverage".into(),
        ])
        .unwrap();
        assert_eq!(dims, vec!["coverage", "reasoning"]);
    }

    #[test]
    fn dimensions_reject_too_many() {
        let raw = (0..13).map(|i| format!("dim_{i}")).collect();
        assert!(matches!(
            normalize_dimensions(raw),
            Err(FusionError::InvalidRequest(_))
        ));
    }

    #[test]
    fn string_payload_fusion_errors_use_a_durable_internal_tag_shape() {
        for error in [
            FusionError::InvalidConfiguration("invalid configuration".into()),
            FusionError::InvalidRequest("invalid request".into()),
            FusionError::InvalidCustomModels("invalid models".into()),
            FusionError::PanelAdmissionRejected("admission rejected".into()),
        ] {
            let encoded =
                serde_json::to_value(&error).expect("string-payload Fusion error must serialize");
            assert!(encoded.get("message").is_some(), "encoded: {encoded}");
            assert_eq!(
                serde_json::from_value::<FusionError>(encoded).expect("error must deserialize"),
                error
            );
        }
    }

    #[test]
    fn dimensions_require_snake_case() {
        assert!(normalize_dimensions(vec!["EvidenceQuality".into()]).is_err());
        assert!(normalize_dimensions(vec!["evidence-quality".into()]).is_err());
        assert!(normalize_dimensions(vec!["_coverage".into()]).is_err());
        assert!(normalize_dimensions(vec!["coverage_".into()]).is_err());
    }

    #[test]
    fn dimensions_reject_panel_identity() {
        assert!(normalize_dimensions(vec!["p1".into()]).is_err());
        assert!(normalize_dimensions(vec!["provider".into()]).is_err());
        assert!(normalize_dimensions(vec!["model".into()]).is_err());
    }

    #[test]
    fn panel_report_rejects_dangling_and_duplicate_evidence() {
        let mut report = PanelReport {
            schema_version: 1,
            summary: "s".into(),
            candidate_answer: "a".into(),
            claims: vec![PanelClaim {
                statement: "x".into(),
                evidence_refs: vec!["e1".into()],
                confidence: 80,
            }],
            evidence: vec![
                PanelEvidence {
                    id: "e1".into(),
                    kind: EvidenceKind::File,
                    locator: "src/a.rs".into(),
                    excerpt: None,
                },
                PanelEvidence {
                    id: "e1".into(),
                    kind: EvidenceKind::File,
                    locator: "src/b.rs".into(),
                    excerpt: None,
                },
            ],
            assumptions: vec![],
            risks: vec![],
            unresolved_questions: vec![],
        };
        assert!(validate_panel_report(&report).is_err());
        report.evidence.pop();
        assert!(validate_panel_report(&report).is_ok());
        report.claims[0].evidence_refs = vec!["missing".into()];
        assert!(validate_panel_report(&report).is_err());
        report.claims[0].evidence_refs = vec!["e1".into()];
        report.claims[0].confidence = 101;
        assert!(validate_panel_report(&report).is_err());
    }

    #[test]
    fn trait_is_object_safe() {
        let _: Option<Arc<dyn FusionExecutor>> = None;
    }

    #[test]
    fn run_id_deserialization_keeps_the_constructor_validation() {
        let valid = serde_json::json!("fu_0123456789abcdef0123456789abcdef");
        let parsed: FusionRunId = serde_json::from_value(valid).unwrap();
        assert_eq!(parsed.as_str(), "fu_0123456789abcdef0123456789abcdef");
        let invalid = serde_json::from_value::<FusionRunId>(serde_json::json!("fu_not-a-run"));
        assert!(invalid.is_err());
    }

    #[test]
    fn supervisor_result_claim_atomically_preserves_the_winning_owner() {
        for origin in [
            FusionOrigin::Agent,
            FusionOrigin::Slash,
            FusionOrigin::Workflow,
        ] {
            let (_session_id, cancelled, _) = terminal_test_control(origin);
            assert!(cancelled.activate_at(Instant::now()));
            assert!(cancelled.request_cancel());
            let ignored_success = terminal_test_result(&cancelled.identity().run_id);
            assert_eq!(
                cancelled.claim_supervisor_result(Ok(ignored_success)),
                Err(FusionError::Cancelled),
                "a cancellation claim that wins the phase lock is authoritative for {origin:?}"
            );

            let (_session_id, natural, _) = terminal_test_control(origin);
            assert!(natural.activate_at(Instant::now()));
            let success = terminal_test_result(&natural.identity().run_id);
            assert_eq!(
                natural.claim_supervisor_result(Ok(success.clone())),
                Ok(success)
            );
            assert!(natural.is_finalizing());
            assert!(
                !natural.request_cancel(),
                "cancellation cannot steal the natural claim for {origin:?}"
            );
        }
    }

    #[tokio::test]
    async fn every_origin_records_one_immutable_candidate_before_terminal_visibility() {
        for origin in [
            FusionOrigin::Agent,
            FusionOrigin::Slash,
            FusionOrigin::Workflow,
        ] {
            let (session_id, control, summary) = terminal_test_control(origin);
            let (recorder, seen_rx, release) =
                ProbeTerminalRecorder::new(true, false, FusionPublicationReceipt::queued());
            let capability = terminal_test_capability(recorder.clone(), origin, session_id);
            let runner_control = control.clone();
            let prepared =
                PreparedFusionRun::new(summary, control.clone(), move |_, _| async move {
                    runner_control.facts().set_allocated_panels(1);
                    FusionRunOutcome::from_control(
                        &runner_control,
                        Ok(terminal_test_result(&runner_control.identity().run_id)),
                    )
                })
                .with_terminal_capability(capability);
            let waiter = tokio::spawn(prepared.activate(FusionActivation::now(), None));

            let (recorded, slash_target) = tokio::time::timeout(Duration::from_secs(1), seen_rx)
                .await
                .expect("common recorder must be reached")
                .expect("common recorder observation must be retained");
            assert_eq!(control.terminal_outcome(), None);
            assert!(control.is_finalizing());
            assert!(!control.request_cancel());
            assert_eq!(recorded.facts.allocated_panels, Some(1));
            assert_eq!(
                slash_target.map(|target| target.session_id),
                (origin == FusionOrigin::Slash).then_some(session_id)
            );

            // A late mutation cannot alter the exact candidate already handed
            // to durable recording.
            control.facts().set_allocated_panels(9);
            release.expect("this recorder is blocked").add_permits(1);
            let visible = waiter.await.expect("activation waiter must join");
            assert_eq!(visible.identity, recorded.identity);
            assert_eq!(visible.result, recorded.result);
            assert_eq!(visible.facts, recorded.facts);
            assert_eq!(visible.facts.allocated_panels, Some(1));
            assert_eq!(visible.publication, FusionPublicationReceipt::queued());
            assert_eq!(recorder.calls.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn dropping_without_a_recorder_seals_exact_zero_synchronously_without_a_runtime() {
        let (_session_id, control, summary) = terminal_test_control(FusionOrigin::Agent);
        let runner_control = control.clone();
        let prepared = PreparedFusionRun::new(summary, control.clone(), move |_, _| async move {
            FusionRunOutcome::from_control(&runner_control, Err(FusionError::Internal))
        });

        drop(prepared);

        let outcome = control
            .terminal_outcome()
            .expect("legacy/no-recorder Drop seals before returning");
        assert_eq!(outcome.result, Err(FusionError::Cancelled));
        assert_eq!(outcome.facts.allocated_panels, Some(0));
        assert_eq!(outcome.facts.dispatched_panels, Some(0));
        assert_eq!(outcome.facts.attempts, Some(0));
        assert_eq!(outcome.facts.usage, Some(FusionUsage::default()));
        assert_eq!(
            outcome.publication,
            FusionPublicationReceipt::not_required()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropping_any_origin_claims_sealing_before_the_recorder_can_run() {
        for origin in [
            FusionOrigin::Agent,
            FusionOrigin::Slash,
            FusionOrigin::Workflow,
        ] {
            let (session_id, control, summary) = terminal_test_control(origin);
            let (recorder, seen_rx, release) =
                ProbeTerminalRecorder::new(true, false, FusionPublicationReceipt::queued());
            let capability = terminal_test_capability(recorder.clone(), origin, session_id);
            let runner_control = control.clone();
            let prepared =
                PreparedFusionRun::new(summary, control.clone(), move |_, _| async move {
                    FusionRunOutcome::from_control(&runner_control, Err(FusionError::Internal))
                })
                .with_terminal_capability(capability);

            drop(prepared);
            // A current-thread runtime cannot poll the detached recorder until
            // this task yields. The claim therefore has to happen in Drop,
            // synchronously, rather than at the start of the spawned future.
            assert!(
                !control.request_cancel(),
                "Drop must own sealing before scheduling for {origin:?}"
            );
            let (recorded, _) = tokio::time::timeout(Duration::from_secs(1), seen_rx)
                .await
                .expect("Drop-owned recorder must run")
                .expect("Drop-owned candidate must be observable");
            assert_eq!(control.terminal_outcome(), None);
            assert_eq!(recorded.result, Err(FusionError::Cancelled));
            assert_eq!(recorded.facts.attempts, Some(0));

            release.expect("this recorder is blocked").add_permits(1);
            let visible = tokio::time::timeout(Duration::from_secs(1), control.wait_terminal())
                .await
                .expect("Drop-owned sealing must wake waiters");
            assert_eq!(visible.result, recorded.result);
            assert_eq!(visible.facts, recorded.facts);
            assert_eq!(recorder.calls.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn runner_and_recorder_panics_are_sealed_once_for_every_origin() {
        for origin in [
            FusionOrigin::Agent,
            FusionOrigin::Slash,
            FusionOrigin::Workflow,
        ] {
            let (session_id, control, summary) = terminal_test_control(origin);
            let (recorder, seen_rx, _) =
                ProbeTerminalRecorder::new(false, true, FusionPublicationReceipt::queued());
            let capability = terminal_test_capability(recorder.clone(), origin, session_id);
            let prepared = PreparedFusionRun::new(summary, control.clone(), move |_, _| {
                panic!("runner construction panic");
                #[allow(unreachable_code)]
                async {
                    unreachable!()
                }
            })
            .with_terminal_capability(capability);

            let visible = prepared.activate(FusionActivation::now(), None).await;
            let (recorded, _) = seen_rx
                .await
                .expect("recorder observes the runner panic candidate before panicking");
            assert_eq!(recorded.result, Err(FusionError::Internal));
            assert_eq!(visible.result, recorded.result);
            assert_eq!(visible.facts, recorded.facts);
            assert_eq!(
                visible.publication,
                FusionPublicationReceipt::storage_failure("terminal recorder panicked")
            );
            assert_eq!(recorder.calls.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn finalizing_claim_cannot_be_stolen_by_cancellation_claim() {
        let control = FusionRunControl::new(
            FusionRunIdentity::new(FusionRunId::generated(), None, FusionOrigin::Agent, None),
            1_000,
            CancellationToken::new(),
            FusionRunFactsRecorder::default(),
        );
        assert!(control.activate_at(Instant::now()));
        assert!(control.begin_finalizing());
        assert!(!control.request_cancel());
        assert!(!control.cancel().is_cancelled());
        assert!(!control.claim_terminal());
        assert!(control.claim_finalizing());
        assert!(control.is_terminal());
    }

    #[tokio::test]
    async fn billing_mode_is_owned_by_supervisor_not_runner() {
        use crate::ModelAttemptBillingMode::{LegacyAggregate, MeteredAttempts};
        for (captured, offered) in [
            (LegacyAggregate, MeteredAttempts),
            (MeteredAttempts, LegacyAggregate),
        ] {
            let (_, legacy, summary) = terminal_test_control(FusionOrigin::Agent);
            assert_eq!(legacy.billing_mode(), LegacyAggregate);
            let control = FusionRunControl::new_with_billing_mode(
                legacy.identity().clone(),
                1_000,
                CancellationToken::new(),
                FusionRunFactsRecorder::default(),
                captured,
            );
            let foreign = FusionRunControl::new_with_billing_mode(
                legacy.identity().clone(),
                1_000,
                CancellationToken::new(),
                FusionRunFactsRecorder::default(),
                offered,
            );
            let (recorder, seen, _) =
                ProbeTerminalRecorder::new(false, false, FusionPublicationReceipt::not_required());
            let prepared =
                PreparedFusionRun::new(summary, control.clone(), move |_, _| async move {
                    FusionRunOutcome::from_control(&foreign, Err(FusionError::Internal))
                })
                .with_terminal_capability(FusionTerminalCapability::new(recorder));
            let outcome = prepared.activate(FusionActivation::now(), None).await;
            assert_eq!(outcome.billing_mode(), captured);
            assert_eq!(seen.await.unwrap().0.billing_mode(), captured);
            assert_eq!(control.wait_terminal().await.billing_mode(), captured);
        }
    }

    #[tokio::test]
    async fn billing_mode_survives_panic_cancel_and_unactivated_drop() {
        use crate::ModelAttemptBillingMode::MeteredAttempts;
        for path in 0..5 {
            let (_, legacy, summary) = terminal_test_control(FusionOrigin::Agent);
            let control = FusionRunControl::new_with_billing_mode(
                legacy.identity().clone(),
                1_000,
                CancellationToken::new(),
                FusionRunFactsRecorder::default(),
                MeteredAttempts,
            );
            let runner_control = control.clone();
            let (recorder, seen, _) =
                ProbeTerminalRecorder::new(false, false, FusionPublicationReceipt::not_required());
            let prepared = PreparedFusionRun::new(summary, control.clone(), move |_, _| {
                assert_ne!(path, 0, "synchronous runner panic");
                async move {
                    assert_ne!(path, 1, "poll panic");
                    if path == 3 {
                        assert!(runner_control.request_cancel());
                    }
                    FusionRunOutcome::from_control(&runner_control, Err(FusionError::Internal))
                }
            })
            .with_terminal_capability(FusionTerminalCapability::new(recorder));
            if path == 2 {
                assert!(control.request_cancel());
            }
            if path == 4 {
                drop(prepared);
            } else {
                assert_eq!(
                    prepared
                        .activate(FusionActivation::now(), None)
                        .await
                        .billing_mode(),
                    MeteredAttempts
                );
            }
            let recorded = tokio::time::timeout(Duration::from_secs(2), seen)
                .await
                .unwrap()
                .unwrap()
                .0;
            assert_eq!(recorded.billing_mode(), MeteredAttempts);
            if matches!(path, 2 | 4) {
                assert_eq!(
                    recorded.facts.attempt_settlement,
                    Some(FusionAttemptSettlementStatus::Settled)
                );
            } else {
                assert!(matches!(
                    recorded.facts.attempt_settlement,
                    Some(FusionAttemptSettlementStatus::Failed { .. })
                ));
            }
            assert_eq!(
                control.wait_terminal().await.billing_mode(),
                MeteredAttempts
            );
        }
    }

    #[tokio::test]
    async fn billing_mode_accounting_failure_preserves_computed_answer() {
        for explicit_failure in [false, true] {
            let (_, legacy, summary) = terminal_test_control(FusionOrigin::Agent);
            let control = FusionRunControl::new_with_billing_mode(
                legacy.identity().clone(),
                1_000,
                CancellationToken::new(),
                FusionRunFactsRecorder::default(),
                crate::ModelAttemptBillingMode::MeteredAttempts,
            );
            let expected = terminal_test_result(&control.identity().run_id);
            let result = expected.clone();
            let runner_control = control.clone();
            let prepared = PreparedFusionRun::new(summary, control, move |_, _| async move {
                if explicit_failure {
                    let facts = runner_control.facts();
                    facts.set_attempt_settlement(FusionAttemptSettlementStatus::Failed {
                        reason: "durable writer unavailable".into(),
                    });
                    facts.set_known_zero();
                    facts.replace_usage(FusionUsage::default(), false);
                    facts.set_attempt_settlement(FusionAttemptSettlementStatus::Settled);
                    facts.set_attempt_settlement(FusionAttemptSettlementStatus::Pending);
                }
                FusionRunOutcome::from_control(&runner_control, Ok(result))
            });
            let outcome = prepared.activate(FusionActivation::now(), None).await;
            assert_eq!(outcome.result, Ok(expected));
            assert!(matches!(
                outcome.facts.attempt_settlement,
                Some(FusionAttemptSettlementStatus::Failed { .. })
            ));
            assert!(outcome.facts.usage_incomplete);
            assert_eq!(
                outcome.publication,
                FusionPublicationReceipt::not_required()
            );
        }
    }

    #[test]
    fn billing_mode_legacy_facts_keep_wire_compatibility() {
        let value = serde_json::to_value(FusionRunFacts::default()).unwrap();
        assert!(value.get("attempt_settlement").is_none());
        assert_eq!(
            serde_json::from_value::<FusionRunFacts>(value)
                .unwrap()
                .attempt_settlement,
            None
        );
        let facts = FusionRunFactsRecorder::default();
        facts.set_attempt_settlement(FusionAttemptSettlementStatus::Settled);
        facts.set_attempt_settlement(FusionAttemptSettlementStatus::Pending);
        assert_eq!(
            facts.snapshot().attempt_settlement,
            Some(FusionAttemptSettlementStatus::Settled)
        );
    }

    #[tokio::test]
    async fn cancellation_claim_is_atomic_and_overrides_an_ignoring_runner() {
        let control = FusionRunControl::new(
            FusionRunIdentity::new(FusionRunId::generated(), None, FusionOrigin::Agent, None),
            1_000,
            CancellationToken::new(),
            FusionRunFactsRecorder::default(),
        );
        let summary = FusionPreparedSummary {
            identity: control.identity().clone(),
            duration_ms: 1_000,
            planned_panels: None,
        };
        let runner_control = control.clone();
        let release = Arc::new(tokio::sync::Notify::new());
        let runner_release = Arc::clone(&release);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let prepared = PreparedFusionRun::new(summary, control.clone(), move |_, _| async move {
            let _ = started_tx.send(());
            runner_release.notified().await;
            FusionRunOutcome::from_control(&runner_control, Err(FusionError::Internal))
        });
        let waiter = tokio::spawn(prepared.activate(FusionActivation::now(), None));
        started_rx.await.expect("owned runner must start");

        assert!(control.request_cancel());
        assert!(control.cancel().is_cancelled());
        release.notify_one();

        let outcome = waiter.await.expect("activation waiter must join");
        assert_eq!(outcome.result, Err(FusionError::Cancelled));
        assert!(!control.request_cancel());
    }

    #[test]
    fn activation_overflow_fails_closed_without_dispatch() {
        let control = FusionRunControl::new(
            FusionRunIdentity::new(FusionRunId::generated(), None, FusionOrigin::Slash, None),
            u64::MAX,
            CancellationToken::new(),
            FusionRunFactsRecorder::default(),
        );
        // A very large duration can still be representable on hosts whose
        // monotonic clock has a correspondingly wide range; checked arithmetic
        // must preserve that valid legacy configuration rather than rejecting
        // it merely because it is large.
        assert!(control.activate_at(Instant::now()));

        // Exercise the actual overflow boundary when this platform exposes a
        // representable instant that close to its upper limit. Some platforms
        // cannot construct that instant, in which case the checked-add API has
        // already demonstrated the only available failure path.
        let near_limit = Instant::now().checked_add(Duration::MAX);
        if let Some(near_limit) = near_limit {
            let bounded = FusionRunControl::new(
                FusionRunIdentity::new(FusionRunId::generated(), None, FusionOrigin::Slash, None),
                1,
                CancellationToken::new(),
                FusionRunFactsRecorder::default(),
            );
            assert!(!bounded.activate_at(near_limit));
            assert!(bounded.is_terminal());
        }
    }

    #[tokio::test]
    async fn prepared_activation_is_one_shot_and_legacy_facts_stay_unknown_on_error() {
        let control = FusionRunControl::new(
            FusionRunIdentity::new(FusionRunId::generated(), None, FusionOrigin::Agent, None),
            1_000,
            CancellationToken::new(),
            FusionRunFactsRecorder::default(),
        );
        let identity = control.identity().clone();
        let summary = FusionPreparedSummary {
            identity,
            duration_ms: 1_000,
            planned_panels: None,
        };
        let runner_control = control.clone();
        let prepared = PreparedFusionRun::new(summary, control.clone(), move |_activation, _| {
            let runner_control = runner_control.clone();
            async move { FusionRunOutcome::from_control(&runner_control, Err(FusionError::Internal)) }
        });
        let outcome = prepared.activate(FusionActivation::now(), None).await;
        assert!(matches!(outcome.result, Err(FusionError::Internal)));
        assert_eq!(outcome.facts.resolved_panels, None);
        assert_eq!(outcome.facts.allocated_panels, None);
        assert_eq!(outcome.facts.usage, None);
    }

    #[tokio::test]
    async fn unpolled_activation_seals_zero_dispatch_for_waiters() {
        let control = FusionRunControl::new(
            FusionRunIdentity::new(FusionRunId::generated(), None, FusionOrigin::Agent, None),
            1_000,
            CancellationToken::new(),
            FusionRunFactsRecorder::default(),
        );
        let summary = FusionPreparedSummary {
            identity: control.identity().clone(),
            duration_ms: 1_000,
            planned_panels: None,
        };
        let runner_control = control.clone();
        let prepared = PreparedFusionRun::new(summary, control.clone(), move |_, _| async move {
            FusionRunOutcome::from_control(&runner_control, Err(FusionError::Internal))
        });
        let never_polled = prepared.activate(FusionActivation::now(), None);
        drop(never_polled);

        let outcome = tokio::time::timeout(Duration::from_secs(1), control.wait_terminal())
            .await
            .expect("dropping an unpolled activation must wake terminal waiters");
        assert!(matches!(outcome.result, Err(FusionError::Cancelled)));
        assert_eq!(outcome.facts.allocated_panels, Some(0));
        assert_eq!(outcome.facts.dispatched_panels, Some(0));
        assert_eq!(outcome.facts.attempts, Some(0));
        assert_eq!(outcome.facts.usage, Some(FusionUsage::default()));
    }

    #[tokio::test]
    async fn owned_supervisor_finishes_after_activation_waiter_is_dropped() {
        let control = FusionRunControl::new(
            FusionRunIdentity::new(FusionRunId::generated(), None, FusionOrigin::Agent, None),
            1_000,
            CancellationToken::new(),
            FusionRunFactsRecorder::default(),
        );
        let summary = FusionPreparedSummary {
            identity: control.identity().clone(),
            duration_ms: 1_000,
            planned_panels: Some(2),
        };
        let runner_control = control.clone();
        let release = Arc::new(tokio::sync::Notify::new());
        let runner_release = Arc::clone(&release);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let prepared = PreparedFusionRun::new(summary, control.clone(), move |_, _| async move {
            let _ = started_tx.send(());
            runner_release.notified().await;
            runner_control.facts().set_allocated_panels(2);
            FusionRunOutcome::from_control(&runner_control, Err(FusionError::Internal))
        });
        let waiter = tokio::spawn(prepared.activate(FusionActivation::now(), None));
        started_rx.await.expect("owned runner must start");
        waiter.abort();
        let _ = waiter.await;
        release.notify_one();

        let outcome = tokio::time::timeout(Duration::from_secs(1), control.wait_terminal())
            .await
            .expect("owned supervisor must outlive its activation waiter");
        assert!(matches!(outcome.result, Err(FusionError::Internal)));
        assert_eq!(outcome.facts.allocated_panels, Some(2));
    }

    #[tokio::test]
    async fn synchronous_runner_panic_is_sealed_as_internal() {
        let control = FusionRunControl::new(
            FusionRunIdentity::new(FusionRunId::generated(), None, FusionOrigin::Slash, None),
            1_000,
            CancellationToken::new(),
            FusionRunFactsRecorder::default(),
        );
        let summary = FusionPreparedSummary {
            identity: control.identity().clone(),
            duration_ms: 1_000,
            planned_panels: None,
        };
        let prepared = PreparedFusionRun::new(summary, control.clone(), move |_, _| {
            panic!("runner construction panic");
            #[allow(unreachable_code)]
            async {
                unreachable!()
            }
        });
        let outcome = prepared.activate(FusionActivation::now(), None).await;
        assert!(matches!(outcome.result, Err(FusionError::Internal)));
        assert!(control.terminal_outcome().is_some());
    }

    /// F005: every progress surface renders `FusionStage` through this ONE
    /// method, so pin the exact §7 copy here — a stray rewording at any
    /// call site cannot silently diverge from what this test locks in.
    #[test]
    fn fusion_stage_label_matches_design_doc_7_copy() {
        assert_eq!(FusionStage::ResolvingModels.label(), "Resolving models");
        assert_eq!(FusionStage::ReservingBudget.label(), "Reserving budget");
        assert_eq!(
            FusionStage::RunningPanels {
                completed: 2,
                total: 3
            }
            .label(),
            "Running panels 2/3"
        );
        assert_eq!(
            FusionStage::PanelsDispatched { total: 3 }.label(),
            "Running panels 0/3",
            "PanelsDispatched is a distinct SIGNAL, not a distinct \
user-visible progress state — it must render the same words as \
RunningPanels{{completed:0,..}}"
        );
        assert_eq!(FusionStage::Analyzing.label(), "Analyzing reports");
        assert_eq!(FusionStage::Selecting.label(), "Selecting answer");
        assert_eq!(FusionStage::Synthesizing.label(), "Synthesizing answer");
        assert_eq!(FusionStage::Completed.label(), "Completed");
        assert_eq!(FusionStage::NeedsParent.label(), "Needs parent");
        assert_eq!(FusionStage::Failed.label(), "Failed");
        assert_eq!(FusionStage::Cancelled.label(), "Cancelled");
    }

    struct PinnedFusionExecutor;

    #[async_trait]
    impl FusionExecutor for PinnedFusionExecutor {
        fn prepare(
            self: ::std::sync::Arc<Self>,
            submission: crate::FusionSubmission,
        ) -> Result<crate::PreparedFusionRun, crate::FusionError> {
            let timeout = self.effective_timeout_ms();
            crate::prepared_from_oneshot(
                submission,
                timeout,
                move |_request, _inherit, _progress| async move {
                    unreachable!("not exercised by this test")
                },
            )
        }

        fn preflight_error(&self) -> Option<FusionError> {
            Some(FusionError::InvalidConfiguration("pinned".into()))
        }
    }

    /// F008: the default `preflight_error()` is inert (`None`); an executor
    /// that overrides it (mirroring `RejectedFusionExecutor`) surfaces its
    /// pinned failure without needing to override `run()`/`agent_surface()`.
    #[test]
    fn preflight_error_defaults_to_none_and_is_overridable() {
        struct DefaultExecutor;
        #[async_trait]
        impl FusionExecutor for DefaultExecutor {
            fn prepare(
                self: ::std::sync::Arc<Self>,
                submission: crate::FusionSubmission,
            ) -> Result<crate::PreparedFusionRun, crate::FusionError> {
                let timeout = self.effective_timeout_ms();
                crate::prepared_from_oneshot(
                    submission,
                    timeout,
                    move |_request, _inherit, _progress| async move {
                        unreachable!("not exercised by this test")
                    },
                )
            }
        }
        assert!(DefaultExecutor.preflight_error().is_none());
        assert!(matches!(
            PinnedFusionExecutor.preflight_error(),
            Some(FusionError::InvalidConfiguration(msg)) if msg == "pinned"
        ));
    }
}
