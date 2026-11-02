#[cfg(windows)]
use platform_windows::process::supervisor as shell_supervisor;
#[cfg(windows)]
use platform_windows::WindowsMcpTransport;
use secret::CredentialManager;
use std::sync::Arc;
use std::sync::Mutex;

use super::{
    fusion_attempts, load_effective_settings_for_config, managed_model_policy_source,
    settings_watch, DesktopConfig,
};

/// `billing_mode` is the OWNING profile's source-resolved
/// `PricingConfig.billing_mode` (the same bit `provider-config::cost_translate`
/// keys `mark_unpriced` on). `None` remains distinct from explicit `Unknown`;
/// only a source-proven subscription mode changes Fusion's cost class.
///
/// Finding [2]: the checked-in `llm_runtime::fusion_hints` table only carries
/// a handful of rows per subscription-billed profile (e.g. 8 of
/// `github-copilot`'s 36 slice models). Every row that table has no entry
/// for defaults to `FusionModelHints::default()` (`cost_class: Medium`), so
/// without this override such a model is priced through
/// `DesktopFusionPriceBook::rates_for`, which the composition root has
/// deliberately marked `explicitly_unpriced` for every Subscription-mode
/// profile — `budget::model_peak` then hard-rejects any capped session that
/// selects it (`"token-billed model \`.../...\` has no price"`), even though
/// the model is subscription-billed and would reserve/settle at $0. Forcing
/// `cost_class: Subscription` here whenever the owning profile is
/// subscription-billed is inert for the rows the hint table already lists
/// for such a profile — every one of them already hand-codes
/// `FusionCostClass::Subscription` — so this only changes the
/// previously-unhinted rows that were wrongly defaulting to `Medium`.
pub(super) fn desktop_fusion_catalog_row(
    profile: &str,
    model: &llm_runtime::ModelProfile,
    billing_mode: Option<lingxi_core::host::ModelBillingMode>,
    protocol: &llm_runtime::ProtocolFamily,
) -> fusion::CatalogModel {
    let mut hints = llm_runtime::hints_for(profile, &model.request_model).unwrap_or_default();
    if billing_mode == Some(lingxi_core::host::ModelBillingMode::Subscription) {
        hints.cost_class = lingxi_core::host::FusionCostClass::Subscription;
    }
    fusion::CatalogModel {
        profile: profile.to_string(),
        model: model.request_model.clone(),
        hints,
        // Round-5 review finding [3]: `capabilities.structured_output` is a
        // property of the MODEL (copied verbatim from the vendored
        // models.dev slice); `CatalogModel::structured_output` is the
        // stronger claim `resolve_analyst`'s `with_schema` gate needs — that
        // THIS profile can actually put a `response_format` on the wire.
        // AND-ing the owning profile's codec in is what makes the two agree.
        structured_output: model.capabilities.structured_output
            && protocol.encodes_response_format(),
        limits: fusion::ModelLimits::from_metadata(&model.metadata),
    }
}

/// Filter the configured catalog by exact-account presence and managed policy.
pub(super) fn filter_fusion_catalog(
    catalog: Vec<fusion::CatalogModel>,
    provider_availability: &std::collections::BTreeMap<String, bool>,
    session_model_restriction: Option<&(
        llm_runtime::model::allowlist::ModelEnforcement,
        Vec<String>,
    )>,
) -> Vec<fusion::CatalogModel> {
    catalog
        .into_iter()
        .filter(|row| {
            provider_availability
                .get(&row.profile)
                .copied()
                .unwrap_or(false)
        })
        .filter(|row| match session_model_restriction {
            None => true,
            Some((enforcement, _)) => {
                llm_runtime::model::allowlist::model_allowed_under(enforcement, &row.model)
                    != Some(false)
            }
        })
        .collect()
}

/// A live account-aware model view over the configured Fusion catalog.
pub(super) struct FusionCatalogModelSource {
    pub(super) unfiltered: Vec<fusion::CatalogModel>,
    pub(super) availability: Arc<std::sync::RwLock<std::collections::BTreeMap<String, bool>>>,
    pub(super) account_directory: llm_runtime::accounts::SharedAccountDirectory,
    pub(super) mutation_epoch: Arc<std::sync::atomic::AtomicU64>,
    pub(super) session_model_restriction:
        Option<(llm_runtime::model::allowlist::ModelEnforcement, Vec<String>)>,
    /// Production reloads managed model policy on every catalog view. Tests
    /// that inject a fixed restriction keep this false for deterministic,
    /// filesystem-independent fixtures.
    pub(super) reload_managed_model_restriction: bool,
    pub(super) managed_policy_snapshot_cache: settings_watch::ManagedPolicySnapshotCache,
}

impl fusion::ModelSource for FusionCatalogModelSource {
    fn list(&self) -> Vec<fusion::CatalogModel> {
        let availability = self
            .availability
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        let live_restriction = self
            .reload_managed_model_restriction
            .then(|| live_managed_model_restriction_sync(&self.managed_policy_snapshot_cache))
            .flatten();
        let restriction = if self.reload_managed_model_restriction {
            live_restriction.as_ref()
        } else {
            self.session_model_restriction.as_ref()
        };
        filter_fusion_catalog(
            self.unfiltered
                .iter()
                .filter(|row| {
                    let snapshot = self
                        .account_directory
                        .read()
                        .unwrap_or_else(|e| e.into_inner());
                    snapshot
                        .model_availability
                        .iter()
                        .find(|model| {
                            model.execution_binding_id == row.profile && model.model_id == row.model
                        })
                        .is_none_or(|model| !model.eligible_account_ids.is_empty())
                })
                .cloned()
                .collect(),
            &availability,
            restriction,
        )
    }

    fn revision(&self) -> u64 {
        self.mutation_epoch
            .load(std::sync::atomic::Ordering::Acquire)
    }
}

pub(super) fn live_managed_model_restriction_sync(
    cache: &settings_watch::ManagedPolicySnapshotCache,
) -> Option<(llm_runtime::model::allowlist::ModelEnforcement, Vec<String>)> {
    use llm_runtime::model::allowlist::{self, ModelEnforcement};

    let source = cache
        .snapshot()
        .as_ref()
        .map(managed_model_policy_source)
        .unwrap_or(llm_runtime::model::allowlist::PolicySource::Failed);
    let enforcement = allowlist::resolve_enforcement(&source, &mut |_| {});
    match enforcement {
        ModelEnforcement::Inactive => None,
        // `Refused` is intentionally retained: a malformed managed policy
        // must remove every Fusion route rather than fail open.
        active_or_refused => Some((active_or_refused, Vec::new())),
    }
}

/// Refreshes exact account presence for one composed Host catalog.
#[derive(Clone)]
pub struct FusionCatalogRefresher {
    pub(super) availability: Arc<std::sync::RwLock<std::collections::BTreeMap<String, bool>>>,
    pub(super) mutation_epoch: Arc<std::sync::atomic::AtomicU64>,
    pub(super) credentials: std::sync::Weak<CredentialManager>,
    pub(super) credential_sources: Vec<provider_config::CredentialSource>,
    pub(super) account_directory: llm_runtime::accounts::SharedAccountDirectory,
}
impl FusionCatalogRefresher {
    pub async fn refresh(&self) {
        let Some(credentials) = self.credentials.upgrade() else {
            self.availability
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            return;
        };
        let epoch = self
            .mutation_epoch
            .load(std::sync::atomic::Ordering::Acquire);
        let snapshot = self
            .account_directory
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let rows = provider_config::compute_availability_with_accounts(
            &credentials,
            &self.credential_sources,
            &snapshot,
        )
        .await;
        let mut availability = self.availability.write().unwrap_or_else(|e| e.into_inner());
        if self
            .mutation_epoch
            .load(std::sync::atomic::Ordering::Acquire)
            != epoch
        {
            return;
        }
        *availability = rows
            .into_iter()
            .map(|row| (row.profile_name, row.available))
            .collect();
    }
    fn publish_snapshot(&self, snapshot: &lingxi_core::host::provider_accounts::AccountSnapshot) {
        use lingxi_core::host::provider_accounts::AccountHealthStatus;
        let mut availability = self.availability.write().unwrap_or_else(|e| e.into_inner());
        *availability = self
            .credential_sources
            .iter()
            .map(|source| {
                let available = snapshot.accounts.iter().any(|account| {
                    account.enabled
                        && (source.profile_name == account.execution_binding_id
                            || source.profile_name == format!("account.{}", account.account_id)
                            || source.credential_id == account.credential_ref.secret_ref)
                        && snapshot.health.iter().any(|health| {
                            health.account_id == account.account_id
                                && health.identity_generation == account.identity_generation
                                && matches!(
                                    health.status,
                                    AccountHealthStatus::Ready
                                        | AccountHealthStatus::CoolingDown
                                        | AccountHealthStatus::QuotaExhausted
                                )
                        })
                });
                (source.profile_name.clone(), available)
            })
            .collect();
        self.mutation_epoch
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }
}

/// Credential catalog notifications shared only by explicitly composed runtimes.
/// Clones share a scope; constructing a new registry isolates its catalogs.
#[derive(Clone, Default)]
pub struct FusionCatalogRegistry {
    pub(super) refreshers: Arc<Mutex<Vec<FusionCatalogRefresher>>>,
    pub(crate) account_sources:
        Arc<Mutex<Vec<std::sync::Weak<crate::account_pool_store::AccountRuntimeSource>>>>,
}

impl FusionCatalogRegistry {
    /// Publish a complete revision-fenced account directory for future calls.
    /// Prepared requests retain their captured route and credential scope.
    pub fn refresh_provider_accounts(
        &self,
        snapshot: lingxi_core::host::provider_accounts::AccountSnapshot,
    ) -> Result<(), String> {
        let mut sources = self
            .account_sources
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        sources.retain(|source| source.strong_count() > 0);
        for source in sources.iter().filter_map(std::sync::Weak::upgrade) {
            source.refresh(snapshot.clone())?;
        }
        for refresher in live_fusion_catalog_refreshers(self) {
            refresher.publish_snapshot(&snapshot);
        }
        Ok(())
    }
}

/// Register a catalog with its owning credential scope.
pub fn register_fusion_catalog_refresher(
    registry: &FusionCatalogRegistry,
    refresher: FusionCatalogRefresher,
) {
    let mut guard = registry
        .refreshers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.retain(|entry| Arc::strong_count(&entry.availability) > 1);
    guard.push(refresher);
}

pub(super) fn live_fusion_catalog_refreshers(
    registry: &FusionCatalogRegistry,
) -> Vec<FusionCatalogRefresher> {
    let mut guard = registry
        .refreshers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.retain(|entry| Arc::strong_count(&entry.availability) > 1);
    guard.clone()
}

/// `fusion::FusionPriceBook` over the session's `cost::PricingCatalog` — the
/// SAME catalog `CostTracker` bills from (see the WP1/F001/G003 comment at
/// its construction site). Before this adapter existed, `FusionOrchestrator`
/// was always built with the `()` price book (`rates_for` always `None`), so
/// `budget::quote`'s `model_peak` hard-rejected every token-billed model
/// under a session `--max-budget` (`InvalidConfiguration("... has no
/// price")`) and, without a cap, every reservation quoted $0 — Fusion's
/// hard-budget invariant (design §4) was wired to nothing.
///
/// `orchestrator::cost_wiring::model_ref_from_string` is the SAME
/// profile+bare-model → `ModelRef` resolution the main turn loop uses for its
/// own `record_api_response_v2` calls, so a Fusion panel/analyst/synth model
/// prices exactly like the corresponding main-loop call would.
pub(super) struct DesktopFusionPriceBook {
    pub(super) catalog: Arc<cost::PricingCatalog>,
    /// Round-7 finding [1]: whether this process assembles its Anthropic
    /// requests with 1-HOUR prompt-cache TTLs, so cache-CREATION tokens are
    /// billed at the catalog's `TokenClass::CacheWrite1h` rate instead of
    /// the 5-minute `TokenClass::CacheWrite` one. See
    /// [`prompt_cache_write_ttl_1h_enabled`].
    pub(super) cache_write_ttl_1h: bool,
}

/// The 1-hour prompt-cache TTL gate, read exactly where the money is priced.
///
/// `llm_runtime::service::ApiService::should_1h_cache_ttl`
/// (llm-runtime/src/service.rs:1090) reads this same variable through the same
/// truthy set (`1|true|yes|on`) and, when it is set, stamps `ttl_1h` on the
/// system cache blocks of EVERY request `build_request` assembles — which
/// includes Fusion's panel turns (`stream_forced_with_opts`) and its
/// analyst/synthesizer side queries. Anthropic bills those cache-creation
/// tokens at its 1-hour rate (~1.6x the 5-minute rate; `cost/src/pricing.rs`
/// derives the class for every Anthropic tier, e.g. sonnet 3_750 -> 6_000).
///
/// `orchestrator::conversation::hooks`'s model-switch cache-write estimator
/// already consults this same variable to pick between
/// `TokenClass::CacheWrite1h` and `TokenClass::CacheWrite` for an aggregate
/// cache-write token count it likewise has no 1h/5m split for — this mirrors
/// that precedent rather than inventing a second rule.
pub(super) fn prompt_cache_write_ttl_1h_enabled() -> bool {
    lingxi_core::host::env::is_env_truthy(std::env::var("ENABLE_PROMPT_CACHING_1H").ok().as_deref())
}

impl DesktopFusionPriceBook {
    /// Production constructor: resolve the session's prompt-cache TTL gate
    /// once, from the same env var `llm_runtime` reads when it builds the
    /// requests this book prices.
    pub(super) fn new(catalog: Arc<cost::PricingCatalog>) -> Self {
        Self {
            catalog,
            cache_write_ttl_1h: prompt_cache_write_ttl_1h_enabled(),
        }
    }

    /// Test constructor with the TTL gate stated explicitly, so a test that
    /// does not care about the gate never has to touch process env (and can
    /// run concurrently with the three tests that do).
    #[cfg(test)]
    pub(super) fn with_ttl_gate(
        catalog: Arc<cost::PricingCatalog>,
        cache_write_ttl_1h: bool,
    ) -> Self {
        Self {
            catalog,
            cache_write_ttl_1h,
        }
    }
}

impl fusion::FusionPriceBook for DesktopFusionPriceBook {
    fn rates_for(&self, profile: &str, model: &str) -> Option<fusion::ModelRates> {
        let model_ref = orchestrator::cost_wiring::model_ref_from_string(model, Some(profile));
        let (pricing, _resolution) = self.catalog.resolve(&model_ref).ok()?;
        let input = pricing
            .token_rates
            .get(&cost::pricing::TokenClass::Input)?
            .nano_usd_per_token;
        let output = pricing
            .token_rates
            .get(&cost::pricing::TokenClass::Output)?
            .nano_usd_per_token;
        // Cache-read/-write rates default to 0 rather than `?`-propagating a
        // `None`: a model with real Input/Output rates but no CacheRead /
        // CacheWrite entry must stay token-priced (only the cache premium is
        // unrecovered), never flip to fully unpriced and hard-reject under a
        // session `--max-budget` (`model_peak`'s `InvalidConfiguration`
        // branch). `cost::calculator::CostCalculator` — the SAME catalog the
        // main turn loop bills from — already treats a missing rate for a
        // class it iterates as "that class contributes 0", so this mirrors
        // it rather than diverging.
        let cache_read = pricing
            .token_rates
            .get(&cost::pricing::TokenClass::CacheRead)
            .map_or(0, |rate| rate.nano_usd_per_token);
        // Round-7 finding [1]: pick the cache-WRITE rate that matches the TTL
        // this process actually stamps on its cache blocks. With
        // `ENABLE_PROMPT_CACHING_1H` armed, `ApiService::build_request` marks
        // the system cache blocks of every Fusion request `ttl_1h`, and
        // Anthropic bills those creation tokens at ~1.6x the 5-minute rate —
        // a rate the SAME catalog already carries as
        // `TokenClass::CacheWrite1h` and that the main turn loop bills
        // through (`orchestrator::cost_wiring` splits
        // `provider_metadata./cache_creation/ephemeral_1h_input_tokens` into
        // `TokenUsage::cache_write_1h`, which `CostCalculator` prices through
        // that class). Fusion has no such split to price from: both seams
        // that feed it flatten the two buckets into one total
        // (`agent/src/handle.rs`'s `cache_creation_input_tokens: bt.cache_write`
        // and `sidequery`'s `cache_write_1h: 0`), so `FusionUsage` carries a
        // single `cache_write_tokens` figure. Given only that total, billing
        // it at the 5-minute rate under an armed 1h gate is a SILENT 37.5%
        // under-charge on the money path (design §4's hard-budget ceiling
        // then sits below what the provider actually charged); billing it at
        // the 1h rate is at worst high by the residual 5-minute
        // message-level breakpoint (service.rs's `CacheControl::Ephemeral`
        // on the last message block), which errs toward reserving/settling
        // MORE than was spent — the safe direction for a budget ceiling.
        // With the gate off (the shipped default — service.rs documents the
        // feature as deliberately dormant, no settings.json route) this is
        // byte-identical to the previous behaviour and exactly correct.
        //
        // The `.or_else` fallback matters: `provider-config::cost_translate`
        // never inserts a `CacheWrite1h` class, so every models.dev /
        // OpenRouter entry keeps its 5-minute rate here instead of dropping
        // to 0 the moment the gate is armed.
        let cache_write_class = if self.cache_write_ttl_1h {
            cost::pricing::TokenClass::CacheWrite1h
        } else {
            cost::pricing::TokenClass::CacheWrite
        };
        let cache_write = pricing
            .token_rates
            .get(&cache_write_class)
            .or_else(|| {
                pricing
                    .token_rates
                    .get(&cost::pricing::TokenClass::CacheWrite)
            })
            .map_or(0, |rate| rate.nano_usd_per_token);
        // Finding [1]: `provider-config::cost_translate::model_pricing_from_token_pricing`
        // now ALWAYS inserts a `ReasoningOutput` rate — a model that publishes
        // a real, separate reasoning price (DeepSeek / a handful of
        // OpenRouter rows) keeps it; everyone else (most OpenAI / Gemini /
        // Copilot / … rows, which bill reasoning tokens at the plain output
        // rate) gets the output rate as the class's value. That fix lives at
        // the shared catalog layer so both this Fusion price book and the
        // main turn loop's `cost::CostCalculator` (which iterates only the
        // classes present in `token_rates`) recover the bucket identically.
        // The `map_or(output, …)` here is a second line of defense for any
        // pricing entry that reaches this book without going through
        // `cost_translate` (e.g. a future direct catalog entry) — it must
        // still stay token-priced, never flip to fully unpriced.
        let reasoning = pricing
            .token_rates
            .get(&cost::pricing::TokenClass::ReasoningOutput)
            .map_or(output, |rate| rate.nano_usd_per_token);
        Some(fusion::ModelRates {
            input_nano_usd_per_token: input,
            output_nano_usd_per_token: output,
            // The pricing catalog has no flat per-request rate today (only
            // per-token + the separate web-search non-token unit); Fusion's
            // quote/settlement formulas treat a zero per-request rate as "no
            // flat fee", not "unpriced" — token rates alone still gate the
            // hard-budget preflight correctly.
            per_request_nano_usd: 0,
            cache_read_nano_usd_per_token: cache_read,
            cache_write_nano_usd_per_token: cache_write,
            reasoning_nano_usd_per_token: reasoning,
            // Round-7 finding [1] residual: with the gate armed, the single
            // rate above is right for the system cache blocks
            // (`service.rs` stamps `ttl_1h` on them) and wrong for the
            // residual last-message breakpoint, which stays 5-minute
            // (`CacheControl::Ephemeral`) — and Fusion carries no split to
            // tell the two apart. Report the rate as approximated so
            // `price_realized_usage` refuses to claim `estimated: false`
            // over it. With the gate off (the shipped default) every cache
            // block is 5-minute and the rate is exact.
            cache_write_rate_is_ttl_approximated: self.cache_write_ttl_1h,
        })
    }
}

/// Resolve the effective Fusion settings for one snapshot (F007).
///
/// Routes through [`load_effective_settings_for_config`] — the SAME
/// managed/CLI/scoped-source-aware loader `build()` uses for every other
/// setting — instead of a bare `Settings::load`, so a managed-policy
/// `fusion.enabled=false`/`allowedProfiles`/`allowCrossProviderForAgent=false`
/// and `--settings '{"fusion":…}'` are honored (previously ignored: see the
/// F007 finding).
pub(super) fn desktop_fusion_runtime_config(
    cfg: &DesktopConfig,
) -> Result<fusion::FusionRuntimeConfig, lingxi_core::host::FusionError> {
    let selection = desktop_fusion_strategy(cfg, None)?;
    fusion::FusionRuntimeConfig::from_settings(&selection.config)
}

fn desktop_fusion_strategy(
    cfg: &DesktopConfig,
    requested: Option<&lingxi_core::host::fusion_strategy::FusionStrategyRef>,
) -> Result<
    lingxi_core::host::fusion_strategy::FusionStrategySelection,
    lingxi_core::host::FusionError,
> {
    lingxi_core::host::fusion_strategy::resolve_fusion_strategy(
        &desktop_fusion_layers(cfg)?,
        requested,
    )
    .map_err(|error| lingxi_core::host::FusionError::InvalidConfiguration(error.to_string()))
}

pub(super) fn desktop_fusion_layers(
    cfg: &DesktopConfig,
) -> Result<
    Vec<lingxi_core::host::fusion_strategy::FusionSettingsLayer>,
    lingxi_core::host::FusionError,
> {
    let snapshot = settings_watch::managed_settings_snapshot_sync(cfg).ok_or_else(|| {
        lingxi_core::host::FusionError::InvalidConfiguration(
            "managed policy snapshot is not initialized".into(),
        )
    })?;
    let effective = load_effective_settings_for_config(cfg, Some(&snapshot)).map_err(|error| {
        let diagnostic = error
            .diagnostic_projection()
            .to_json_string()
            .expect("settings diagnostics are valid JSON");
        tracing::warn!(diagnostic = %diagnostic, "settings merge failed for Fusion settings");
        lingxi_core::host::FusionError::InvalidConfiguration(diagnostic)
    })?;
    Ok(effective.fusion_layers)
}

/// F007: reload point handed to `FusionOrchestrator` — `load()` re-resolves
/// the effective settings on every call instead of a config frozen at
/// construction, so a settings-file edit or the design's §11 kill switch
/// (`fusion.enabled=false`) takes effect on the NEXT run, not the next
/// process restart.
pub(super) struct DesktopFusionConfigSource {
    pub(super) cfg: DesktopConfig,
}

impl fusion::FusionConfigSource for DesktopFusionConfigSource {
    fn capture_strategy(
        &self,
        requested: Option<&lingxi_core::host::fusion_strategy::FusionStrategyRef>,
    ) -> Result<
        Option<lingxi_core::host::fusion_strategy::FusionStrategySelection>,
        lingxi_core::host::FusionError,
    > {
        desktop_fusion_strategy(&self.cfg, requested).map(Some)
    }
    fn load(&self) -> Result<fusion::FusionRuntimeConfig, lingxi_core::host::FusionError> {
        desktop_fusion_runtime_config(&self.cfg)
    }
}

/// Finding [14]: wraps the constructed live `FusionOrchestrator` so a
/// boot-time-invalid `fusion.*` value does NOT pin `RejectedFusionExecutor`
/// for the rest of the process. `preflight_error()` re-validates FRESH on
/// every call — mirroring how `FusionOrchestrator::run` /
/// `agent_surface` / `workflow_fusion_call_cap` already reload the config
/// per call (F007) — instead of freezing the first boot-time error, so a
/// user who fixes and saves the settings file recovers within the session
/// exactly as the F007 doc comment promises ("takes effect on the NEXT run,
/// not the next process restart"), rather than only after a restart.
pub(super) struct DesktopFusionExecutor {
    pub(super) inner: Arc<fusion::FusionOrchestrator>,
    pub(super) cfg: DesktopConfig,
}

#[async_trait::async_trait]
impl lingxi_core::host::FusionExecutor for DesktopFusionExecutor {
    fn capture_strategy(
        &self,
        requested: Option<&lingxi_core::host::fusion_strategy::FusionStrategyRef>,
    ) -> Result<
        Option<lingxi_core::host::fusion_strategy::FusionStrategySelection>,
        lingxi_core::host::FusionError,
    > {
        self.inner.capture_strategy(requested)
    }

    fn agent_surface_for_strategy(
        &self,
        selection: Option<&lingxi_core::host::fusion_strategy::FusionStrategySelection>,
    ) -> Result<lingxi_core::host::FusionAgentSurface, lingxi_core::host::FusionError> {
        self.inner.agent_surface_for_strategy(selection)
    }

    fn implement_confirmation(
        &self,
        request: &lingxi_core::host::FusionRequest,
    ) -> lingxi_core::host::ImplementConfirmation {
        self.inner.implement_confirmation(request)
    }
    fn prepare(
        self: Arc<Self>,
        submission: lingxi_core::host::FusionSubmission,
    ) -> Result<lingxi_core::host::PreparedFusionRun, lingxi_core::host::FusionError> {
        Arc::clone(&self.inner).prepare(submission)
    }

    fn effective_timeout_ms(&self) -> Option<u64> {
        self.inner.effective_timeout_ms()
    }

    fn agent_surface(&self) -> lingxi_core::host::FusionAgentSurface {
        self.inner.agent_surface()
    }

    fn preflight_error(&self) -> Option<lingxi_core::host::FusionError> {
        // Selection-specific validation belongs to capture_strategy. In
        // particular, a missing default must not block an explicit strategy.
        desktop_fusion_layers(&self.cfg)
            .and_then(|layers| {
                lingxi_core::host::fusion_strategy::fusion_global_settings(&layers)
                    .map(|_| ())
                    .map_err(|error| {
                        lingxi_core::host::FusionError::InvalidConfiguration(error.to_string())
                    })
            })
            .err()
    }

    fn resolve_parent_profile(
        &self,
        parent_model: &str,
        explicit_profile: Option<&str>,
    ) -> Option<String> {
        self.inner
            .resolve_parent_profile(parent_model, explicit_profile)
    }
}

/// Install one shared physical-attempt host only after durable boot has
/// published its output scope. The registry validates captured session
/// hydration again during prepare; this factory grants no dispatch authority.
pub(super) fn desktop_fusion_attempts(
    service: Arc<llm_runtime::ApiService>,
    budget: Arc<cost::BudgetEnforcer>,
    tracker: Arc<cost::CostTracker>,
    pricing: Arc<cost::PricingCatalog>,
    outputs: Arc<dyn lingxi_core::host::WorkflowOutputScopes>,
) -> Arc<fusion_attempts::DesktopFusionAttempts> {
    let attempts = fusion_attempts::DesktopFusionAttempts::new(
        service.clone(),
        budget,
        tracker,
        pricing,
        outputs,
    );
    service.set_model_attempt_hooks(attempts.clone());
    attempts
}
