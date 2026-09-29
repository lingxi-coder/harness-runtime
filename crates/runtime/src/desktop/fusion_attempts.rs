//! Opt-in physical-attempt host. Construction alone does not install hooks.
//! Service -> hooks is strong; hooks -> service and registry -> run are Weak.
mod pricing;
#[cfg(test)]
mod tests;

use async_trait::async_trait;
use futures::FutureExt;
use lingxi_core::host::{
    ModelAttemptContext, ModelAttemptRegistrationId, ModelAttemptRun, ModelAttemptStage,
    WorkflowOutputScope, WorkflowOutputScopes,
};
use llm_runtime::{LlmError, ModelAttemptUsageCompleteness};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
    time::Instant,
};

type RouteKey = (ModelAttemptStage, Option<u32>);
type ProfilePermits = Arc<Mutex<HashMap<String, Arc<tokio::sync::Semaphore>>>>;

pub(crate) struct DesktopFusionAttempts {
    service: Weak<llm_runtime::ApiService>,
    budget: Arc<cost::BudgetEnforcer>,
    tracker: Arc<cost::CostTracker>,
    prices: Arc<cost::PricingCatalog>,
    outputs: Arc<dyn WorkflowOutputScopes>,
    profiles: ProfilePermits,
    registry: Mutex<HashMap<ModelAttemptRegistrationId, Weak<RunAuthority>>>,
}

struct PinnedRoute {
    resolved: llm_runtime::ResolvedRoute,
    limits: fusion::ModelLimits,
    output_cap: u32,
    input_cap: Option<u64>,
    pricing: cost::ModelPricing,
    fast: Option<cost::ModelPricing>,
    token_pricing_requires_quote: bool,
    default_fast: bool,
}

struct RunAuthority {
    captured: fusion::FusionAttemptRegistration,
    output: WorkflowOutputScope,
    tracker: Arc<cost::CostTracker>,
    budget: Arc<cost::BudgetEnforcer>,
    profiles: ProfilePermits,
    routes: HashMap<RouteKey, PinnedRoute>,
    state: Mutex<RunState>,
    changed: tokio::sync::Notify,
    runtime: Mutex<Option<tokio::runtime::Handle>>,
}

#[derive(Default)]
struct RunState {
    closed: bool,
    panel_closed: bool,
    panel_pending: usize,
    pending: usize,
    ordinals: HashMap<u64, u64>,
    entries: HashMap<String, Entry>,
    error: Option<String>,
}

struct Entry {
    intent: cost::AttemptIntent,
    contribution: Option<cost::AttemptContribution>,
    dispatched: bool,
    last_raw: Option<llm_runtime::ExecutionUsage>,
    last_known: Option<cost::Usage>,
}

#[derive(Default)]
struct WaitSlot {
    result: Mutex<Option<Result<(), String>>>,
    changed: tokio::sync::Notify,
}
impl WaitSlot {
    fn complete(&self, result: Result<(), String>) {
        *self
            .result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result);
        self.changed.notify_waiters();
    }
    async fn wait(&self) -> Result<(), String> {
        loop {
            let notified = self.changed.notified();
            if let Some(result) = self
                .result
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
            {
                return result;
            }
            notified.await;
        }
    }
}

fn unavailable(error: impl ToString) -> LlmError {
    LlmError::CostUnavailable {
        message: error.to_string(),
    }
}
fn fusion_error(error: impl ToString) -> lingxi_core::host::FusionError {
    lingxi_core::host::FusionError::InvalidConfiguration(error.to_string())
}

impl DesktopFusionAttempts {
    pub(crate) fn new(
        service: Arc<llm_runtime::ApiService>,
        budget: Arc<cost::BudgetEnforcer>,
        tracker: Arc<cost::CostTracker>,
        prices: Arc<cost::PricingCatalog>,
        outputs: Arc<dyn WorkflowOutputScopes>,
    ) -> Arc<Self> {
        Arc::new(Self {
            service: Arc::downgrade(&service),
            budget,
            tracker,
            prices,
            outputs,
            profiles: Arc::new(Mutex::new(HashMap::new())),
            registry: Mutex::new(HashMap::new()),
        })
    }
}

impl fusion::FusionAttemptRegistrar for DesktopFusionAttempts {
    fn register(
        &self,
        captured: fusion::FusionAttemptRegistration,
    ) -> Result<fusion::RegisteredFusionAttempts, lingxi_core::host::FusionError> {
        self.register_routes(captured, true)
    }
}

impl DesktopFusionAttempts {
    fn register_routes(
        &self,
        captured: fusion::FusionAttemptRegistration,
        include_judges: bool,
    ) -> Result<fusion::RegisteredFusionAttempts, lingxi_core::host::FusionError> {
        let session = captured
            .control
            .identity()
            .session_id
            .ok_or_else(|| fusion_error("attempt registration requires canonical session"))?;
        if captured.control.billing_mode()
            != lingxi_core::host::ModelAttemptBillingMode::MeteredAttempts
        {
            return Err(fusion_error(
                "attempt registration requires metered control",
            ));
        }
        let output = match captured.inherit.output_scope.as_ref() {
            Some(scope) => scope.clone(),
            None => self.outputs.capture(session).map_err(fusion_error)?,
        };
        if output.session_id() != session {
            return Err(fusion_error("captured output session mismatch"));
        }
        let tracker = self.tracker.scoped(session);
        tracker
            .validate_attempt_host_binding(session)
            .map_err(fusion_error)?;
        let service = self
            .service
            .upgrade()
            .ok_or_else(|| fusion_error("attempt service unavailable"))?;
        let config = &captured.snapshot.config;
        let mut selected = captured
            .resolved
            .panels
            .iter()
            .enumerate()
            .map(|(slot, panel)| {
                Ok((
                    (
                        ModelAttemptStage::Panel,
                        Some(
                            u32::try_from(slot)
                                .map_err(|_| fusion_error("panel index overflow"))?,
                        ),
                    ),
                    panel.clone(),
                    config.panel_max_output_tokens_per_turn,
                ))
            })
            .collect::<Result<Vec<_>, lingxi_core::host::FusionError>>()?;
        if include_judges {
            selected.push((
                (ModelAttemptStage::Analyst, None),
                captured.resolved.analyst.clone(),
                config.analyst_max_output_tokens,
            ));
        }
        let mut routes = HashMap::new();
        for (key, panel, configured_output) in selected {
            let pinned = (|| -> Result<PinnedRoute, lingxi_core::host::FusionError> {
                let resolved = service
                    .resolve_media_route(&panel.model, Some(&panel.profile))
                    .map_err(fusion_error)?
                    .main;
                let limits = captured
                    .snapshot
                    .catalog
                    .limits_for(&panel.profile, &panel.model)
                    .ok_or_else(|| fusion_error("missing captured model limits"))?;
                if !limits.has_usable_capacity(configured_output) {
                    return Err(fusion_error("unbounded attempt route"));
                }
                let configured_pricing = service
                    .profile_pricing_config(&resolved.profile_name)
                    .ok_or_else(|| {
                        fusion_error("captured profile pricing configuration unavailable")
                    })?;
                let (mut pricing, mut fast) = pricing::captured_prices(
                    &self.prices,
                    &pricing::model_ref(&resolved.pricing_model),
                    &resolved,
                    &configured_pricing,
                )
                .map_err(fusion_error)?;
                let bounds = service
                    .attempt_price_bounds(&resolved)
                    .map_err(fusion_error)?;
                let token_pricing_requires_quote = bounds.is_some();
                let default_fast = bounds.as_ref().is_some_and(|bounds| bounds.default_fast);
                if let Some(bounds) = bounds {
                    fast = bounds
                        .fast
                        .as_ref()
                        .map(|rates| pricing::with_rate_bounds(&pricing, rates))
                        .transpose()
                        .map_err(fusion_error)?;
                    pricing = pricing::with_rate_bounds(&pricing, &bounds.standard)
                        .map_err(fusion_error)?;
                }
                Ok(PinnedRoute {
                    resolved,
                    limits,
                    output_cap: limits.output_cap(configured_output),
                    input_cap: (key.0 == ModelAttemptStage::Panel)
                        .then_some(u64::from(config.panel_reserved_input_tokens_per_turn)),
                    pricing,
                    fast,
                    token_pricing_requires_quote,
                    default_fast,
                })
            })();
            match pinned {
                Ok(route) => {
                    routes.insert(key, route);
                }
                Err(error) => return Err(error),
            }
        }
        let authority = Arc::new(RunAuthority {
            captured,
            output,
            tracker,
            budget: self.budget.clone(),
            profiles: self.profiles.clone(),
            routes,
            state: Mutex::new(RunState::default()),
            changed: tokio::sync::Notify::new(),
            runtime: Mutex::new(tokio::runtime::Handle::try_current().ok()),
        });
        let run = Arc::new(ModelAttemptRun::new(authority.clone()));
        let mut registry = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.retain(|_, authority| authority.strong_count() != 0);
        registry.insert(run.registration_id(), Arc::downgrade(&authority));
        Ok(fusion::RegisteredFusionAttempts {
            run,
            panel_fence: Some(authority.clone()),
            finalizer: Box::new(RunFinalizer {
                authority: Some(authority),
            }),
        })
    }
}

#[async_trait]
impl fusion::FusionPanelAttemptFence for RunAuthority {
    fn close(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .panel_closed = true;
        self.changed.notify_waiters();
    }

    async fn wait(&self) -> Result<(), lingxi_core::host::FusionError> {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if !state.panel_closed {
                    return Err(lingxi_core::host::FusionError::InvalidConfiguration(
                        "panel fence is not closed".into(),
                    ));
                }
                if state.panel_pending == 0 {
                    return state.error.as_ref().map_or(Ok(()), |reason| {
                        Err(lingxi_core::host::FusionError::InvalidConfiguration(
                            reason.clone(),
                        ))
                    });
                }
            }
            notified.await;
        }
    }
}

impl RunAuthority {
    fn fail(&self, reason: impl ToString) {
        let reason = reason.to_string();
        self.tracker.durability_gate().freeze(reason.clone());
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .error
            .get_or_insert(reason);
        self.changed.notify_waiters();
    }
    fn live(&self, key: RouteKey) -> Result<(), String> {
        {
            let state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.closed
                || state.error.is_some()
                || (key.0 == ModelAttemptStage::Panel && state.panel_closed)
            {
                return Err("attempt authority closed".into());
            }
        }
        self.tracker
            .validate_attempt_host_binding(self.output.session_id())
            .map_err(|error| error.to_string())?;
        self.captured
            .live_policy
            .validate(key.0, key.1)
            .map_err(|error| error.to_string())
    }
    fn complete(
        &self,
        id: &str,
        result: Result<cost::CostAttemptSettlement, String>,
    ) -> Result<(), String> {
        let result = result.and_then(|ack| {
            ack.receipt
                .ok_or_else(|| "missing attempt receipt acknowledgment".into())
        });
        let error = result.as_ref().err().cloned();
        // Publish failure before pending reaches zero; a concurrent run drain
        // must never observe "all done" while the error is still unpublished.
        if let Some(error) = &error {
            self.fail(error);
        }
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Ok(ack) = result {
                if let Some(entry) = state.entries.get_mut(id) {
                    entry.contribution = Some(ack.contribution);
                }
            }
            if state
                .entries
                .get(id)
                .is_some_and(|entry| entry.intent.stage == ModelAttemptStage::Panel)
            {
                state.panel_pending -= 1;
            }
            state.pending -= 1;
        }
        if let Some(error) = error {
            self.changed.notify_waiters();
            return Err(error);
        }
        self.changed.notify_waiters();
        Ok(())
    }

    fn reject_admission(&self, id: &str, error: String) {
        if self.tracker.durability_gate().frozen_reason().is_some() {
            let _ = self.complete(id, Err(error));
        } else {
            // A live-policy/budget refusal before a lease exists sent nothing.
            // Cost admission guarantees no hold mutation on these rejections.
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(entry) = state.entries.get_mut(id) {
                entry.contribution = Some(cost::AttemptContribution::default());
            }
            if state
                .entries
                .get(id)
                .is_some_and(|entry| entry.intent.stage == ModelAttemptStage::Panel)
            {
                state.panel_pending -= 1;
            }
            state.pending -= 1;
            drop(state);
            self.changed.notify_waiters();
        }
    }
}

#[async_trait]
impl llm_runtime::ModelAttemptHooks for DesktopFusionAttempts {
    async fn begin(
        &self,
        context: &ModelAttemptContext,
        request: &llm_runtime::LlmRequest,
        prepared: &llm_runtime::PreparedLlmCall,
    ) -> Result<Box<dyn llm_runtime::ModelAttemptLease>, LlmError> {
        let attached = request
            .execution
            .model_attempt
            .as_ref()
            .ok_or_else(|| unavailable("registered request lacks its context"))?;
        if attached.registration_id() != context.registration_id()
            || attached.logical_call_id() != context.logical_call_id()
            || attached.stage() != context.stage()
            || attached.panel_slot() != context.panel_slot()
        {
            return Err(unavailable("request and hook attempt identities differ"));
        }
        let authority = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&context.registration_id())
            .and_then(Weak::upgrade)
            .ok_or_else(|| unavailable("unregistered attempt context"))?;
        let key = (context.stage(), context.panel_slot());
        let runtime = tokio::runtime::Handle::try_current().map_err(unavailable)?;
        *authority
            .runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(runtime.clone());
        authority.live(key).map_err(unavailable)?;
        let route = authority
            .routes
            .get(&key)
            .ok_or_else(|| unavailable("invalid registered stage/slot"))?;
        if prepared.route.resolved_route != route.resolved {
            return Err(unavailable("prepared route differs from registration"));
        }
        let (pricing, usage_contract, input, output, money) =
            pricing::quote(route, prepared).map_err(unavailable)?;
        let profile = route.resolved.profile_name.clone();
        let id = lingxi_core::types::MessageId::new().to_string();
        let intent = {
            let mut state = authority
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.closed
                || state.error.is_some()
                || (key.0 == ModelAttemptStage::Panel && state.panel_closed)
            {
                return Err(unavailable("attempt admission closed"));
            }
            let ordinal = state.ordinals.entry(context.logical_call_id()).or_default();
            *ordinal = ordinal
                .checked_add(1)
                .ok_or_else(|| unavailable("wire ordinal overflow"))?;
            let intent = cost::AttemptIntent {
                schema_version: if route.token_pricing_requires_quote {
                    2
                } else {
                    1
                },
                session_id: authority.output.session_id(),
                attempt_id: id.clone(),
                run_id: authority.captured.control.identity().run_id.to_string(),
                logical_call_id: context.logical_call_id().to_string(),
                wire_ordinal: *ordinal,
                stage: key.0,
                panel_slot: key.1,
                profile_id: profile.clone(),
                model: pricing.model_ref.clone(),
                route_revision: authority.captured.snapshot.catalog.revision().content,
                pricing,
                token_pricing_requires_quote: route.token_pricing_requires_quote,
                authorized_nano_usd: money,
                authorized_input_tokens: input,
                authorized_output_tokens: output,
                billing_mode: cost::AttemptBillingMode::MeteredAttempts,
                usage_contract,
                output_scope: None,
            };
            state.pending = state
                .pending
                .checked_add(1)
                .ok_or_else(|| unavailable("pending attempt count overflow"))?;
            if key.0 == ModelAttemptStage::Panel {
                // panel_pending <= pending, whose checked increment succeeded.
                state.panel_pending += 1;
            }
            state.entries.insert(
                id.clone(),
                Entry {
                    intent: intent.clone(),
                    contribution: None,
                    dispatched: false,
                    last_raw: None,
                    last_known: None,
                },
            );
            intent
        };
        let permit_pool = authority
            .profiles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(profile)
            .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(4)))
            .clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        runtime.spawn(async move {
            let worker = async {
                let cancel = authority.captured.control.cancel();
                let deadline = authority.captured.control.deadline().ok_or("attempt not activated")?;
                let permit = tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Err("attempt cancelled before admission".into()),
                    () = tokio::time::sleep_until(deadline) => return Err("attempt deadline expired before admission".into()),
                    permit = permit_pool.acquire_owned() => permit.map_err(|_| "profile admission closed".to_string())?,
                };
                authority.live(key)?;
                let lease = authority.budget.begin_model_attempt(intent.clone(), &authority.output,
                    authority.captured.snapshot.config.max_reserved_nano_usd.unwrap_or(u64::MAX), permit).await.map_err(|error| error.to_string())?;
                Ok::<_, String>(HostLease { authority: authority.clone(), key, intent, lease: Some(lease),
                    observation: None, dispatched: false, conversion_failed: false,
                    no_provider_response: false, started: Instant::now() })
            };
            match std::panic::AssertUnwindSafe(worker).catch_unwind().await {
                Ok(Ok(lease)) => { let _ = tx.send(Ok(Box::new(lease) as Box<dyn llm_runtime::ModelAttemptLease>)); }
                result => {
                    let (error, panicked) = match result { Ok(Err(error)) => (error, false), _ => ("attempt admission owner panicked".into(), true) };
                    if panicked { authority.fail(&error); }
                    authority.reject_admission(&id, error.clone());
                    let _ = tx.send(Err(unavailable(error)));
                }
            }
        });
        rx.await
            .map_err(|_| unavailable("attempt admission owner disappeared"))?
    }
}

struct HostLease {
    authority: Arc<RunAuthority>,
    key: RouteKey,
    intent: cost::AttemptIntent,
    lease: Option<cost::budget::CostBudgetAttempt>,
    observation: Option<cost::AttemptReceipt>,
    dispatched: bool,
    conversion_failed: bool,
    no_provider_response: bool,
    started: Instant,
}

impl HostLease {
    fn transfer(&mut self) -> Arc<WaitSlot> {
        let slot = Arc::new(WaitSlot::default());
        let Some(mut lease) = self.lease.take() else {
            slot.complete(Err("attempt already finished".into()));
            return slot;
        };
        if self.dispatched {
            if let Some(mut observation) = self.observation.take() {
                if self.conversion_failed {
                    observation.disposition = cost::AttemptDisposition::Unknown;
                    observation.token_quote_nano_usd = None;
                }
                if let Err(error) = lease.observe(observation) {
                    self.authority.fail(error);
                }
            } else if self.no_provider_response {
                // Dispatch was marked and nothing ever came back. Settle it as
                // such rather than letting the default Unknown receipt stand
                // in for a response that never existed.
                lease.mark_no_provider_response();
            }
        }
        let receipt = lease.finish().map_err(|error| error.to_string());
        let authority = self.authority.clone();
        let id = self.intent.attempt_id.clone();
        let worker_slot = slot.clone();
        let runtime = authority
            .runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .expect("accepted attempt captured runtime");
        runtime.spawn(async move {
            let settled = std::panic::AssertUnwindSafe(async {
                receipt?.settle().await.map_err(|error| error.to_string())
            })
            .catch_unwind()
            .await
            .unwrap_or_else(|_| Err("attempt settlement owner panicked".into()));
            worker_slot.complete(authority.complete(&id, settled));
        });
        slot
    }
}
impl Drop for HostLease {
    fn drop(&mut self) {
        if self.lease.is_some() {
            let _ = self.transfer();
        }
    }
}
impl llm_runtime::ModelAttemptLease for HostLease {
    fn mark_no_provider_response(&mut self) {
        self.no_provider_response = true;
    }

    fn mark_dispatched(&mut self) -> Result<(), LlmError> {
        self.authority.live(self.key).map_err(unavailable)?;
        // Serialize the final dispatch transition with panel close. Live-policy
        // callbacks run outside this lock; closure is checked again inside it.
        let mut state = self
            .authority
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed
            || state.error.is_some()
            || (self.key.0 == ModelAttemptStage::Panel && state.panel_closed)
        {
            return Err(unavailable("attempt admission closed"));
        }
        self.lease
            .as_mut()
            .ok_or_else(|| unavailable("attempt already finished"))?
            .mark_dispatched()
            .map_err(unavailable)?;
        self.dispatched = true;
        if let Some(entry) = state.entries.get_mut(&self.intent.attempt_id) {
            entry.dispatched = true;
        }
        Ok(())
    }
    fn observe_usage(
        &mut self,
        usage: &llm_runtime::ExecutionUsage,
        completeness: ModelAttemptUsageCompleteness,
    ) {
        if let Some(entry) = self
            .authority
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entries
            .get_mut(&self.intent.attempt_id)
        {
            entry.last_raw = Some(usage.clone());
        }
        let converted = orchestrator::cost_wiring::llm_usage_to_cost_usage_checked(usage);
        match converted {
            Ok(converted) => {
                if let Some(entry) = self
                    .authority
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .entries
                    .get_mut(&self.intent.attempt_id)
                {
                    entry.last_known = Some(converted);
                }
                if self
                    .observation
                    .as_ref()
                    .is_some_and(|old| old.disposition == cost::AttemptDisposition::Exact)
                    && completeness == ModelAttemptUsageCompleteness::Partial
                {
                    return;
                }
                let duration =
                    u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
                let token_quote_nano_usd = self
                    .intent
                    .token_pricing_requires_quote
                    .then(|| {
                        usage
                            .cost_estimate
                            .as_ref()
                            .and_then(|quote| pricing::token_quote(quote, &self.intent.model))
                    })
                    .flatten();
                let exact = completeness == ModelAttemptUsageCompleteness::Complete
                    && !self.conversion_failed
                    && (!self.intent.token_pricing_requires_quote
                        || token_quote_nano_usd.is_some());
                self.observation = Some(cost::AttemptReceipt {
                    session_id: self.intent.session_id,
                    attempt_id: self.intent.attempt_id.clone(),
                    revision: 1,
                    replaces_revision: None,
                    disposition: if exact {
                        cost::AttemptDisposition::Exact
                    } else {
                        cost::AttemptDisposition::Unknown
                    },
                    usage: converted,
                    token_quote_nano_usd: if exact { token_quote_nano_usd } else { None },
                    cache_read_input_tokens: usage.counts().cache_read_tokens,
                    cache_creation_input_tokens: usage.counts().cache_write_tokens,
                    api_duration_ms: duration,
                    api_duration_without_retries_ms: duration,
                });
            }
            Err(error) => {
                self.conversion_failed = true;
                // Typed normalized counters remain known even when TTL/tool
                // metadata cannot be mapped. Do not invent a cache-write tier.
                self.observation = Some(cost::AttemptReceipt {
                    session_id: self.intent.session_id,
                    attempt_id: self.intent.attempt_id.clone(),
                    revision: 1,
                    replaces_revision: None,
                    disposition: cost::AttemptDisposition::Unknown,
                    token_quote_nano_usd: None,
                    usage: cost::Usage {
                        tokens: cost::TokenUsage {
                            input: usage.counts().input_tokens,
                            output: usage
                                .counts()
                                .output_tokens
                                .saturating_sub(usage.counts().reasoning_tokens),
                            reasoning_output: usage.counts().reasoning_tokens,
                            cache_read: usage.counts().cache_read_tokens,
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    cache_read_input_tokens: usage.counts().cache_read_tokens,
                    cache_creation_input_tokens: usage.counts().cache_write_tokens,
                    api_duration_ms: 0,
                    api_duration_without_retries_ms: 0,
                });
                self.authority
                    .fail(format!("attempt usage conversion failed: {error}"));
            }
        }
    }
    fn finish(mut self: Box<Self>) -> Box<dyn llm_runtime::ModelAttemptSettlement> {
        Box::new(HostWaiter(self.transfer()))
    }
}
struct HostWaiter(Arc<WaitSlot>);
#[async_trait]
impl llm_runtime::ModelAttemptSettlement for HostWaiter {
    async fn wait(self: Box<Self>) -> Result<(), LlmError> {
        self.0.wait().await.map_err(unavailable)
    }
}

struct RunFinalizer {
    authority: Option<Arc<RunAuthority>>,
}
struct RunWaiter {
    authority: Arc<RunAuthority>,
    slot: Arc<WaitSlot>,
}
impl RunFinalizer {
    fn transfer(&mut self) -> RunWaiter {
        let authority = self.authority.take().expect("unique run finalizer");
        authority
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .closed = true;
        let slot = Arc::new(WaitSlot::default());
        let ready = {
            let state = authority
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (state.pending == 0).then(|| state.error.clone().map_or(Ok(()), Err))
        };
        if let Some(result) = ready {
            slot.complete(result);
            return RunWaiter { authority, slot };
        }
        let worker = authority.clone();
        let worker_slot = slot.clone();
        let runtime = authority
            .runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .expect("accepted attempt captured runtime");
        runtime.spawn(async move {
            loop {
                let notified = worker.changed.notified();
                let result = {
                    let state = worker
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    (state.pending == 0).then(|| state.error.clone().map_or(Ok(()), Err))
                };
                if let Some(result) = result {
                    worker_slot.complete(result);
                    break;
                }
                notified.await;
            }
        });
        RunWaiter { authority, slot }
    }
}
impl Drop for RunFinalizer {
    fn drop(&mut self) {
        if self.authority.is_some() {
            let _ = self.transfer();
        }
    }
}
impl fusion::FusionAttemptFinalizer for RunFinalizer {
    fn finish(mut self: Box<Self>) -> Box<dyn fusion::FusionAttemptSettlement> {
        Box::new(self.transfer())
    }
}
#[async_trait]
impl fusion::FusionAttemptSettlement for RunWaiter {
    async fn wait(
        self: Box<Self>,
    ) -> Result<fusion::FusionAttemptSummary, fusion::FusionAttemptSettlementError> {
        let result = self.slot.wait().await;
        let summary = self.authority.summary();
        let result = result.and_then(|()| {
            self.authority
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .error
                .clone()
                .map_or(Ok(()), Err)
        });
        match result {
            Ok(()) => Ok(summary),
            Err(error) => Err(fusion::FusionAttemptSettlementError {
                error: fusion_error(error),
                summary,
            }),
        }
    }
}

impl RunAuthority {
    fn summary(&self) -> fusion::FusionAttemptSummary {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut usage = lingxi_core::host::FusionUsage::default();
        let mut possible = Vec::new();
        let mut confirmed = Vec::new();
        let mut overflow = false;
        let mut requests = 0_u64;
        let mut pricing_failed = false;
        let mut add = |to: &mut u64, amount: u64| {
            *to = to.checked_add(amount).unwrap_or_else(|| {
                overflow = true;
                u64::MAX
            });
        };
        for entry in state.entries.values() {
            if entry.dispatched {
                possible.push(entry.intent.profile_id.clone());
            }
            if let Some(contribution) = &entry.contribution {
                add(&mut usage.input_tokens, contribution.usage.tokens.input);
                add(&mut usage.output_tokens, contribution.usage.tokens.output);
                add(
                    &mut usage.reasoning_tokens,
                    contribution.usage.tokens.reasoning_output,
                );
                add(
                    &mut usage.cache_read_tokens,
                    contribution.usage.tokens.cache_read,
                );
                add(
                    &mut usage.cache_write_tokens,
                    contribution.usage.tokens.cache_write,
                );
                add(
                    &mut usage.cache_write_tokens,
                    contribution.usage.tokens.cache_write_1h,
                );
                add(&mut usage.realized_nano_usd, contribution.nano_usd);
                add(&mut requests, contribution.request_count);
                usage.estimated |= contribution.unknown_count != 0;
                if entry.dispatched && contribution.unknown_count == 0 {
                    confirmed.push(entry.intent.profile_id.clone());
                }
            } else {
                if let Some(raw) = &entry.last_raw {
                    add(&mut usage.input_tokens, raw.counts().input_tokens);
                    add(
                        &mut usage.output_tokens,
                        raw.counts()
                            .output_tokens
                            .saturating_sub(raw.counts().reasoning_tokens),
                    );
                    add(&mut usage.reasoning_tokens, raw.counts().reasoning_tokens);
                    add(&mut usage.cache_read_tokens, raw.counts().cache_read_tokens);
                    add(
                        &mut usage.cache_write_tokens,
                        raw.counts().cache_write_tokens,
                    );
                }
                let known_money =
                    match entry.last_known.as_ref().map(|known| {
                        cost::calculate_pinned_attempt_cost(known, &entry.intent.pricing)
                    }) {
                        None => 0,
                        Some(Ok(money)) => money,
                        Some(Err(_)) => {
                            pricing_failed = true;
                            u64::MAX
                        }
                    };
                add(
                    &mut usage.realized_nano_usd,
                    entry.intent.authorized_nano_usd.max(known_money),
                );
                add(&mut requests, u64::from(entry.dispatched));
                usage.estimated = true;
            }
        }
        usage.provider_requests = u32::try_from(requests).unwrap_or_else(|_| {
            overflow = true;
            u32::MAX
        });
        usage.estimated |= overflow || pricing_failed || state.error.is_some();
        usage.reserved_max_nano_usd = self
            .captured
            .snapshot
            .config
            .max_reserved_nano_usd
            .unwrap_or(0);
        drop(state);
        if overflow || pricing_failed {
            self.fail("run summary overflow");
        }
        confirmed.sort();
        confirmed.dedup();
        possible.sort();
        possible.dedup();
        possible.retain(|p| !confirmed.contains(p));
        fusion::FusionAttemptSummary {
            usage,
            confirmed_egress: confirmed,
            possible_egress: possible,
        }
    }
}
