//! Automatic and explicit Fusion panel selection.

use crate::config::FusionRuntimeConfig;
use lingxi_core::host::{
    FusionCostClass, FusionError, FusionLatencyClass, FusionModelChoice, FusionModelHints,
    FusionModelRef, FusionModelRole, FusionOrigin, FusionPreset, FusionRequest, FUSION_MAX_PANEL,
    FUSION_MIN_PANEL,
};

/// Provider/profile-specific capacity facts captured with a Fusion route.
///
/// Missing provider facts remain unknown. Callers and fixtures that know a
/// route's limits must state them explicitly; default construction must never
/// invent capacity that could authorize an oversized provider request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModelLimits {
    /// Total context window, when the provider publishes one.
    pub context_window_tokens: Option<u64>,
    /// Input-only ceiling, when the route publishes one.
    pub max_input_tokens: Option<u64>,
    /// Output ceiling, when the route publishes one.
    pub max_output_tokens: Option<u64>,
}

impl ModelLimits {
    /// Explicit unknown-capacity value used by production metadata adapters and
    /// tests that exercise fail-closed analyst selection.
    #[must_use]
    pub const fn unknown() -> Self {
        Self {
            context_window_tokens: None,
            max_input_tokens: None,
            max_output_tokens: None,
        }
    }

    /// Build route limits from provider-neutral model metadata.
    #[must_use]
    pub const fn from_metadata(metadata: &lingxi_core::host::ModelMetadata) -> Self {
        Self {
            context_window_tokens: metadata.context_window_tokens,
            max_input_tokens: metadata.max_input_tokens,
            max_output_tokens: metadata.max_output_tokens,
        }
    }

    /// Whether the route supplies enough metadata for a bounded judge request.
    #[must_use]
    pub const fn has_input_capacity(self) -> bool {
        matches!(self.context_window_tokens, Some(value) if value > 0)
            || matches!(self.max_input_tokens, Some(value) if value > 0)
    }

    /// Whether the published limits leave positive input and output capacity
    /// for this stage's configured output cap.
    #[must_use]
    pub fn has_usable_capacity(self, configured_output: u32) -> bool {
        let output = self.output_cap(configured_output);
        output > 0 && self.input_cap(output).is_some_and(|input| input > 0)
    }

    /// Effective maximum output for a configured cap.
    #[must_use]
    pub fn output_cap(self, configured: u32) -> u32 {
        match self.max_output_tokens {
            Some(limit) => configured.min(u32::try_from(limit).unwrap_or(u32::MAX)),
            None => configured,
        }
    }

    /// Effective input-token budget after reserving configured output and the
    /// approved context margin. None means no route capacity is known.
    #[must_use]
    pub fn input_cap(self, configured_output: u32) -> Option<u64> {
        let direct = self.max_input_tokens;
        let contextual = self.context_window_tokens.map(|context| {
            let margin = (context / 20).clamp(1_024, 20_000);
            context
                .saturating_sub(u64::from(configured_output))
                .saturating_sub(margin)
        });
        match (direct, contextual) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        }
    }
}

/// Explicit, finite capacity for unit fixtures that are not backed by provider
/// metadata. Keeping it test-only prevents `Default` from becoming a hidden
/// production authorization policy.
#[cfg(test)]
pub(crate) const fn known_test_limits() -> ModelLimits {
    ModelLimits {
        context_window_tokens: Some(200_000),
        max_input_tokens: Some(180_000),
        max_output_tokens: Some(32_000),
    }
}

/// One catalog row the orchestrator may select.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogModel {
    /// Provider profile name.
    pub profile: String,
    /// Wire model id.
    pub model: String,
    /// Checked-in hints. Unhinted models stay ineligible for auto presets.
    pub hints: FusionModelHints,
    /// Whether THIS PROFILE can actually request constrained JSON for this
    /// model — i.e. the model's `capabilities.structured_output` bit AND the
    /// owning profile's wire codec being able to encode an
    /// `LlmRequest.response_format`.
    ///
    /// Producers combine the model bit with the SDK codec's wire support via
    /// `llm_runtime::ProtocolFamily::encodes_response_format`. All current
    /// SDK codecs, including Gemini, support structured response formats;
    /// the model capability bit remains independently required.
    pub structured_output: bool,
    /// Provider/profile-specific context and input/output limits.
    pub limits: ModelLimits,
}

/// Injected model listing. Production later wraps the live catalog.
pub trait ModelSource: Send + Sync {
    /// Currently available models.
    fn list(&self) -> Vec<CatalogModel>;

    /// Monotonic source revision when the host can provide one. The content
    /// digest captured by CatalogSnapshot remains authoritative when this
    /// default is used.
    fn revision(&self) -> u64 {
        0
    }
}

impl ModelSource for Vec<CatalogModel> {
    fn list(&self) -> Vec<CatalogModel> {
        self.clone()
    }
}

/// One resolved panel or analyst target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPanel {
    /// Provider profile.
    pub profile: String,
    /// Wire model id.
    pub model: String,
}

/// Every route a run will call, resolved before any provider call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSet {
    /// Panel targets in spawn order.
    pub panels: Vec<ResolvedPanel>,
    /// Analyst / judge target.
    pub analyst: ResolvedPanel,
}

/// Resolve the panel set and analyst. Performs no provider calls.
///
/// # Errors
///
/// Preflight [`FusionError`] variants (too few models, cross-provider, …).
pub fn resolve(
    request: &FusionRequest,
    config: &FusionRuntimeConfig,
    catalog: &dyn ModelSource,
) -> Result<ResolvedSet, FusionError> {
    deny_cross_provider_if_needed(request, config)?;
    let available = catalog.list();
    let required = config.min_successful_panels.max(FUSION_MIN_PANEL);
    let requested_max = request.max_panel.unwrap_or(config.max_panel);
    if requested_max < required {
        return Err(FusionError::InvalidRequest(format!(
            "max_panel ({requested_max}) must be at least fusion.minSuccessfulPanels ({required})"
        )));
    }
    let max_panel = requested_max.clamp(FUSION_MIN_PANEL, config.max_panel.min(FUSION_MAX_PANEL));

    // Every role must be configured before ANY of them is resolved, so a
    // half-configured file reports all of its gaps at once instead of making
    // the operator re-run and discover the next missing role. A per-run
    // `--models` list supplies the panel role itself, so it clears that gap.
    let mut missing = config.missing_model_roles();
    if request.models.is_some() {
        missing.retain(|role| *role != FusionModelRole::Panels);
    }
    if !missing.is_empty() {
        return Err(FusionError::NotConfigured { missing });
    }

    let panels = if let Some(refs) = request.models.as_ref() {
        resolve_custom(refs, request, config, &available, max_panel, required)?
    } else {
        resolve_configured_panels(request, config, &available, max_panel, required)?
    };
    if panels.len() < usize::from(required) {
        return Err(too_few_models(request, panels.len(), required));
    }
    reject_duplicate_underlying_models(&panels)?;
    let analyst = resolve_analyst(request, config, &available)?;
    Ok(ResolvedSet { panels, analyst })
}

/// The configured roster, trimmed to the preset's panel count.
///
/// Roster ORDER is the operator's priority order, so a preset takes a prefix
/// rather than re-ranking: `fusion.panelModels` is what the settings file says
/// will run, and a run that quietly substituted a different subset would make
/// that statement false.
fn resolve_configured_panels(
    request: &FusionRequest,
    config: &FusionRuntimeConfig,
    available: &[CatalogModel],
    max_panel: u8,
    required: u8,
) -> Result<Vec<ResolvedPanel>, FusionError> {
    let wanted = usize::from(
        match request.preset {
            FusionPreset::Quality => config.quality_panel_count,
            FusionPreset::Fast => config.fast_panel_count,
        }
        .min(max_panel)
        .max(required),
    );
    let mut out = Vec::new();
    for choice in config.panel_models.iter().take(wanted) {
        out.push(resolve_configured_route(
            choice,
            request,
            config,
            available,
            FusionModelRole::Panels,
            config.panel_max_output_tokens_per_turn,
        )?);
    }
    Ok(out)
}

/// Validate one configured `(profile, model)` against the live catalog.
///
/// Every failure here is a HARD error naming the exact route, never a silent
/// drop. A roster of three that quietly ran as two would bill for an ensemble
/// the operator never approved and would hide a typo or a disconnected provider
/// for as long as the remaining models still met the bar.
fn resolve_configured_route(
    choice: &FusionModelChoice,
    request: &FusionRequest,
    config: &FusionRuntimeConfig,
    available: &[CatalogModel],
    role: FusionModelRole,
    configured_output_tokens: u32,
) -> Result<ResolvedPanel, FusionError> {
    let key = role.setting_key();
    if !profile_allowed(&choice.profile, config) {
        return Err(FusionError::InvalidConfiguration(format!(
            "{key} names `{choice}`, whose profile is not in fusion.allowedProfiles"
        )));
    }
    if !request.cross_provider && choice.profile != request.parent_profile {
        return Err(FusionError::CrossProviderDenied);
    }
    let Some(row) = available
        .iter()
        .find(|row| row.profile == choice.profile && row.model == choice.model)
    else {
        return Err(FusionError::InvalidConfiguration(format!(
            "{key} names `{choice}`, which this session's model catalog does not \
             contain; connect that provider or run `/fusion setup` to pick another model"
        )));
    };
    if !row.limits.has_usable_capacity(configured_output_tokens) {
        return Err(FusionError::InvalidConfiguration(format!(
            "{key} names `{choice}`, whose published context/output limits leave no \
             room for the configured {configured_output_tokens}-token output cap"
        )));
    }
    Ok(ResolvedPanel {
        profile: choice.profile.clone(),
        model: choice.model.clone(),
    })
}

/// Reject a roster that seats one underlying model twice.
///
/// Gateways republish the same model under their own id (`openrouter`'s
/// `openai/gpt-5.6-sol` is `openai`'s `gpt-5.6-sol`), so an exact-pair
/// uniqueness check — all `FusionSettingsJson::validate` can do without the
/// canonical-key normaliser — passes a two-entry "ensemble" that is one model
/// asked twice. Fusion's whole premise is independent answers that can
/// disagree, so this is a configuration error, not a preference.
fn reject_duplicate_underlying_models(panels: &[ResolvedPanel]) -> Result<(), FusionError> {
    for (index, panel) in panels.iter().enumerate() {
        if let Some(earlier) = panels[..index]
            .iter()
            .find(|earlier| canonical_key(&earlier.model) == canonical_key(&panel.model))
        {
            return Err(FusionError::InvalidConfiguration(format!(
                "fusion.panelModels seats the same underlying model twice: `{}/{}` and \
                 `{}/{}` are the same model behind different profiles",
                earlier.profile, earlier.model, panel.profile, panel.model
            )));
        }
    }
    Ok(())
}

/// Build a data-carrying [`FusionError::TooFewModels`] (F011) so the caller
/// can point at the setting that would fix it instead of a bare "too few".
fn too_few_models(request: &FusionRequest, eligible: usize, required: u8) -> FusionError {
    FusionError::TooFewModels {
        eligible,
        required,
        same_provider_only: !request.cross_provider,
        parent_profile: request.parent_profile.clone(),
    }
}

fn deny_cross_provider_if_needed(
    request: &FusionRequest,
    config: &FusionRuntimeConfig,
) -> Result<(), FusionError> {
    if !request.cross_provider {
        return Ok(());
    }
    let allowed = match request.origin {
        FusionOrigin::Slash => true,
        FusionOrigin::Agent => config.allow_cross_provider_for_agent,
    };
    if allowed {
        Ok(())
    } else {
        Err(FusionError::CrossProviderDenied)
    }
}

fn resolve_custom(
    refs: &[FusionModelRef],
    request: &FusionRequest,
    config: &FusionRuntimeConfig,
    available: &[CatalogModel],
    max_panel: u8,
    required: u8,
) -> Result<Vec<ResolvedPanel>, FusionError> {
    if refs.len() < usize::from(required) {
        return Err(FusionError::InvalidCustomModels(format!(
            "explicit models must contain at least {required} entries to satisfy fusion.minSuccessfulPanels"
        )));
    }
    // F011 item 5: an explicit list longer than max_panel is a caller error,
    // not a silent truncation — a caller who asked for 5 named models has no
    // way to learn only 3 actually ran.
    if refs.len() > usize::from(max_panel) {
        return Err(FusionError::InvalidCustomModels(format!(
            "explicit models has {} entries, exceeding the {max_panel} panel cap",
            refs.len()
        )));
    }
    let mut out = Vec::new();
    for model_ref in refs {
        let profile = model_ref
            .profile
            .clone()
            .unwrap_or_else(|| request.parent_profile.clone());
        if !profile_allowed(&profile, config) {
            return Err(FusionError::InvalidCustomModels(format!(
                "profile `{profile}` is not in fusion.allowedProfiles"
            )));
        }
        if !request.cross_provider && profile != request.parent_profile {
            return Err(FusionError::CrossProviderDenied);
        }
        let found = available
            .iter()
            .find(|row| row.profile == profile && row.model == model_ref.model);
        let Some(found) = found else {
            return Err(FusionError::InvalidCustomModels(format!(
                "unknown model `{profile}/{}`",
                model_ref.model
            )));
        };
        if !found
            .limits
            .has_usable_capacity(config.panel_max_output_tokens_per_turn)
        {
            return Err(FusionError::InvalidCustomModels(format!(
                "model `{profile}/{}` has unknown or unusable capacity metadata",
                model_ref.model
            )));
        }
        let resolved = ResolvedPanel {
            profile,
            model: model_ref.model.clone(),
        };
        if out.iter().any(|existing: &ResolvedPanel| {
            existing.profile == resolved.profile && existing.model == resolved.model
        }) {
            return Err(FusionError::InvalidCustomModels(
                "explicit models must be distinct".into(),
            ));
        }
        out.push(resolved);
    }
    Ok(out)
}

/// Canonical underlying-model key for cross-gateway identity (F011): an
/// `OpenRouter` row's `request_model` carries a `vendor/` prefix
/// (`"openai/gpt-5.6-sol"`) that the SAME model's direct-profile row
/// (`"openai" -> "gpt-5.6-sol"`) does not, so a bare string compare misses the
/// duplicate. Stripping to the last `/`-segment aligns both spellings.
///
/// (Round-3 review finding 5): the `/`-strip alone is not enough — the same
/// underlying model's anthropic/openrouter spellings diverge in punctuation
/// (`"claude-fable-5-1"` vs `"anthropic/claude-fable-5.1"`), so an exact byte
/// compare after the strip still reads them as two distinct models. Case-fold
/// and normalise `.` to `-` as well, so a gateway's dotted-version spelling of
/// the same id collapses onto its dashed sibling.
///
/// `pub(crate)` so `orchestrator.rs`'s `analyst_overlaps_panel` telemetry and
/// [`reject_duplicate_underlying_models`] key off ONE identity.
pub(crate) fn canonical_key(model: &str) -> String {
    model
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .to_ascii_lowercase()
        .replace('.', "-")
}

pub(crate) fn route_key(profile: &str, model: &str) -> String {
    format!("{profile}\0{model}")
}

/// The configured analyst, validated against the live catalog.
///
/// Beyond the shared route checks this adds the one requirement that is a
/// property of the STAGE rather than of the operator's taste: the analyst is
/// asked for constrained JSON, and a route whose codec cannot put a
/// `response_format` on the wire fails inside `analyst.rs` AFTER every panel
/// has already spent real money. `CatalogModel::structured_output` is the
/// profile-level claim (model capability AND the owning codec), which is why
/// the weaker model-only capability bit is not what is checked here.
///
/// Note what is deliberately NOT checked: `FusionModelHints::judge_eligible`.
/// That flag exists to RANK suggestions in the setup wizard; an operator who
/// names a judge has made the call themselves.
fn resolve_analyst(
    request: &FusionRequest,
    config: &FusionRuntimeConfig,
    available: &[CatalogModel],
) -> Result<ResolvedPanel, FusionError> {
    let choice = config
        .analyst_model
        .as_ref()
        .ok_or_else(|| FusionError::NotConfigured {
            missing: vec![FusionModelRole::Analyst],
        })?;
    let resolved = resolve_configured_route(
        choice,
        request,
        config,
        available,
        FusionModelRole::Analyst,
        config.analyst_max_output_tokens,
    )?;
    let row = available
        .iter()
        .find(|row| row.profile == resolved.profile && row.model == resolved.model)
        .expect("resolve_configured_route only returns routes it found in the catalog");
    if !row.structured_output {
        return Err(FusionError::StructuredOutputUnsupported);
    }
    Ok(resolved)
}

fn profile_allowed(profile: &str, config: &FusionRuntimeConfig) -> bool {
    config.allowed_profiles.is_empty() || config.allowed_profiles.iter().any(|name| name == profile)
}

/// Latency class ordering used by tests (Instant < Fast < Standard < Slow).
#[allow(dead_code)]
fn _latency_ord() -> FusionLatencyClass {
    FusionLatencyClass::Fast
}

/// Cost class ordering used by tests (Low < Medium < High).
#[allow(dead_code)]
fn _cost_ord() -> FusionCostClass {
    FusionCostClass::Low
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_limits_are_unknown_and_never_authorize_capacity() {
        assert_eq!(ModelLimits::default(), ModelLimits::unknown());
        assert!(!ModelLimits::default().has_input_capacity());
    }

    /// A catalog row. `hints` are kept on the fixtures because production still
    /// carries them (the `/fusion setup` wizard sorts its suggestions by them),
    /// but NOTHING in this module reads them any more — that is the point of
    /// several tests below.
    fn hinted(
        profile: &str,
        model: &str,
        rank: u16,
        latency: FusionLatencyClass,
        cost: FusionCostClass,
        judge: bool,
    ) -> CatalogModel {
        CatalogModel {
            profile: profile.into(),
            model: model.into(),
            hints: FusionModelHints {
                eligible: true,
                quality_rank: rank,
                latency_class: latency,
                cost_class: cost,
                judge_eligible: judge,
            },
            structured_output: judge,
            limits: known_test_limits(),
        }
    }

    fn req() -> FusionRequest {
        FusionRequest {
            schema_version: 1,
            origin: FusionOrigin::Slash,
            prompt: "task".into(),
            preset: FusionPreset::Quality,
            models: None,
            dimensions: vec!["coverage".into()],
            partial_ok: true,
            max_panel: None,
            cross_provider: true,
            parent_profile: "anthropic".into(),
            parent_model: "claude-sonnet-5".into(),
        }
    }

    fn three_provider_catalog() -> Vec<CatalogModel> {
        vec![
            hinted(
                "anthropic",
                "claude-opus-5",
                100,
                FusionLatencyClass::Standard,
                FusionCostClass::High,
                true,
            ),
            hinted(
                "anthropic",
                "claude-haiku-4-5",
                60,
                FusionLatencyClass::Fast,
                FusionCostClass::Low,
                false,
            ),
            hinted(
                "openai",
                "gpt-5.6-sol",
                100,
                FusionLatencyClass::Slow,
                FusionCostClass::High,
                true,
            ),
            hinted(
                "openai",
                "gpt-5.6-terra",
                85,
                FusionLatencyClass::Fast,
                FusionCostClass::Medium,
                true,
            ),
            hinted(
                "google",
                "gemini-3-pro",
                95,
                FusionLatencyClass::Standard,
                FusionCostClass::Medium,
                true,
            ),
        ]
    }

    /// A fully configured three-panel setup over [`three_provider_catalog`].
    fn configured() -> FusionRuntimeConfig {
        FusionRuntimeConfig {
            panel_models: vec![
                FusionModelChoice::new("anthropic", "claude-opus-5"),
                FusionModelChoice::new("openai", "gpt-5.6-sol"),
                FusionModelChoice::new("google", "gemini-3-pro"),
            ],
            analyst_model: Some(FusionModelChoice::new("openai", "gpt-5.6-terra")),
            ..FusionRuntimeConfig::defaults()
        }
    }

    fn routes(panels: &[ResolvedPanel]) -> Vec<String> {
        panels
            .iter()
            .map(|panel| format!("{}/{}", panel.profile, panel.model))
            .collect()
    }

    // ---- the configured path -------------------------------------------

    #[test]
    fn the_configured_roster_runs_in_the_order_it_was_written() {
        let resolved = resolve(&req(), &configured(), &three_provider_catalog())
            .expect("a complete configuration resolves");
        assert_eq!(
            routes(&resolved.panels),
            vec![
                "anthropic/claude-opus-5".to_string(),
                "openai/gpt-5.6-sol".to_string(),
                "google/gemini-3-pro".to_string(),
            ],
            "roster order is the operator's priority order, not a re-ranking"
        );
        assert_eq!(resolved.analyst.model, "gpt-5.6-terra");
    }

    /// The hint table used to decide which models ran. It must not any more:
    /// this catalog's hints rank the roster's models LAST and mark them
    /// ineligible, and the roster must still run exactly as written.
    #[test]
    fn selection_no_longer_consults_the_hint_table_at_all() {
        let mut catalog = three_provider_catalog();
        for row in &mut catalog {
            row.hints.eligible = false;
            row.hints.quality_rank = 0;
            row.hints.judge_eligible = false;
        }
        let resolved = resolve(&req(), &configured(), &catalog)
            .expect("an unhinted catalog must still serve an explicit configuration");
        assert_eq!(
            routes(&resolved.panels),
            vec![
                "anthropic/claude-opus-5".to_string(),
                "openai/gpt-5.6-sol".to_string(),
                "google/gemini-3-pro".to_string(),
            ]
        );
        assert_eq!(
            resolved.analyst.model, "gpt-5.6-terra",
            "judge_eligible ranks wizard suggestions; it must not veto an \
             operator's explicit analyst"
        );
    }

    #[test]
    fn a_preset_takes_a_prefix_of_the_roster_rather_than_re_ranking_it() {
        let mut config = configured();
        config.fast_panel_count = 2;
        let mut request = req();
        request.preset = FusionPreset::Fast;
        let resolved = resolve(&request, &config, &three_provider_catalog()).unwrap();
        assert_eq!(
            routes(&resolved.panels),
            vec![
                "anthropic/claude-opus-5".to_string(),
                "openai/gpt-5.6-sol".to_string(),
            ]
        );
    }

    #[test]
    fn a_request_panel_cap_trims_the_roster_from_the_end() {
        let mut request = req();
        request.max_panel = Some(2);
        let resolved = resolve(&request, &configured(), &three_provider_catalog()).unwrap();
        assert_eq!(resolved.panels.len(), 2);
        assert_eq!(resolved.panels[0].model, "claude-opus-5");
    }

    // ---- the unconfigured path -----------------------------------------

    #[test]
    fn an_empty_configuration_names_every_missing_role_at_once() {
        let error = resolve(
            &req(),
            &FusionRuntimeConfig::defaults(),
            &three_provider_catalog(),
        )
        .expect_err("nothing configured must not silently pick models");
        let FusionError::NotConfigured { missing } = &error else {
            panic!("expected NotConfigured, got {error:?}");
        };
        assert_eq!(missing, &FusionModelRole::ALL.to_vec());
        // An operator who has to re-run to discover the next gap will conclude
        // the wizard is broken, so the message must list them together.
        let rendered = error.to_string();
        for key in ["fusion.panelModels", "fusion.analystModel"] {
            assert!(rendered.contains(key), "{rendered}");
        }
        assert!(rendered.contains("/fusion setup"), "{rendered}");
    }

    #[test]
    fn a_one_model_roster_is_reported_as_a_missing_roster() {
        let mut config = configured();
        config.panel_models.truncate(1);
        let error = resolve(&req(), &config, &three_provider_catalog()).unwrap_err();
        assert!(
            matches!(&error, FusionError::NotConfigured { missing } if missing == &vec![FusionModelRole::Panels]),
            "got {error:?}"
        );
    }

    #[test]
    fn an_explicit_models_list_supplies_the_panel_role_but_not_the_others() {
        let mut request = req();
        request.models = Some(vec![
            FusionModelRef {
                profile: Some("anthropic".into()),
                model: "claude-opus-5".into(),
            },
            FusionModelRef {
                profile: Some("openai".into()),
                model: "gpt-5.6-sol".into(),
            },
        ]);
        let error = resolve(
            &request,
            &FusionRuntimeConfig::defaults(),
            &three_provider_catalog(),
        )
        .unwrap_err();
        assert!(
            matches!(
                &error,
                FusionError::NotConfigured { missing }
                    if missing == &vec![FusionModelRole::Analyst]
            ),
            "a per-run --models list covers the panels only; got {error:?}"
        );

        let mut config = FusionRuntimeConfig {
            analyst_model: Some(FusionModelChoice::new("openai", "gpt-5.6-terra")),
            ..FusionRuntimeConfig::defaults()
        };
        config.min_successful_panels = 2;
        let resolved = resolve(&request, &config, &three_provider_catalog())
            .expect("an explicit list plus a configured analyst resolves");
        assert_eq!(resolved.panels.len(), 2);
    }

    // ---- configured routes are validated, never silently dropped -------

    #[test]
    fn a_roster_entry_the_catalog_does_not_have_fails_by_name() {
        let mut config = configured();
        config.panel_models[1] = FusionModelChoice::new("openai", "gpt-5.6-typo");
        let error = resolve(&req(), &config, &three_provider_catalog()).unwrap_err();
        let rendered = error.to_string();
        // Running the other two and reporting success would bill for an
        // ensemble the operator never approved and hide the typo indefinitely.
        assert!(rendered.contains("openai/gpt-5.6-typo"), "{rendered}");
        assert!(rendered.contains("fusion.panelModels"), "{rendered}");
        assert!(matches!(error, FusionError::InvalidConfiguration(_)));
    }

    #[test]
    fn a_roster_entry_without_usable_capacity_fails_before_dispatch() {
        let mut catalog = three_provider_catalog();
        catalog[2].limits = ModelLimits::unknown();
        let error = resolve(&req(), &configured(), &catalog).unwrap_err();
        let rendered = error.to_string();
        assert!(rendered.contains("openai/gpt-5.6-sol"), "{rendered}");
        assert!(rendered.contains("limits"), "{rendered}");
    }

    #[test]
    fn a_roster_entry_outside_the_profile_allowlist_fails_by_name() {
        let mut config = configured();
        config.allowed_profiles = vec!["anthropic".into(), "openai".into()];
        let error = resolve(&req(), &config, &three_provider_catalog()).unwrap_err();
        let rendered = error.to_string();
        assert!(rendered.contains("google/gemini-3-pro"), "{rendered}");
        assert!(rendered.contains("fusion.allowedProfiles"), "{rendered}");
    }

    #[test]
    fn a_cross_provider_roster_is_denied_when_the_run_is_same_provider() {
        let mut request = req();
        request.cross_provider = false;
        let error = resolve(&request, &configured(), &three_provider_catalog()).unwrap_err();
        assert!(
            matches!(error, FusionError::CrossProviderDenied),
            "{error:?}"
        );
    }

    #[test]
    fn cross_provider_denied_for_agent_origin_by_default() {
        let mut request = req();
        request.origin = FusionOrigin::Agent;
        let error = resolve(&request, &configured(), &three_provider_catalog()).unwrap_err();
        assert!(matches!(error, FusionError::CrossProviderDenied));
    }

    /// Two gateway spellings of ONE model are not an ensemble. Exact-pair
    /// uniqueness (all `FusionSettingsJson::validate` can check without the
    /// canonical normaliser) passes this roster, so preflight has to catch it.
    #[test]
    fn a_roster_that_seats_one_underlying_model_twice_is_rejected() {
        let mut catalog = three_provider_catalog();
        catalog.push(hinted(
            "openrouter",
            "openai/gpt-5.6-sol",
            100,
            FusionLatencyClass::Slow,
            FusionCostClass::High,
            true,
        ));
        let mut config = configured();
        config.panel_models = vec![
            FusionModelChoice::new("openai", "gpt-5.6-sol"),
            FusionModelChoice::new("openrouter", "openai/gpt-5.6-sol"),
        ];
        config.min_successful_panels = 2;
        let error = resolve(&req(), &config, &catalog).unwrap_err();
        let rendered = error.to_string();
        assert!(rendered.contains("same underlying model"), "{rendered}");
        assert!(
            rendered.contains("openrouter/openai/gpt-5.6-sol"),
            "{rendered}"
        );
    }

    #[test]
    fn a_dotted_gateway_spelling_of_the_same_model_is_also_caught() {
        let mut catalog = three_provider_catalog();
        catalog.push(hinted(
            "anthropic",
            "claude-fable-5-1",
            105,
            FusionLatencyClass::Slow,
            FusionCostClass::High,
            true,
        ));
        catalog.push(hinted(
            "openrouter",
            "anthropic/claude-fable-5.1",
            105,
            FusionLatencyClass::Slow,
            FusionCostClass::High,
            true,
        ));
        let mut config = configured();
        config.panel_models = vec![
            FusionModelChoice::new("anthropic", "claude-fable-5-1"),
            FusionModelChoice::new("openrouter", "anthropic/claude-fable-5.1"),
        ];
        config.min_successful_panels = 2;
        let error = resolve(&req(), &config, &catalog).unwrap_err();
        assert!(
            error.to_string().contains("same underlying model"),
            "{error}"
        );
    }

    // ---- the analyst ---------------------------------------------------

    #[test]
    fn an_analyst_that_cannot_emit_constrained_json_fails_at_preflight() {
        let mut catalog = three_provider_catalog();
        // The Gemini shape: the MODEL claims structured output but the owning
        // profile's codec cannot put a `response_format` on the wire, which
        // used to surface only after every panel had already spent.
        catalog[3].structured_output = false;
        let error = resolve(&req(), &configured(), &catalog).unwrap_err();
        assert!(
            matches!(error, FusionError::StructuredOutputUnsupported),
            "{error:?}"
        );
    }

    #[test]
    fn an_analyst_the_catalog_does_not_have_fails_by_name() {
        let mut config = configured();
        config.analyst_model = Some(FusionModelChoice::new("openai", "gpt-5.6-ghost"));
        let error = resolve(&req(), &config, &three_provider_catalog()).unwrap_err();
        let rendered = error.to_string();
        assert!(rendered.contains("fusion.analystModel"), "{rendered}");
        assert!(rendered.contains("openai/gpt-5.6-ghost"), "{rendered}");
    }

    #[test]
    fn an_analyst_without_room_for_its_configured_output_cap_fails_by_name() {
        let mut catalog = three_provider_catalog();
        catalog[3].limits = ModelLimits {
            context_window_tokens: Some(200_000),
            max_input_tokens: Some(180_000),
            max_output_tokens: Some(0),
        };
        let error = resolve(&req(), &configured(), &catalog).unwrap_err();
        assert!(
            error.to_string().contains("openai/gpt-5.6-terra"),
            "{error}"
        );
    }

    /// An operator may name their panel #1 as the judge. Self-preference bias
    /// is real, which is why the wizard warns about it — but the resolver does
    /// not overrule an explicit choice, and it must not silently substitute.
    #[test]
    fn an_analyst_that_is_also_a_panelist_is_honoured_not_substituted() {
        let mut config = configured();
        config.analyst_model = Some(FusionModelChoice::new("anthropic", "claude-opus-5"));
        let resolved = resolve(&req(), &config, &three_provider_catalog()).unwrap();
        assert_eq!(resolved.analyst.model, "claude-opus-5");
    }

    // ---- the explicit per-run `--models` path (unchanged contract) ------

    fn explicit(
        config_min: u8,
        models: Vec<FusionModelRef>,
    ) -> (FusionRequest, FusionRuntimeConfig) {
        let mut request = req();
        request.models = Some(models);
        let mut config = configured();
        config.min_successful_panels = config_min;
        (request, config)
    }

    #[test]
    fn custom_ineligible_model_is_allowed_when_listed() {
        let (request, config) = explicit(
            2,
            vec![
                FusionModelRef {
                    profile: Some("anthropic".into()),
                    model: "claude-haiku-4-5".into(),
                },
                FusionModelRef {
                    profile: Some("openai".into()),
                    model: "gpt-5.6-sol".into(),
                },
            ],
        );
        let resolved = resolve(&request, &config, &three_provider_catalog()).unwrap();
        assert_eq!(
            routes(&resolved.panels),
            vec![
                "anthropic/claude-haiku-4-5".to_string(),
                "openai/gpt-5.6-sol".to_string(),
            ]
        );
    }

    #[test]
    fn explicit_model_without_capacity_metadata_fails_before_dispatch() {
        let mut catalog = three_provider_catalog();
        catalog[1].limits = ModelLimits::unknown();
        let (request, config) = explicit(
            2,
            vec![
                FusionModelRef {
                    profile: Some("anthropic".into()),
                    model: "claude-haiku-4-5".into(),
                },
                FusionModelRef {
                    profile: Some("openai".into()),
                    model: "gpt-5.6-sol".into(),
                },
            ],
        );
        let error = resolve(&request, &config, &catalog)
            .expect_err("explicit unknown capacity must fail closed");
        assert!(
            matches!(error, FusionError::InvalidCustomModels(_)),
            "{error:?}"
        );
    }

    #[test]
    fn explicit_models_over_max_panel_is_rejected_not_truncated() {
        let mut request = req();
        request.max_panel = Some(2);
        request.models = Some(vec![
            FusionModelRef {
                profile: Some("anthropic".into()),
                model: "claude-opus-5".into(),
            },
            FusionModelRef {
                profile: Some("openai".into()),
                model: "gpt-5.6-sol".into(),
            },
            FusionModelRef {
                profile: Some("google".into()),
                model: "gemini-3-pro".into(),
            },
        ]);
        let mut config = configured();
        config.min_successful_panels = 2;
        let error = resolve(&request, &config, &three_provider_catalog()).unwrap_err();
        assert!(
            matches!(&error, FusionError::InvalidCustomModels(msg) if msg.contains("exceeding")),
            "{error:?}"
        );
    }

    #[test]
    fn explicit_model_list_cannot_lower_the_configured_success_minimum() {
        let (request, config) = explicit(
            3,
            vec![
                FusionModelRef {
                    profile: Some("anthropic".into()),
                    model: "claude-opus-5".into(),
                },
                FusionModelRef {
                    profile: Some("openai".into()),
                    model: "gpt-5.6-sol".into(),
                },
            ],
        );
        let error = resolve(&request, &config, &three_provider_catalog()).unwrap_err();
        assert!(
            matches!(&error, FusionError::InvalidCustomModels(msg)
                if msg.contains("fusion.minSuccessfulPanels")),
            "{error:?}"
        );
    }

    #[test]
    fn request_panel_cap_cannot_lower_the_configured_success_minimum() {
        let mut request = req();
        request.max_panel = Some(2);
        let mut config = configured();
        config.min_successful_panels = 3;
        config.quality_panel_count = 3;
        config.fast_panel_count = 3;
        let error = resolve(&request, &config, &three_provider_catalog()).unwrap_err();
        assert!(
            matches!(&error, FusionError::InvalidRequest(msg)
                if msg.contains("fusion.minSuccessfulPanels")),
            "{error:?}"
        );
    }

    #[test]
    fn a_preflight_configuration_error_guarantees_zero_provider_calls() {
        // The spawn-slot release path (`tools/agent`'s `fusion_error_is_preflight`)
        // reads this, so a variant that could follow a provider call must not
        // claim it.
        assert!(FusionError::NotConfigured {
            missing: vec![FusionModelRole::Panels],
        }
        .guarantees_zero_provider_calls());
    }
}
