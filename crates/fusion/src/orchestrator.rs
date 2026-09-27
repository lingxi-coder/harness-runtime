//! Fusion state machine. Implements [`platform_api::FusionExecutor`].

use crate::analyst::{AnalystError, AnalystUsage};
use crate::budget::{self, FusionPriceBook, FusionQuote, ReservationLease};
use crate::config::{FusionConfigSource, FusionRuntimeConfig};
use crate::decision::{interpret, panel_by_id, successful, HostDecision};
use crate::model_resolver::{self, ModelSource, ResolvedPanel, ResolvedSet};
use crate::panel::{self, PanelInternal};
use crate::progress;
use crate::snapshot::{CatalogSnapshot, FusionRuntimeSnapshot};
use crate::synthesizer::SynthError;
use async_trait::async_trait;
use platform_api::subagent_spawn::SubagentSpawner;
use platform_api::{
    normalize_dimensions, FusionActivation, FusionAgentSurface, FusionAnalysis, FusionDecision,
    FusionError, FusionExecutor, FusionInheritance, FusionNeedsParentReason, FusionOrigin,
    FusionPreparedSummary, FusionPreset, FusionProgress, FusionRequest, FusionResult,
    FusionRunControl, FusionRunFactsRecorder, FusionRunIdentity, FusionRunOutcome, FusionStage,
    FusionStatus, FusionSubmission, FusionTiming, FusionUsage, PanelOutcome, PanelRunStatus,
    PreparedFusionRun, FUSION_MIN_PANEL,
};
use sidequery::SideQueryClient;
// Used only by the tests below. Kept at file scope behind `cfg(test)` so the
// lib target carries no unused import (which `clippy --fix` deletes) while the
// test module still reaches it through `use super::*`.
#[cfg(test)]
use platform_api::FusionRunId;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use telemetry::sink::{AnalyticsValue, LogEventMetadata};
use telemetry::AnalyticsBus;
use tokio::sync::mpsc::Sender;
use tokio::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// Catch a panic while polling an owned orchestration future. The prepared
/// supervisor still holds the lease/fact cells outside this future, so it can
/// synchronously settle them before publishing the terminal envelope.
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
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            std::pin::Pin::new(&mut self.inner).poll(cx)
        })) {
            Ok(std::task::Poll::Ready(output)) => std::task::Poll::Ready(Ok(output)),
            Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
            Err(_) => std::task::Poll::Ready(Err(())),
        }
    }
}

/// Extra headroom the OUTER `run()` deadline carries past
/// `config.total_timeout_ms` (F004 review fix).
///
/// Every stage inside `run_inner` bounds itself by [`FusionOrchestrator::remaining`],
/// which is derived from the SAME `total_timeout_ms` — but `remaining()` computes
/// its deadline with `Duration::from_millis(total_timeout_ms - millis_since(started))`,
/// and `millis_since` truncates the elapsed time down to whole milliseconds via
/// `as_millis()`. That truncation makes `remaining()` over-report the time left by
/// up to ~1ms, so an inner per-stage deadline built from it can land at
/// `started + total_timeout_ms + <sub-ms fraction>` — a handful of MICROSECONDS
/// after the outer wrapper's own `started + total_timeout_ms` deadline. Since both
/// deadlines are registered with tokio's timer wheel (1ms granularity), whichever
/// of the two fires first is effectively a coin flip on that fraction, so the OUTER
/// wrapper can (rarely) win the race and degrade the whole run to
/// `Err(TimedOutEmpty)` even though an inner stage was about to hand back a
/// legitimate `Ok(NeedsParent)` with real panel material.
///
/// Giving the outer wrapper a fixed grace past `total_timeout_ms` makes it a
/// strict backstop that can never fire before every inner per-stage deadline has
/// had its chance — the inner deadlines still bound user-visible latency at
/// `total_timeout_ms`, this grace only covers the tail (result assembly,
/// `lease.commit`, telemetry) after an inner stage has already degraded.
const FINALIZE_GRACE_MS: u64 = 250;

/// Analytics is non-authoritative and must never keep a settled computation
/// in Finalizing. Ordinary in-memory/queued sinks complete immediately; a
/// stalled external sink gets this small best-effort tail, capped again by
/// the run's captured outer deadline.
const TERMINAL_TELEMETRY_GRACE_MS: u64 = 100;

/// Runtime sources are local and normally immutable or lock-backed. A small
/// bounded retry absorbs one concurrent credential/settings publication while
/// still failing closed under sustained churn.
const SNAPSHOT_CAPTURE_ATTEMPTS: usize = 4;

/// Injected Fusion orchestrator. Settings, catalog, spawner, and side-query
/// client live here; [`FusionInheritance`] carries the parent session handles.
#[derive(Clone)]
pub struct FusionOrchestrator {
    spawner: Arc<dyn SubagentSpawner>,
    side_query: Arc<dyn SideQueryClient>,
    config_source: Arc<dyn FusionConfigSource>,
    catalog: Arc<dyn ModelSource>,
    prices: Arc<dyn FusionPriceBook>,
    bus: Arc<AnalyticsBus>,
    attempt_registrar: Option<Arc<dyn crate::FusionAttemptRegistrar>>,
    attempt_run: Option<Arc<platform_api::ModelAttemptRun>>,
    panel_admission: bool,
    panel_fence: Option<Arc<dyn crate::FusionPanelAttemptFence>>,
}

/// Return value of [`FusionOrchestrator::analyze_and_decide`]: the resolved
/// decision plus everything `run_inner`'s finalize step needs afterward
/// (timing for [`FusionTiming`], the running [`FusionUsage`], and the priced
/// analyst/synth call usage `price_realized_usage` needs).
struct AnalysisOutcome {
    decision: FusionDecision,
    final_text: String,
    analysis: Option<FusionAnalysis>,
    analyst_ms: u64,
    synthesizer_ms: u64,
    usage: FusionUsage,
    priced_analyst: Option<(cost::Usage, u32)>,
    priced_synth: Option<cost::Usage>,
    /// T1 item 1 (analyst/synth half): `true` once the synthesizer was
    /// actually CALLED (`run_synthesis` sets this unconditionally at its own
    /// start, before racing its timeout), `false` when the `Merge` branch was
    /// never reached (a `Pick`/`NeedsParent` analyst decision). Distinguishes
    /// "the synthesizer never ran — $0 is the exact truth" from "the
    /// synthesizer ran and lost its usage to a failure/timeout — real,
    /// already-billed spend the settlement fallback must estimate" at
    /// `price_realized_usage`, which otherwise cannot tell the two apart from
    /// `priced_synth: None` alone.
    synth_attempted: bool,
    /// Round-3 review finding 10: `true` when at least one analyst attempt
    /// is known to have been billed by the provider without a usage figure
    /// this crate could capture for it (an `AnalystUsage::incomplete` from
    /// an `InvalidResponse` retry), OR the analyst call ultimately failed
    /// (finding 8 — even when `priced_analyst` carries real, non-empty
    /// usage from earlier attempts, the failing final attempt's own spend,
    /// if any, is still uncaptured by definition). ORed into
    /// `FusionUsage.estimated` at `finalize_result` so the run never claims
    /// an exact total over a figure that is known-short.
    analyst_usage_incomplete: bool,
    /// Whether the analyst provider boundary was actually entered. A run can
    /// exhaust its operational deadline after panels but before this stage;
    /// that path must not invent analyst cost or egress.
    analyst_attempted: bool,
}

impl FusionOrchestrator {
    /// Build an orchestrator from composition-root handles.
    ///
    /// `config_source` is consulted fresh on every [`Self::run`] and every
    /// `agent_surface()`/`resolve_parent_profile()`/`workflow_fusion_call_cap()`
    /// call (F007) — pass a bare [`FusionRuntimeConfig`] (which implements
    /// [`FusionConfigSource`] as a fixed value) for a config that never
    /// reloads, or a closure/struct backed by a live settings loader.
    #[must_use]
    pub fn new(
        spawner: Arc<dyn SubagentSpawner>,
        side_query: Arc<dyn SideQueryClient>,
        config_source: Arc<dyn FusionConfigSource>,
        catalog: Arc<dyn ModelSource>,
    ) -> Self {
        Self {
            spawner,
            side_query,
            config_source,
            catalog,
            prices: Arc::new(()),
            bus: Arc::new(AnalyticsBus::new()),
            attempt_registrar: None,
            attempt_run: None,
            panel_admission: false,
            panel_fence: None,
        }
    }

    /// Require atomic whole-panel admission from the host. Legacy standalone
    /// hosts remain compatible; production composition must explicitly opt in.
    #[must_use]
    pub fn with_panel_admission(mut self) -> Self {
        self.panel_admission = true;
        self
    }

    /// Attach trusted prepare-time physical-attempt accounting.
    #[must_use]
    pub fn with_attempt_registrar(
        mut self,
        registrar: Arc<dyn crate::FusionAttemptRegistrar>,
    ) -> Self {
        self.attempt_registrar = Some(registrar);
        self
    }

    /// Attach a price book so hard reservation can quote nano-USD.
    #[must_use]
    pub fn with_price_book(mut self, prices: Arc<dyn FusionPriceBook>) -> Self {
        self.prices = prices;
        self
    }

    /// Attach the analytics bus used for privacy-safe Fusion telemetry.
    #[must_use]
    pub fn with_bus(mut self, bus: Arc<AnalyticsBus>) -> Self {
        self.bus = bus;
        self
    }

    fn operational_deadline(
        config: &FusionRuntimeConfig,
        started: Instant,
    ) -> Result<Instant, FusionError> {
        started
            .checked_add(Duration::from_millis(config.total_timeout_ms))
            .ok_or(FusionError::Internal)
    }

    fn remaining_until(deadline: Instant) -> Duration {
        deadline.saturating_duration_since(Instant::now())
    }

    fn ensure_operational_time(deadline: Instant) -> Result<(), FusionError> {
        if deadline <= Instant::now() {
            Err(FusionError::TimedOutEmpty)
        } else {
            Ok(())
        }
    }

    fn prepared_config(
        &self,
        effective_timeout_ms: Option<u64>,
    ) -> Result<FusionRuntimeConfig, FusionError> {
        let mut config = self.config_source.load()?;
        if let Some(captured_total) =
            effective_timeout_ms.map(|timeout_ms| timeout_ms.saturating_sub(FINALIZE_GRACE_MS))
        {
            config.total_timeout_ms = captured_total;
        }
        Ok(config)
    }

    /// Capture one coherent immutable view across effective settings, route
    /// rows/limits and prices. Catalog reads are revision-bracketed; repeating
    /// the aggregate prevents resolution from observing a settings/catalog
    /// straddle. The production price catalog is an immutable `Arc`, while the
    /// equality check also keeps mutable test implementations honest.
    pub(crate) fn capture_runtime_snapshot(
        &self,
        request: &FusionRequest,
        effective_timeout_ms: Option<u64>,
    ) -> Result<FusionRuntimeSnapshot, FusionError> {
        let extra_routes = [(request.parent_profile.clone(), request.parent_model.clone())];
        for _ in 0..SNAPSHOT_CAPTURE_ATTEMPTS {
            let config_before = self.prepared_config(effective_timeout_ms)?;
            let catalog_before = CatalogSnapshot::capture(self.catalog.as_ref())?;
            let prices_before = catalog_before.capture_prices(self.prices.as_ref(), &extra_routes);

            let catalog_after = CatalogSnapshot::capture(self.catalog.as_ref())?;
            let config_after = self.prepared_config(effective_timeout_ms)?;
            let prices_after = catalog_after.capture_prices(self.prices.as_ref(), &extra_routes);

            if config_before == config_after
                && catalog_before == catalog_after
                && prices_before == prices_after
            {
                return Ok(FusionRuntimeSnapshot::new(
                    config_after,
                    catalog_after,
                    prices_after,
                ));
            }
        }
        Err(FusionError::InvalidConfiguration(
            "fusion runtime inputs changed during preparation".into(),
        ))
    }

    /// Reject a captured route when current auth/policy metadata revoked it or
    /// narrowed its capacity. This is deliberately stage-level: additions do
    /// not reroute a prepared run, and PR06 will add the final per-wire check.
    pub(crate) fn ensure_live_routes(
        live_catalog: &dyn ModelSource,
        snapshot: &CatalogSnapshot,
        routes: &[(&str, &str, bool)],
        configured_output_tokens: u32,
        stage: &str,
    ) -> Result<(), FusionError> {
        let current = live_catalog.list();
        for (profile, model, judge) in routes {
            let Some(captured) = snapshot.row_for(profile, model) else {
                return Err(FusionError::InvalidConfiguration(format!(
                    "fusion {stage} route {profile}/{model} was not present in the prepared catalog"
                )));
            };
            let Some(row) = current
                .iter()
                .find(|row| row.profile == *profile && row.model == *model)
            else {
                return Err(FusionError::InvalidConfiguration(format!(
                    "fusion {stage} route {profile}/{model} is no longer available"
                )));
            };
            let eligibility_revoked = if *judge {
                (captured.hints.judge_eligible && !row.hints.judge_eligible)
                    || (captured.structured_output && !row.structured_output)
            } else {
                // Explicit panel routes are allowed without automatic-selection
                // hints. Only a capability that was present at prepare time can
                // be revoked here; unchanged `false` is not a new restriction.
                captured.hints.eligible && !row.hints.eligible
            };
            if eligibility_revoked
                || capacity_narrowed(captured.limits, row.limits, configured_output_tokens)
            {
                return Err(FusionError::InvalidConfiguration(format!(
                    "fusion {stage} route {profile}/{model} was restricted after preparation"
                )));
            }
        }
        Ok(())
    }

    /// Restrictive settings changes must stop a later stage, not silently
    /// reinterpret the prepared selection.  The normal composition root
    /// reloads effective settings on every call; this check keeps a prepared
    /// run fail-closed when that source changes between stages.
    pub(crate) fn ensure_live_config(
        source: &dyn FusionConfigSource,
        captured: &FusionRuntimeConfig,
        request: &FusionRequest,
        routes: &[&ResolvedPanel],
        stage: &str,
    ) -> Result<(), FusionError> {
        let current = source.load().map_err(|_| {
            FusionError::InvalidConfiguration(format!(
                "fusion {stage} settings could not be revalidated"
            ))
        })?;
        if captured.enabled && !current.enabled {
            return Err(FusionError::InvalidConfiguration(format!(
                "fusion was disabled before the {stage} stage"
            )));
        }
        let has_cross_provider_route = routes
            .iter()
            .any(|route| route.profile != request.parent_profile);
        let cross_provider_revoked = request.cross_provider
            && has_cross_provider_route
            && match request.origin {
                FusionOrigin::Slash => false,
                FusionOrigin::Agent => !current.allow_cross_provider_for_agent,
                FusionOrigin::Workflow => !current.allow_cross_provider_for_workflow,
            };
        if cross_provider_revoked {
            return Err(FusionError::CrossProviderDenied);
        }
        if !current.allowed_profiles.is_empty()
            && routes
                .iter()
                .any(|route| !current.allowed_profiles.contains(&route.profile))
        {
            return Err(FusionError::InvalidConfiguration(format!(
                "fusion {stage} route is no longer allowed by current settings"
            )));
        }
        let output_narrowed = match stage {
            "panel" => {
                current.panel_max_output_tokens_per_turn < captured.panel_max_output_tokens_per_turn
            }
            "analyst" => current.analyst_max_output_tokens < captured.analyst_max_output_tokens,
            "synthesizer" => {
                current.synthesizer_max_output_tokens < captured.synthesizer_max_output_tokens
            }
            _ => false,
        };
        if output_narrowed {
            return Err(FusionError::InvalidConfiguration(format!(
                "fusion {stage} output capacity was restricted after preparation"
            )));
        }
        let reservation_cap_narrowed = match current.max_reserved_nano_usd {
            Some(current_limit) => captured
                .max_reserved_nano_usd
                .map_or(true, |captured_limit| current_limit < captured_limit),
            None => false,
        };
        // All stage and total deadlines are part of the prepared run's
        // immutable activation contract. A later timeout edit applies to the
        // next preparation; treating it as a live restriction would silently
        // shorten the deadline already handed to Task/CLI waiters.
        if reservation_cap_narrowed {
            return Err(FusionError::InvalidConfiguration(format!(
                "fusion {stage} run policy was restricted after preparation"
            )));
        }
        let stage_policy_narrowed = match stage {
            "panel" => {
                current.max_panel < routes.len() as u8
                    || current.panel_max_turns < captured.panel_max_turns
                    || current.panel_reserved_input_tokens_per_turn
                        < captured.panel_reserved_input_tokens_per_turn
                    || (captured.partial_ok && !current.partial_ok)
                    || current.min_successful_panels > captured.min_successful_panels
            }
            "analyst" => current.analysis_protocol_retries < captured.analysis_protocol_retries,
            "synthesizer" => false,
            _ => false,
        };
        if stage_policy_narrowed {
            return Err(FusionError::InvalidConfiguration(format!(
                "fusion {stage} policy was restricted after preparation"
            )));
        }
        Ok(())
    }

    /// Reserve a route resolved during `prepare`. Resolution and quoting are
    /// deliberately side-effect free; only activation may acquire the
    /// monetary lease and therefore consume the parent's budget handle.
    #[allow(clippy::too_many_arguments)]
    async fn reserve_resolved(
        &self,
        quote: &FusionQuote,
        inherit: &FusionInheritance,
        progress: &Option<Sender<FusionProgress>>,
        operational_deadline: Instant,
        lease_cell: &Arc<Mutex<Option<SettlementLease>>>,
        settlement: &Arc<Mutex<Option<(u64, bool)>>>,
        facts: &FusionRunFactsRecorder,
    ) -> Result<(), FusionError> {
        progress::emit(
            progress,
            FusionStage::ReservingBudget,
            None,
            FusionStage::ReservingBudget.label(),
        );
        let acquire = budget::acquire_quoted(quote.clone(), inherit.budget());
        tokio::pin!(acquire);
        let acquired = tokio::select! {
            biased;
            result = &mut acquire => result,
            () = inherit.cancel.cancelled() => return Err(FusionError::Cancelled),
            () = tokio::time::sleep_until(operational_deadline) => {
                return Err(FusionError::TimedOutEmpty);
            }
        };
        match acquired {
            Ok(lease) => {
                facts.set_reserved_max_nano_usd(quote.reserved_nano_usd);
                if let Ok(mut guard) = settlement.lock() {
                    *guard = Some((0, true));
                }
                if let Ok(mut guard) = lease_cell.lock() {
                    *guard = Some(SettlementLease {
                        lease: Some(lease),
                        settlement: Arc::clone(settlement),
                    });
                }
            }
            Err(error) => return Err(error),
        }

        if inherit.cancel.is_cancelled() {
            return Err(FusionError::Cancelled);
        }
        Ok(())
    }

    /// `resolve_and_reserve` helper: emit the `STARTED` telemetry event once
    /// the panel/analyst set is resolved. Split out purely to keep the
    /// caller under the line-count lint.
    async fn emit_started(
        &self,
        request: &FusionRequest,
        resolved: &ResolvedSet,
        run_id: &str,
        cancel: &CancellationToken,
        operational_deadline: Instant,
    ) {
        let mut md = fusion_event_metadata(request);
        md.insert("run_id".into(), AnalyticsValue::String(run_id.to_string()));
        md.insert(
            "panel_count".into(),
            AnalyticsValue::Int(saturating_i64(resolved.panels.len())),
        );
        md.insert(
            "analyst_profile".into(),
            AnalyticsValue::String(resolved.analyst.profile.clone()),
        );
        // F011 item 3: surface whether `resolve_analyst` had to fall back to
        // a panelist judge (no non-panelist alternative existed) so
        // operators can see how often the self-preference-bias avoidance
        // actually applies. Round-2 fix: compare the CANONICAL model key
        // (same helper `resolve_analyst`'s `is_panelist` uses), not the
        // exact (profile, model) pair — otherwise this flag disagrees with
        // the selection rule and reports `false` for exactly the
        // leftover-gateway-duplicate case it exists to catch.
        md.insert(
            "analyst_overlaps_panel".into(),
            AnalyticsValue::Bool(resolved.panels.iter().any(|panel| {
                model_resolver::canonical_key(&panel.model)
                    == model_resolver::canonical_key(&resolved.analyst.model)
            })),
        );
        self.log_running_event(
            telemetry::tengu::fusion::STARTED,
            md,
            cancel,
            operational_deadline,
        )
        .await;
    }

    async fn emit_preparation_failed(
        &self,
        request: &FusionRequest,
        run_id: &str,
        progress: &Option<Sender<FusionProgress>>,
        error: &FusionError,
    ) {
        let mut md = fusion_event_metadata(request);
        md.insert("run_id".into(), AnalyticsValue::String(run_id.to_string()));
        add_fusion_error_metadata(&mut md, error);
        self.log_terminal_event(telemetry::tengu::fusion::FAILED, md, None)
            .await;
        progress::emit(
            progress,
            FusionStage::Failed,
            None,
            FusionStage::Failed.label(),
        );
    }

    async fn log_terminal_event(
        &self,
        name: &str,
        metadata: LogEventMetadata,
        outer_deadline: Option<Instant>,
    ) {
        let now = Instant::now();
        let local_deadline = now
            .checked_add(Duration::from_millis(TERMINAL_TELEMETRY_GRACE_MS))
            .unwrap_or(now);
        let deadline =
            outer_deadline.map_or(local_deadline, |deadline| deadline.min(local_deadline));
        let _ = tokio::time::timeout_at(deadline, self.bus.log_event(name, metadata)).await;
    }

    async fn log_running_event(
        &self,
        name: &str,
        metadata: LogEventMetadata,
        cancel: &CancellationToken,
        operational_deadline: Instant,
    ) {
        let event = self.bus.log_event(name, metadata);
        tokio::pin!(event);
        tokio::select! {
            biased;
            () = &mut event => {},
            () = cancel.cancelled() => {},
            () = tokio::time::sleep_until(operational_deadline) => {},
        }
    }

    /// `run_inner` stage 2/3: run every panel (concurrent, progress-emitting)
    /// and its per-panel telemetry, then anonymize. Split out of `run_inner`
    /// purely to keep that function under the line-count lint — same
    /// telemetry, same ordering, same early-return-on-error shape. The panel
    /// bar check itself stays in `run_inner` (it needs `request` and `config`
    /// alongside the returned panels/duration).
    #[allow(clippy::too_many_arguments)]
    async fn run_panel_stage(
        &self,
        config: &FusionRuntimeConfig,
        request: &FusionRequest,
        resolved: &ResolvedSet,
        catalog_snapshot: &CatalogSnapshot,
        inherit: &FusionInheritance,
        progress: &Option<Sender<FusionProgress>>,
        run_id: &str,
        started: Instant,
        operational_deadline: Instant,
        facts: &FusionRunFactsRecorder,
        panel_tasks: &panel::PanelTaskBarrier,
        admission: Option<platform_api::PanelPoolLease>,
        // [Round-4 rework, item 2] The three survives-a-drop cells, threaded
        // all the way down into `panel::run_panels` so a cancel landing
        // mid-fan-out — after some panels have already finished with real,
        // billed usage but before the whole stage returns `Ok` — still
        // leaves them holding that real data instead of `None`/the coarse
        // pre-panel guess. See `panel::RealizedSpendSink`.
        realized_tokens: &Arc<Mutex<Option<u64>>>,
        resolved_egress: &Arc<Mutex<Option<Vec<String>>>>,
        settlement: &Arc<Mutex<Option<(u64, bool)>>>,
    ) -> Result<(Vec<PanelInternal>, u64), FusionError> {
        let panel_started = Instant::now();
        let panel_total = u8::try_from(resolved.panels.len()).unwrap_or(u8::MAX);
        let initial_running_stage = FusionStage::RunningPanels {
            completed: 0,
            total: panel_total,
        };
        progress::emit(
            progress,
            initial_running_stage.clone(),
            None,
            initial_running_stage.label(),
        );
        for (index, _) in resolved.panels.iter().enumerate() {
            let mut md = fusion_event_metadata(request);
            md.insert("run_id".into(), AnalyticsValue::String(run_id.to_string()));
            md.insert(
                "panel_slot".into(),
                AnalyticsValue::Int(saturating_i64(index + 1)),
            );
            self.log_running_event(
                telemetry::tengu::fusion::PANEL_STARTED,
                md,
                &inherit.cancel,
                operational_deadline,
            )
            .await;
            if inherit.cancel.is_cancelled() {
                return Err(FusionError::Cancelled);
            }
            Self::ensure_operational_time(operational_deadline)?;
        }
        Self::ensure_operational_time(operational_deadline)?;
        // F005: `run_panels` emits `RunningPanels{completed:k,total}` (with the
        // finishing panel's anonymous id) after EVERY `join_next`, so the
        // longest stage of a run — up to `panel_total_timeout_ms` per panel —
        // is no longer a single stalled "0/N" progress event for its whole
        // duration.
        // [Round-4 rework, item 2] Bundles the three survives-a-drop cells
        // with everything `price_realized_usage` needs so `run_panels` can
        // refresh them from real, already-completed panels after EVERY
        // panel finishes, not just once the whole stage returns `Ok` — see
        // `panel::RealizedSpendSink`.
        let sink = panel::RealizedSpendSink {
            realized_tokens,
            resolved_egress,
            settlement,
            catalog: self.catalog.as_ref(),
            prices: self.prices.as_ref(),
            analyst: &resolved.analyst,
            synth_profile: &resolved.synthesizer.profile,
            synth_model: &resolved.synthesizer.model,
            request_prompt: &request.prompt,
            catalog_snapshot: Some(catalog_snapshot),
            reserved_max_nano_usd: facts
                .snapshot()
                .usage
                .map_or(0, |usage| usage.reserved_max_nano_usd),
            facts: facts.clone(),
            started,
        };
        let (admission, producer_drain) = match admission {
            Some(lease) => {
                let (permits, drain) = lease.into_parts();
                (Some(platform_api::PanelPoolLease::new(permits)), drain)
            }
            None => (None, None),
        };
        if let Some(drain) = &producer_drain {
            panel_tasks.set_producer_drain(drain.clone());
        }
        let panel_result = panel::run_panels_supervised(
            Arc::clone(&self.spawner),
            inherit,
            config,
            // [Finding 20] the early-abort bar must seal on the request's
            // own `partial_ok` opt-out too, not only the settings-level
            // default — see `run_panels`'s doc comment.
            request.partial_ok && config.partial_ok,
            &request.prompt,
            &resolved.panels,
            run_id,
            Self::remaining_until(operational_deadline),
            progress,
            Some(&sink),
            panel_tasks,
            admission,
        )
        .await;
        if let Some(fence) = &self.panel_fence {
            fence.close();
        }
        // Wrapper cancellation can detach runtime cancellation. Do not start
        // the analyst until the actual producers and their durable receipts
        // have drained; these tickets do not retain physical pool permits.
        if let Some(drain) = producer_drain {
            drain.wait().await;
        }
        if let Some(fence) = &self.panel_fence {
            if let Err(error) = fence.wait().await {
                facts.set_attempt_settlement(platform_api::FusionAttemptSettlementStatus::Failed {
                    reason: error.to_string(),
                });
                // Panels have already executed. A host diagnostic such as
                // InvalidConfiguration must not regain a preflight/zero-call
                // meaning and refund their lifetime spawn quota.
                return Err(FusionError::Internal);
            }
        }
        let mut panels = match panel_result {
            Ok(panels) => panels,
            Err(error) => return Err(error),
        };
        panel::anonymize(&mut panels, run_id);
        let panels_ms = millis_since(panel_started);
        for panel in &panels {
            let mut md = fusion_event_metadata(request);
            md.insert("run_id".into(), AnalyticsValue::String(run_id.to_string()));
            md.insert(
                "panel_id".into(),
                AnalyticsValue::String(panel.anonymous_id.clone()),
            );
            md.insert(
                "duration_ms".into(),
                AnalyticsValue::Int(saturating_i64(panel.duration_ms)),
            );
            md.insert(
                "status".into(),
                AnalyticsValue::String(
                    match panel.status {
                        PanelRunStatus::Completed => "completed",
                        PanelRunStatus::Failed => "failed",
                        PanelRunStatus::TimedOut => "timed_out",
                        PanelRunStatus::Cancelled => "cancelled",
                    }
                    .to_string(),
                ),
            );
            if let Some(error_category) = panel.error_category.clone() {
                md.insert(
                    "error_category".into(),
                    AnalyticsValue::String(error_category),
                );
            }
            if let Some(usage) = panel.usage.as_ref() {
                add_usage_metadata(&mut md, usage);
            }
            let event = if panel.status == PanelRunStatus::Completed {
                telemetry::tengu::fusion::PANEL_COMPLETED
            } else {
                telemetry::tengu::fusion::PANEL_FAILED
            };
            self.log_running_event(event, md, &inherit.cancel, operational_deadline)
                .await;
            if inherit.cancel.is_cancelled() {
                return Err(FusionError::Cancelled);
            }
            Self::ensure_operational_time(operational_deadline)?;
        }
        Ok((panels, panels_ms))
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_inner(
        &self,
        config: &FusionRuntimeConfig,
        request: FusionRequest,
        inherit: FusionInheritance,
        progress: Option<Sender<FusionProgress>>,
        run_id: String,
        started: Instant,
        operational_deadline: Instant,
        resolved: ResolvedSet,
        quote: Option<FusionQuote>,
        runtime_snapshot: Arc<FusionRuntimeSnapshot>,
        live_catalog: Arc<dyn ModelSource>,
        facts: FusionRunFactsRecorder,
        control: FusionRunControl,
        panel_tasks: panel::PanelTaskBarrier,
        // Review-round3 item 27 (rework): a plain local variable here is
        // invisible to `run()`'s outer `Err` handling once this whole
        // future is DROPPED (the outer cancel race) or abandoned
        // (`tokio::time::timeout` on the outer total-timeout race) — there
        // is no unwind, the stack (including a local `panels` binding) is
        // simply gone. This cell is the one piece of state that survives
        // that: `run()` allocates it, hands a clone in here, and reads
        // whatever was last written after the race decides the run.
        realized_tokens: Arc<Mutex<Option<u64>>>,
        // [Round-3 review B2, reworked] Same survives-a-drop cell as
        // `realized_tokens`, for the resolved egress profile list. Unlike
        // `realized_tokens` this is NOT latched from the `resolved` set
        // returned by `resolve_and_reserve` — that set is merely the
        // INTENDED panels, not the ones actually dispatched, and
        // `FusionProgress::egress_profiles`'s own contract (platform-api's
        // `fusion.rs`) promises "the provider profiles the request was
        // ACTUALLY dispatched to". It is written only once `run_panel_stage`
        // returns (below), from panels that made a real provider call, and
        // the analyst's profile is folded in only once `run_analyst_call`
        // actually issues that call — never on a path where dispatch never
        // happened (e.g. every panel rejected pre-allocation by the
        // spawner).
        resolved_egress: Arc<Mutex<Option<Vec<String>>>>,
        // [Round-4 review findings 1/2/3/19] Survives-a-drop cell holding
        // the reservation lease itself — see `resolve_and_reserve`'s
        // parameter doc. Whichever of this function's own commit points
        // runs (the `check_panel_bar`-failure arm below, or
        // `finalize_result`) takes the lease OUT of this cell to commit it;
        // if this whole future is dropped before either does, the lease is
        // still sitting here for `run()`'s outer `Err` arm to settle.
        lease_cell: Arc<Mutex<Option<SettlementLease>>>,
        // Priced-so-far settlement (nano-USD, estimated) paired with
        // `lease_cell` — refreshed at each stage boundary this function
        // reaches, so `run()`'s recovery commit (if it ever runs) uses the
        // most precise figure available. See `StageSettlement`, whose doc
        // comment tabulates what this cell holds at every boundary.
        settlement: Arc<Mutex<Option<(u64, bool)>>>,
    ) -> Result<FusionResult, FusionError> {
        Self::ensure_operational_time(operational_deadline)?;
        let request = validate_request(request)?;
        self.emit_started(
            &request,
            &resolved,
            &run_id,
            &inherit.cancel,
            operational_deadline,
        )
        .await;
        if inherit.cancel.is_cancelled() {
            return Err(FusionError::Cancelled);
        }
        Self::ensure_operational_time(operational_deadline)?;
        // Queue before any profile permit or monetary/output hold. A lease
        // remains local until every original panel consumes exactly one slot.
        let admission = if self.panel_admission {
            Some(
                self.spawner
                    .reserve_fusion_panel_group(
                        resolved.panels.len(),
                        operational_deadline,
                        inherit.cancel.clone(),
                    )
                    .await
                    .map_err(|error| {
                        if inherit.cancel.is_cancelled() {
                            FusionError::Cancelled
                        } else {
                            FusionError::PanelAdmissionRejected(error.to_string())
                        }
                    })?,
            )
        } else {
            None
        };
        if let Some(quote) = &quote {
            self.reserve_resolved(
                quote,
                &inherit,
                &progress,
                operational_deadline,
                &lease_cell,
                &settlement,
                &facts,
            )
            .await?;
        }
        facts.set_resolved_panels(u8::try_from(resolved.panels.len()).unwrap_or(u8::MAX));
        Self::ensure_operational_time(operational_deadline)?;
        let panel_routes: Vec<(&str, &str, bool)> = resolved
            .panels
            .iter()
            .map(|panel| (panel.profile.as_str(), panel.model.as_str(), false))
            .collect();
        let panel_route_refs: Vec<&ResolvedPanel> = resolved.panels.iter().collect();
        Self::ensure_live_config(
            self.config_source.as_ref(),
            &runtime_snapshot.config,
            &request,
            &panel_route_refs,
            "panel",
        )?;
        Self::ensure_live_routes(
            live_catalog.as_ref(),
            &runtime_snapshot.catalog,
            &panel_routes,
            config.panel_max_output_tokens_per_turn,
            "panel",
        )?;

        let (panels, panels_ms) = self
            .run_panel_stage(
                config,
                &request,
                &resolved,
                &runtime_snapshot.catalog,
                &inherit,
                &progress,
                &run_id,
                started,
                operational_deadline,
                &facts,
                &panel_tasks,
                admission,
                &realized_tokens,
                &resolved_egress,
                &settlement,
            )
            .await?;
        // [Round-5 review items 1/2/4] Every stage from here to
        // `finalize_result` refreshes both survives-a-drop money cells
        // through this one borrow — see `StageSettlement`'s stage table for
        // what each boundary is required to leave behind.
        let stage_settlement = StageSettlement {
            realized_tokens: &realized_tokens,
            settlement: &settlement,
            catalog: self.catalog.as_ref(),
            live_catalog: live_catalog.as_ref(),
            catalog_snapshot: &runtime_snapshot.catalog,
            prices: self.prices.as_ref(),
            analyst: &resolved.analyst,
            synthesizer: &resolved.synthesizer,
            request_prompt: &request.prompt,
            request: Some(&request),
            config: Some(config),
            side_query: Some(self.side_query.as_ref()),
            synth_estimate: Arc::new(Mutex::new(None)),
            panels: &panels,
            operational_deadline,
            cancel: inherit.cancel.clone(),
            reserved_max_nano_usd: facts
                .snapshot()
                .usage
                .map_or(0, |usage| usage.reserved_max_nano_usd),
            facts: facts.clone(),
            started,
        };
        // Panels are the earliest point in `run_inner` where real,
        // already-billed provider spend exists. Record it now so a cancel
        // or outer-timeout that later drops this future's own stack still
        // leaves `run()` able to report it — see the field doc above and
        // `run()`'s outer `Err` arm. The analyst has not run yet, so its
        // contribution here is exactly $0 — not an estimate.
        stage_settlement.refresh(None, false, None, false, false);
        // [Round-3 review B2, reworked] Latch the DISPATCHED egress set
        // (panels that made a real provider call — see
        // `dispatched_egress_profiles`) now, right after dispatch is known,
        // so an outer cancel/timeout that drops this future's own stack
        // during the analyst/synthesizer stage still reports it. A panel
        // set where nothing dispatched (every panel rejected pre-allocation)
        // latches `None`, matching the contract that this field discloses
        // only real egress.
        if let Ok(mut guard) = resolved_egress.lock() {
            *guard = dispatched_egress_profiles(&panels);
        }
        if let Err(error) = check_panel_bar(&panels, &request, config) {
            // Settling real panel spend is irreversible even when the
            // computation result is an error. Take the same finalization
            // claim as the success path before committing so a concurrent
            // TaskStop cannot replace this truthful terminal.
            if !control.begin_finalizing() {
                return Err(FusionError::Cancelled);
            }
            if self.attempt_run.is_some() {
                return Err(error);
            }
            // The bar failing (not enough successful panels, or an
            // incomplete set under `partial_ok: false`) does not mean
            // nothing was spent: any panel with `Some(usage)` already made a
            // real, billed provider call before the run gave up on it. Price
            // and commit that realized spend now, before propagating the
            // error — otherwise `lease`'s `Drop` only releases the hold and
            // those tokens are charged to nobody (never reach
            // `CostTracker::total_nano_usd`, so a `--max-budget` session
            // never sees them). A commit failure here leaves the lease armed
            // and falls back to `Drop`'s release, exactly as before this
            // settlement was added.
            let (priced_nano_usd, _estimated) = price_realized_usage(
                self.catalog.as_ref(),
                self.prices.as_ref(),
                &panels,
                &resolved.analyst,
                None,
                // The panel bar failed before `analyze_and_decide` ever ran
                // — the analyst and synthesizer are both genuinely un-called
                // here, so their missing usage must stay exact $0, never a
                // fabricated estimate.
                false,
                &resolved.synthesizer.profile,
                &resolved.synthesizer.model,
                None,
                false,
                &request.prompt,
            );
            // [Round-4 review findings 1/2/3/19] Take the lease back out of
            // the shared cell to commit it — see `resolve_and_reserve`'s
            // parameter doc for why it no longer lives as a plain local.
            if let Some(lease) = lease_cell.lock().ok().and_then(|mut guard| guard.take()) {
                let _ = lease.commit(priced_nano_usd).await;
            }

            // [Finding 12] The dollar side is now accounted for (the commit
            // above), but a caller tracking a SEPARATE token budget (e.g.
            // `local_workflow`'s `fusion()` bridge arm, whose shared `spent`
            // pool only advances on `Ok`) has no way to learn what this
            // errored run already billed — emit it on the progress channel
            // so such a caller can charge it before propagating the error.
            //
            // [Round-3 review B2, reworked] `egress_profiles` here is built
            // from `panels` directly (not read back from the `resolved_egress`
            // latch, though the two agree at this point) and deliberately
            // does NOT add `resolved.analyst.profile`: the panel bar failing
            // means `analyze_and_decide` never ran, so the analyst is
            // provably un-called on this arm — see the comment a few lines
            // above on the sibling `price_realized_usage` call. An
            // all-`"spawn"` panel set (nothing ever dispatched, e.g.
            // `AllPanelsFailedPreflight`) yields `None` here, matching the
            // "guarantees zero provider calls" contract on that error
            // variant.
            let realized_usage = aggregate_panel_usage(&panels);
            progress::emit_with_realized_tokens(
                &progress,
                FusionStage::Failed,
                "panel bar failed after real provider spend",
                realized_usage.output_tokens,
                dispatched_egress_profiles(&panels),
            );

            return Err(error);
        }

        let outcome = tokio::select! {
            biased;
            () = inherit.cancel.cancelled() => return Err(FusionError::Cancelled),
            outcome = self.analyze_and_decide(
                config,
                &request,
                &resolved,
                &panels,
                &progress,
                &run_id,
                started,
                &resolved_egress,
                &stage_settlement,
            ) => outcome,
        };

        self.finalize_result(
            outcome,
            &resolved,
            &request,
            &panels,
            panels_ms,
            &lease_cell,
            &progress,
            run_id,
            started,
            &control,
        )
        .await
    }

    /// `run_inner`'s tail: emit the terminal progress stage, compute the
    /// egress profile list, price + commit the actual spend against the
    /// lease, emit `COMPLETED` telemetry, and assemble the [`FusionResult`].
    /// Split out of `run_inner` purely to keep that function under the
    /// line-count lint — same telemetry, same ordering, same
    /// early-return-on-error shape.
    #[allow(clippy::too_many_arguments)]
    async fn finalize_result(
        &self,
        outcome: AnalysisOutcome,
        resolved: &ResolvedSet,
        request: &FusionRequest,
        panels: &[PanelInternal],
        panels_ms: u64,
        // [Round-4 review findings 1/2/3/19] Taken back out of the cell
        // below rather than owned outright — see `resolve_and_reserve`'s
        // parameter doc for why the lease lives in a shared cell instead of
        // being handed down as a plain local.
        lease_cell: &Arc<Mutex<Option<SettlementLease>>>,
        progress: &Option<Sender<FusionProgress>>,
        run_id: String,
        started: Instant,
        // The control phase is set before the FIRST irreversible side effect
        // below (the terminal `progress::emit`).
        control: &FusionRunControl,
    ) -> Result<FusionResult, FusionError> {
        if !control.begin_finalizing() {
            return Err(FusionError::Cancelled);
        }
        let AnalysisOutcome {
            decision,
            final_text,
            analysis,
            analyst_ms,
            synthesizer_ms,
            mut usage,
            priced_analyst,
            priced_synth,
            synth_attempted,
            analyst_usage_incomplete,
            analyst_attempted,
        } = outcome;

        let status = match &decision {
            FusionDecision::NeedsParent { .. } => FusionStatus::NeedsParent,
            FusionDecision::Picked { .. } | FusionDecision::Merged => FusionStatus::Completed,
        };
        let stage = match status {
            FusionStatus::Completed => FusionStage::Completed,
            FusionStatus::NeedsParent => FusionStage::NeedsParent,
        };
        if self.attempt_run.is_none() {
            progress::emit(progress, stage.clone(), None, stage.label());
        }

        // [Round-4 review finding 14] Built from the panels that were
        // ACTUALLY dispatched (`dispatched_egress_profiles`, which excludes
        // every panel for which no subagent was ever allocated —
        // `panel::is_never_dispatched_category`: `"spawn"` and
        // `"not_dispatched"`), not from `resolved.panels` — the merely
        // INTENDED set. Such a panel makes zero provider calls, so
        // including its profile here would falsely tell the user their
        // prompt reached a provider it never touched (this run still
        // completes when `partial_ok` covers the rejection).
        let mut egress: Vec<String> = dispatched_egress_profiles(panels).unwrap_or_default();
        if analyst_attempted {
            egress.push(resolved.analyst.profile.clone());
        }
        // `Merged` is only the SUCCESS outcome of the synthesizer stage —
        // `synthesize` issues its side query to `request.parent_profile`
        // (carrying the prompt, the full analysis, and every panel's
        // candidate answer/summary) on every attempt, including the two
        // failure arms below. The parent profile received that data whether
        // or not the call then succeeded, so it belongs in `egress_profiles`
        // on those arms too — otherwise a run whose synthesizer call failed
        // or timed out under-reports a provider that demonstrably received
        // every panel's answer.
        if synth_attempted {
            egress.push(resolved.synthesizer.profile.clone());
        }
        egress.sort();
        egress.dedup();

        // [Round-4 review findings 1/2/3/19] Take the lease back out of the
        // shared cell now — see `resolve_and_reserve`'s parameter doc.
        // Normally still `Some` here: nothing else takes it once
        // `check_panel_bar` has passed (its own failure arm, the only other
        // taker, would have already returned before reaching this
        // function). `None` would mean this future was somehow resumed
        // after having already been settled elsewhere — treat that as
        // already handled rather than panicking or double-committing.
        if self.attempt_run.is_none() {
            usage.reserved_max_nano_usd = lease_cell
                .lock()
                .ok()
                .and_then(|guard| guard.as_ref().map(|lease| lease.quote().reserved_nano_usd))
                .unwrap_or_default();
            let (priced_nano_usd, priced_estimated) = price_realized_usage(
                self.catalog.as_ref(),
                self.prices.as_ref(),
                panels,
                &resolved.analyst,
                priced_analyst.as_ref().map(|(u, calls)| (u, *calls)),
                analyst_attempted,
                &resolved.synthesizer.profile,
                &resolved.synthesizer.model,
                priced_synth.as_ref(),
                synth_attempted,
                &request.prompt,
            );
            usage.realized_nano_usd = priced_nano_usd;
            // Round-3 review finding 10: `analyst_usage_incomplete` catches the
            // case `price_realized_usage` cannot see on its own —
            // `priced_analyst` carrying REAL, non-empty usage that is still
            // known-short (a billed `InvalidResponse` attempt whose tokens
            // could not be captured, or an earlier attempt priced in place of a
            // discarded failure — see finding 8). Without this, `price_component`
            // succeeds on the real-but-short figure and `priced_estimated` never
            // fires, so the run would claim `estimated: false` over a number
            // that is silently missing real, already-billed spend.
            usage.estimated = usage.estimated || priced_estimated || analyst_usage_incomplete;
        }
        let facts = control.facts();
        if self.attempt_run.is_none() {
            facts.replace_usage(usage.clone(), usage.estimated);
        }
        for profile in &egress {
            facts.add_possible_egress(profile.clone());
        }
        // Keep the lease in the supervisor-owned cell until all panicable
        // pricing is complete. A pricing panic then leaves cleanup ownership
        // intact instead of detaching settlement through Drop.
        let lease = lease_cell.lock().ok().and_then(|mut guard| guard.take());
        if let Some(lease) = lease {
            lease.commit(usage.realized_nano_usd).await?
        }

        let timing = FusionTiming {
            total_ms: millis_since(started),
            panels_ms,
            analyst_ms,
            synthesizer_ms,
        };
        facts.set_timing(timing.clone());
        let result = FusionResult {
            schema_version: platform_api::FUSION_SCHEMA_VERSION,
            run_id,
            status,
            decision,
            final_text,
            analysis,
            panels: panels.iter().map(panel_outcome).collect(),
            usage,
            timing,
            egress_profiles: egress,
        };
        if self.attempt_run.is_none() {
            self.emit_completed(
                request,
                (
                    panels.len(),
                    successful(panels).len(),
                    panels
                        .iter()
                        .filter(|panel| panel.status != PanelRunStatus::Completed)
                        .count(),
                ),
                &result.run_id,
                &result.usage,
                &result.egress_profiles,
                &result.decision,
                &result.timing,
                control.deadline(),
            )
            .await;
        }

        Ok(result)
    }

    /// `finalize_result` helper: emit the `COMPLETED` telemetry event once
    /// the lease has committed. Split out purely to keep the caller under
    /// the line-count lint.
    #[allow(clippy::too_many_arguments)]
    async fn emit_completed(
        &self,
        request: &FusionRequest,
        panel_counts: (usize, usize, usize),
        run_id: &str,
        usage: &FusionUsage,
        egress: &[String],
        decision: &FusionDecision,
        timing: &FusionTiming,
        outer_deadline: Option<Instant>,
    ) {
        let decision_label = match decision {
            FusionDecision::Picked { .. } => "picked",
            FusionDecision::Merged => "merged",
            FusionDecision::NeedsParent { .. } => "needs_parent",
        };
        let mut completion_md = fusion_event_metadata(request);
        completion_md.insert("run_id".into(), AnalyticsValue::String(run_id.to_string()));
        for (key, count) in [
            ("panel_count", panel_counts.0),
            ("panel_success_count", panel_counts.1),
            ("panel_failed_count", panel_counts.2),
        ] {
            completion_md.insert(key.into(), AnalyticsValue::Int(saturating_i64(count)));
        }
        add_usage_metadata(&mut completion_md, usage);
        add_egress_metadata(&mut completion_md, egress);
        completion_md.insert(
            "decision".into(),
            AnalyticsValue::String(decision_label.to_string()),
        );
        completion_md.insert(
            "duration_ms".into(),
            AnalyticsValue::Int(saturating_i64(timing.total_ms)),
        );
        completion_md.insert(
            "panels_duration_ms".into(),
            AnalyticsValue::Int(saturating_i64(timing.panels_ms)),
        );
        completion_md.insert(
            "analysis_duration_ms".into(),
            AnalyticsValue::Int(saturating_i64(timing.analyst_ms)),
        );
        completion_md.insert(
            "synthesis_duration_ms".into(),
            AnalyticsValue::Int(saturating_i64(timing.synthesizer_ms)),
        );
        self.log_terminal_event(
            telemetry::tengu::fusion::COMPLETED,
            completion_md,
            outer_deadline,
        )
        .await;
    }

    /// `run_inner` stage 3/3: run the analyst, `interpret` its verdict, and
    /// (on `Merge`) run the synthesizer — all the telemetry and `NeedsParent`
    /// degradation paths for each. Split out of `run_inner` purely to keep
    /// that function under the line-count lint; unlike stages 1/2 this one
    /// never fails the run outright (every branch produces a decision, even
    /// if it is `NeedsParent`), so it returns a plain [`AnalysisOutcome`]
    /// rather than a `Result`.
    #[allow(clippy::too_many_arguments)]
    async fn analyze_and_decide(
        &self,
        config: &FusionRuntimeConfig,
        request: &FusionRequest,
        resolved: &ResolvedSet,
        panels: &[PanelInternal],
        progress: &Option<Sender<FusionProgress>>,
        run_id: &str,
        started: Instant,
        resolved_egress: &Arc<Mutex<Option<Vec<String>>>>,
        // [Round-5 review items 1/2/4] Refreshed at every boundary of this
        // stage and the synthesizer stage below it — see `StageSettlement`.
        stage_settlement: &StageSettlement<'_>,
    ) -> AnalysisOutcome {
        progress::emit(
            progress,
            FusionStage::Analyzing,
            None,
            FusionStage::Analyzing.label(),
        );
        let (analysis_outcome, analyst_ms, analyst_attempted) = self
            .run_analyst_call(
                config,
                request,
                resolved,
                panels,
                started,
                resolved_egress,
                stage_settlement,
            )
            .await;

        let mut usage = aggregate_panel_usage(panels);
        // Captured inside the match arms below so `price_realized_usage` (run
        // after the decision is known) can price the analyst/synth calls
        // against their OWN model/profile — a session-wide CostTracker delta
        // cannot tell Fusion's spend apart from a concurrent parent turn's.
        let mut priced_analyst: Option<(cost::Usage, u32)> = None;
        let mut priced_synth: Option<cost::Usage> = None;
        let mut synth_attempted = false;
        // Round-3 review findings 8/10: `true` once ANY analyst usage is
        // known-incomplete — set by `record_failed_analyst_usage` on every
        // error exit, or copied from `AnalystUsage::incomplete` on success
        // (an `InvalidResponse` attempt that was retried into a decode).
        let mut analyst_usage_incomplete = false;
        let (decision, final_text, analysis, synthesizer_ms) = match analysis_outcome {
            Err((AnalystError::ParseFailed, acc)) => {
                Self::record_failed_analyst_usage(
                    acc,
                    &mut usage,
                    &mut priced_analyst,
                    &mut analyst_usage_incomplete,
                    analyst_attempted,
                    stage_settlement,
                );
                self.analysis_failed_outcome(
                    request,
                    panels,
                    run_id,
                    analyst_ms,
                    FusionNeedsParentReason::AnalysisParseFailed,
                    "analysis_parse_failed",
                    "analyst JSON could not be parsed",
                    stage_settlement,
                )
                .await
            }
            // F004: previously a hard `Err` after every panel had already been
            // paid for. Reachable only when the catalog's `judge_eligible`
            // hint was wrong for the model `model_resolver::resolve` picked
            // (the preflight check there is the normal gate) — degrade to
            // NeedsParent like every other post-panel analyst failure instead
            // of throwing the panel material away.
            Err((AnalystError::Unsupported, acc)) => {
                Self::record_failed_analyst_usage(
                    acc,
                    &mut usage,
                    &mut priced_analyst,
                    &mut analyst_usage_incomplete,
                    analyst_attempted,
                    stage_settlement,
                );
                self.analysis_failed_outcome(
                    request,
                    panels,
                    run_id,
                    analyst_ms,
                    FusionNeedsParentReason::AnalysisFailed {
                        category: "structured_output_unsupported".into(),
                    },
                    "structured_output_unsupported",
                    "analyst structured output is unsupported",
                    stage_settlement,
                )
                .await
            }
            Err((AnalystError::Failed(category), acc)) => {
                Self::record_failed_analyst_usage(
                    acc,
                    &mut usage,
                    &mut priced_analyst,
                    &mut analyst_usage_incomplete,
                    analyst_attempted,
                    stage_settlement,
                );
                self.analysis_failed_outcome(
                    request,
                    panels,
                    run_id,
                    analyst_ms,
                    FusionNeedsParentReason::AnalysisFailed {
                        category: category.clone(),
                    },
                    "analysis_failed",
                    &format!("analyst call failed: {category}"),
                    stage_settlement,
                )
                .await
            }
            Ok((analysis, acc)) => {
                analyst_usage_incomplete = acc.incomplete || acc.unreported_calls > 0;
                let analyst_usage = stage_settlement.analyst_usage_with_missing_estimates(&acc);
                self.handle_analyst_success(
                    config,
                    request,
                    panels,
                    progress,
                    run_id,
                    started,
                    analyst_ms,
                    analysis,
                    analyst_usage,
                    acc.calls,
                    &mut usage,
                    &mut priced_analyst,
                    &mut priced_synth,
                    &mut synth_attempted,
                    analyst_usage_incomplete,
                    stage_settlement,
                )
                .await
            }
        };

        AnalysisOutcome {
            decision,
            final_text,
            analysis,
            analyst_ms,
            synthesizer_ms,
            usage,
            priced_analyst,
            priced_synth,
            synth_attempted,
            analyst_usage_incomplete,
            analyst_attempted,
        }
    }

    /// `analyze_and_decide` helper (round-3 review finding 8): when the
    /// analyst call ultimately failed, price whatever REAL usage `acc`
    /// accumulated before the error — an earlier attempt in the same retry
    /// loop can have billed the provider and decoded valid JSON before a
    /// LATER attempt failed — instead of discarding it and forcing
    /// `price_realized_usage`'s fallback to substitute a coarse, input-only,
    /// zero-output byte estimate for money that is already known. `estimated`
    /// is still forced true unconditionally either way: `acc.incomplete`
    /// (an `InvalidResponse` attempt's own tokens are unrecoverable) may
    /// already be set, and even when it is not, the failing FINAL attempt's
    /// own usage — if it billed at all — is by definition not reflected in
    /// `acc.usage` for a non-decode-failure error (timeout, unsupported, or
    /// a transport/4xx/5xx arm never had usage to capture in the first
    /// place). This never reports a knowingly incomplete total as exact.
    ///
    /// [Round-4 review finding 4] Also rolls `acc.usage`/`acc.calls` into
    /// the run's `FusionUsage` token/request rollup via `add_cost_usage` —
    /// the same call the SUCCESS path (`handle_analyst_success`) makes.
    /// Without this, a failed analyst call whose earlier attempt(s) DID
    /// decode a real, usage-bearing response left `realized_nano_usd`
    /// (priced from `priced_analyst`, via `price_realized_usage`) counting
    /// tokens that `input_tokens`/`output_tokens`/`provider_requests` never
    /// did — so `<usage>` and a workflow-tracked token budget both
    /// under-reported the very tokens the run was just billed for.
    fn record_failed_analyst_usage(
        acc: AnalystUsage,
        usage: &mut FusionUsage,
        priced_analyst: &mut Option<(cost::Usage, u32)>,
        analyst_usage_incomplete: &mut bool,
        analyst_attempted: bool,
        // [Round-5 review items 1/2/4] A failed analyst call still ran:
        // whatever real usage `acc` recovered must reach the
        // survives-a-drop cells too, or a cancel/timeout landing in the
        // NeedsParent degradation below (which still emits telemetry and
        // builds a summary) would settle the panel-only figure.
        stage_settlement: &StageSettlement<'_>,
    ) {
        *analyst_usage_incomplete = analyst_attempted;
        if acc.calls > 0 {
            let settlement_usage = stage_settlement.analyst_usage_with_missing_estimates(&acc);
            add_cost_usage(usage, &settlement_usage, acc.calls);
            *priced_analyst = Some((settlement_usage, acc.calls));
        }
        stage_settlement.refresh(
            priced_analyst
                .as_ref()
                .map(|(usage, calls)| (usage, *calls)),
            analyst_attempted,
            None,
            false,
            analyst_attempted,
        );
    }

    /// `analyze_and_decide` helper: the `Ok` arm of the analyst-call match —
    /// price the analyst usage, emit `ANALYSIS_COMPLETED`, `interpret` the
    /// verdict, and dispatch `Pick`/`NeedsParent`/`Merge`. Split out purely
    /// to keep the caller under the line-count lint — same telemetry, same
    /// ordering.
    #[allow(clippy::too_many_arguments)]
    async fn handle_analyst_success(
        &self,
        config: &FusionRuntimeConfig,
        request: &FusionRequest,
        panels: &[PanelInternal],
        progress: &Option<Sender<FusionProgress>>,
        run_id: &str,
        started: Instant,
        analyst_ms: u64,
        analysis: FusionAnalysis,
        analyst_usage: cost::Usage,
        analyst_calls: u32,
        usage: &mut FusionUsage,
        priced_analyst: &mut Option<(cost::Usage, u32)>,
        priced_synth: &mut Option<cost::Usage>,
        synth_attempted: &mut bool,
        analyst_usage_incomplete: bool,
        stage_settlement: &StageSettlement<'_>,
    ) -> (FusionDecision, String, Option<FusionAnalysis>, u64) {
        add_cost_usage(usage, &analyst_usage, analyst_calls);
        *priced_analyst = Some((analyst_usage, analyst_calls));
        // [Round-5 review items 1/2/4] The analyst's EXACT, provider-
        // reported usage is known right here. Publish it to the
        // survives-a-drop cells before the verdict is even interpreted —
        // everything below this line (the `interpret` match, the whole
        // synthesizer stage) is a window in which an outer cancel drops
        // `run_inner`'s stack, and until this refresh existed that window
        // committed the panel-only figure and billed these tokens to
        // nobody.
        stage_settlement.refresh(
            Some((&analyst_usage, analyst_calls)),
            true,
            None,
            false,
            analyst_usage_incomplete,
        );

        let mut md = fusion_event_metadata(request);
        md.insert("run_id".into(), AnalyticsValue::String(run_id.to_string()));
        add_panel_counts(&mut md, panels);
        md.insert(
            "duration_ms".into(),
            AnalyticsValue::Int(saturating_i64(analyst_ms)),
        );
        add_usage_metadata(&mut md, usage);
        self.log_running_event(
            telemetry::tengu::fusion::ANALYSIS_COMPLETED,
            md,
            &stage_settlement.cancel,
            stage_settlement.operational_deadline,
        )
        .await;
        match interpret(&analysis, panels) {
            HostDecision::Pick { panel_id } => {
                progress::emit(
                    progress,
                    FusionStage::Selecting,
                    Some(panel_id.clone()),
                    FusionStage::Selecting.label(),
                );
                let text = panel_by_id(panels, &panel_id)
                    .and_then(|panel| panel.report.as_ref())
                    .map_or_else(
                        || {
                            needs_parent_text(
                                panels,
                                "picked panel had no candidate",
                                Some(&analysis),
                            )
                        },
                        |report| report.candidate_answer.clone(),
                    );
                (FusionDecision::Picked { panel_id }, text, Some(analysis), 0)
            }
            HostDecision::NeedsParent { reason } => {
                let summary = needs_parent_text(panels, &reason_line(&reason), Some(&analysis));
                (
                    FusionDecision::NeedsParent { reason },
                    summary,
                    Some(analysis),
                    0,
                )
            }
            HostDecision::Merge => {
                self.run_synthesis(
                    config,
                    request,
                    analysis,
                    panels,
                    progress,
                    run_id,
                    started,
                    usage,
                    priced_synth,
                    synth_attempted,
                    stage_settlement,
                    // `cost::Usage` is `Copy`; the synthesizer stage needs
                    // the analyst's figure so its own refreshes keep
                    // reporting it rather than dropping back to the
                    // panel-only total.
                    Some((analyst_usage, analyst_calls)),
                    analyst_usage_incomplete,
                )
                .await
            }
        }
    }

    /// `analyze_and_decide` helper: the timeout-bounded analyst call itself.
    /// Split out purely to keep the caller under the line-count lint.
    #[allow(clippy::too_many_arguments)]
    async fn run_analyst_call(
        &self,
        config: &FusionRuntimeConfig,
        request: &FusionRequest,
        resolved: &ResolvedSet,
        panels: &[PanelInternal],
        _started: Instant,
        resolved_egress: &Arc<Mutex<Option<Vec<String>>>>,
        stage_settlement: &StageSettlement<'_>,
    ) -> (
        Result<(FusionAnalysis, AnalystUsage), (AnalystError, AnalystUsage)>,
        u64,
        bool,
    ) {
        let operational_deadline = stage_settlement.operational_deadline;
        if operational_deadline <= Instant::now() {
            return (
                Err((
                    AnalystError::Failed("timeout".into()),
                    AnalystUsage::default(),
                )),
                0,
                false,
            );
        }
        let analyst_route_refs = [&resolved.analyst];
        if Self::ensure_live_config(
            self.config_source.as_ref(),
            config,
            request,
            &analyst_route_refs,
            "analyst",
        )
        .is_err()
        {
            return (
                Err((
                    AnalystError::Failed("settings_restricted".into()),
                    AnalystUsage::default(),
                )),
                0,
                false,
            );
        }
        if Self::ensure_live_routes(
            stage_settlement.live_catalog,
            stage_settlement.catalog_snapshot,
            &[(
                resolved.analyst.profile.as_str(),
                resolved.analyst.model.as_str(),
                true,
            )],
            config.analyst_max_output_tokens,
            "analyst",
        )
        .is_err()
        {
            return (
                Err((
                    AnalystError::Failed("route_restricted".into()),
                    AnalystUsage::default(),
                )),
                0,
                false,
            );
        }
        let analyst_limits = stage_settlement
            .catalog_snapshot
            .limits_for(&resolved.analyst.profile, &resolved.analyst.model)
            .unwrap_or_else(crate::model_resolver::ModelLimits::unknown);
        if let Err(error) = crate::analyst::preflight_request(
            self.side_query.as_ref(),
            config,
            request,
            &resolved.analyst,
            panels,
            analyst_limits,
        ) {
            return (
                Err((
                    AnalystError::Failed(error.category().into()),
                    AnalystUsage::default(),
                )),
                0,
                false,
            );
        }
        // [Round-3 review B2, reworked] Fold the analyst's profile into the
        // survives-a-drop egress latch right here, before the call below is
        // issued — this is the one place in `run_inner`'s call graph where
        // the analyst call is actually about to be dispatched. An outer
        // cancel/timeout that drops `run_inner`'s future while this call is
        // in flight now correctly reports the analyst profile as reached;
        // nothing before this point ever adds it.
        if let Ok(mut guard) = resolved_egress.lock() {
            let mut egress = guard.clone().unwrap_or_default();
            if !egress
                .iter()
                .any(|profile| profile == &resolved.analyst.profile)
            {
                egress.push(resolved.analyst.profile.clone());
                egress.sort();
            }
            *guard = Some(egress);
        }
        // [Round-5 review items 1/2/4] Same reasoning as the egress latch
        // directly above, for the money: from the next line on, the analyst
        // call is in flight and its input (the task prompt plus every
        // successful panel report) has been egressed and will be billed. An
        // outer cancel/timeout dropping `run_inner`'s stack while it streams
        // used to settle the panel-only figure — exact $0 for a call that
        // was demonstrably made. `analyst_attempted: true` with no usage yet
        // is precisely `price_realized_usage`'s "attempted, usage unknown"
        // case: it prices `judge_input_token_estimate` and flags `estimated`.
        stage_settlement.refresh(None, true, None, false, true);
        let analyst_started = Instant::now();
        // F004: bound the analyst stage by what actually remains of the
        // end-to-end deadline, not just its own `analystTimeoutMs` budget —
        // `analyze`'s own internal retry loop can otherwise run past `total`
        // before the outer `run()` timeout ever gets polled (nested
        // `tokio::time::timeout`s always poll their inner future first, so
        // this always resolves before — never after — that outer wrapper).
        // Reusing `AnalystError::Failed("timeout")` here folds this into the
        // SAME NeedsParent handling as `analyze`'s own per-attempt timeout,
        // below. This outer timeout races the WHOLE `analyze` call, so on
        // expiry there is no accumulator to recover — `analyze` itself is
        // cancelled mid-flight and `AnalystUsage::default()` (empty) is the
        // honest answer; `record_failed_analyst_usage` still marks the run
        // `estimated` for it.
        let latest = Arc::new(Mutex::new(AnalystUsage::default()));
        let observed_latest = Arc::clone(&latest);
        let mut observe = |snapshot: &AnalystUsage, incomplete: bool| {
            if let Ok(mut latest) = observed_latest.lock() {
                *latest = snapshot.clone();
            }
            let settlement_usage = stage_settlement.analyst_usage_with_missing_estimates(snapshot);
            let analyst_usage = (snapshot.calls > 0).then_some((&settlement_usage, snapshot.calls));
            stage_settlement.refresh(
                analyst_usage,
                true,
                None,
                false,
                incomplete || snapshot.incomplete || snapshot.unreported_calls > 0,
            );
        };
        let analysis_outcome = tokio::select! {
            biased;
            () = tokio::time::sleep_until(operational_deadline) => {
                let latest = latest
                    .lock()
                    .map_or_else(|poisoned| poisoned.into_inner().clone(), |latest| latest.clone());
                Err((AnalystError::Failed("timeout".into()), latest))
            }
            outcome = crate::analyst::analyze_registered(
                Arc::clone(&self.side_query),
                config,
                request,
                &resolved.analyst,
                panels,
                analyst_limits,
                &mut observe,
                self.attempt_run.as_deref(),
            ) => outcome,
        };
        let attempted = latest
            .lock()
            .map(|snapshot| snapshot.calls > 0)
            .unwrap_or(true);
        (analysis_outcome, millis_since(analyst_started), attempted)
    }

    /// `run_synthesis` helper: the `SYNTHESIS_FAILED` telemetry both
    /// failure arms emit — identical metadata, only `error` differs. Split
    /// out to keep `run_synthesis` under the line-count lint.
    async fn emit_synthesis_failed(
        &self,
        request: &FusionRequest,
        panels: &[PanelInternal],
        run_id: &str,
        synthesizer_ms: u64,
        error_label: &str,
        stage_settlement: &StageSettlement<'_>,
    ) {
        let mut md = fusion_event_metadata(request);
        md.insert("run_id".into(), AnalyticsValue::String(run_id.to_string()));
        add_panel_counts(&mut md, panels);
        md.insert(
            "duration_ms".into(),
            AnalyticsValue::Int(saturating_i64(synthesizer_ms)),
        );
        md.insert(
            "error".into(),
            AnalyticsValue::String(error_label.to_string()),
        );
        self.log_running_event(
            telemetry::tengu::fusion::SYNTHESIS_FAILED,
            md,
            &stage_settlement.cancel,
            stage_settlement.operational_deadline,
        )
        .await;
    }

    /// `run_synthesis` helper [Round-5 review item 9]: a synthesizer call
    /// that FAILED can still have been billed — an empty-text completion
    /// (all reasoning, or a refusal) and a `SideQueryError::Partial` both
    /// arrive carrying the provider's own `cost::Usage`. Roll it into the
    /// run's token/request totals and into the survives-a-drop settlement
    /// cell exactly the way `record_failed_analyst_usage` does for the
    /// analyst, instead of leaving `priced_synth` at `None` and letting
    /// `price_realized_usage` re-price a known call as an input-only
    /// estimate with output/cache/reasoning hard-coded to 0.
    ///
    /// `usage.estimated` is forced regardless: a failed call's figure is
    /// never claimed as this run's exact total (a `Partial` is short by
    /// construction, and a timeout/transport arm hands back an empty usage
    /// that `price_realized_usage`'s `synth_attempted` fallback then has to
    /// estimate).
    fn record_failed_synth_usage(
        lost_usage: cost::Usage,
        usage: &mut FusionUsage,
        priced_synth: &mut Option<cost::Usage>,
        stage_settlement: &StageSettlement<'_>,
        analyst_for_refresh: Option<(&cost::Usage, u32)>,
        analyst_usage_incomplete: bool,
    ) {
        usage.estimated = true;
        if lost_usage.total_tokens() == 0 {
            return;
        }
        add_cost_usage(usage, &lost_usage, 1);
        *priced_synth = Some(lost_usage);
        stage_settlement.refresh(
            analyst_for_refresh,
            true,
            Some(&lost_usage),
            true,
            analyst_usage_incomplete,
        );
    }

    /// `analyze_and_decide` helper: the shared `NeedsParent` degrade+telemetry
    /// path for the three `AnalystError` arms (parse failure, structured
    /// output unsupported, and any other analyst call failure). Split out
    /// purely to keep the caller under the line-count lint — same telemetry,
    /// same event name, same `(decision, text, analysis, synthesizer_ms)`
    /// shape every other `analyze_and_decide` arm returns.
    #[allow(clippy::too_many_arguments)]
    async fn analysis_failed_outcome(
        &self,
        request: &FusionRequest,
        panels: &[PanelInternal],
        run_id: &str,
        analyst_ms: u64,
        reason: FusionNeedsParentReason,
        error_label: &str,
        message: &str,
        stage_settlement: &StageSettlement<'_>,
    ) -> (FusionDecision, String, Option<FusionAnalysis>, u64) {
        let mut md = fusion_event_metadata(request);
        md.insert("run_id".into(), AnalyticsValue::String(run_id.to_string()));
        add_panel_counts(&mut md, panels);
        md.insert(
            "duration_ms".into(),
            AnalyticsValue::Int(saturating_i64(analyst_ms)),
        );
        md.insert(
            "error".into(),
            AnalyticsValue::String(error_label.to_string()),
        );
        self.log_running_event(
            telemetry::tengu::fusion::ANALYSIS_FAILED,
            md,
            &stage_settlement.cancel,
            stage_settlement.operational_deadline,
        )
        .await;
        (
            FusionDecision::NeedsParent { reason },
            needs_parent_text(panels, message, None),
            None,
            0,
        )
    }

    /// `analyze_and_decide` helper: the `HostDecision::Merge` branch — run
    /// the synthesizer (timeout-bounded the same way the analyst call is)
    /// and its `NeedsParent` degradation paths. Split out purely to keep the
    /// caller under the line-count lint — same telemetry, same ordering.
    #[allow(clippy::too_many_arguments)]
    async fn run_synthesis(
        &self,
        config: &FusionRuntimeConfig,
        request: &FusionRequest,
        analysis: FusionAnalysis,
        panels: &[PanelInternal],
        progress: &Option<Sender<FusionProgress>>,
        run_id: &str,
        _started: Instant,
        usage: &mut FusionUsage,
        priced_synth: &mut Option<cost::Usage>,
        synth_attempted: &mut bool,
        stage_settlement: &StageSettlement<'_>,
        // The analyst's already-known usage, so every refresh below keeps
        // reporting it instead of regressing to the panel-only total.
        priced_analyst: Option<(cost::Usage, u32)>,
        analyst_usage_incomplete: bool,
    ) -> (FusionDecision, String, Option<FusionAnalysis>, u64) {
        let operational_deadline = stage_settlement.operational_deadline;
        if operational_deadline <= Instant::now() {
            self.emit_synthesis_failed(
                request,
                panels,
                run_id,
                0,
                "synthesis_timed_out",
                stage_settlement,
            )
            .await;
            return (
                FusionDecision::NeedsParent {
                    reason: FusionNeedsParentReason::SynthesisTimedOut,
                },
                needs_parent_text(panels, "synthesizer timed out", Some(&analysis)),
                Some(analysis),
                0,
            );
        }
        let synth_route_refs = [stage_settlement.synthesizer];
        if Self::ensure_live_config(
            self.config_source.as_ref(),
            config,
            request,
            &synth_route_refs,
            "synthesizer",
        )
        .is_err()
        {
            self.emit_synthesis_failed(
                request,
                panels,
                run_id,
                0,
                "settings_restricted",
                stage_settlement,
            )
            .await;
            return (
                FusionDecision::NeedsParent {
                    reason: FusionNeedsParentReason::SynthesisFailed,
                },
                needs_parent_text(
                    panels,
                    "synthesizer settings were restricted after preparation",
                    Some(&analysis),
                ),
                Some(analysis),
                0,
            );
        }
        if Self::ensure_live_routes(
            stage_settlement.live_catalog,
            stage_settlement.catalog_snapshot,
            &[(
                stage_settlement.synthesizer.profile.as_str(),
                stage_settlement.synthesizer.model.as_str(),
                false,
            )],
            config.synthesizer_max_output_tokens,
            "synthesizer",
        )
        .is_err()
        {
            self.emit_synthesis_failed(
                request,
                panels,
                run_id,
                0,
                "route_restricted",
                stage_settlement,
            )
            .await;
            return (
                FusionDecision::NeedsParent {
                    reason: FusionNeedsParentReason::SynthesisFailed,
                },
                needs_parent_text(
                    panels,
                    "synthesizer route was restricted after preparation",
                    Some(&analysis),
                ),
                Some(analysis),
                0,
            );
        }
        let synth_limits = stage_settlement
            .catalog_snapshot
            .limits_for(
                &stage_settlement.synthesizer.profile,
                &stage_settlement.synthesizer.model,
            )
            .unwrap_or_else(crate::model_resolver::ModelLimits::unknown);
        if crate::synthesizer::preflight_request(
            self.side_query.as_ref(),
            config,
            request,
            stage_settlement.synthesizer,
            &analysis,
            panels,
            synth_limits,
        )
        .is_err()
        {
            self.emit_synthesis_failed(
                request,
                panels,
                run_id,
                0,
                "synthesis_input_too_large",
                stage_settlement,
            )
            .await;
            return (
                FusionDecision::NeedsParent {
                    reason: FusionNeedsParentReason::SynthesisFailed,
                },
                needs_parent_text(
                    panels,
                    "synthesizer input does not fit the prepared parent route",
                    Some(&analysis),
                ),
                Some(analysis),
                0,
            );
        }
        stage_settlement.capture_synth_estimate(&analysis);
        // T1 item 1: set unconditionally, before the call and its own
        // timeout race — every path below (success, timeout, failure) is a
        // genuine ATTEMPT that may have reached and been billed by the
        // parent provider, unlike the `Pick`/`NeedsParent` analyst branches
        // that never call this function at all. This is the only signal
        // `price_realized_usage` has to tell "never ran, $0 is exact" apart
        // from "ran and lost its usage, needs the settlement estimate".
        *synth_attempted = true;
        // [Round-5 review items 1/2/4] The synthesizer's payload (the
        // prompt, the whole analysis and every panel's candidate answer)
        // reaches the parent provider on every path below, so publish the
        // attempt to the survives-a-drop cells BEFORE the call: a cancel
        // landing mid-synthesis then settles panels + analyst + the
        // synthesizer's estimated input rather than the panel-only figure.
        let analyst_for_refresh = priced_analyst
            .as_ref()
            .map(|(usage, calls)| (usage, *calls));
        stage_settlement.refresh(
            analyst_for_refresh,
            true,
            None,
            true,
            analyst_usage_incomplete,
        );
        progress::emit(
            progress,
            FusionStage::Synthesizing,
            None,
            FusionStage::Synthesizing.label(),
        );
        let synth_started = Instant::now();
        // F004: same remaining-budget bound as the analyst stage above, so a
        // hanging synthesizer degrades to NeedsParent (SynthesisTimedOut,
        // which already exists) rather than letting the run blow past
        // `total_timeout_ms` and lose everything to the outer
        // `TimedOutEmpty`.
        let synth = tokio::select! {
            biased;
            () = tokio::time::sleep_until(operational_deadline) => {
                Err((SynthError::TimedOut, cost::Usage::default()))
            }
            outcome = crate::synthesizer::synthesize_registered(
                Arc::clone(&self.side_query),
                config,
                request,
                stage_settlement.synthesizer,
                &analysis,
                panels,
                synth_limits,
                self.attempt_run.as_deref(),
            ) => outcome,
        };
        let synthesizer_ms = millis_since(synth_started);
        match synth {
            Ok((text, synth_usage)) => {
                add_cost_usage(usage, &synth_usage, 1);
                *priced_synth = Some(synth_usage);
                // [Round-5 review items 1/2/4] The synthesizer's exact usage
                // is known here; publish it before the telemetry below, so
                // a cancel racing `finalize_result` settles the real figure.
                stage_settlement.refresh(
                    analyst_for_refresh,
                    true,
                    Some(&synth_usage),
                    true,
                    analyst_usage_incomplete,
                );
                let mut md = fusion_event_metadata(request);
                md.insert("run_id".into(), AnalyticsValue::String(run_id.to_string()));
                add_panel_counts(&mut md, panels);
                md.insert(
                    "duration_ms".into(),
                    AnalyticsValue::Int(saturating_i64(synthesizer_ms)),
                );
                add_usage_metadata(&mut md, usage);
                self.log_running_event(
                    telemetry::tengu::fusion::SYNTHESIS_COMPLETED,
                    md,
                    &stage_settlement.cancel,
                    stage_settlement.operational_deadline,
                )
                .await;
                (FusionDecision::Merged, text, Some(analysis), synthesizer_ms)
            }
            Err((error, lost_usage)) => {
                let mut lost_usage = lost_usage;
                if error != SynthError::InvalidCitations
                    && lost_usage.total_tokens() == 0
                    && self.side_query.has_canonical_estimator()
                {
                    if let Ok(input_tokens) = crate::synthesizer::estimate_input_tokens(
                        self.side_query.as_ref(),
                        config,
                        request,
                        stage_settlement.synthesizer,
                        &analysis,
                        panels,
                        synth_limits,
                    ) {
                        lost_usage.tokens.input = input_tokens;
                    }
                }
                Self::record_failed_synth_usage(
                    lost_usage,
                    usage,
                    priced_synth,
                    stage_settlement,
                    analyst_for_refresh,
                    analyst_usage_incomplete,
                );
                let (reason, error_label, message) = match error {
                    SynthError::InvalidCitations => (
                        FusionNeedsParentReason::SynthesisFailed,
                        "citation_integrity",
                        "synthesizer cited evidence not authorized by the final request",
                    ),
                    SynthError::TimedOut => (
                        FusionNeedsParentReason::SynthesisTimedOut,
                        "synthesis_timed_out",
                        "synthesizer timed out",
                    ),
                    SynthError::Failed => (
                        FusionNeedsParentReason::SynthesisFailed,
                        "synthesis_failed",
                        "synthesizer failed",
                    ),
                };
                self.emit_synthesis_failed(
                    request,
                    panels,
                    run_id,
                    synthesizer_ms,
                    error_label,
                    stage_settlement,
                )
                .await;
                (
                    FusionDecision::NeedsParent { reason },
                    needs_parent_text(panels, message, Some(&analysis)),
                    Some(analysis),
                    synthesizer_ms,
                )
            }
        }
    }

    /// Settle registered accounting without replacing the computation outcome.
    async fn settle_registered_attempts(
        finalizer: Box<dyn crate::FusionAttemptFinalizer>,
        facts: &FusionRunFactsRecorder,
    ) -> Option<crate::FusionAttemptSummary> {
        // Capture BOTH synchronous ownership transfer and polling panics.
        // The host Drop contract still owns cleanup/freezing; catching a panic
        // cannot prove settlement or manufacture a replacement usage summary.
        let settled = CatchPanicFuture {
            inner: Box::pin(async move { finalizer.finish().wait().await }),
        }
        .await;
        let (summary, status) = match settled {
            Ok(Ok(summary)) => (
                summary,
                platform_api::FusionAttemptSettlementStatus::Settled,
            ),
            Ok(Err(failure)) => (
                failure.summary,
                platform_api::FusionAttemptSettlementStatus::Failed {
                    reason: failure.error.to_string(),
                },
            ),
            Err(()) => {
                facts.set_attempt_settlement(platform_api::FusionAttemptSettlementStatus::Failed {
                    reason: "attempt settlement owner panicked".into(),
                });
                return None;
            }
        };
        facts.replace_usage(summary.usage.clone(), summary.usage.estimated);
        facts.replace_attempts(Some(summary.usage.provider_requests));
        facts.replace_egress(
            summary.confirmed_egress.clone(),
            summary.possible_egress.clone(),
        );
        facts.set_attempt_settlement(status);
        Some(summary)
    }

    /// Supervise activation, producer drain, settlement, and terminal result.
    #[allow(clippy::too_many_arguments)]
    async fn run_prepared_snapshot(
        &self,
        config: FusionRuntimeConfig,
        request: FusionRequest,
        inherit: FusionInheritance,
        progress: Option<Sender<FusionProgress>>,
        control: FusionRunControl,
        facts: FusionRunFactsRecorder,
        resolved: ResolvedSet,
        quote: Option<FusionQuote>,
        runtime_snapshot: Arc<FusionRuntimeSnapshot>,
        live_catalog: Arc<dyn ModelSource>,
        started: Instant,
        attempt_finalizer: Option<Box<dyn crate::FusionAttemptFinalizer>>,
    ) -> Result<FusionResult, FusionError> {
        let run_id = control.identity().run_id.to_string();
        let deadline = control.deadline().ok_or(FusionError::Internal)?;
        let operational_deadline = Self::operational_deadline(&config, started)?;
        if operational_deadline <= Instant::now() || deadline <= Instant::now() {
            facts.set_timing(FusionTiming {
                total_ms: millis_since(started),
                ..FusionTiming::default()
            });
            if let Some(finalizer) = attempt_finalizer {
                let _ = Self::settle_registered_attempts(finalizer, &facts).await;
            }
            return Err(FusionError::TimedOutEmpty);
        }
        let realized_tokens: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
        let resolved_egress: Arc<Mutex<Option<Vec<String>>>> = Arc::new(Mutex::new(None));
        let lease_cell: Arc<Mutex<Option<SettlementLease>>> = Arc::new(Mutex::new(None));
        let settlement: Arc<Mutex<Option<(u64, bool)>>> = Arc::new(Mutex::new(None));
        let mut panel_tasks = panel::PanelTaskBarrier::default();
        panel_tasks.attempt_run = self.attempt_run.clone();
        panel_tasks.panel_fence = self.panel_fence.clone();
        let cancel = control.cancel();
        let mut run_future = Box::pin(CatchPanicFuture {
            inner: Box::pin(self.run_inner(
                &config,
                request.clone(),
                inherit,
                progress.clone(),
                run_id.clone(),
                started,
                operational_deadline,
                resolved,
                quote,
                runtime_snapshot,
                live_catalog,
                facts.clone(),
                control.clone(),
                panel_tasks.clone(),
                Arc::clone(&realized_tokens),
                Arc::clone(&resolved_egress),
                Arc::clone(&lease_cell),
                Arc::clone(&settlement),
            )),
        });
        let mut outcome = tokio::select! {
            biased;
            result = run_future.as_mut() => match result {
                Ok(result) => result,
                Err(()) => Err(FusionError::Internal),
            },
            () = cancel.cancelled() => {
                if control.is_finalizing() {
                    match run_future.as_mut().await {
                        Ok(result) => result,
                        Err(()) => Err(FusionError::Internal),
                    }
                } else {
                    let _ = control.claim_terminal();
                    // Drive cancellation cleanup to completion. In
                    // particular, `run_panels` aborts and joins every child
                    // before the facts snapshot or reservation is settled.
                    let _ = run_future.as_mut().await;
                    Err(FusionError::Cancelled)
                }
            },
            () = tokio::time::sleep_until(deadline) => {
                // Once finalization has taken the control claim, dropping the
                // future would detach the budget commit and publish terminal
                // facts before money settled. The configured outer duration
                // already includes FINALIZE_GRACE_MS; if that tail is still
                // pending, let its owned commit finish instead of stealing it.
                if control.is_finalizing() {
                    match run_future.as_mut().await {
                        Ok(result) => result,
                        Err(()) => Err(FusionError::Internal),
                    }
                } else {
                    let _ = control.claim_terminal();
                    cancel.cancel();
                    // A full-deadline timeout is a terminal decision, but it
                    // is not permission to leave spawned panels mutating
                    // allocation/usage facts in the background.
                    let _ = run_future.as_mut().await;
                    Err(FusionError::TimedOutEmpty)
                }
            }
        };
        // Destroy a panicked inner future before waiting: it can still own
        // untransferred permits or a JoinSet. The supervisor barrier retains
        // the actual-producer ticket independently of this future's locals.
        drop(run_future);
        panel_tasks.abort_and_wait().await;
        // Every terminal branch above has either observed `run_inner` ready
        // or explicitly driven its cancellation cleanup through the panel
        // JoinSet drain. It is now safe to seal the facts and settlement.
        if let Some(finalizer) = attempt_finalizer {
            if let Some(summary) = Self::settle_registered_attempts(finalizer, &facts).await {
                if let Ok(result) = &mut outcome {
                    result.usage = summary.usage;
                    result.egress_profiles = summary.confirmed_egress;
                    result.egress_profiles.extend(summary.possible_egress);
                    result.egress_profiles.sort();
                    result.egress_profiles.dedup();
                    let stage = match result.status {
                        FusionStatus::Completed => FusionStage::Completed,
                        FusionStatus::NeedsParent => FusionStage::NeedsParent,
                    };
                    progress::emit(&progress, stage.clone(), None, stage.label());
                    let completed = result
                        .panels
                        .iter()
                        .filter(|panel| panel.status == PanelRunStatus::Completed)
                        .count();
                    self.emit_completed(
                        &request,
                        (
                            result.panels.len(),
                            completed,
                            result.panels.len() - completed,
                        ),
                        &result.run_id,
                        &result.usage,
                        &result.egress_profiles,
                        &result.decision,
                        &result.timing,
                        Some(deadline),
                    )
                    .await;
                }
            }
        }
        if let Err(error) = &outcome {
            if self.attempt_run.is_none() {
                let priced = settlement
                    .lock()
                    .ok()
                    .and_then(|guard| *guard)
                    .unwrap_or((0, false));
                if let Some(lease) = lease_cell.lock().ok().and_then(|mut guard| guard.take()) {
                    let _ = lease.commit(priced.0).await;
                }
                let snapshot = facts.snapshot();
                let mut usage = snapshot.usage.clone().unwrap_or_default();
                usage.realized_nano_usd = priced.0;
                let any_dispatch = snapshot.dispatched_panels.is_some_and(|count| count > 0);
                usage.estimated |= priced.1 && any_dispatch;
                let usage_incomplete = snapshot.usage_incomplete || usage.estimated;
                facts.replace_usage(usage, usage_incomplete);
            }
            let snapshot = facts.snapshot();
            let mut timing = snapshot.timing;
            timing.total_ms = millis_since(started);
            facts.set_timing(timing);
            let stage = if matches!(error, FusionError::Cancelled) {
                FusionStage::Cancelled
            } else {
                FusionStage::Failed
            };
            let label = stage.label();
            if let Some(tokens) = realized_tokens.lock().ok().and_then(|guard| *guard) {
                progress::emit_with_realized_tokens(
                    &progress,
                    stage,
                    label,
                    tokens,
                    resolved_egress.lock().ok().and_then(|guard| guard.clone()),
                );
            } else {
                progress::emit(&progress, stage, None, label);
            }
            let mut md = fusion_event_metadata(&request);
            md.insert("run_id".into(), AnalyticsValue::String(run_id.clone()));
            md.insert(
                "duration_ms".into(),
                AnalyticsValue::Int(saturating_i64(millis_since(started))),
            );
            add_fusion_error_metadata(&mut md, error);
            let event = if matches!(error, FusionError::Cancelled) {
                telemetry::tengu::fusion::CANCELLED
            } else {
                telemetry::tengu::fusion::FAILED
            };
            self.log_terminal_event(event, md, Some(deadline)).await;
        }
        outcome
    }
}

#[async_trait]
impl FusionExecutor for FusionOrchestrator {
    fn prepare(
        self: Arc<Self>,
        submission: FusionSubmission,
    ) -> Result<PreparedFusionRun, FusionError> {
        let FusionSubmission {
            request,
            inherit,
            identity,
        } = submission;
        let submission = FusionSubmission::new(request, inherit, identity)?;
        let FusionSubmission {
            request,
            inherit,
            identity,
        } = submission;
        let inherit = scope_inheritance_to_identity(&identity, inherit)?;
        let request = validate_request(request)?;
        let runtime_snapshot =
            self.capture_runtime_snapshot(&request, inherit.effective_timeout_ms)?;
        let config = runtime_snapshot.config.clone();
        if config.completion_policy == crate::FusionCompletionPolicy::QuorumAfterGrace
            && !(request.partial_ok && config.partial_ok)
        {
            return Err(FusionError::InvalidRequest(
                "quorum_after_grace requires partial_ok=true".into(),
            ));
        }
        let catalog_snapshot = runtime_snapshot.catalog.clone();
        let resolved = model_resolver::resolve(&request, &config, &catalog_snapshot)?;
        let captured_prices = runtime_snapshot.prices.clone();
        // Quote during preparation so an invalid price/configuration fails
        // before TaskCreated. The actual reservation remains activation-only.
        let quote = if self.attempt_registrar.is_none() {
            Some(budget::quote(
                &config,
                &resolved,
                &catalog_snapshot,
                &captured_prices,
                inherit.budget().max_session_nano_usd().is_some(),
            )?)
        } else {
            None
        };
        let runtime_snapshot = Arc::new(runtime_snapshot);
        let duration_ms = inherit
            .effective_timeout_ms
            .unwrap_or_else(|| config.total_timeout_ms.saturating_add(FINALIZE_GRACE_MS));
        let summary = FusionPreparedSummary {
            identity: identity.clone(),
            duration_ms,
            planned_panels: Some(u8::try_from(resolved.panels.len()).unwrap_or(u8::MAX)),
        };
        let control = FusionRunControl::new_with_billing_mode(
            identity,
            duration_ms,
            inherit.cancel.clone(),
            FusionRunFactsRecorder::default(),
            if self.attempt_registrar.is_some() {
                platform_api::ModelAttemptBillingMode::MeteredAttempts
            } else {
                platform_api::ModelAttemptBillingMode::LegacyAggregate
            },
        );
        let facts = control.facts();
        facts.set_resolved_panels(u8::try_from(resolved.panels.len()).unwrap_or(u8::MAX));
        // This exact zero is provisional until a provider-capable boundary is
        // reached. It lets pre-reservation failures remain distinguishable
        // from legacy runners that expose no facts at all.
        facts.set_known_zero();
        let live_catalog = Arc::clone(&self.catalog);
        let mut snapshot_orchestrator = (*self).clone();
        snapshot_orchestrator.catalog = Arc::new(catalog_snapshot);
        snapshot_orchestrator.prices = Arc::new(captured_prices);
        let registration = self
            .attempt_registrar
            .as_ref()
            .map(|registrar| {
                registrar.register(crate::FusionAttemptRegistration {
                    control: control.clone(),
                    inherit: inherit.clone(),
                    request: request.clone(),
                    resolved: resolved.clone(),
                    snapshot: runtime_snapshot.clone(),
                    live_policy: Arc::new(crate::attempts::CapturedLivePolicy {
                        control: control.clone(),
                        request: request.clone(),
                        resolved: resolved.clone(),
                        snapshot: runtime_snapshot.clone(),
                        config: self.config_source.clone(),
                        catalog: live_catalog.clone(),
                    }),
                })
            })
            .transpose()?;
        let attempt_finalizer = registration.map(|registered| {
            snapshot_orchestrator.panel_fence = registered.panel_fence;
            snapshot_orchestrator.attempt_run = Some(registered.run);
            registered.finalizer
        });
        let snapshot_orchestrator = Arc::new(snapshot_orchestrator);
        Ok(PreparedFusionRun::new(
            summary,
            control.clone(),
            move |activation: FusionActivation, progress| {
                let orchestrator = Arc::clone(&snapshot_orchestrator);
                let runtime_snapshot = Arc::clone(&runtime_snapshot);
                let live_catalog = Arc::clone(&live_catalog);
                let control = control.clone();
                let facts = facts.clone();
                let request = request.clone();
                let inherit = inherit.clone();
                let config = config.clone();
                let resolved = resolved.clone();
                let quote = quote.clone();
                async move {
                    let started = activation.activated_at;
                    let result = orchestrator
                        .run_prepared_snapshot(
                            config,
                            request,
                            inherit,
                            progress,
                            control.clone(),
                            facts.clone(),
                            resolved,
                            quote,
                            runtime_snapshot,
                            live_catalog,
                            started,
                            attempt_finalizer,
                        )
                        .await;
                    FusionRunOutcome::from_control(&control, result)
                }
            },
        ))
    }

    fn effective_timeout_ms(&self) -> Option<u64> {
        // Match the outer `run()` deadline, including the small tail grace
        // used for settlement and telemetry after an inner stage expires.
        self.config_source
            .load()
            .ok()
            .map(|config| config.total_timeout_ms.saturating_add(FINALIZE_GRACE_MS))
    }

    fn agent_surface(&self) -> FusionAgentSurface {
        // F007: reload per call. A config that fails to (re)load — a
        // settings file edit that now fails validation — fails CLOSED to the
        // disabled default rather than serving the last-known-good surface,
        // consistent with `RejectedFusionExecutor`'s boot-time behavior.
        let Ok(config) = self.config_source.load() else {
            return FusionAgentSurface::default();
        };
        FusionAgentSurface {
            // Advertising the `fusion` agent while its models are unconfigured
            // would put a subagent type in front of the model that cannot
            // complete a single call: every spawn dies at preflight with
            // `NotConfigured`, having spent a turn to learn it. The master
            // switch and a complete configuration are both required, and the
            // operator sees the reason on the `/fusion` path, which stays
            // reachable either way and opens the setup wizard.
            enabled: config.enabled && config.missing_model_roles().is_empty(),
            allow_cross_provider: config.allow_cross_provider_for_agent,
            default_preset: config.default_preset,
            default_partial_ok: config.partial_ok,
            quality_panel_count: config.quality_panel_count,
            fast_panel_count: config.fast_panel_count,
            max_panel: config.max_panel,
            slash_cross_provider_default: config.slash_cross_provider_default,
        }
    }

    fn resolve_parent_profile(
        &self,
        parent_model: &str,
        explicit_profile: Option<&str>,
    ) -> Option<String> {
        explicit_profile
            .map(str::trim)
            .filter(|profile| !profile.is_empty())
            .map(str::to_string)
            .or_else(|| {
                let mut profiles = self
                    .catalog
                    .list()
                    .into_iter()
                    .filter(|row| row.model == parent_model)
                    .map(|row| row.profile)
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter();
                let profile = profiles.next()?;
                profiles.next().is_none().then_some(profile)
            })
    }

    fn workflow_fusion_call_cap(&self) -> u32 {
        // F007: reload per call; fail closed to the trait default (20, the
        // global hard ceiling — see the trait doc) on a reload error.
        self.config_source
            .load()
            .map_or(20, |config| config.workflow_fusion_call_cap)
    }

    fn workflow_batch_concurrency(&self) -> usize {
        let Some(registrar) = &self.attempt_registrar else {
            return 1;
        };
        let Ok(config) = self.config_source.load() else {
            return 1;
        };
        if !config.enabled {
            return 1;
        }
        usize::from(config.workflow_concurrency)
            .min(registrar.workflow_batch_concurrency())
            .clamp(1, 2)
    }
}

/// One-shot entrypoint retained for the crate's own tests.
///
/// Production never had a caller: every host goes through `prepare` and the
/// returned handle. Keeping this as an inherent test-only method rather than
/// a trait method means a second, identity-less way to start a run cannot be
/// reached from outside the crate.
#[cfg(test)]
impl FusionOrchestrator {
    pub(crate) async fn run(
        &self,
        request: FusionRequest,
        inherit: FusionInheritance,
        progress: Option<Sender<FusionProgress>>,
    ) -> Result<FusionResult, FusionError> {
        self.run_scoped(request, None, inherit, progress).await
    }

    /// The same path bound to a session, for the tests that assert
    /// scoped-ledger behaviour.
    pub(crate) async fn run_scoped(
        &self,
        request: FusionRequest,
        session_id: Option<protocol::SessionId>,
        inherit: FusionInheritance,
        progress: Option<Sender<FusionProgress>>,
    ) -> Result<FusionResult, FusionError> {
        // Routed through the same immutable preparation snapshot and owned
        // supervisor every host uses.
        let request_for_failure = request.clone();
        let parent_operation_id = if request.origin == FusionOrigin::Workflow {
            request.workflow_run_id.clone()
        } else {
            None
        };
        let identity = FusionRunIdentity::new(
            FusionRunId::generated(),
            session_id,
            request.origin,
            parent_operation_id,
        );
        let run_id = identity.run_id.to_string();
        let submission = match FusionSubmission::new(request, inherit, identity) {
            Ok(submission) => submission,
            Err(error) => {
                self.emit_preparation_failed(&request_for_failure, &run_id, &progress, &error)
                    .await;
                return Err(error);
            }
        };
        let prepared = match Arc::new(self.clone()).prepare(submission) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.emit_preparation_failed(&request_for_failure, &run_id, &progress, &error)
                    .await;
                return Err(error);
            }
        };
        prepared
            .activate(FusionActivation::now(), progress)
            .await
            .result
    }
}

fn validate_request(mut request: FusionRequest) -> Result<FusionRequest, FusionError> {
    if request.prompt.trim().is_empty() {
        return Err(FusionError::InvalidRequest(
            "prompt must be non-empty".into(),
        ));
    }
    request.dimensions = normalize_dimensions(request.dimensions)?;
    Ok(request)
}

fn scope_inheritance_to_identity(
    identity: &FusionRunIdentity,
    mut inherit: FusionInheritance,
) -> Result<FusionInheritance, FusionError> {
    let Some(session_id) = identity.session_id else {
        return Ok(inherit);
    };
    let Some(budget) = inherit.budget().scoped_for_session(session_id) else {
        return Err(FusionError::BudgetReservationUnavailable);
    };
    inherit.subagent.budget = budget;
    Ok(inherit)
}

pub(crate) fn check_panel_bar(
    panels: &[PanelInternal],
    request: &FusionRequest,
    config: &FusionRuntimeConfig,
) -> Result<(), FusionError> {
    let ok = successful(panels).len();
    // Resolution rejects a request/catalog that cannot provide the configured
    // success minimum. Keep the terminal bar defensive too: silently reducing
    // the operator's minimum to `panels.len()` turns a hard policy into a soft
    // target if a future/custom resolver ever hands us an undersized set.
    let min = usize::from(config.min_successful_panels.max(FUSION_MIN_PANEL));
    if ok == 0 {
        // [Finding 19] A panel sealed off by the early-abort bar (G004,
        // `panel.rs`'s `join_set.abort_all()`) never gets the chance to
        // time out on its own — its `JoinError` is synthesized as
        // `status: Failed, error_category: "aborted"` (panel.rs's
        // `JoinError` collection arm), which carries no information about
        // WHY the run failed. Excluding those slots here means the
        // zero-success classification reflects only the panels that
        // actually ran to a terminal outcome: if every one of THOSE timed
        // out, the run-level error stays `TimedOutEmpty` even when the bar
        // sealed early and aborted a still-running sibling before its own
        // timeout could fire.
        // [Round-5 review item 8] `"not_dispatched"` is excluded for the
        // same reason `"aborted"` is: the slot was killed before its task
        // ever called the spawner, so it carries no information about WHY
        // the run failed and must not mask a set of genuine timeouts.
        let real_outcomes: Vec<&PanelInternal> = panels
            .iter()
            .filter(|panel| {
                !matches!(
                    panel.error_category.as_deref(),
                    Some("aborted" | "not_dispatched")
                )
            })
            .collect();
        if !real_outcomes.is_empty()
            && real_outcomes
                .iter()
                .all(|panel| panel.status == PanelRunStatus::TimedOut)
        {
            return Err(FusionError::TimedOutEmpty);
        }
        // [round-3 review, finding 12] When EVERY panel's error_category is
        // "spawn" — a pre-allocation spawner rejection (panel.rs's
        // `Ok(Err(err)) => PanelFinish::Failed { category: "spawn".into(),
        // .. }` arm) — no panel here could possibly have made a provider
        // call. That is a strictly stronger, provably-preflight shape than
        // the general `AllPanelsFailed` (which also covers panels that DID
        // call a provider and lost), so report it distinctly: the caller's
        // spawn-reservation accounting must not keep a session's spawn cap
        // charged for subagents that never existed. Deliberately narrow —
        // this does NOT extend to `MinPanelsNotMet`/`PanelSetIncomplete`
        // below, where at least `min`/some panels genuinely ran.
        // [Round-5 review item 8] `"not_dispatched"` counts as a
        // preflight failure alongside `"spawn"`: the early-abort bar
        // (`panel::run_panels`' `join_set.abort_all()`) kills siblings the
        // instant the run is sealed, and a slot killed before its task
        // ever reached the spawner is exactly as provably-pre-provider-call
        // as a spawner rejection. Before this, ONE such slot made the
        // `all(== "spawn")` test below false forever, so an all-rejected
        // run degraded to the generic `AllPanelsFailed` — which
        // `fusion_error_is_preflight` does not release the spawn
        // reservation for — and whether a session stayed charged for
        // subagents that never existed came down to which rejection
        // happened to return last. A panel that was actually ALLOCATED a
        // child still keeps its `"aborted"` category and still blocks this
        // classification: we cannot prove it made no provider call.
        // [Round-6 blocking B1] "Allocated", not "reached the spawner":
        // `panel::panel_from_join_error` keys the distinction on the
        // spawner's own `Allocated` observation, because entering
        // `spawn_workflow_with_observer` (which connects the panel's inline
        // MCP servers before the pool can even refuse) proves nothing about
        // whether a subagent — and therefore a billable provider call —
        // ever came to exist.
        if !panels.is_empty()
            && panels.iter().all(|panel| {
                crate::panel::is_never_dispatched_category(panel.error_category.as_deref())
            })
        {
            return Err(FusionError::AllPanelsFailedPreflight);
        }
        return Err(FusionError::AllPanelsFailed);
    }
    if ok < min {
        return Err(FusionError::MinPanelsNotMet);
    }
    let partial_ok = request.partial_ok && config.partial_ok;
    if !partial_ok && ok != panels.len() {
        return Err(FusionError::PanelSetIncomplete);
    }
    Ok(())
}

/// Own a reservation together with the last settlement snapshot for this
/// run. The outer executor normally drains this guard and awaits `commit`,
/// but a parent is also allowed to drop the whole Fusion future (for example
/// when a workflow forcibly aborts its tool task). In that case the guard's
/// Drop path must preserve known spend instead of delegating to
/// `ReservationLease::drop`, whose purpose is only to release an untouched
/// hold.
struct SettlementLease {
    lease: Option<ReservationLease>,
    settlement: Arc<Mutex<Option<(u64, bool)>>>,
}

impl SettlementLease {
    fn quote(&self) -> &budget::FusionQuote {
        self.lease
            .as_ref()
            .expect("settlement lease quote requested after commit")
            .quote()
    }

    async fn commit(mut self, actual_nano_usd: u64) -> Result<(), FusionError> {
        let Some(lease) = self.lease.take() else {
            return Ok(());
        };
        lease.commit(actual_nano_usd).await
    }
}

impl Drop for SettlementLease {
    fn drop(&mut self) {
        let Some(lease) = self.lease.take() else {
            return;
        };
        let actual_nano_usd = self
            .settlement
            .lock()
            .ok()
            .and_then(|guard| *guard)
            .map_or(0, |(amount, _)| amount);
        // `ReservationLease::commit` shields its budget await from this
        // guard's caller, so this detached fallback remains exactly-once even
        // when the parent drops the entire `FusionExecutor::run` future.
        tokio::spawn(async move {
            let _ = lease.commit(actual_nano_usd).await;
        });
    }
}

/// [Round-5 review items 1/2/4] The two survives-a-drop money cells
/// (`realized_tokens`, `settlement`) plus everything `price_realized_usage`
/// needs, threaded through EVERY stage that runs after the panel fan-out.
///
/// `run()`'s outer `Err` arm is the only settler when `run_inner`'s future
/// is DROPPED (an outer cancel, or the outer total timeout): it commits
/// whatever these cells last held. Round 4 taught `panel::RealizedSpendSink`
/// to keep them fresh during the panel fan-out and `run_inner` to refresh
/// them once the panel stage returned — and then stopped. The analyst's and
/// the synthesizer's usage lived only on `run_inner`'s own stack, so a
/// cancel during either stage committed the panel-only figure and charged
/// real, already-billed judge tokens to nobody.
///
/// The full stage table this type exists to keep honest — what the
/// settlement cell holds at each boundary a run can be dropped on:
///
/// | boundary | settlement holds | source |
/// |---|---|---|
/// | lease acquired, nothing dispatched | `(0, true)` | exact — no call yet (`resolve_and_reserve`) |
/// | a panel task reaches the spawner | one priced `panel_prompt` turn per DISPATCHED panel | estimate (`panel::PanelDispatch` + `RealizedSpendSink`) |
/// | a panel finishes | that panel's real usage + the floor for the ones still in flight | provider usage where known |
/// | panel stage returns | every panel's real usage, analyst/synth exactly $0 | provider usage (`refresh(None, false, None, false)`) |
/// | analyst call dispatched | panels + canonical analyst request estimate | estimate (`refresh(None, true, ..)`) |
/// | analyst returned | panels + the analyst's REAL usage | provider usage (`refresh(Some(..), true, ..)`) |
/// | synthesizer dispatched | + canonical synthesizer request estimate | estimate (`refresh(.., None, true)`) |
/// | synthesizer returned | + the synthesizer's REAL usage | provider usage (`refresh(.., Some(..), true)`) |
/// | `finalize_result` | the same figure, committed against the lease | `price_realized_usage` |
///
/// Provider-reported usage always wins; an estimate is only ever
/// substituted for a call that was ATTEMPTED and lost its usage, and always
/// with `estimated = true` (`price_realized_usage`'s own contract).
struct StageSettlement<'a> {
    realized_tokens: &'a Arc<Mutex<Option<u64>>>,
    settlement: &'a Arc<Mutex<Option<(u64, bool)>>>,
    catalog: &'a dyn ModelSource,
    live_catalog: &'a dyn ModelSource,
    catalog_snapshot: &'a CatalogSnapshot,
    prices: &'a dyn FusionPriceBook,
    analyst: &'a ResolvedPanel,
    /// The configured synthesizer route (`fusion.synthesizerModel`) — the one
    /// the merge side query actually targets, which is NOT necessarily the
    /// session's own model.
    synthesizer: &'a ResolvedPanel,
    request_prompt: &'a str,
    request: Option<&'a FusionRequest>,
    config: Option<&'a FusionRuntimeConfig>,
    side_query: Option<&'a dyn SideQueryClient>,
    synth_estimate: Arc<Mutex<Option<cost::Usage>>>,
    /// The finished, anonymized panel set — fixed for the whole life of
    /// this borrow (every stage below only reads it).
    panels: &'a [PanelInternal],
    operational_deadline: Instant,
    cancel: CancellationToken,
    reserved_max_nano_usd: u64,
    facts: FusionRunFactsRecorder,
    started: Instant,
}

impl StageSettlement<'_> {
    /// Combine exact usage returned by completed analyst attempts with an
    /// input-only estimate for every attempt that returned no usage. A retry
    /// must add its estimate to prior actual spend, not replace that spend or
    /// disappear merely because `analyst_usage` is already `Some`.
    fn analyst_usage_with_missing_estimates(&self, snapshot: &AnalystUsage) -> cost::Usage {
        let mut usage = snapshot.usage;
        let input_estimate = match (self.request, self.config, self.side_query) {
            (Some(request), Some(config), Some(client)) if client.has_canonical_estimator() => {
                let limits = self
                    .catalog_snapshot
                    .limits_for(&self.analyst.profile, &self.analyst.model)
                    .unwrap_or_else(crate::model_resolver::ModelLimits::unknown);
                crate::analyst::estimate_input_tokens(
                    client,
                    config,
                    request,
                    self.analyst,
                    self.panels,
                    limits,
                )
                .unwrap_or_else(|_| judge_input_token_estimate(self.request_prompt, self.panels))
            }
            _ => judge_input_token_estimate(self.request_prompt, self.panels),
        };
        let missing_input = input_estimate.saturating_mul(u64::from(snapshot.unreported_calls));
        usage.tokens.input = usage.tokens.input.saturating_add(missing_input);
        usage
    }

    fn canonical_analyst_estimate(&self) -> Option<cost::Usage> {
        if !self.side_query?.has_canonical_estimator() {
            return None;
        }
        let (request, config, client) = (self.request?, self.config?, self.side_query?);
        let limits = self
            .catalog_snapshot
            .limits_for(&self.analyst.profile, &self.analyst.model)
            .unwrap_or_else(crate::model_resolver::ModelLimits::unknown);
        let input = crate::analyst::estimate_input_tokens(
            client,
            config,
            request,
            self.analyst,
            self.panels,
            limits,
        )
        .ok()?;
        let mut usage = cost::Usage::default();
        usage.tokens.input = input;
        Some(usage)
    }

    fn canonical_synth_estimate(&self) -> Option<cost::Usage> {
        self.synth_estimate.lock().ok().and_then(|guard| *guard)
    }

    fn capture_synth_estimate(&self, analysis: &FusionAnalysis) {
        let (request, config, client) = match (self.request, self.config, self.side_query) {
            (Some(request), Some(config), Some(client)) if client.has_canonical_estimator() => {
                (request, config, client)
            }
            _ => return,
        };
        let limits = self
            .catalog_snapshot
            .limits_for(&self.synthesizer.profile, &self.synthesizer.model)
            .unwrap_or_else(crate::model_resolver::ModelLimits::unknown);
        let Ok(input) = crate::synthesizer::estimate_input_tokens(
            client,
            config,
            request,
            self.synthesizer,
            analysis,
            self.panels,
            limits,
        ) else {
            return;
        };
        let mut usage = cost::Usage::default();
        usage.tokens.input = input;
        if let Ok(mut guard) = self.synth_estimate.lock() {
            *guard = Some(usage);
        }
    }

    /// Rewrite both cells for the stage boundary just reached. Arguments
    /// mirror `price_realized_usage`'s own analyst/synth parameters exactly,
    /// so "what does this stage know" is the only decision at each call
    /// site.
    fn refresh(
        &self,
        analyst_usage: Option<(&cost::Usage, u32)>,
        analyst_attempted: bool,
        synth_usage: Option<&cost::Usage>,
        synth_attempted: bool,
        analyst_usage_incomplete: bool,
    ) {
        if self.facts.snapshot().attempt_settlement.is_some() {
            return;
        }
        let mut output_tokens = aggregate_panel_usage(self.panels).output_tokens;
        if let Some((usage, _)) = analyst_usage {
            output_tokens = output_tokens.saturating_add(usage.tokens.output);
        }
        if let Some(usage) = synth_usage {
            output_tokens = output_tokens.saturating_add(usage.tokens.output);
        }
        if let Ok(mut guard) = self.realized_tokens.lock() {
            *guard = Some(output_tokens);
        }

        let previous_priced = self
            .settlement
            .lock()
            .ok()
            .and_then(|guard| *guard)
            .unwrap_or((0, true));
        let mut usage = aggregate_panel_usage(self.panels);
        if let Some((analyst, calls)) = analyst_usage {
            add_cost_usage(&mut usage, analyst, calls);
        }
        if let Some(synth) = synth_usage {
            add_cost_usage(&mut usage, synth, 1);
        }
        let estimated_analyst = (analyst_attempted && analyst_usage.is_none())
            .then(|| self.canonical_analyst_estimate())
            .flatten();
        if let Some(estimate) = &estimated_analyst {
            add_cost_usage(&mut usage, estimate, 1);
        }
        let estimated_synth = (synth_attempted && synth_usage.is_none())
            .then(|| self.canonical_synth_estimate())
            .flatten();
        if let Some(estimate) = &estimated_synth {
            add_cost_usage(&mut usage, estimate, 1);
        }
        let usage_was_estimated = usage.estimated;
        usage.realized_nano_usd = previous_priced.0;
        usage.reserved_max_nano_usd = self.reserved_max_nano_usd;
        // Provider usage and egress are reliable before pricing. Publish a
        // conservative monetary snapshot first so a panicable price-book
        // lookup cannot erase a response that already crossed the wire.
        usage.estimated = true;
        self.facts.replace_usage(usage.clone(), true);

        let panels_incomplete = self.panels.iter().any(|panel| {
            !panel::is_never_dispatched_category(panel.error_category.as_deref())
                && panel.usage.as_ref().is_none_or(|usage| usage.estimated)
        });
        let attempts_incomplete = panels_incomplete
            || (analyst_attempted && analyst_usage.is_none())
            || (synth_attempted && synth_usage.is_none());
        if attempts_incomplete {
            self.facts.mark_attempts_unknown();
        } else {
            self.facts.replace_attempts(Some(usage.provider_requests));
        }
        if let Some(profiles) = dispatched_egress_profiles(self.panels) {
            for profile in profiles {
                self.facts.add_possible_egress(profile);
            }
        }
        for panel in self.panels.iter().filter(|panel| {
            panel
                .usage
                .as_ref()
                .is_some_and(|usage| !usage.estimated || usage.provider_requests > 0)
        }) {
            self.facts.add_confirmed_egress(panel.profile.clone());
        }
        if analyst_attempted {
            self.facts.add_possible_egress(self.analyst.profile.clone());
            if analyst_usage.is_some() {
                self.facts
                    .add_confirmed_egress(self.analyst.profile.clone());
            }
        }
        if synth_attempted {
            self.facts
                .add_possible_egress(self.synthesizer.profile.clone());
            if synth_usage.is_some() {
                self.facts
                    .add_confirmed_egress(self.synthesizer.profile.clone());
            }
        }
        self.facts.set_timing(FusionTiming {
            total_ms: u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            ..Default::default()
        });

        let analyst_for_pricing =
            analyst_usage.or_else(|| estimated_analyst.as_ref().map(|estimate| (estimate, 1)));
        let synth_for_pricing = synth_usage.or(estimated_synth.as_ref());
        let (priced_nano_usd, mut priced_estimated) = price_realized_usage(
            self.catalog,
            self.prices,
            self.panels,
            self.analyst,
            analyst_for_pricing,
            analyst_attempted,
            self.synthesizer.profile.as_str(),
            self.synthesizer.model.as_str(),
            synth_for_pricing,
            synth_attempted,
            self.request_prompt,
        );
        priced_estimated |= estimated_analyst.is_some() || estimated_synth.is_some();
        if let Ok(mut guard) = self.settlement.lock() {
            *guard = Some((
                priced_nano_usd,
                priced_estimated || analyst_usage_incomplete,
            ));
        }
        usage.realized_nano_usd = priced_nano_usd;
        usage.estimated = usage_was_estimated || priced_estimated || analyst_usage_incomplete;
        self.facts.replace_usage(
            usage,
            priced_estimated || analyst_usage_incomplete || attempts_incomplete,
        );
    }
}

/// [Round-3 review B2, reworked] The provider profiles this panel set was
/// ACTUALLY dispatched to — every panel except those whose `error_category`
/// satisfies [`panel::is_never_dispatched_category`]. As of round-5 item 8
/// and round-6 blocking B1 that is TWO categories, not one: `"spawn"`
/// (`panel.rs`'s pre-allocation spawner-rejection arm) and
/// `"not_dispatched"` (a slot cancelled before it called the spawner, or
/// aborted while parked inside a spawner call that never allocated a child).
/// Both are provably reached before any provider call could have been made.
/// Equivalently: every panel with `usage.is_some()`, plus any panel that
/// dispatched but failed AFTER making its call (a timeout or a provider
/// error, say — it still reached the provider even though it has no `usage`
/// to show for it). Returns `None` (never
/// `Some(vec![])`) when nothing dispatched, so a caller renders no
/// `<egress-profiles>` section rather than an empty one — this is what
/// keeps `AllPanelsFailedPreflight`'s "guarantees zero provider calls"
/// contract honest all the way out to the failure disclosure.
fn dispatched_egress_profiles(panels: &[PanelInternal]) -> Option<Vec<String>> {
    let mut profiles: Vec<String> = panels
        .iter()
        .filter(|panel| {
            // [Round-5 review items 8/12/16] `"not_dispatched"` joins
            // `"spawn"` here: both name a slot whose task provably never
            // called the spawner — see `panel::is_never_dispatched_category`.
            !crate::panel::is_never_dispatched_category(panel.error_category.as_deref())
        })
        .map(|panel| panel.profile.clone())
        .collect();
    if profiles.is_empty() {
        return None;
    }
    profiles.sort();
    profiles.dedup();
    Some(profiles)
}

fn panel_outcome(panel: &PanelInternal) -> PanelOutcome {
    PanelOutcome {
        panel_id: panel.anonymous_id.clone(),
        status: panel.status,
        duration_ms: panel.duration_ms,
        error_category: panel.error_category.clone(),
        error_detail: panel.error_detail.clone(),
        usage: panel.usage.clone(),
    }
}

pub(crate) fn aggregate_panel_usage(panels: &[PanelInternal]) -> FusionUsage {
    let mut acc = FusionUsage::default();
    for panel in panels {
        if let Some(usage) = &panel.usage {
            acc.input_tokens = acc.input_tokens.saturating_add(usage.input_tokens);
            acc.output_tokens = acc.output_tokens.saturating_add(usage.output_tokens);
            acc.reasoning_tokens = acc.reasoning_tokens.saturating_add(usage.reasoning_tokens);
            acc.cache_read_tokens = acc
                .cache_read_tokens
                .saturating_add(usage.cache_read_tokens);
            acc.cache_write_tokens = acc
                .cache_write_tokens
                .saturating_add(usage.cache_write_tokens);
            acc.provider_requests = acc
                .provider_requests
                .saturating_add(usage.provider_requests);
            acc.estimated = acc.estimated || usage.estimated;
        }
    }
    acc
}

/// Price this run's OWN usage through `prices`, one component at a time,
/// instead of reading a session-wide `CostTracker` delta (which misattributes
/// a concurrent parent turn's — or a sibling Fusion run's — spend to this
/// run; see finding G001). A component with no price (and not a
/// `Subscription`-class hint) marks the whole result `estimated = true`
/// rather than guessing a dollar figure for it — §4 forbids a conservative
/// fallback estimate on the money path.
#[allow(clippy::too_many_arguments)]
pub(crate) fn price_realized_usage(
    catalog: &dyn ModelSource,
    prices: &dyn FusionPriceBook,
    panels: &[PanelInternal],
    analyst: &ResolvedPanel,
    analyst_usage: Option<(&cost::Usage, u32)>,
    // T1 item 1 (user-directed policy): `true` when the analyst was actually
    // CALLED (every path through `analyze_and_decide` attempts it), `false`
    // at the `check_panel_bar`-failure call site in `run_inner`, where the
    // analyst never runs at all. Required to tell "attempted, usage lost"
    // (estimate + price it) apart from "never ran" (real, exact $0) — the
    // two collapse to the same `analyst_usage: None` otherwise.
    analyst_attempted: bool,
    // The configured synthesizer route — what the merge call was billed on.
    // NOT necessarily the session's own model any more: `fusion.synthesizerModel`
    // names it explicitly.
    synth_profile: &str,
    synth_model: &str,
    synth_usage: Option<&cost::Usage>,
    // Same distinction as `analyst_attempted`, for the synthesizer — which,
    // unlike the analyst, legitimately has NO attempt on most decisions (it
    // only runs on the `Merge` branch). See `AnalysisOutcome::synth_attempted`.
    synth_attempted: bool,
    // The original task prompt, shared by both the analyst's and the
    // synthesizer's real request payloads (`analyst_user_message` /
    // `synthesize`'s `user` JSON) — the basis for the missing-usage estimate
    // below, alongside each successful panel's own report.
    request_prompt: &str,
) -> (u64, bool) {
    let mut total_nano_usd = 0_u64;
    let mut estimated = false;
    for panel in panels {
        // Price every panel that produced usage, regardless of `status`.
        // `finish_panel` sets `internal.usage` for every
        // `SubagentResult::Completed`, including one whose report then
        // fails `parse_and_sanitize` (status stays `Failed`) — that panel
        // still spent real tokens, and `aggregate_panel_usage` (above)
        // already counts them, so skipping it here would silently
        // understate `realized_nano_usd` while `FusionUsage.input_tokens` /
        // `output_tokens` kept the full count. A panel with genuinely no
        // usage is exact $0 for a provably pre-dispatch slot (`spawn` or
        // `not_dispatched`), while a reached provider with no usage still
        // makes the total estimated.
        let Some(usage) = &panel.usage else {
            if !crate::panel::is_never_dispatched_category(panel.error_category.as_deref()) {
                estimated = true;
            }
            continue;
        };
        // Finding [9]: `usage.estimated` is set when the panel's own count is
        // known-incomplete (the runner's `api_error_partial` salvage, or a
        // `SubagentResult::Failed` usage that excludes the failing turn) —
        // OR it into the run-level flag so the exact-figure claim
        // (`estimated: false`) is never made over a short number.
        if usage.estimated {
            estimated = true;
        }
        match budget::price_component(
            &panel.profile,
            &panel.model,
            catalog,
            prices,
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_read_tokens,
            usage.cache_write_tokens,
            usage.reasoning_tokens,
            u64::from(usage.provider_requests),
        ) {
            Some(nano_usd) => total_nano_usd = total_nano_usd.saturating_add(nano_usd),
            None => estimated = true,
        }
        // Round-7 finding [1] residual, applied at EVERY priced component
        // (panel / analyst / synthesizer), not just this one: when the price
        // book had to guess which prompt-cache TTL this flattened
        // `cache_write_tokens` bucket was written with, a run that actually
        // spent cache-write tokens cannot claim an exact total.
        if usage.cache_write_tokens > 0
            && budget::cache_write_rate_is_ttl_approximated(
                &panel.profile,
                &panel.model,
                catalog,
                prices,
            )
        {
            estimated = true;
        }
    }
    if let Some((usage, calls)) = analyst_usage {
        match budget::price_component(
            &analyst.profile,
            &analyst.model,
            catalog,
            prices,
            usage.tokens.input,
            usage.tokens.output,
            usage.tokens.cache_read,
            usage.tokens.cache_write,
            usage.tokens.reasoning_output,
            u64::from(calls),
        ) {
            Some(nano_usd) => total_nano_usd = total_nano_usd.saturating_add(nano_usd),
            None => estimated = true,
        }
        // Same TTL-approximation rule as the panel loop above.
        if usage.tokens.cache_write > 0
            && budget::cache_write_rate_is_ttl_approximated(
                &analyst.profile,
                &analyst.model,
                catalog,
                prices,
            )
        {
            estimated = true;
        }
    } else {
        // `analyst_usage` is `None` either because the analyst was never
        // called (the early `check_panel_bar`-failure settlement in
        // `run_inner`, `analyst_attempted: false` — real, exact $0, nothing
        // to estimate) or because an ATTEMPTED call failed
        // (`analyst_attempted: true` — every `AnalystError` arm leaves
        // `priced_analyst` at its `None` initialization). Either way the run
        // cannot claim an exact total, so `estimated` is set unconditionally
        // (unchanged from before this fix) — but a dollar figure is only
        // ever invented for the SECOND case: T1 item 1 (user-directed
        // policy) estimates the attempted call's usage from what we know it
        // read (the task prompt + every successful panel's report) using
        // main's shared byte-length approximation, rather than reporting
        // real, already-billed spend as exact $0.
        estimated = true;
        if analyst_attempted {
            let estimated_input = judge_input_token_estimate(request_prompt, panels);
            if let Some(nano_usd) = budget::price_component(
                &analyst.profile,
                &analyst.model,
                catalog,
                prices,
                estimated_input,
                0,
                0,
                0,
                0,
                1,
            ) {
                total_nano_usd = total_nano_usd.saturating_add(nano_usd);
            }
        }
    }
    if let Some(usage) = synth_usage {
        match budget::price_component(
            synth_profile,
            synth_model,
            catalog,
            prices,
            usage.tokens.input,
            usage.tokens.output,
            usage.tokens.cache_read,
            usage.tokens.cache_write,
            usage.tokens.reasoning_output,
            1,
        ) {
            Some(nano_usd) => total_nano_usd = total_nano_usd.saturating_add(nano_usd),
            None => estimated = true,
        }
        // Same TTL-approximation rule as the panel loop and the analyst arm.
        if usage.tokens.cache_write > 0
            && budget::cache_write_rate_is_ttl_approximated(
                synth_profile,
                synth_model,
                catalog,
                prices,
            )
        {
            estimated = true;
        }
    } else if synth_attempted {
        // T1 item 1: previously a synthesizer call that was attempted and
        // lost its usage to a failure/timeout (`SynthError::Failed` /
        // `TimedOut`) was treated exactly like "never ran" — real,
        // already-billed spend silently reported as exact $0, without even
        // flagging `estimated`. Same estimate-and-flag policy as the
        // analyst branch above.
        estimated = true;
        let estimated_input = judge_input_token_estimate(request_prompt, panels);
        if let Some(nano_usd) = budget::price_component(
            synth_profile,
            synth_model,
            catalog,
            prices,
            estimated_input,
            0,
            0,
            0,
            0,
            1,
        ) {
            total_nano_usd = total_nano_usd.saturating_add(nano_usd);
        }
    }
    (total_nano_usd, estimated)
}

/// Conservative fallback used only by isolated test fixtures or when a
/// provider estimator is unavailable. Real Session-backed analyst/synth
/// paths use the canonical request builders in `packing` and the
/// `SideQueryClient::estimate_request` hook, including system/schema/framing.
/// `pub(crate)` remains for tests that exercise the no-client fallback.
pub(crate) fn judge_input_token_estimate(prompt: &str, panels: &[PanelInternal]) -> u64 {
    let mut bytes = prompt.len() as u64;
    for panel in panels {
        if let Some(report) = &panel.report {
            bytes = bytes.saturating_add(
                serde_json::to_vec(report)
                    .map(|body| body.len() as u64)
                    .unwrap_or(0),
            );
        }
    }
    llm_runtime::model::count_tokens::approximate_tokens_for_bytes(bytes)
}

fn add_cost_usage(acc: &mut FusionUsage, usage: &cost::Usage, calls: u32) {
    acc.input_tokens = acc.input_tokens.saturating_add(usage.tokens.input);
    acc.output_tokens = acc.output_tokens.saturating_add(usage.tokens.output);
    acc.reasoning_tokens = acc
        .reasoning_tokens
        .saturating_add(usage.tokens.reasoning_output);
    acc.cache_read_tokens = acc
        .cache_read_tokens
        .saturating_add(usage.tokens.cache_read);
    acc.cache_write_tokens = acc
        .cache_write_tokens
        .saturating_add(usage.tokens.cache_write);
    acc.provider_requests = acc.provider_requests.saturating_add(calls);
}

fn capacity_narrowed(
    captured: crate::model_resolver::ModelLimits,
    current: crate::model_resolver::ModelLimits,
    configured_output_tokens: u32,
) -> bool {
    let captured_output = captured.output_cap(configured_output_tokens);
    let current_output = current.output_cap(configured_output_tokens);
    if current_output < captured_output {
        return true;
    }

    match (
        captured.input_cap(captured_output),
        current.input_cap(current_output),
    ) {
        (Some(before), Some(after)) => after < before,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// Byte cap on each panel's rendered `candidate_answer` inside
/// [`needs_parent_text`], keeping the `NeedsParent` summary bounded when
/// panels wrote long patches.
///
/// [Round 12 finding 1] This cap's earlier doc claimed "the full text is
/// still in `FusionResult.panels`" — that is FALSE and the correction is
/// load-bearing for the budget split below: `PanelOutcome`
/// (platform-api/src/fusion.rs:471-490) carries only `panel_id` / `status` /
/// `duration_ms` / `error_category` / `error_detail` / `usage`, no report
/// text at all. Whatever this renderer drops is gone for good, which is why
/// the budgets below are split per-section instead of letting one tail cut
/// decide who survives.
const NEEDS_PARENT_CANDIDATE_BYTE_CAP: usize = 4096;

/// [Finding 10] Same bound as [`NEEDS_PARENT_CANDIDATE_BYTE_CAP`], applied to
/// `report.summary` — the schema places no `maxLength` on `summary`
/// (`panel_report_json_schema`) and the only ceiling upstream is the
/// panel's whole-turn output budget (tens of KB), so leaving this field
/// unbounded let a verbose or injection-steered panel bypass the adjacent
/// cap entirely and push unbounded panel-authored text into the parent's
/// context via `final_text`.
const NEEDS_PARENT_SUMMARY_BYTE_CAP: usize = 4096;

/// [Finding 10, rework round 2] The per-field caps above bound `summary` and
/// `candidate_answer`, but `needs_parent_text` also renders the
/// analyst-authored `analysis.consensus`, `contradictions[].topic`/
/// `positions[].position` and `coverage_gaps` with no cap of their own —
/// none of those pass through `truncate_bytes`, so a verbose or
/// injection-steered analyst call could still push the assembled string
/// arbitrarily large even with every panel field capped. This is the final
/// backstop on the whole assembled body, so no sink inside it (present or
/// added later) can bypass it.
///
/// [Round 12 finding 1] It is no longer the ONLY bound, because as a lone
/// bound it was a tail cut over one concatenation: the sections are rendered
/// header → analyst → panels → closing directive, so overflow was paid for
/// entirely by whoever came last. That made the always-dropped casualty the
/// `"Next: ..."` directive, and the next-dropped the PAID panel material —
/// exactly inverting what this renderer exists to hand over (and see
/// [`NEEDS_PARENT_CANDIDATE_BYTE_CAP`]: dropped panel material has no other
/// route to the parent). Two shapes the config supports reached it:
/// `FUSION_MAX_PANEL = 8` panels at the per-field caps render ~66 KB, so
/// half the panels were cut outright; and one large analyst section, rendered
/// AHEAD of the panel loop with no cap of its own, could consume the entire
/// budget and evict 100% of the panel rows. So now
/// [`NEEDS_PARENT_ANALYST_BYTE_CAP`] bounds the analyst block on its own, the
/// panel block gets the remainder divided evenly per panel (so every panel
/// keeps its row), this backstop guards the assembled body, and the closing
/// directive is appended AFTER truncation so it can never be cut.
const NEEDS_PARENT_TEXT_BYTE_CAP: usize = 32 * 1024;

/// [Round 12 finding 1] The analyst block's own share of
/// [`NEEDS_PARENT_TEXT_BYTE_CAP`]. The analyst-authored sections are rendered
/// before the panel block and have no per-item cap, so without an independent
/// budget one verbose or injection-steered `consensus` item evicts every
/// paid panel report. 8 KiB holds a realistic consensus / contradictions /
/// coverage-gaps set while leaving 24 KiB for the panels.
const NEEDS_PARENT_ANALYST_BYTE_CAP: usize = 8 * 1024;

/// [Round 12 finding 1] Byte cap on the `reason` interpolated into
/// [`needs_parent_text`]'s header — the THIRD uncapped model-authored sink in
/// this renderer, and the one rendered FIRST, so an oversized one starves
/// every section after it. `FusionNeedsParentReason::AnalystRequested`'s
/// `reason` is the analyst's own prose (see `reason_line`), which
/// `analyst::sanitize_analysis` guards for control tags but never
/// length-caps. Every host-authored reason this renderer is called with is a
/// short one-liner, so 1 KiB is far above any legitimate value.
const NEEDS_PARENT_REASON_BYTE_CAP: usize = 1024;

/// [Round 12 finding 1] Per-panel fixed overhead charged against a panel's
/// share of the panel budget: the two field labels (`"  summary: "` = 11,
/// `"  candidate: "` = 13), the three joining newlines, and the two `…`
/// markers `truncate_bytes` may append (3 bytes each), rounded up.
const NEEDS_PARENT_PANEL_ROW_OVERHEAD: usize = 40;

/// [Round 12 finding 1] The closing directive, appended AFTER every
/// truncation so no budget overrun can drop it. It is the one line that tells
/// the parent what to DO with the material above, and under the old
/// single-tail-cut backstop it was the first thing lost.
const NEEDS_PARENT_CLOSING_LINE: &str =
    "Next: review the panel material above and provide the final answer yourself.";

/// Render the `NeedsParent` summary (F004): unlike a bare status list, this
/// carries the actual paid deliberation material — consensus, contradictions,
/// coverage gaps, per-panel scores, and each successful panel's (already
/// sanitized, see `panel::sanitize_report` / `analyst::sanitize_analysis`)
/// summary and candidate answer — so the parent does not have to redo the
/// work from a bare "Fusion failed" line. `analysis` is `None` when the
/// analyst never returned a usable payload (parse failure, transport
/// failure, or a pre-analysis abort).
///
/// `pub(crate)` so `orchestrator_test` can exercise [Finding 10]'s
/// bounded-size regression directly instead of driving a full `run()`.
pub(crate) fn needs_parent_text(
    panels: &[PanelInternal],
    reason: &str,
    analysis: Option<&FusionAnalysis>,
) -> String {
    // [Round 12 finding 1] Three independently budgeted sections, assembled
    // in this order so that overflow is paid for by the section that caused
    // it rather than by whatever happens to be rendered last:
    //   1. the header, whose only variable part (`reason`) is capped at
    //      NEEDS_PARENT_REASON_BYTE_CAP,
    //   2. the analyst block, capped at NEEDS_PARENT_ANALYST_BYTE_CAP,
    //   3. the panel block, given the remainder of NEEDS_PARENT_TEXT_BYTE_CAP
    //      divided EVENLY per panel so every panel keeps its `anonymous_id`
    //      row even at FUSION_MAX_PANEL with both fields at their caps,
    // and then NEEDS_PARENT_CLOSING_LINE appended after the final backstop.
    let header = format!(
        "Fusion did not produce a conclusive answer ({}).",
        truncate_bytes(reason, NEEDS_PARENT_REASON_BYTE_CAP)
    );

    let mut analyst_lines: Vec<String> = Vec::new();
    if let Some(analysis) = analysis {
        if !analysis.consensus.is_empty() {
            analyst_lines.push(String::new());
            analyst_lines.push("Consensus:".into());
            for item in &analysis.consensus {
                analyst_lines.push(format!("- {item}"));
            }
        }
        if !analysis.contradictions.is_empty() {
            analyst_lines.push(String::new());
            analyst_lines.push("Contradictions:".into());
            for contradiction in &analysis.contradictions {
                analyst_lines.push(format!(
                    "- [{:?}] {}",
                    contradiction.severity, contradiction.topic
                ));
                for position in &contradiction.positions {
                    analyst_lines.push(format!("  - {}: {}", position.panel_id, position.position));
                }
            }
        }
        if !analysis.coverage_gaps.is_empty() {
            analyst_lines.push(String::new());
            analyst_lines.push("Coverage gaps:".into());
            for gap in &analysis.coverage_gaps {
                analyst_lines.push(format!("- {gap}"));
            }
        }
    }

    let mut head = header;
    if !analyst_lines.is_empty() {
        head.push('\n');
        // The analyst sections have no per-item cap of their own, so this is
        // the bound that stops one verbose item from evicting the panels.
        head.push_str(&truncate_bytes(
            &analyst_lines.join("\n"),
            NEEDS_PARENT_ANALYST_BYTE_CAP,
        ));
    }

    let mut lines = vec![head, String::new(), "Panels:".into()];
    // Whatever the header + analyst block did not use is the panel block's,
    // split evenly so a late panel is never starved by an early one.
    let panel_budget = NEEDS_PARENT_TEXT_BYTE_CAP
        .saturating_sub(lines.iter().map(|line| line.len() + 1).sum::<usize>());
    let per_panel = panel_budget / panels.len().max(1);
    let mut ordered = panels.to_vec();
    ordered.sort_by(|a, b| a.anonymous_id.cmp(&b.anonymous_id));
    for panel in &ordered {
        let mut row = format!("- {}: {:?}", panel.anonymous_id, panel.status);
        if let Some(scores) = analysis.and_then(|a| a.scores.get(&panel.anonymous_id)) {
            let mut dims: Vec<(&String, &u8)> = scores.iter().collect();
            dims.sort_by(|a, b| a.0.cmp(b.0));
            if !dims.is_empty() {
                let rendered = dims
                    .iter()
                    .map(|(dim, score)| format!("{dim}={score}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                row.push_str(&format!(" ({rendered})"));
            }
        }
        // The row goes in unconditionally: it is the identity of a panel the
        // run already paid for, and it is what the parent needs to reason
        // about coverage even when this panel's fields had to be trimmed.
        let field_budget = per_panel
            .saturating_sub(row.len())
            .saturating_sub(NEEDS_PARENT_PANEL_ROW_OVERHEAD);
        lines.push(row);
        if let Some(report) = &panel.report {
            // Never ABOVE the per-field caps; only further down when the
            // panel count makes this panel's share the tighter constraint.
            let summary_cap = NEEDS_PARENT_SUMMARY_BYTE_CAP.min(field_budget / 2);
            let candidate_cap =
                NEEDS_PARENT_CANDIDATE_BYTE_CAP.min(field_budget.saturating_sub(summary_cap));
            lines.push(format!(
                "  summary: {}",
                truncate_bytes(&report.summary, summary_cap)
            ));
            lines.push(format!(
                "  candidate: {}",
                truncate_bytes(&report.candidate_answer, candidate_cap)
            ));
        }
    }

    // Final backstop on the whole assembled BODY — see
    // NEEDS_PARENT_TEXT_BYTE_CAP's doc comment. It runs before the closing
    // directive is appended, so no sink inside the body (present or added
    // later) can bypass the bound AND the directive can never be the casualty
    // of one.
    let mut text = truncate_bytes(&lines.join("\n"), NEEDS_PARENT_TEXT_BYTE_CAP);
    text.push_str("\n\n");
    text.push_str(NEEDS_PARENT_CLOSING_LINE);
    text
}

/// Truncate `s` to at most `cap` bytes on a UTF-8 char boundary, marking a cut
/// with a trailing `…`.
fn truncate_bytes(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

fn reason_line(reason: &FusionNeedsParentReason) -> String {
    match reason {
        FusionNeedsParentReason::AnalystRequested { reason } => reason.clone(),
        FusionNeedsParentReason::AnalysisParseFailed => "analyst JSON could not be parsed".into(),
        FusionNeedsParentReason::AnalysisFailed { category } => {
            format!("analyst call failed: {category}")
        }
        FusionNeedsParentReason::CriticalContradiction => {
            "unresolved critical contradiction".into()
        }
        FusionNeedsParentReason::LowConfidence => "analyst confidence below merge threshold".into(),
        FusionNeedsParentReason::SynthesisFailed => "synthesizer failed".into(),
        FusionNeedsParentReason::SynthesisTimedOut => "synthesizer timed out".into(),
    }
}

fn millis_since(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Saturating conversion into `AnalyticsValue::Int`'s `i64` payload. Every
/// caller here is a count or a millisecond/token/nano-usd duration that never
/// approaches `i64::MAX` in practice; saturating instead of wrapping keeps a
/// theoretical overflow a visibly-wrong metric rather than a silently
/// negative one.
fn saturating_i64<T: TryInto<i64>>(value: T) -> i64 {
    value.try_into().unwrap_or(i64::MAX)
}

fn fusion_origin_label(origin: FusionOrigin) -> &'static str {
    match origin {
        FusionOrigin::Agent => "agent",
        FusionOrigin::Slash => "slash",
        FusionOrigin::Workflow => "workflow",
    }
}

fn fusion_preset_label(preset: FusionPreset) -> &'static str {
    match preset {
        FusionPreset::Quality => "quality",
        FusionPreset::Fast => "fast",
    }
}

fn fusion_error_label(error: &FusionError) -> &'static str {
    match error {
        FusionError::Disabled => "disabled",
        FusionError::UnavailableOnPlatform => "unavailable_on_platform",
        FusionError::InvalidConfiguration(_) => "invalid_configuration",
        FusionError::InvalidRequest(_) => "invalid_request",
        FusionError::TooFewModels { .. } => "too_few_models",
        FusionError::NotConfigured { .. } => "not_configured",
        FusionError::InvalidCustomModels(_) => "invalid_custom_models",
        FusionError::CrossProviderDenied => "cross_provider_denied",
        FusionError::NoJudgeModel { .. } => "no_judge_model",
        FusionError::StructuredOutputUnsupported => "structured_output_unsupported",
        FusionError::BudgetReservationUnavailable => "budget_reservation_unavailable",
        FusionError::BudgetExceeded => "budget_exceeded",
        FusionError::SpawnLimitExceeded => "spawn_limit_exceeded",
        FusionError::PanelAdmissionRejected(_) => "panel_admission_rejected",
        FusionError::AllPanelsFailed => "all_panels_failed",
        FusionError::AllPanelsFailedPreflight => "all_panels_failed_preflight",
        FusionError::MinPanelsNotMet => "min_panels_not_met",
        FusionError::PanelSetIncomplete => "panel_set_incomplete",
        FusionError::TimedOutEmpty => "timed_out_empty",
        FusionError::Cancelled => "cancelled",
        FusionError::Internal => "internal",
    }
}

fn add_fusion_error_metadata(md: &mut LogEventMetadata, error: &FusionError) {
    md.insert(
        "error".into(),
        AnalyticsValue::String(fusion_error_label(error).to_string()),
    );
}

fn fusion_event_metadata(request: &FusionRequest) -> LogEventMetadata {
    let mut md: LogEventMetadata = HashMap::new();
    md.insert(
        "origin".into(),
        AnalyticsValue::String(fusion_origin_label(request.origin).to_string()),
    );
    md.insert(
        "preset".into(),
        AnalyticsValue::String(fusion_preset_label(request.preset).to_string()),
    );
    md.insert(
        "dimensions_count".into(),
        AnalyticsValue::Int(saturating_i64(request.dimensions.len())),
    );
    md.insert(
        "partial_ok".into(),
        AnalyticsValue::Bool(request.partial_ok),
    );
    md.insert(
        "cross_provider".into(),
        AnalyticsValue::Bool(request.cross_provider),
    );
    md.insert(
        "explicit_models".into(),
        AnalyticsValue::Bool(request.models.is_some()),
    );
    if let Some(max_panel) = request.max_panel {
        md.insert(
            "max_panel".into(),
            AnalyticsValue::Int(i64::from(max_panel)),
        );
    }
    md
}

fn add_panel_counts(md: &mut LogEventMetadata, panels: &[PanelInternal]) {
    md.insert(
        "panel_count".into(),
        AnalyticsValue::Int(saturating_i64(panels.len())),
    );
    md.insert(
        "panel_success_count".into(),
        AnalyticsValue::Int(saturating_i64(successful(panels).len())),
    );
    md.insert(
        "panel_failed_count".into(),
        AnalyticsValue::Int(saturating_i64(
            panels
                .iter()
                .filter(|panel| panel.status != PanelRunStatus::Completed)
                .count(),
        )),
    );
}

fn add_usage_metadata(md: &mut LogEventMetadata, usage: &FusionUsage) {
    md.insert(
        "input_tokens".into(),
        AnalyticsValue::Int(saturating_i64(usage.input_tokens)),
    );
    md.insert(
        "output_tokens".into(),
        AnalyticsValue::Int(saturating_i64(usage.output_tokens)),
    );
    md.insert(
        "reasoning_tokens".into(),
        AnalyticsValue::Int(saturating_i64(usage.reasoning_tokens)),
    );
    md.insert(
        "cache_read_tokens".into(),
        AnalyticsValue::Int(saturating_i64(usage.cache_read_tokens)),
    );
    md.insert(
        "cache_write_tokens".into(),
        AnalyticsValue::Int(saturating_i64(usage.cache_write_tokens)),
    );
    md.insert(
        "provider_requests".into(),
        AnalyticsValue::Int(i64::from(usage.provider_requests)),
    );
    md.insert(
        "realized_nano_usd".into(),
        AnalyticsValue::Int(saturating_i64(usage.realized_nano_usd)),
    );
    md.insert(
        "reserved_max_nano_usd".into(),
        AnalyticsValue::Int(saturating_i64(usage.reserved_max_nano_usd)),
    );
    md.insert("estimated".into(), AnalyticsValue::Bool(usage.estimated));
}

fn add_egress_metadata(md: &mut LogEventMetadata, egress: &[String]) {
    md.insert(
        "egress_profile_count".into(),
        AnalyticsValue::Int(saturating_i64(egress.len())),
    );
    if !egress.is_empty() {
        md.insert(
            "egress_profiles".into(),
            AnalyticsValue::String(egress.join(",")),
        );
    }
}

/// Narrow unit tests for `record_failed_analyst_usage` (round-3 review
/// finding 8) — kept inline rather than in `orchestrator_test.rs` so this
/// fixer's changes stay isolated to files it owns. Full end-to-end coverage
/// (the analyst retry loop itself, on both the success and failure paths)
/// lives in `analyst.rs`'s own test module.
#[cfg(test)]
mod record_failed_analyst_usage_tests {
    use super::*;
    use crate::model_resolver::CatalogModel;

    /// Minimal, price-less stage cell: these tests only assert what
    /// `record_failed_analyst_usage` writes into `usage`/`priced_analyst`,
    /// but it now also refreshes the survives-a-drop cells (round-5 review
    /// items 1/2/4), so it needs one to write into. `()` is the
    /// no-rates `FusionPriceBook` and an empty `Vec<CatalogModel>` the empty
    /// `ModelSource`.
    struct StageFixture {
        realized_tokens: Arc<Mutex<Option<u64>>>,
        settlement: Arc<Mutex<Option<(u64, bool)>>>,
        catalog: Vec<CatalogModel>,
        snapshot: CatalogSnapshot,
        prices: (),
        analyst: ResolvedPanel,
        synthesizer: ResolvedPanel,
        facts: FusionRunFactsRecorder,
    }

    impl StageFixture {
        fn new() -> Self {
            let catalog = Vec::new();
            let snapshot = CatalogSnapshot::capture(&catalog).unwrap();
            Self {
                realized_tokens: Arc::new(Mutex::new(None)),
                settlement: Arc::new(Mutex::new(None)),
                catalog,
                snapshot,
                prices: (),
                analyst: ResolvedPanel {
                    profile: "profile".into(),
                    model: "model".into(),
                },
                synthesizer: ResolvedPanel {
                    profile: "profile".into(),
                    model: "model".into(),
                },
                facts: FusionRunFactsRecorder::default(),
            }
        }

        fn cell(&self) -> StageSettlement<'_> {
            StageSettlement {
                realized_tokens: &self.realized_tokens,
                settlement: &self.settlement,
                catalog: &self.catalog,
                live_catalog: &self.catalog,
                catalog_snapshot: &self.snapshot,
                prices: &self.prices,
                analyst: &self.analyst,
                synthesizer: &self.synthesizer,
                request_prompt: "task",
                request: None,
                config: None,
                side_query: None,
                synth_estimate: Arc::new(Mutex::new(None)),
                panels: &[],
                operational_deadline: Instant::now() + Duration::from_secs(60),
                cancel: CancellationToken::new(),
                reserved_max_nano_usd: 0,
                facts: self.facts.clone(),
                started: Instant::now(),
            }
        }
    }

    struct PanicPrices;

    impl FusionPriceBook for PanicPrices {
        fn rates_for(&self, _profile: &str, _model: &str) -> Option<crate::budget::ModelRates> {
            panic!("injected stage pricing panic")
        }
    }

    #[test]
    fn stage_refresh_latches_returned_usage_before_pricing_can_panic() {
        let fixture = StageFixture::new();
        *fixture.settlement.lock().unwrap() = Some((17, false));
        let prices = PanicPrices;
        let mut stage = fixture.cell();
        stage.prices = &prices;
        let analyst = cost::Usage {
            tokens: cost::TokenUsage {
                input: 5,
                output: 3,
                ..cost::TokenUsage::default()
            },
            ..cost::Usage::default()
        };
        let synth = cost::Usage {
            tokens: cost::TokenUsage {
                input: 17,
                output: 19,
                ..cost::TokenUsage::default()
            },
            ..cost::Usage::default()
        };

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            stage.refresh(Some((&analyst, 1)), true, Some(&synth), true, false);
        }));
        assert!(panic.is_err());

        let facts = fixture.facts.snapshot();
        let usage = facts
            .usage
            .expect("provider-returned analyst and synth usage must be latched first");
        assert_eq!(usage.input_tokens, 22);
        assert_eq!(usage.output_tokens, 22);
        assert_eq!(usage.provider_requests, 2);
        assert_eq!(usage.realized_nano_usd, 17);
        assert!(facts.usage_incomplete);
        assert_eq!(facts.confirmed_egress, vec!["profile"]);
        assert_eq!(*fixture.settlement.lock().unwrap(), Some((17, false)));
    }

    #[test]
    fn prices_real_non_empty_usage_instead_of_discarding_it_and_flags_incomplete() {
        let acc = AnalystUsage {
            usage: cost::Usage {
                tokens: cost::TokenUsage {
                    input: 120,
                    output: 60,
                    ..cost::TokenUsage::default()
                },
                ..cost::Usage::default()
            },
            calls: 2,
            unreported_calls: 0,
            incomplete: false,
        };
        let mut priced_analyst: Option<(cost::Usage, u32)> = None;
        let mut incomplete = false;
        let mut usage = FusionUsage::default();
        let fixture = StageFixture::new();
        FusionOrchestrator::record_failed_analyst_usage(
            acc,
            &mut usage,
            &mut priced_analyst,
            &mut incomplete,
            true,
            &fixture.cell(),
        );
        assert!(
            incomplete,
            "any analyst error exit must flag the run's usage as not exact"
        );
        let (priced_usage, calls) =
            priced_analyst.expect("a non-empty accumulator must be priced for real, not discarded");
        assert_eq!(
            priced_usage.tokens.input, 120,
            "real input tokens must survive"
        );
        assert_eq!(
            priced_usage.tokens.output, 60,
            "real output tokens must survive"
        );
        assert_eq!(calls, 2, "both billed attempts must be counted");
        // [Round-4 review finding 4] The same real usage that gets PRICED
        // into `realized_nano_usd` (via `priced_analyst`, asserted above)
        // must also be rolled into the run's `FusionUsage` token/request
        // counts — otherwise `<usage>` and a workflow token budget both
        // under-report the very tokens this run was just billed for.
        assert_eq!(
            usage.input_tokens, 120,
            "a failed analyst call's real, already-billed input tokens must still be \
counted in FusionUsage, not just priced into realized_nano_usd"
        );
        assert_eq!(
            usage.output_tokens, 60,
            "a failed analyst call's real, already-billed output tokens must still be \
counted in FusionUsage, not just priced into realized_nano_usd"
        );
        assert_eq!(
            usage.provider_requests, 2,
            "a failed analyst call's billed provider requests must still be counted"
        );
    }

    #[test]
    fn leaves_priced_analyst_none_when_the_accumulator_is_genuinely_empty() {
        // The outer per-stage timeout (`run_analyst_call`) cancels `analyze`
        // mid-flight and hands back `AnalystUsage::default()` — nothing to
        // price for real, so the caller's `judge_input_token_estimate`
        // fallback must still run.
        let acc = AnalystUsage::default();
        let mut priced_analyst: Option<(cost::Usage, u32)> = None;
        let mut incomplete = false;
        let mut usage = FusionUsage::default();
        let fixture = StageFixture::new();
        FusionOrchestrator::record_failed_analyst_usage(
            acc,
            &mut usage,
            &mut priced_analyst,
            &mut incomplete,
            true,
            &fixture.cell(),
        );
        assert!(incomplete);
        assert!(
            priced_analyst.is_none(),
            "a genuinely empty accumulator must fall through to the coarse \
estimate fallback, not price a bogus exact zero"
        );
        assert_eq!(
            usage,
            FusionUsage::default(),
            "an empty accumulator must not roll any tokens into FusionUsage"
        );
    }
}

/// Narrow unit tests for `check_panel_bar`'s `ok == 0` branch (round-3 review
/// finding 12, rework) — kept inline for the same reason as
/// `record_failed_analyst_usage_tests` above. `check_panel_bar` is a pure
/// function of `&[PanelInternal]` plus a request/config, so these need no
/// spawner mock at all.
#[cfg(test)]
mod check_panel_bar_preflight_tests {
    use super::*;

    fn panel_with_category(category: Option<&str>) -> PanelInternal {
        PanelInternal {
            index: 0,
            profile: "profile".into(),
            model: "model".into(),
            anonymous_id: String::new(),
            status: PanelRunStatus::Failed,
            report: None,
            duration_ms: 0,
            error_category: category.map(str::to_string),
            error_detail: None,
            usage: None,
            spawn_prompt: String::new(),
        }
    }

    /// `check_panel_bar`'s `request` parameter is only read in the
    /// `ok >= min` tail (`request.partial_ok`) — every case below is in the
    /// `ok == 0` branch, so the exact field values here are irrelevant.
    fn minimal_request() -> FusionRequest {
        FusionRequest {
            schema_version: 1,
            origin: FusionOrigin::Agent,
            prompt: "task".into(),
            preset: FusionPreset::Quality,
            models: None,
            dimensions: vec![],
            partial_ok: true,
            max_panel: None,
            cross_provider: false,
            parent_profile: "p".into(),
            parent_model: "m".into(),
            workflow_run_id: None,
        }
    }

    /// The defect this fixes: a spawner-rejection ahead of every panel
    /// (`SubagentSpawnError::PoolFull`, or an unresolvable panel agent
    /// definition) collapses every panel to `error_category: "spawn"`, and
    /// before this fix that reported the same `AllPanelsFailed` as a run
    /// where panels genuinely called a provider and lost — `tools/agent`
    /// cannot then tell "zero provider calls" apart from "provider calls
    /// that failed", so it keeps the whole spawn reservation charged for
    /// subagents that never existed.
    #[test]
    fn every_panel_spawn_rejected_reports_the_distinct_preflight_variant() {
        let panels = vec![
            panel_with_category(Some("spawn")),
            panel_with_category(Some("spawn")),
            panel_with_category(Some("spawn")),
        ];
        let err = check_panel_bar(
            &panels,
            &minimal_request(),
            &FusionRuntimeConfig::defaults(),
        )
        .unwrap_err();
        assert_eq!(
            err,
            FusionError::AllPanelsFailedPreflight,
            "every panel failed via a pre-allocation spawner rejection — zero provider calls \
were possible — so this must be reported distinctly from a genuine all-panels-failed run"
        );
    }

    /// Negative guard: a MIXED failure set (at least one panel genuinely
    /// called a provider and lost) must keep reporting the general
    /// `AllPanelsFailed` — this is NOT a preflight shape, since at least one
    /// panel really spawned and was billed.
    #[test]
    fn a_single_genuine_provider_failure_keeps_the_general_all_panels_failed_variant() {
        let panels = vec![
            panel_with_category(Some("spawn")),
            panel_with_category(Some("provider")),
        ];
        let err = check_panel_bar(
            &panels,
            &minimal_request(),
            &FusionRuntimeConfig::defaults(),
        )
        .unwrap_err();
        assert_eq!(
            err,
            FusionError::AllPanelsFailed,
            "at least one panel genuinely called a provider (and was billed) — this must NOT \
be classified as preflight, or a real spend would be refunded as if it never happened"
        );
    }

    /// [Round-5 review item 8] The early-abort bar (`join_set.abort_all()`)
    /// seals a doomed run by killing every still-running sibling, and a
    /// slot killed before its task ever reached the spawner is exactly as
    /// provably-preflight as a spawner rejection — `panel::run_panels`
    /// labels it `"not_dispatched"`. Before this fix ONE such slot made the
    /// `all(== "spawn")` test false forever, so a run in which the spawner
    /// rejected every panel degraded to the generic `AllPanelsFailed` and
    /// the caller kept the whole spawn reservation charged for subagents
    /// that never existed — and WHICH variant came out depended on which
    /// rejection happened to return last.
    #[test]
    fn spawn_rejections_plus_an_undispatched_abort_still_report_preflight() {
        let panels = vec![
            panel_with_category(Some("spawn")),
            panel_with_category(Some("spawn")),
            panel_with_category(Some("not_dispatched")),
        ];
        let err = check_panel_bar(
            &panels,
            &minimal_request(),
            &FusionRuntimeConfig::defaults(),
        )
        .unwrap_err();
        assert_eq!(
            err,
            FusionError::AllPanelsFailedPreflight,
            "no panel here reached the spawner, so zero provider calls were possible — the \
bar-abort of an unstarted slot must not mask that"
        );
    }

    /// Negative guard for the case directly above: an `"aborted"` slot was
    /// ALLOCATED a real child and then cut down mid-flight, so it can have
    /// been billed. Mixing one into an otherwise all-rejected set must keep
    /// the general variant — releasing the reservation there would refund a
    /// subagent that really existed.
    ///
    /// [Round-6 blocking B1] Re-pointed from "reached the spawner" to
    /// "was allocated": `panel::panel_from_join_error` now only labels a
    /// bar-aborted slot `"aborted"` when the spawner reported
    /// `SubagentObservation::Allocated` for it. A slot aborted while still
    /// INSIDE a spawner call that had not allocated anything is
    /// `"not_dispatched"` and belongs to the preflight case above.
    #[test]
    fn an_allocated_slot_cut_down_mid_flight_blocks_the_preflight_variant() {
        let panels = vec![
            panel_with_category(Some("spawn")),
            panel_with_category(Some("aborted")),
        ];
        let err = check_panel_bar(
            &panels,
            &minimal_request(),
            &FusionRuntimeConfig::defaults(),
        )
        .unwrap_err();
        assert_eq!(
            err,
            FusionError::AllPanelsFailed,
            "a mid-flight abort of an ALLOCATED child may have been billed; only a slot \
that never had a subagent allocated is provably preflight"
        );
    }

    /// Every real provider-side failure category (not just `\"provider\"`
    /// itself) must also keep the general variant — none of them guarantee
    /// zero provider calls the way `\"spawn\"` does.
    #[test]
    fn all_panels_failed_stays_general_for_non_spawn_categories() {
        for category in ["provider", "aborted", "panic", "no_structured_output"] {
            let panels = vec![
                panel_with_category(Some(category)),
                panel_with_category(Some(category)),
            ];
            let err = check_panel_bar(
                &panels,
                &minimal_request(),
                &FusionRuntimeConfig::defaults(),
            )
            .unwrap_err();
            assert_eq!(
                err,
                FusionError::AllPanelsFailed,
                "category {category:?} does not guarantee zero provider calls; must not be \
classified as preflight"
            );
        }
    }

    #[test]
    fn terminal_bar_never_clamps_configured_minimum_to_panel_count() {
        let mut panels = vec![panel_with_category(None), panel_with_category(None)];
        for panel in &mut panels {
            panel.status = PanelRunStatus::Completed;
            panel.report = Some(platform_api::PanelReport {
                schema_version: platform_api::FUSION_SCHEMA_VERSION,
                summary: "ok".into(),
                candidate_answer: "answer".into(),
                claims: Vec::new(),
                evidence: Vec::new(),
                assumptions: Vec::new(),
                risks: Vec::new(),
                unresolved_questions: Vec::new(),
            });
        }
        let mut config = FusionRuntimeConfig::defaults();
        config.min_successful_panels = 3;

        let err = check_panel_bar(&panels, &minimal_request(), &config).unwrap_err();
        assert_eq!(
            err,
            FusionError::MinPanelsNotMet,
            "two successes must not satisfy an explicitly configured minimum of three"
        );
    }
}

/// Review-round3 item 27 (rework): `run()`'s outer `Err` choke point (the
/// `if let Err(error) = &outcome` block right before `run()` returns) must
/// not silently emit `realized_output_tokens: None` when the run's own
/// panels already made real, billed provider calls before the outer
/// cancel/timeout race decided the run — that number is what lets a caller
/// tracking a SEPARATE token budget (`local_workflow`'s `fusion()` bridge
/// arm) charge already-spent tokens instead of leaving its budget ceiling
/// stuck at whatever it was before the call. Kept inline rather than in
/// `orchestrator_test.rs` so this fixer's changes stay isolated to files it
/// owns (same rationale as `record_failed_analyst_usage_tests` above) — the
/// mocks below are deliberately minimal duplicates of the ones in that file,
/// not a shared import, for the same reason.
#[cfg(test)]
mod outer_err_arm_realized_tokens_tests {
    use super::*;
    use crate::model_resolver::CatalogModel;
    use platform_api::budget::{BudgetEnforcerHandle, BudgetError};
    use platform_api::subagent_spawn::{
        SubagentInheritance, SubagentResult, SubagentSpawnError, SubagentSpawnRequest,
        SubagentUsage,
    };
    use platform_api::tool_invoker::{SubagentInvocationContext, ToolInvoker, ToolInvokerError};
    use platform_api::{
        EvidenceKind, FusionModelHints, FusionModelRef, FusionOrigin, FusionPreset, PanelClaim,
        PanelEvidence, PanelReport, DEFAULT_FUSION_DIMENSIONS,
    };
    use serde_json::Value;
    use sidequery::{
        SideQueryClient, SideQueryError, SideQueryRequest, SideQueryResponse,
        StrictStructuredQueryRequest, StrictStructuredQueryResponse,
    };
    use tokio_util::sync::CancellationToken;

    struct InertInvoker;
    #[async_trait]
    impl ToolInvoker for InertInvoker {
        async fn invoke(
            &self,
            _name: &str,
            _input: Value,
            _ctx: SubagentInvocationContext,
        ) -> Result<Value, ToolInvokerError> {
            Ok(Value::Null)
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    struct InertBudget;
    #[async_trait]
    impl BudgetEnforcerHandle for InertBudget {
        async fn check_and_charge(&self, _nano_usd: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        async fn snapshot_total_nano_usd(&self) -> u64 {
            0
        }
    }

    /// Every panel completes immediately with real, non-zero usage (8 input
    /// + 4 output tokens each — 12 output tokens total across 3 panels).
    struct QuickReportSpawner;
    #[async_trait]
    impl SubagentSpawner for QuickReportSpawner {
        async fn spawn(
            &self,
            _request: SubagentSpawnRequest,
            _inherit: SubagentInheritance,
        ) -> Result<SubagentResult, SubagentSpawnError> {
            let report = PanelReport {
                schema_version: 1,
                summary: "summary".into(),
                candidate_answer: "answer".into(),
                claims: vec![PanelClaim {
                    statement: "claim".into(),
                    evidence_refs: vec!["e1".into()],
                    confidence: 80,
                }],
                evidence: vec![PanelEvidence {
                    id: "e1".into(),
                    kind: EvidenceKind::File,
                    locator: "src/lib.rs".into(),
                    excerpt: None,
                }],
                assumptions: vec![],
                risks: vec![],
                unresolved_questions: vec![],
            };
            Ok(SubagentResult::Completed {
                agent_id: protocol::AgentId::new(),
                content: serde_json::to_value(&report).unwrap(),
                usage: SubagentUsage {
                    total_tokens: 12,
                    input_tokens: 8,
                    output_tokens: 4,
                    ..SubagentUsage::default()
                },
                total_tool_use_count: 0,
                total_duration_ms: 1,
                total_tokens: 12,
                assistant_message_count: 1,
                response_char_count: 1,
                last_request_id: None,
                cumulative_usage: SubagentUsage {
                    total_tokens: 12,
                    input_tokens: 8,
                    output_tokens: 4,
                    ..SubagentUsage::default()
                },
                usage_complete: true,
            })
        }
    }

    /// The analyst call (`query_json_schema`) never returns — models the
    /// run being cancelled while it is genuinely still in flight, well
    /// after every panel already completed and billed real usage.
    struct HangingAnalyst;
    #[async_trait]
    impl SideQueryClient for HangingAnalyst {
        async fn query(
            &self,
            _request: SideQueryRequest,
        ) -> Result<SideQueryResponse, SideQueryError> {
            Err(SideQueryError::InvalidResponse(
                "unexpected synthesizer call in this test".into(),
            ))
        }
        async fn query_json_schema(
            &self,
            _request: StrictStructuredQueryRequest,
        ) -> Result<StrictStructuredQueryResponse, SideQueryError> {
            std::future::pending::<()>().await;
            unreachable!("cancelled before the pending future is ever polled to completion")
        }
    }

    fn catalog() -> Vec<CatalogModel> {
        [
            "anthropic:claude-sonnet-5",
            "openai:gpt-5.6-terra",
            "deepseek:deepseek-v4-pro",
        ]
        .into_iter()
        .map(|pair| {
            let (profile, model) = pair.split_once(':').unwrap();
            CatalogModel {
                profile: profile.into(),
                model: model.into(),
                hints: FusionModelHints {
                    eligible: true,
                    quality_rank: 90,
                    judge_eligible: true,
                    ..FusionModelHints::default()
                },
                structured_output: true,
                limits: crate::model_resolver::known_test_limits(),
            }
        })
        .collect()
    }

    fn request() -> FusionRequest {
        FusionRequest {
            schema_version: 1,
            origin: FusionOrigin::Slash,
            prompt: "task".into(),
            preset: FusionPreset::Quality,
            models: Some(vec![
                FusionModelRef {
                    profile: Some("anthropic".into()),
                    model: "claude-sonnet-5".into(),
                },
                FusionModelRef {
                    profile: Some("openai".into()),
                    model: "gpt-5.6-terra".into(),
                },
                FusionModelRef {
                    profile: Some("deepseek".into()),
                    model: "deepseek-v4-pro".into(),
                },
            ]),
            dimensions: DEFAULT_FUSION_DIMENSIONS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            partial_ok: true,
            max_panel: None,
            cross_provider: true,
            parent_profile: "anthropic".into(),
            parent_model: "claude-sonnet-5".into(),
            workflow_run_id: None,
        }
    }

    fn test_config() -> FusionRuntimeConfig {
        let mut cfg = FusionRuntimeConfig::defaults();
        cfg.panel_total_timeout_ms = 5_000;
        cfg.analyst_timeout_ms = 5_000;
        cfg.synthesizer_timeout_ms = 5_000;
        cfg.total_timeout_ms = 5_000;
        cfg.min_successful_panels = 2;
        // The panels come from this fixture's explicit `models` list; the
        // analyst and synthesizer roles are configuration and have no
        // automatic fallback.
        cfg.analyst_model = Some(platform_api::FusionModelChoice::new(
            "anthropic",
            "claude-sonnet-5",
        ));
        cfg.synthesizer_model = Some(platform_api::FusionModelChoice::new(
            "anthropic",
            "claude-sonnet-5",
        ));
        cfg
    }

    #[tokio::test]
    async fn cancel_after_real_panel_spend_carries_realized_tokens_on_the_outer_err_arm() {
        let orch = FusionOrchestrator::new(
            Arc::new(QuickReportSpawner),
            Arc::new(HangingAnalyst),
            Arc::new(test_config()),
            Arc::new(catalog()),
        );
        let cancel = CancellationToken::new();
        let inherit = FusionInheritance::new(
            SubagentInheritance {
                tool_invoker: Arc::new(InertInvoker),
                budget: Arc::new(InertBudget),
            },
            cancel.clone(),
        );
        let (tx, mut rx) = tokio::sync::mpsc::channel::<FusionProgress>(64);
        let handle = tokio::spawn(async move { orch.run(request(), inherit, Some(tx)).await });
        // Panels resolve on the next few poll cycles; the analyst call then
        // blocks forever. 100ms is generous headroom for the panels to
        // really finish (and bill real usage) before cancellation lands
        // mid-analyst-call.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        cancel.cancel();
        let err = handle
            .await
            .expect("run() task must not panic")
            .expect_err("a cancelled run must return Err");
        assert_eq!(err, FusionError::Cancelled);

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        let cancelled_event = events
            .iter()
            .find(|event| matches!(event.stage, FusionStage::Cancelled))
            .expect("run() must emit a Cancelled-stage progress event on the outer cancel arm");
        assert_eq!(
            cancelled_event.realized_output_tokens,
            Some(12),
            "the outer cancel arm must carry the 3 panels' already-billed \
output tokens (3 * 4 = 12), not silently report None for real, billed \
spend — got {:?} (all events: {:?})",
            cancelled_event.realized_output_tokens,
            events
                .iter()
                .map(|e| (format!("{:?}", e.stage), e.realized_output_tokens))
                .collect::<Vec<_>>()
        );
        // [Round-3 review B2, reworked, blocking issue 2] All 3 panels
        // (anthropic/openai/deepseek) genuinely dispatched and completed
        // before the analyst call hung and the cancel landed — the real
        // orchestrator, not a fabricated progress event, must report all 3
        // as egress on the outer cancel arm.
        let mut got = cancelled_event.egress_profiles.clone().unwrap_or_default();
        got.sort();
        assert_eq!(
            got,
            vec![
                "anthropic".to_string(),
                "deepseek".to_string(),
                "openai".to_string(),
            ],
            "the outer cancel arm must disclose the 3 panel profiles that \
were genuinely dispatched before cancellation, not None or a subset — got \
{:?} (all events: {:?})",
            cancelled_event.egress_profiles,
            events
                .iter()
                .map(|e| (format!("{:?}", e.stage), e.egress_profiles.clone()))
                .collect::<Vec<_>>()
        );
    }

    /// One panel (`deepseek`) is rejected pre-allocation by the spawner
    /// (`SubagentSpawnError::PoolFull`, the same shape `panel.rs` folds
    /// into `error_category: "spawn"`) while the other two genuinely
    /// dispatch and complete with real, billed usage. `partial_ok: false`
    /// on the request means `check_panel_bar` rejects this as
    /// `PanelSetIncomplete` (2 of 3 completed) — the `check_panel_bar`
    /// arm in `run_inner`, not the outer cancel/timeout choke point.
    struct PartialSpawnFailureSpawner {
        /// The profile whose spawn is rejected before dispatch.
        failing_profile: &'static str,
    }
    #[async_trait]
    impl SubagentSpawner for PartialSpawnFailureSpawner {
        async fn spawn(
            &self,
            request: SubagentSpawnRequest,
            _inherit: SubagentInheritance,
        ) -> Result<SubagentResult, SubagentSpawnError> {
            if request.model_profile.as_deref() == Some(self.failing_profile) {
                return Err(SubagentSpawnError::PoolFull);
            }
            let report = PanelReport {
                schema_version: 1,
                summary: "summary".into(),
                candidate_answer: "answer".into(),
                claims: vec![PanelClaim {
                    statement: "claim".into(),
                    evidence_refs: vec!["e1".into()],
                    confidence: 80,
                }],
                evidence: vec![PanelEvidence {
                    id: "e1".into(),
                    kind: EvidenceKind::File,
                    locator: "src/lib.rs".into(),
                    excerpt: None,
                }],
                assumptions: vec![],
                risks: vec![],
                unresolved_questions: vec![],
            };
            Ok(SubagentResult::Completed {
                agent_id: protocol::AgentId::new(),
                content: serde_json::to_value(&report).unwrap(),
                usage: SubagentUsage {
                    total_tokens: 12,
                    input_tokens: 8,
                    output_tokens: 4,
                    ..SubagentUsage::default()
                },
                total_tool_use_count: 0,
                total_duration_ms: 1,
                total_tokens: 12,
                assistant_message_count: 1,
                response_char_count: 1,
                last_request_id: None,
                cumulative_usage: SubagentUsage {
                    total_tokens: 12,
                    input_tokens: 8,
                    output_tokens: 4,
                    ..SubagentUsage::default()
                },
                usage_complete: true,
            })
        }
    }

    #[tokio::test]
    async fn check_panel_bar_failure_discloses_only_dispatched_panel_profiles_never_the_analyst() {
        let orch = FusionOrchestrator::new(
            Arc::new(PartialSpawnFailureSpawner {
                failing_profile: "deepseek",
            }),
            // The analyst must never be called on this arm — a client that
            // DOES call it would make this test fail with an unexpected-call
            // panic/error, which is exactly the assertion this test wants
            // for the analyst side of the finding.
            Arc::new(HangingAnalyst),
            Arc::new(test_config()),
            Arc::new(catalog()),
        );
        let cancel = CancellationToken::new();
        let inherit = FusionInheritance::new(
            SubagentInheritance {
                tool_invoker: Arc::new(InertInvoker),
                budget: Arc::new(InertBudget),
            },
            cancel,
        );
        let (tx, mut rx) = tokio::sync::mpsc::channel::<FusionProgress>(64);
        let mut req = request();
        req.partial_ok = false;
        let err = orch
            .run(req, inherit, Some(tx))
            .await
            .expect_err("2 of 3 panels completing under partial_ok:false must fail the bar");
        assert_eq!(err, FusionError::PanelSetIncomplete);

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        let failed_event = events
            .iter()
            .find(|event| matches!(event.stage, FusionStage::Failed))
            .expect("check_panel_bar's Err arm must emit a Failed-stage progress event");
        let mut got = failed_event.egress_profiles.clone().unwrap_or_default();
        got.sort();
        assert_eq!(
            got,
            vec!["anthropic".to_string(), "openai".to_string()],
            "must disclose exactly the 2 panels that genuinely dispatched \
(anthropic, openai) — never the rejected `deepseek` panel (error_category \
\"spawn\", zero provider calls) and never the analyst profile (the panel \
bar failed before `analyze_and_decide` ever ran, so the analyst was \
provably never called) — got {:?} (all events: {:?})",
            failed_event.egress_profiles,
            events
                .iter()
                .map(|e| (format!("{:?}", e.stage), e.egress_profiles.clone()))
                .collect::<Vec<_>>()
        );
    }
}
