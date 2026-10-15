//! Production helper for the Native 2.1.295 `bQn` model-validation boundary.
//!
//! The current selection/resolver owner supplies exact route, policy, settings,
//! and credential observations. This module preserves source model UTF-16,
//! applies the observed Native local validation rules in source order, and owns
//! the validated-name cache and probe wait lifecycle.

use agent::model_resolution::{
    ModelProviderKind, NativeModelObservation as HostModelObservation, NativeModelValidationContext,
};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_llm_client::protocol::{
    native_model_suggestion, JsonPayload, NativeModelObservation, NativeModelProvider,
    NativeModelSuggestionFacts,
};
use llm_runtime::LlmError;
use std::collections::HashSet;
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;
use tokio::task::JoinHandle;

/// Native `bQn`'s bounded wait for one model-validation probe (`M=15000`).
///
/// A wait timeout is only a caller result. It does not abort the spawned
/// validation operation; a late successful probe still updates the cache.
pub const NATIVE_MODEL_VALIDATION_WAIT: Duration = Duration::from_secs(15);

/// The exact short-name fast-path set imported as `tV` in Native 2.1.295.
const NATIVE_KNOWN_MODEL_ALIASES: [&str; 9] = [
    "sonnet",
    "opus",
    "haiku",
    "fable",
    "best",
    "sonnet[1m]",
    "opus[1m]",
    "fable[1m]",
    "opusplan",
];

/// Facts that remain owned by the existing resolver/host integration.
///
/// `policy_allowed` is the result of the real current model-policy gate (Native
/// `Kr` plus any already-bound model-access rules). It must be computed before
/// asking this module about aliases or the validated-name cache. The picker
/// observation combines exact `dSe()` option matching with Native
/// `yfo(model)`; Unobserved is not evidence that no option exists.
/// `first_party_base_url` is the
/// source result `bi() || hostname === anthropic.com || hostname ends in
/// .anthropic.com || hostname appears in the imported first-party host list`;
/// endpoint parsing/host-list ownership stays with the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeModelValidationFacts {
    pub policy_allowed: bool,
    pub force_server_probe: bool,
    pub server_probe: bool,
    pub custom_model_option_matches: bool,
    pub picker_model_option_is_valid: NativeModelObservation<bool>,
    pub provider: ModelProviderKind,
    pub first_party_base_url: bool,
    /// Source facts used only if the provider probe returns a typed 404.
    pub suggestion_facts: NativeModelSuggestionFacts,
}

/// Combine the observed Native `dSe()` picker options and `yfo(candidate)`.
/// Missing observations stay Unobserved; only a known exact option with a
/// positive eligibility result can take the picker fast path.
#[must_use]
pub fn native_picker_model_option_is_valid(
    candidate_source: &Utf16JsonProjection,
    context: &NativeModelValidationContext,
) -> NativeModelObservation<bool> {
    let Ok(candidate) = TrimmedModelSource::from_projection(candidate_source) else {
        return NativeModelObservation::Unobserved;
    };
    let has_exact_picker_option = match &context.picker_options {
        HostModelObservation::Unobserved => return NativeModelObservation::Unobserved,
        HostModelObservation::Observed(None) => false,
        HostModelObservation::Observed(Some(options)) => options.iter().any(|option| {
            TrimmedModelSource::from_projection(option)
                .is_ok_and(|option| option.code_units() == candidate.code_units())
        }),
    };
    if !has_exact_picker_option {
        return NativeModelObservation::Observed(false);
    }
    match &context.picker_eligibility {
        HostModelObservation::Unobserved => NativeModelObservation::Unobserved,
        HostModelObservation::Observed(eligible) => NativeModelObservation::Observed(*eligible),
    }
}

/// Convert current host route facts to SDK suggestion facts without replacing
/// a missing observation with an empty/default value.
#[must_use]
pub fn native_model_suggestion_facts(
    context: &NativeModelValidationContext,
) -> NativeModelSuggestionFacts {
    let effective_model_by_catalog_id = match &context.effective_model_by_catalog_id {
        HostModelObservation::Unobserved => NativeModelObservation::Unobserved,
        HostModelObservation::Observed(models) => NativeModelObservation::Observed(
            models
                .iter()
                .filter_map(|(catalog_id, model)| {
                    projection_string_payload(model).map(|model| (catalog_id.clone(), model))
                })
                .collect(),
        ),
    };
    let anthropic_default_opus_model = match &context.anthropic_default_opus_model {
        HostModelObservation::Unobserved => NativeModelObservation::Unobserved,
        HostModelObservation::Observed(None) => NativeModelObservation::Observed(None),
        HostModelObservation::Observed(Some(model)) => projection_string_payload(model)
            .map(Some)
            .map(NativeModelObservation::Observed)
            .unwrap_or(NativeModelObservation::Unobserved),
    };
    NativeModelSuggestionFacts {
        effective_model_by_catalog_id,
        anthropic_default_opus_model,
    }
}

fn projection_string_payload(source: &Utf16JsonProjection) -> Option<JsonPayload> {
    source
        .string_units("")
        .map(|units| JsonPayload::string_from_utf16_code_units(&units))
}

/// Exact source string after the one Native `String.trim()` operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrimmedModelSource {
    source: Utf16JsonProjection,
    cache_key: Vec<u16>,
}

impl TrimmedModelSource {
    /// Extract a root JSON string and apply ECMAScript `String.trim()` while
    /// preserving unpaired surrogates and exact cache-key units.
    pub fn from_projection(
        source: &Utf16JsonProjection,
    ) -> Result<Self, ModelValidationInputError> {
        let units = source
            .string_units("")
            .ok_or(ModelValidationInputError::ModelMustBeString)?;
        let trimmed = trim_ecmascript_whitespace(&units);
        let display = String::from_utf16_lossy(&trimmed);
        let source = Utf16JsonProjection::root_string(display, trimmed.clone())
            .map_err(|_| ModelValidationInputError::ModelMustBeString)?;
        Ok(Self {
            source,
            cache_key: trimmed,
        })
    }

    #[must_use]
    pub fn source(&self) -> &Utf16JsonProjection {
        &self.source
    }

    #[must_use]
    pub fn code_units(&self) -> &[u16] {
        &self.cache_key
    }

    #[must_use]
    pub fn cache_key(&self) -> &[u16] {
        &self.cache_key
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cache_key.is_empty()
    }
}

/// The source value supplied to `bQn` was not a root JSON string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ModelValidationInputError {
    #[error("model input must be a string")]
    ModelMustBeString,
}

/// Why the local `bQn` path returned without issuing a provider request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelValidationFastPath {
    /// The lowercased trimmed value is in Native's imported `tV` set.
    KnownAlias,
    /// The caller established exact equality with the custom-model option.
    CustomModelOption,
    /// The caller established `yfo(model)` and exact picker-option equality.
    PickerOption,
    /// Exact UTF-16 trimmed model-name cache hit.
    PreviouslyValidated,
    /// Native caller disabled server probing.
    ServerProbeDisabled,
}

/// The local validation decision. `ProbeRequired` is data only; it does not
/// contact an SDK, provider, credential service, or network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeModelValidationDecision {
    Accepted {
        model: TrimmedModelSource,
        fast_path: ModelValidationFastPath,
    },
    Rejected(NativeModelValidationFailure),
    ProbeRequired(NativeModelProbePayload),
}

/// A request to execute Native's fixed model-validation protocol. The adapter
/// derives the fixed token limit, retry count, query source, message, and cache
/// marker from this purpose instead of copying them into caller data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeModelProbePayload {
    pub model: TrimmedModelSource,
    suggestion_provider: NativeModelObservation<NativeModelProvider>,
    suggestion_facts: NativeModelSuggestionFacts,
}

impl NativeModelProbePayload {
    fn new(
        model: TrimmedModelSource,
        suggestion_provider: NativeModelObservation<NativeModelProvider>,
        suggestion_facts: NativeModelSuggestionFacts,
    ) -> Self {
        Self {
            model,
            suggestion_provider,
            suggestion_facts,
        }
    }
}

/// Apply `bQn`'s local checks in source order. The actual allowlist/model-access
/// decision is supplied by the current resolver so this helper cannot create a
/// second, divergent policy implementation.
#[must_use]
pub fn prepare_native_model_validation(
    source_model: &Utf16JsonProjection,
    facts: NativeModelValidationFacts,
    cache: &NativeModelValidationCache,
) -> Result<NativeModelValidationDecision, ModelValidationInputError> {
    let model = TrimmedModelSource::from_projection(source_model)?;

    if model.is_empty() {
        return Ok(NativeModelValidationDecision::Rejected(
            NativeModelValidationFailure::local(
                NativeModelValidationFailureKind::Empty,
                "Model name cannot be empty".into(),
                false,
            ),
        ));
    }

    // `Kr` is before every alias/picker/cache fast path in `bQn`.
    if !facts.policy_allowed {
        return Ok(NativeModelValidationDecision::Rejected(
            NativeModelValidationFailure::not_allowed(&model),
        ));
    }

    if !facts.force_server_probe {
        if is_native_known_model_alias(model.code_units()) {
            return Ok(accepted(model, ModelValidationFastPath::KnownAlias));
        }
        if facts.custom_model_option_matches {
            return Ok(accepted(model, ModelValidationFastPath::CustomModelOption));
        }
        if matches!(
            &facts.picker_model_option_is_valid,
            NativeModelObservation::Observed(true)
        ) {
            return Ok(accepted(model, ModelValidationFastPath::PickerOption));
        }
        if cache.contains(model.cache_key()) {
            return Ok(accepted(
                model,
                ModelValidationFastPath::PreviouslyValidated,
            ));
        }
    }

    // Native's format rejection is after all non-forced fast paths and before
    // `serverProbe === false`.
    let whitespace_allowed = facts.provider == ModelProviderKind::Gateway
        || (facts.provider == ModelProviderKind::FirstParty && !facts.first_party_base_url);
    if !whitespace_allowed && contains_native_model_format_character(model.code_units()) {
        return Ok(NativeModelValidationDecision::Rejected(
            NativeModelValidationFailure::not_found(&model, None),
        ));
    }

    if !facts.server_probe {
        return Ok(accepted(
            model,
            ModelValidationFastPath::ServerProbeDisabled,
        ));
    }

    Ok(NativeModelValidationDecision::ProbeRequired(
        NativeModelProbePayload::new(
            model,
            native_suggestion_provider(facts.provider),
            facts.suggestion_facts,
        ),
    ))
}

fn native_suggestion_provider(
    provider: ModelProviderKind,
) -> NativeModelObservation<NativeModelProvider> {
    let provider = match provider {
        ModelProviderKind::FirstParty => NativeModelProvider::FirstParty,
        ModelProviderKind::Bedrock => NativeModelProvider::Bedrock,
        ModelProviderKind::Vertex => NativeModelProvider::Vertex,
        ModelProviderKind::Foundry => NativeModelProvider::Foundry,
        ModelProviderKind::AnthropicAws => NativeModelProvider::AnthropicAws,
        ModelProviderKind::AnthropicGoogleCloud => NativeModelProvider::AnthropicGoogleCloud,
        ModelProviderKind::Mantle => NativeModelProvider::Mantle,
        ModelProviderKind::Gateway => NativeModelProvider::Gateway,
        ModelProviderKind::Other => return NativeModelObservation::Unobserved,
    };
    NativeModelObservation::Observed(provider)
}

fn accepted(
    model: TrimmedModelSource,
    fast_path: ModelValidationFastPath,
) -> NativeModelValidationDecision {
    NativeModelValidationDecision::Accepted { model, fast_path }
}

fn is_native_known_model_alias(units: &[u16]) -> bool {
    let Ok(model) = String::from_utf16(units) else {
        return false;
    };
    let normalized = model.to_lowercase();
    NATIVE_KNOWN_MODEL_ALIASES.contains(&normalized.as_str())
}

fn trim_ecmascript_whitespace(units: &[u16]) -> Vec<u16> {
    let start = units
        .iter()
        .position(|unit| !is_ecmascript_trim_unit(*unit))
        .unwrap_or(units.len());
    let end = units
        .iter()
        .rposition(|unit| !is_ecmascript_trim_unit(*unit))
        .map_or(start, |index| index + 1);
    units[start..end].to_vec()
}

fn is_ecmascript_trim_unit(unit: u16) -> bool {
    matches!(
        unit,
        0x0009..=0x000d
            | 0x0020
            | 0x00a0
            | 0x1680
            | 0x2000..=0x200a
            | 0x2028
            | 0x2029
            | 0x202f
            | 0x205f
            | 0x3000
            | 0xfeff
    )
}

/// Source regex `/[\s\p{Cc}\p{Cf}]/u` over UTF-16. Valid surrogate pairs are
/// interpreted as one scalar; a lone surrogate is neither `Cc` nor `Cf` and
/// therefore does not match this format check.
fn contains_native_model_format_character(units: &[u16]) -> bool {
    static FORMAT: OnceLock<regex::Regex> = OnceLock::new();
    // JavaScript `\s` is spelled out because Rust regex `\s` also includes
    // U+0085, which ECMAScript does not. `\p{Cc}`/`\p{Cf}` use the regex
    // crate's Unicode property table. Lossy projection maps only isolated
    // surrogate units to U+FFFD (General_Category=So); Native's `u` regex
    // sees those as Cs, so neither representation matches Cc/Cf/whitespace.
    let pattern = r"[\x{0009}-\x{000D}\x{0020}\x{00A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}\p{Cc}\p{Cf}]";
    FORMAT
        .get_or_init(|| regex::Regex::new(pattern).expect("fixed Native model-format regex"))
        .is_match(&String::from_utf16_lossy(units))
}

/// Exact source-name cache used by Native `providerCache.validatedModels`.
/// The cache intentionally keys on the trimmed UTF-16 units, preserving case
/// and unpaired surrogates; it is not keyed by a lossy display string.
#[derive(Debug, Clone, Default)]
pub struct NativeModelValidationCache {
    names: Arc<Mutex<HashSet<Vec<u16>>>>,
}

impl NativeModelValidationCache {
    #[must_use]
    pub fn contains(&self, model_key: &[u16]) -> bool {
        self.names
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(model_key)
    }

    pub fn insert(&self, model_key: &[u16]) -> bool {
        self.names
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(model_key.to_vec())
    }

    fn remove(&self, model_key: &[u16]) -> bool {
        self.names
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(model_key)
    }

    /// Clear the owner-scoped cache when its provider/account generation is
    /// replaced. An already-running ordinary probe may still repopulate it on
    /// successful completion, matching Native's unfenced direct `bQn` path.
    pub fn clear(&self) {
        self.names
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    /// Spawn the host-owned probe. Only successful eventual completion updates
    /// the validated-name cache, even when the caller's 15s wait has expired.
    /// Dropping `NativeModelProbeTask` detaches the Tokio task (JoinHandle's
    /// native behavior); the explicit deadline path uses `abort_and_join`.
    pub fn spawn_probe<F>(
        &self,
        payload: &NativeModelProbePayload,
        probe: F,
    ) -> NativeModelProbeTask
    where
        F: Future<Output = Result<(), LlmError>> + Send + 'static,
    {
        let model_key = payload.model.cache_key().to_vec();
        let cache = self.clone();
        let task_cache = cache.clone();
        let handle = tokio::spawn(async move {
            let result = probe.await;
            if result.is_ok() {
                cache.insert(&model_key);
            }
            result
        });
        NativeModelProbeTask {
            model: payload.model.clone(),
            suggestion_provider: payload.suggestion_provider.clone(),
            suggestion_facts: payload.suggestion_facts.clone(),
            handle: Some(handle),
            cache: task_cache,
        }
    }
}

/// A still-owned validation operation. A wait timeout leaves the task alive;
/// the explicit deadline path can abort and reap it using `abort_and_join`.
pub struct NativeModelProbeTask {
    model: TrimmedModelSource,
    suggestion_provider: NativeModelObservation<NativeModelProvider>,
    suggestion_facts: NativeModelSuggestionFacts,
    handle: Option<JoinHandle<Result<(), LlmError>>>,
    cache: NativeModelValidationCache,
}

impl std::fmt::Debug for NativeModelProbeTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeModelProbeTask")
            .field("model", &self.model)
            .field("is_finished", &self.is_finished())
            .finish_non_exhaustive()
    }
}

impl NativeModelProbeTask {
    /// Await the source 15s budget. `TimedOutDetached` does not abort the probe and the
    /// task retains its handle for a later completion check.
    pub async fn wait(&mut self) -> NativeModelProbeWait {
        let Some(handle) = self.handle.as_mut() else {
            return NativeModelProbeWait::AlreadyReaped;
        };
        match tokio::time::timeout(NATIVE_MODEL_VALIDATION_WAIT, handle).await {
            Err(_) => NativeModelProbeWait::TimedOutDetached,
            Ok(joined) => {
                self.handle.take();
                map_probe_join(
                    self.model.clone(),
                    self.suggestion_provider.clone(),
                    &self.suggestion_facts,
                    joined,
                )
            }
        }
    }

    /// Native Gzr's explicit deadline path aborts the pending request, then
    /// reaps the task. A completed result is discarded if the caller's account
    /// generation changed during the request, and its cached model is removed.
    pub async fn abort_and_join<F>(
        &mut self,
        deadline: Duration,
        starting_account_generation: u64,
        current_account_generation: F,
    ) -> NativeModelProbeWait
    where
        F: FnOnce() -> u64,
    {
        let Some(handle) = self.handle.as_mut() else {
            return NativeModelProbeWait::AlreadyReaped;
        };
        match tokio::time::timeout(deadline, handle).await {
            Ok(joined) => {
                self.handle.take();
                if current_account_generation() != starting_account_generation {
                    self.cache.remove(self.model.cache_key());
                    NativeModelProbeWait::AccountGenerationChanged
                } else {
                    map_probe_join(
                        self.model.clone(),
                        self.suggestion_provider.clone(),
                        &self.suggestion_facts,
                        joined,
                    )
                }
            }
            Err(_) => {
                if let Some(handle) = self.handle.take() {
                    handle.abort();
                    let _ = handle.await;
                }
                NativeModelProbeWait::TimedOutAborted
            }
        }
    }

    /// Reap a completed operation without imposing another timeout. Returns
    /// `None` while the task remains active.
    #[cfg(test)]
    pub async fn reap_if_finished(&mut self) -> Option<NativeModelProbeWait> {
        if !self.is_finished() {
            return None;
        }
        let handle = self.handle.take()?;
        Some(map_probe_join(
            self.model.clone(),
            self.suggestion_provider.clone(),
            &self.suggestion_facts,
            handle.await,
        ))
    }

    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }
}

fn map_probe_join(
    model: TrimmedModelSource,
    suggestion_provider: NativeModelObservation<NativeModelProvider>,
    suggestion_facts: &NativeModelSuggestionFacts,
    joined: Result<Result<(), LlmError>, tokio::task::JoinError>,
) -> NativeModelProbeWait {
    match joined {
        Ok(Ok(())) => NativeModelProbeWait::Validated,
        Ok(Err(error)) => NativeModelProbeWait::Rejected(classify_native_probe_error(
            &model,
            &error,
            suggestion_provider,
            suggestion_facts,
        )),
        Err(error) => NativeModelProbeWait::Rejected(NativeModelValidationFailure::unexpected(
            &error.to_string(),
        )),
    }
}

/// Wait result for the host's bounded probe lifecycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeModelProbeWait {
    Validated,
    Rejected(NativeModelValidationFailure),
    TimedOutDetached,
    TimedOutAborted,
    AccountGenerationChanged,
    AlreadyReaped,
}

/// Native validation failure flags and source-preserving message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeModelValidationFailure {
    pub kind: NativeModelValidationFailureKind,
    pub message: Utf16JsonProjection,
    pub retryable: bool,
    pub not_found: bool,
    pub not_allowed: bool,
    pub permission_denied: bool,
    pub authentication_failed: bool,
    pub http_status: Option<u16>,
    /// Native's object spread emits these properties only when the predicate
    /// is true. `None` therefore means the property is omitted, except when a
    /// listed source gap records that the available error carrier cannot
    /// determine the predicate.
    pub credits_required: Option<bool>,
    pub server_refused_model: Option<bool>,
    /// `Sjn(e)` currently returns false in the fixed 2.1.295 source, so Native
    /// omits this property for every error.
    pub session_mode_refused: Option<bool>,
    pub source_gaps: Vec<NativeModelValidationSourceGap>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeModelValidationSourceGap {
    /// Native's final Error.message is unavailable from this variant. The
    /// CLI display projection is not equivalent and cannot be treated as parity.
    UnexpectedErrorMessageNotRetained,
    /// Native `ikn(model)` needs current route/model settings that the host did
    /// not observe, so this typed 404 could not be rendered with its suggestion.
    NotFoundSuggestionFactsUnobserved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeModelValidationFailureKind {
    Empty,
    NotAllowed,
    NotFound,
    Authentication,
    Network,
    PermissionDenied,
    Api,
    Unexpected,
}

impl NativeModelValidationFailure {
    fn local(
        kind: NativeModelValidationFailureKind,
        message: Utf16JsonProjection,
        retryable: bool,
    ) -> Self {
        Self {
            kind,
            message,
            retryable,
            not_found: false,
            not_allowed: false,
            permission_denied: false,
            authentication_failed: false,
            http_status: None,
            credits_required: None,
            server_refused_model: None,
            session_mode_refused: None,
            source_gaps: Vec::new(),
        }
    }

    fn not_allowed(model: &TrimmedModelSource) -> Self {
        let mut units = "Model '".encode_utf16().collect::<Vec<_>>();
        units.extend_from_slice(model.code_units());
        units.extend("' is not in the list of available models".encode_utf16());
        Self {
            kind: NativeModelValidationFailureKind::NotAllowed,
            message: projection_string(units),
            retryable: false,
            not_found: false,
            not_allowed: true,
            permission_denied: false,
            authentication_failed: false,
            http_status: None,
            credits_required: None,
            server_refused_model: None,
            session_mode_refused: None,
            source_gaps: Vec::new(),
        }
    }

    fn not_found(model: &TrimmedModelSource, suggestion: Option<&JsonPayload>) -> Self {
        let mut units = "Model '".encode_utf16().collect::<Vec<_>>();
        units.extend_from_slice(model.code_units());
        units.extend("' not found".encode_utf16());
        if let Some(suggestion) = suggestion.and_then(JsonPayload::root_string_code_units) {
            units.extend(". Try '".encode_utf16());
            units.extend(suggestion);
            units.extend("' instead".encode_utf16());
        }
        Self {
            kind: NativeModelValidationFailureKind::NotFound,
            message: projection_string(units),
            retryable: false,
            not_found: true,
            not_allowed: false,
            permission_denied: false,
            authentication_failed: false,
            http_status: None,
            credits_required: None,
            server_refused_model: None,
            session_mode_refused: None,
            source_gaps: Vec::new(),
        }
    }

    fn unexpected(detail: &str) -> Self {
        let mut units = "Unable to validate model: "
            .encode_utf16()
            .collect::<Vec<_>>();
        units.extend(detail.encode_utf16());
        Self {
            kind: NativeModelValidationFailureKind::Unexpected,
            message: projection_string(units),
            retryable: true,
            not_found: false,
            not_allowed: false,
            permission_denied: false,
            authentication_failed: false,
            http_status: None,
            credits_required: None,
            server_refused_model: None,
            session_mode_refused: None,
            source_gaps: Vec::new(),
        }
    }
}

/// Map retained Native Anthropic API error facts to Native `A`'s model-
/// validation categories. Error-body predicates use the source-preserving
/// `ProviderErrorText`; this does not use the CLI-facing `error_display_source`
/// as a substitute for Native `hue`.
#[must_use]
pub fn classify_native_probe_error(
    model: &TrimmedModelSource,
    error: &LlmError,
    suggestion_provider: NativeModelObservation<NativeModelProvider>,
    suggestion_facts: &NativeModelSuggestionFacts,
) -> NativeModelValidationFailure {
    use llm_runtime::LlmError as E;

    let source_error = error.classification_source();
    let status = error.http_status();
    let native_api = error
        .native_anthropic_error_facts()
        .is_some_and(|facts| facts.response.is_some())
        && error.provider_error_text().is_some();
    let body = native_api.then(|| native_error_body(error)).flatten();

    match source_error {
        E::ModelUnavailable => {
            let (suggestion, suggestion_unobserved) = if native_api {
                match native_model_suggestion(
                    model.code_units(),
                    suggestion_provider,
                    suggestion_facts,
                ) {
                    NativeModelObservation::Observed(suggestion) => (suggestion, false),
                    NativeModelObservation::Unobserved => (None, true),
                }
            } else {
                (None, false)
            };
            let mut failure = NativeModelValidationFailure::not_found(model, suggestion.as_ref());
            failure.http_status = status;
            if native_api {
                if suggestion_unobserved {
                    failure
                        .source_gaps
                        .push(NativeModelValidationSourceGap::NotFoundSuggestionFactsUnobserved);
                }
                if body
                    .as_ref()
                    .is_some_and(|body| native_qre_matches(error, body, status))
                {
                    failure.server_refused_model = Some(true);
                }
            }
            failure
        }
        E::Authentication { .. } | E::OAuthRefreshDead => NativeModelValidationFailure {
            kind: NativeModelValidationFailureKind::Authentication,
            message: "Authentication failed. Please check your API credentials.".into(),
            retryable: false,
            not_found: false,
            not_allowed: false,
            permission_denied: false,
            authentication_failed: true,
            http_status: status,
            credits_required: None,
            server_refused_model: None,
            session_mode_refused: None,
            source_gaps: Vec::new(),
        },
        E::Transport { .. } | E::TransportTimeout { .. } => NativeModelValidationFailure {
            kind: NativeModelValidationFailureKind::Network,
            message: "Network error. Please check your internet connection.".into(),
            retryable: true,
            not_found: false,
            not_allowed: false,
            permission_denied: false,
            authentication_failed: false,
            http_status: status,
            credits_required: None,
            server_refused_model: None,
            session_mode_refused: None,
            source_gaps: Vec::new(),
        },
        _ if native_api
            && status == Some(403)
            && body
                .as_ref()
                .is_some_and(|body| native_xct_matches(error, body)) =>
        {
            let mut units = "Model '".encode_utf16().collect::<Vec<_>>();
            units.extend_from_slice(model.code_units());
            units.extend("' isn't available for your account".encode_utf16());
            NativeModelValidationFailure {
                kind: NativeModelValidationFailureKind::PermissionDenied,
                message: projection_string(units),
                retryable: false,
                not_found: false,
                not_allowed: false,
                permission_denied: true,
                authentication_failed: false,
                http_status: status,
                credits_required: None,
                server_refused_model: Some(true),
                session_mode_refused: None,
                source_gaps: Vec::new(),
            }
        }
        _ if native_api => {
            let mut failure = NativeModelValidationFailure {
                kind: NativeModelValidationFailureKind::Api,
                message: native_api_error_message(error),
                retryable: status
                    .is_none_or(|status| status == 408 || status == 429 || status >= 500),
                not_found: false,
                not_allowed: false,
                permission_denied: false,
                authentication_failed: false,
                http_status: status,
                credits_required: None,
                server_refused_model: None,
                session_mode_refused: None,
                source_gaps: Vec::new(),
            };

            if status == Some(429) && body.as_ref().is_some_and(native_credits_required_body) {
                failure.credits_required = Some(true);
                if native_server_refused_model(error, body.as_ref().expect("body was checked")) {
                    failure.server_refused_model = Some(true);
                }
            }

            // Native `Sjn(e)` is source-bound as `return !1`; the property is
            // therefore omitted even for an HTTP 400 response.
            failure
        }
        _ => {
            let (detail, retained) = native_unexpected_error_message(error);
            let mut units = "Unable to validate model: "
                .encode_utf16()
                .collect::<Vec<_>>();
            units.extend(detail);
            let mut failure = NativeModelValidationFailure {
                kind: NativeModelValidationFailureKind::Unexpected,
                message: projection_string(units),
                retryable: true,
                not_found: false,
                not_allowed: false,
                permission_denied: false,
                authentication_failed: false,
                http_status: status,
                credits_required: None,
                server_refused_model: None,
                session_mode_refused: None,
                source_gaps: Vec::new(),
            };
            if !retained {
                failure
                    .source_gaps
                    .push(NativeModelValidationSourceGap::UnexpectedErrorMessageNotRetained);
            }
            failure
        }
    }
}

fn native_error_body(error: &LlmError) -> Option<Utf16JsonProjection> {
    let json_source = error
        .native_anthropic_error_facts()?
        .response
        .as_ref()?
        .body
        .as_ref()?
        .as_json_source();
    let units = json_source.encode_utf16().collect::<Vec<_>>();
    Utf16JsonProjection::parse_utf16_json(&units).ok()
}

fn native_qre_matches(error: &LlmError, body: &Utf16JsonProjection, status: Option<u16>) -> bool {
    if status != Some(404) {
        return false;
    }
    let Some(text) = error.provider_error_text() else {
        return false;
    };
    (native_body_has_not_found_type(body)
        || contains_ascii_units(text.utf16_code_units(), "\"type\":\"not_found_error\""))
        && contains_ascii_units(text.utf16_code_units(), "model:")
}

fn native_body_has_not_found_type(body: &Utf16JsonProjection) -> bool {
    body.string_at("/error/type") == Some("not_found_error")
}

fn native_xct_matches(error: &LlmError, body: &Utf16JsonProjection) -> bool {
    let Some(text) = error.provider_error_text() else {
        return false;
    };
    (body.string_at("/error/type") == Some("permission_error")
        || contains_ascii_units(text.utf16_code_units(), "\"type\":\"permission_error\""))
        && contains_ascii_units(text.utf16_code_units(), "model:")
}

fn native_credits_required_body(body: &Utf16JsonProjection) -> bool {
    body.string_at("/error/details/error_code") == Some("credits_required")
}

fn native_server_refused_model(error: &LlmError, body: &Utf16JsonProjection) -> bool {
    let Some(response) = error
        .native_anthropic_error_facts()
        .and_then(|facts| facts.response.as_ref())
    else {
        return false;
    };
    if response
        .unified_representative_claim
        .as_deref()
        .is_some_and(|value| !value.is_empty())
    {
        return false;
    }
    let disabled_reason = response
        .unified_overage_disabled_reason
        .as_deref()
        .or_else(|| body.string_at("/error/details/disabled_reason"))
        .unwrap_or("");
    !matches!(
        disabled_reason,
        "fetch_error" | "unknown" | "org_level_disabled_until"
    )
}

fn contains_ascii_units(haystack: &[u16], needle: &str) -> bool {
    let needle = needle.encode_utf16().collect::<Vec<_>>();
    haystack
        .windows(needle.len())
        .any(|window| window == needle.as_slice())
}

fn native_api_error_message(error: &LlmError) -> Utf16JsonProjection {
    let mut detail = native_hue_message_units(error);
    while detail
        .last()
        .is_some_and(|unit| matches!(*unit, 0x0021 | 0x002e | 0x003f | 0x2026))
    {
        detail.pop();
    }

    // Native A always adds exactly one fixed prefix around `hue(e)`. Using the
    // provider's source message (and applying hue's body extraction here)
    // avoids feeding the separate CLI-facing renderer back into that wrapper.
    let mut units = "API error: ".encode_utf16().collect::<Vec<_>>();
    units.extend(detail);
    units.extend(" · model not changed".encode_utf16());
    projection_string(units)
}

fn native_hue_message_units(error: &LlmError) -> Vec<u16> {
    let Some(text) = error.provider_error_text() else {
        // This function is only called after the Native API facts gate, which
        // requires ProviderErrorText. Do not substitute the CLI display text
        // if that invariant is violated.
        return Vec::new();
    };
    let raw = text.utf16_code_units();
    let status = error.http_status();
    if raw.is_empty() {
        if let Some(body) = native_error_body(error) {
            if let Some(message) = native_hue_error_object_message(&body) {
                return message;
            }
        }
        return format!(
            "API error (status {})",
            status.map_or_else(|| "unknown".to_owned(), |v| v.to_string())
        )
        .encode_utf16()
        .collect();
    }

    if contains_ascii_units(raw, "{\"") {
        if let Some(body) = native_error_body(error) {
            if let Some(message) = native_hue_error_object_message(&body) {
                return prefix_native_status(status, message);
            }
        }
    }

    let title = native_html_title(raw);
    if let Some(title) = title {
        if title.is_empty() {
            if raw_matches_native_html_fallback(raw) {
                return native_html_status_fallback(status);
            }
        } else {
            let status_text = status
                .filter(|value| *value != 0)
                .map(|value| value.to_string());
            if let Some(status_text) = status_text {
                if !contains_ascii_digit_sequence(&title, &status_text) {
                    return prefix_native_status(status, title);
                }
            }
            return title;
        }
    }
    trim_ecmascript_end(raw)
}

fn native_hue_error_object_message(body: &Utf16JsonProjection) -> Option<Vec<u16>> {
    // Native O(e): e.error.error.message, then e.error.message.
    for pointer in ["/error/error/message", "/error/message"] {
        let Some(units) = body.string_units(pointer) else {
            continue;
        };
        if units.is_empty() {
            continue;
        }
        let title = native_html_title(&units);
        let normalized = title.unwrap_or(units);
        if !normalized.is_empty() {
            return Some(normalized);
        }
    }
    None
}

fn prefix_native_status(status: Option<u16>, mut message: Vec<u16>) -> Vec<u16> {
    if let Some(status) = status.filter(|status| *status != 0) {
        let mut prefixed = format!("{status} ").encode_utf16().collect::<Vec<_>>();
        prefixed.append(&mut message);
        prefixed
    } else {
        message
    }
}

fn contains_ascii_digit_sequence(units: &[u16], expected: &str) -> bool {
    let mut start = 0;
    while start < units.len() {
        if (b'0' as u16..=b'9' as u16).contains(&units[start]) {
            let mut end = start + 1;
            while units
                .get(end)
                .is_some_and(|unit| (b'0' as u16..=b'9' as u16).contains(unit))
            {
                end += 1;
            }
            if units[start..end]
                .iter()
                .map(|unit| char::from_u32(u32::from(*unit)).expect("ASCII digit"))
                .collect::<String>()
                == expected
            {
                return true;
            }
            start = end;
        } else {
            start += 1;
        }
    }
    false
}

fn native_html_title(units: &[u16]) -> Option<Vec<u16>> {
    if !raw_starts_with_html(units) {
        return None;
    }
    let open = "<title>".encode_utf16().collect::<Vec<_>>();
    let close = "</title>".encode_utf16().collect::<Vec<_>>();
    let Some(title_start) = units
        .windows(open.len())
        .position(|window| window == open)
        .map(|index| index + open.len())
    else {
        return Some(Vec::new());
    };
    let Some(title_end) = units[title_start..]
        .windows(close.len())
        .position(|window| window == close)
        .map(|index| index + title_start)
    else {
        return Some(Vec::new());
    };
    if units[title_start..title_end].contains(&(b'<' as u16)) {
        return Some(Vec::new());
    }
    Some(trim_ecmascript_whitespace(&units[title_start..title_end]))
}

fn raw_starts_with_html(units: &[u16]) -> bool {
    contains_ascii_units(units, "<!DOCTYPE html") || contains_ascii_units(units, "<html")
}

fn raw_matches_native_html_fallback(units: &[u16]) -> bool {
    let mut position = 0;
    if units
        .first()
        .is_some_and(|unit| (b'0' as u16..=b'9' as u16).contains(unit))
    {
        while units
            .get(position)
            .is_some_and(|unit| (b'0' as u16..=b'9' as u16).contains(unit))
        {
            position += 1;
        }
        if units.get(position) != Some(&(b' ' as u16)) {
            return false;
        }
        position += 1;
    }
    while units
        .get(position)
        .is_some_and(|unit| is_ecmascript_trim_unit(*unit))
    {
        position += 1;
    }
    units.get(position) == Some(&(b'<' as u16))
}

fn native_html_status_fallback(status: Option<u16>) -> Vec<u16> {
    let Some(status) = status.filter(|status| *status != 0) else {
        return "API error (status unknown)".encode_utf16().collect();
    };
    let phrase = match status {
        400 => Some("Bad Request"),
        401 => Some("Unauthorized"),
        402 => Some("Payment Required"),
        403 => Some("Forbidden"),
        404 => Some("Not Found"),
        405 => Some("Method Not Allowed"),
        406 => Some("Not Acceptable"),
        407 => Some("Proxy Authentication Required"),
        408 => Some("Request Timeout"),
        409 => Some("Conflict"),
        410 => Some("Gone"),
        411 => Some("Length Required"),
        412 => Some("Precondition Failed"),
        413 => Some("Payload Too Large"),
        414 => Some("URI Too Long"),
        415 => Some("Unsupported Media Type"),
        416 => Some("Range Not Satisfiable"),
        417 => Some("Expectation Failed"),
        418 => Some("I'm a Teapot"),
        421 => Some("Misdirected Request"),
        422 => Some("Unprocessable Entity"),
        423 => Some("Locked"),
        424 => Some("Failed Dependency"),
        425 => Some("Too Early"),
        426 => Some("Upgrade Required"),
        428 => Some("Precondition Required"),
        429 => Some("Too Many Requests"),
        431 => Some("Request Header Fields Too Large"),
        451 => Some("Unavailable For Legal Reasons"),
        500 => Some("Internal Server Error"),
        501 => Some("Not Implemented"),
        502 => Some("Bad Gateway"),
        503 => Some("Service Unavailable"),
        504 => Some("Gateway Timeout"),
        505 => Some("HTTP Version Not Supported"),
        506 => Some("Variant Also Negotiates"),
        507 => Some("Insufficient Storage"),
        508 => Some("Loop Detected"),
        509 => Some("Bandwidth Limit Exceeded"),
        510 => Some("Not Extended"),
        511 => Some("Network Authentication Required"),
        _ => None,
    };
    match phrase {
        Some(phrase) => format!("{status} {phrase}").encode_utf16().collect(),
        None => status.to_string().encode_utf16().collect(),
    }
}

fn trim_ecmascript_end(units: &[u16]) -> Vec<u16> {
    let end = units
        .iter()
        .rposition(|unit| !is_ecmascript_trim_unit(*unit))
        .map_or(0, |index| index + 1);
    units[..end].to_vec()
}

fn projection_string(units: Vec<u16>) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("display is derived from exact UTF-16 code units")
}

fn native_unexpected_error_message(error: &LlmError) -> (Vec<u16>, bool) {
    if let Some(text) = error.provider_error_text() {
        return (text.utf16_code_units().to_vec(), true);
    }
    let source_message = match error.classification_source() {
        LlmError::InvalidRequest { message }
        | LlmError::Transport { message }
        | LlmError::TransportTimeout { message }
        | LlmError::ProviderTimeout { message, .. }
        | LlmError::StreamInterrupted { message } => Some(message),
        _ => None,
    };
    if let Some(message) = source_message {
        return (message.encode_utf16().collect(), true);
    }
    let display = llm_runtime::error_display_source(error)
        .string_units("")
        .expect("error_display_source returns a root string projection");
    (display, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_llm_client::protocol::{
        native_static_model_suggestion_defaults, JsonPayload, NativeAnthropicErrorFacts,
        NativeModelObservation, NativeModelSuggestionFacts, ProtocolFamily, ProviderErrorText,
        ProviderId,
    };
    use tokio::sync::oneshot;

    fn model(units: &[u16]) -> Utf16JsonProjection {
        Utf16JsonProjection::root_string(String::from_utf16_lossy(units), units.to_vec()).unwrap()
    }

    fn native_suggestion_facts(provider: ModelProviderKind) -> NativeModelSuggestionFacts {
        match super::native_suggestion_provider(provider) {
            NativeModelObservation::Observed(provider) => NativeModelSuggestionFacts {
                effective_model_by_catalog_id: NativeModelObservation::Observed(
                    native_static_model_suggestion_defaults(provider),
                ),
                anthropic_default_opus_model: NativeModelObservation::Observed(None),
            },
            NativeModelObservation::Unobserved => NativeModelSuggestionFacts::unobserved(),
        }
    }

    fn facts(provider: ModelProviderKind) -> NativeModelValidationFacts {
        NativeModelValidationFacts {
            policy_allowed: true,
            force_server_probe: false,
            server_probe: true,
            custom_model_option_matches: false,
            picker_model_option_is_valid: NativeModelObservation::Unobserved,
            provider,
            first_party_base_url: true,
            suggestion_facts: native_suggestion_facts(provider),
        }
    }

    fn classify(model: &TrimmedModelSource, error: &LlmError) -> NativeModelValidationFailure {
        let provider = ModelProviderKind::Bedrock;
        let suggestion_facts = native_suggestion_facts(provider);
        super::classify_native_probe_error(
            model,
            error,
            super::native_suggestion_provider(provider),
            &suggestion_facts,
        )
    }

    fn test_probe_payload(model: TrimmedModelSource) -> NativeModelProbePayload {
        let provider = ModelProviderKind::Bedrock;
        NativeModelProbePayload::new(
            model,
            super::native_suggestion_provider(provider),
            native_suggestion_facts(provider),
        )
    }

    fn native_provider_error(status: u16, body: &str, source: LlmError) -> LlmError {
        native_provider_error_with_headers(status, body, source, None, None)
    }

    fn native_provider_error_with_headers(
        status: u16,
        body: &str,
        source: LlmError,
        representative_claim: Option<&str>,
        overage_disabled_reason: Option<&str>,
    ) -> LlmError {
        let body_units = body.encode_utf16().collect::<Vec<_>>();
        let body_projection = Utf16JsonProjection::parse_utf16_json(&body_units).ok();
        let selected_message = body_projection
            .as_ref()
            .and_then(|projection| projection.string_units("/message"))
            .filter(|units| !units.is_empty())
            .unwrap_or_else(|| body.encode_utf16().collect());
        let mut units = format!("{status} ").encode_utf16().collect::<Vec<_>>();
        units.extend(selected_message);
        let display = String::from_utf16_lossy(&units);
        LlmError::ProviderErrorText {
            source: Box::new(source),
            text: ProviderErrorText::new(display, units).unwrap(),
            native_anthropic_facts: Some(NativeAnthropicErrorFacts {
                provider_id: ProviderId::from("anthropic"),
                protocol: ProtocolFamily::AnthropicMessages,
                request_authority: None,
                thinking_resumption_refused: false,
                thinking_resumption_beta_requested: None,
                thinking_resumption_beta_refused: false,
                native_thinking_binding_controls_beta_requested: None,
                native_thinking_binding_controls_error_evidence: None,
                native_hq_model_source: None,
                native_hq_model_provider_family: None,
                native_hq_known_error_evidence: None,
                native_hq_hrn15_evidence: None,
                native_thinking_type_rejection: None,
                response: Some(lingxi_llm_client::protocol::NativeAnthropicErrorResponse {
                    body: lingxi_llm_client::protocol::JsonPayload::parse(body).ok(),
                    thinking_prefix_mismatch: None,
                    api_error_name: None,
                    amzn_error_type: None,
                    unified_representative_claim: representative_claim.map(str::to_owned),
                    unified_overage_disabled_reason: overage_disabled_reason.map(str::to_owned),
                }),
            }),
        }
    }

    #[test]
    fn policy_gate_precedes_alias_and_cached_unknown_fast_paths() {
        let cache = NativeModelValidationCache::default();
        let unknown = TrimmedModelSource::from_projection(&model(&[b'x' as u16])).unwrap();
        cache.insert(unknown.cache_key());

        let mut denied = facts(ModelProviderKind::Vertex);
        denied.policy_allowed = false;
        assert!(matches!(
            prepare_native_model_validation(&model(&[b'x' as u16]), denied, &cache).unwrap(),
            NativeModelValidationDecision::Rejected(NativeModelValidationFailure {
                kind: NativeModelValidationFailureKind::NotAllowed,
                ..
            })
        ));
        assert!(matches!(
            prepare_native_model_validation(
                &model(&"SONNET".encode_utf16().collect::<Vec<_>>()),
                facts(ModelProviderKind::Vertex),
                &cache,
            )
            .unwrap(),
            NativeModelValidationDecision::Accepted {
                fast_path: ModelValidationFastPath::KnownAlias,
                ..
            }
        ));
        assert!(matches!(
            prepare_native_model_validation(
                &model(&[b'x' as u16]),
                facts(ModelProviderKind::Vertex),
                &cache
            )
            .unwrap(),
            NativeModelValidationDecision::Accepted {
                fast_path: ModelValidationFastPath::PreviouslyValidated,
                ..
            }
        ));
    }

    #[test]
    fn picker_fastpath_requires_an_observed_positive_source_result() {
        let cache = NativeModelValidationCache::default();
        let source = model(&"custom-model".encode_utf16().collect::<Vec<_>>());

        let mut unobserved = facts(ModelProviderKind::FirstParty);
        unobserved.picker_model_option_is_valid = NativeModelObservation::Unobserved;
        assert!(matches!(
            prepare_native_model_validation(&source, unobserved, &cache).unwrap(),
            NativeModelValidationDecision::ProbeRequired(_)
        ));

        let mut observed_no_match = facts(ModelProviderKind::FirstParty);
        observed_no_match.picker_model_option_is_valid = NativeModelObservation::Observed(false);
        assert!(matches!(
            prepare_native_model_validation(&source, observed_no_match, &cache).unwrap(),
            NativeModelValidationDecision::ProbeRequired(_)
        ));

        let mut observed_match = facts(ModelProviderKind::FirstParty);
        observed_match.picker_model_option_is_valid = NativeModelObservation::Observed(true);
        assert!(matches!(
            prepare_native_model_validation(&source, observed_match, &cache).unwrap(),
            NativeModelValidationDecision::Accepted {
                fast_path: ModelValidationFastPath::PickerOption,
                ..
            }
        ));
    }

    fn host_validation_context(
        picker_options: HostModelObservation<Option<Vec<Utf16JsonProjection>>>,
        picker_eligibility: HostModelObservation<bool>,
    ) -> NativeModelValidationContext {
        NativeModelValidationContext {
            effective_model_by_catalog_id: HostModelObservation::Unobserved,
            picker_options,
            picker_eligibility,
            anthropic_default_opus_model: HostModelObservation::Unobserved,
        }
    }

    #[test]
    fn picker_fastpath_requires_observed_options_exact_trim_match_and_yfo() {
        let candidate = model(&" custom-model ".encode_utf16().collect::<Vec<_>>());
        let unobserved = host_validation_context(
            HostModelObservation::Unobserved,
            HostModelObservation::Observed(true),
        );
        assert_eq!(
            native_picker_model_option_is_valid(&candidate, &unobserved),
            NativeModelObservation::Unobserved
        );

        let no_picker = host_validation_context(
            HostModelObservation::Observed(None),
            HostModelObservation::Observed(true),
        );
        assert_eq!(
            native_picker_model_option_is_valid(&candidate, &no_picker),
            NativeModelObservation::Observed(false)
        );

        let display_label_only = host_validation_context(
            HostModelObservation::Observed(Some(vec![Utf16JsonProjection::from(
                "Friendly Claude",
            )])),
            HostModelObservation::Observed(true),
        );
        assert_eq!(
            native_picker_model_option_is_valid(&candidate, &display_label_only),
            NativeModelObservation::Observed(false)
        );

        let exact_option = Utf16JsonProjection::from("  custom-model  ");
        let eligible = host_validation_context(
            HostModelObservation::Observed(Some(vec![exact_option.clone()])),
            HostModelObservation::Observed(true),
        );
        assert_eq!(
            native_picker_model_option_is_valid(&candidate, &eligible),
            NativeModelObservation::Observed(true)
        );

        let eligibility_unobserved = host_validation_context(
            HostModelObservation::Observed(Some(vec![exact_option.clone()])),
            HostModelObservation::Unobserved,
        );
        assert_eq!(
            native_picker_model_option_is_valid(&candidate, &eligibility_unobserved),
            NativeModelObservation::Unobserved
        );

        let ineligible = host_validation_context(
            HostModelObservation::Observed(Some(vec![exact_option])),
            HostModelObservation::Observed(false),
        );
        assert_eq!(
            native_picker_model_option_is_valid(&candidate, &ineligible),
            NativeModelObservation::Observed(false)
        );

        let lone_units = vec![0xd800];
        let lone_candidate = Utf16JsonProjection::root_string(
            String::from_utf16_lossy(&lone_units),
            lone_units.clone(),
        )
        .expect("source projection keeps an unpaired code unit");
        let lone_option =
            Utf16JsonProjection::root_string(String::from_utf16_lossy(&lone_units), lone_units)
                .expect("picker option keeps an unpaired code unit");
        let exact_lone_option = host_validation_context(
            HostModelObservation::Observed(Some(vec![lone_option])),
            HostModelObservation::Observed(true),
        );
        assert_eq!(
            native_picker_model_option_is_valid(&lone_candidate, &exact_lone_option),
            NativeModelObservation::Observed(true)
        );
    }

    #[test]
    fn suggestion_context_conversion_preserves_empty_opus_source_and_utf16_catalog_values() {
        let lone_surrogate =
            Utf16JsonProjection::root_string(String::from_utf16_lossy(&[0xd800]), vec![0xd800])
                .expect("root string projection");
        let mut defaults = std::collections::BTreeMap::new();
        defaults.insert("opus55".to_owned(), lone_surrogate);
        let context = NativeModelValidationContext {
            effective_model_by_catalog_id: HostModelObservation::Observed(defaults),
            picker_options: HostModelObservation::Observed(None),
            picker_eligibility: HostModelObservation::Unobserved,
            anthropic_default_opus_model: HostModelObservation::Observed(Some(
                Utf16JsonProjection::from(""),
            )),
        };
        let facts = native_model_suggestion_facts(&context);
        assert!(matches!(
            facts.effective_model_by_catalog_id,
            NativeModelObservation::Observed(models)
                if models.get("opus55").is_some_and(|model| model.root_string_code_units() == Some(vec![0xd800]))
        ));
        assert!(matches!(
            facts.anthropic_default_opus_model,
            NativeModelObservation::Observed(Some(model))
                if model.root_string_code_units() == Some(Vec::new())
        ));
    }

    #[test]
    fn cache_key_is_exact_trimmed_utf16_not_lossy_display_or_case_folded() {
        let cache = NativeModelValidationCache::default();
        let high = TrimmedModelSource::from_projection(&model(&[0xd800])).unwrap();
        let low = TrimmedModelSource::from_projection(&model(&[0xdc00])).unwrap();
        let upper = TrimmedModelSource::from_projection(&model(&[b'A' as u16])).unwrap();
        let lower = TrimmedModelSource::from_projection(&model(&[b'a' as u16])).unwrap();
        cache.insert(high.cache_key());
        cache.insert(upper.cache_key());
        assert!(cache.contains(high.cache_key()));
        assert!(!cache.contains(low.cache_key()));
        assert!(cache.contains(upper.cache_key()));
        assert!(!cache.contains(lower.cache_key()));
        cache.clear();
        assert!(!cache.contains(high.cache_key()));
        assert!(!cache.contains(upper.cache_key()));
    }

    #[test]
    fn aliases_run_before_format_gate_but_only_in_source_known_alias_set() {
        let cache = NativeModelValidationCache::default();
        assert!(matches!(
            prepare_native_model_validation(
                &model(&"SONNET[1M]".encode_utf16().collect::<Vec<_>>()),
                facts(ModelProviderKind::Bedrock),
                &cache,
            )
            .unwrap(),
            NativeModelValidationDecision::Accepted {
                fast_path: ModelValidationFastPath::KnownAlias,
                ..
            }
        ));
        let unknown_control = model(&[b'x' as u16, 0x200b]);
        assert!(matches!(
            prepare_native_model_validation(
                &unknown_control,
                facts(ModelProviderKind::Bedrock),
                &cache,
            )
            .unwrap(),
            NativeModelValidationDecision::Rejected(NativeModelValidationFailure {
                kind: NativeModelValidationFailureKind::NotFound,
                ..
            })
        ));

        // Native checks its exact validated-name set before the format regex,
        // but only after the caller's policy gate.
        cache.insert(&[b'x' as u16, 0x200b]);
        assert!(matches!(
            prepare_native_model_validation(
                &unknown_control,
                facts(ModelProviderKind::Bedrock),
                &cache,
            )
            .unwrap(),
            NativeModelValidationDecision::Accepted {
                fast_path: ModelValidationFastPath::PreviouslyValidated,
                ..
            }
        ));
    }

    #[test]
    fn endpoint_format_exception_is_only_gateway_or_nonfirstparty_firstparty_base() {
        let cache = NativeModelValidationCache::default();
        let value = model(&[b'x' as u16, 0x200b]);
        assert!(matches!(
            prepare_native_model_validation(&value, facts(ModelProviderKind::Gateway), &cache)
                .unwrap(),
            NativeModelValidationDecision::ProbeRequired(_)
        ));
        let mut custom_first_party = facts(ModelProviderKind::FirstParty);
        custom_first_party.first_party_base_url = false;
        assert!(matches!(
            prepare_native_model_validation(&value, custom_first_party, &cache).unwrap(),
            NativeModelValidationDecision::ProbeRequired(_)
        ));
        assert!(matches!(
            prepare_native_model_validation(&value, facts(ModelProviderKind::FirstParty), &cache)
                .unwrap(),
            NativeModelValidationDecision::Rejected(NativeModelValidationFailure {
                kind: NativeModelValidationFailureKind::NotFound,
                ..
            })
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn wait_timeout_keeps_probe_owned_and_late_success_populates_cache() {
        let cache = NativeModelValidationCache::default();
        let trimmed = TrimmedModelSource::from_projection(&model(&[0xd800, b'x' as u16])).unwrap();
        let payload = test_probe_payload(trimmed.clone());
        let (release, wait) = oneshot::channel::<()>();
        let mut task = cache.spawn_probe(&payload, async move {
            wait.await.map_err(|error| LlmError::Transport {
                message: error.to_string(),
            })?;
            Ok(())
        });

        tokio::task::yield_now().await;
        tokio::time::advance(NATIVE_MODEL_VALIDATION_WAIT).await;
        assert_eq!(task.wait().await, NativeModelProbeWait::TimedOutDetached);
        assert!(!task.is_finished());
        assert!(!cache.contains(trimmed.cache_key()));

        release.send(()).unwrap();
        tokio::task::yield_now().await;
        assert!(task.is_finished());
        assert_eq!(
            task.reap_if_finished().await,
            Some(NativeModelProbeWait::Validated)
        );
        assert!(cache.contains(trimmed.cache_key()));
    }

    #[tokio::test(start_paused = true)]
    async fn explicit_deadline_aborts_and_reaps_without_caching() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let cache = NativeModelValidationCache::default();
        let trimmed = TrimmedModelSource::from_projection(&model(&[b'x' as u16])).unwrap();
        let payload = test_probe_payload(trimmed.clone());
        let dropped = Arc::new(AtomicBool::new(false));
        let dropped_probe = dropped.clone();
        let mut task = cache.spawn_probe(&payload, async move {
            struct MarkDropped(Arc<AtomicBool>);
            impl Drop for MarkDropped {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::SeqCst);
                }
            }
            let _mark = MarkDropped(dropped_probe);
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(())
        });

        tokio::task::yield_now().await;
        let waiter = tokio::spawn(async move {
            let result = task.abort_and_join(Duration::from_secs(5), 3, || 3).await;
            (task, result)
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(5)).await;
        let (task, result) = waiter.await.unwrap();
        assert_eq!(result, NativeModelProbeWait::TimedOutAborted);
        assert!(task.is_finished());
        assert!(dropped.load(Ordering::SeqCst));
        assert!(!cache.contains(trimmed.cache_key()));
    }

    #[tokio::test(start_paused = true)]
    async fn account_generation_change_discards_completed_probe_cache_entry() {
        let cache = NativeModelValidationCache::default();
        let trimmed = TrimmedModelSource::from_projection(&model(&[b'x' as u16])).unwrap();
        let payload = test_probe_payload(trimmed.clone());
        let mut task = cache.spawn_probe(&payload, async { Ok(()) });

        tokio::task::yield_now().await;
        assert_eq!(
            task.abort_and_join(Duration::from_secs(5), 7, || 8).await,
            NativeModelProbeWait::AccountGenerationChanged
        );
        assert!(task.is_finished());
        assert!(!cache.contains(trimmed.cache_key()));
    }

    #[test]
    fn native_api_400_uses_hue_message_and_preserves_unpaired_utf16() {
        let body =
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad \ud800."}}"#;
        let error = native_provider_error(
            400,
            body,
            LlmError::InvalidRequest {
                message: "provider rejected model".into(),
            },
        );
        let source_model = TrimmedModelSource::from_projection(&model(&[b'x' as u16])).unwrap();
        let classified = classify(&source_model, &error);
        let units = classified.message.string_units("").unwrap();
        let mut expected = "API error: 400 bad ".encode_utf16().collect::<Vec<_>>();
        expected.push(0xd800);
        expected.extend(" · model not changed".encode_utf16());
        assert_eq!(units, expected);
        assert_eq!(classified.http_status, Some(400));
        assert!(!classified.retryable);
        assert_eq!(classified.session_mode_refused, None);
        assert!(classified.source_gaps.is_empty());
    }

    #[test]
    fn native_not_found_uses_current_post_override_model_config_suggestion() {
        let source_model = TrimmedModelSource::from_projection(&model(
            &"claude-sonnet-5".encode_utf16().collect::<Vec<_>>(),
        ))
        .unwrap();
        let error = native_provider_error(
            404,
            r#"{"type":"error","error":{"type":"not_found_error","message":"model: claude-sonnet-5"}}"#,
            LlmError::ModelUnavailable,
        );
        let provider = ModelProviderKind::Bedrock;
        let mut suggestion_facts = native_suggestion_facts(provider);
        let NativeModelObservation::Observed(effective) =
            &mut suggestion_facts.effective_model_by_catalog_id
        else {
            panic!("test host config is observed");
        };
        effective.insert(
            "claude-sonnet-4-6".to_owned(),
            JsonPayload::string("us.host-current-sonnet-4-6-profile"),
        );
        let classified = super::classify_native_probe_error(
            &source_model,
            &error,
            super::native_suggestion_provider(provider),
            &suggestion_facts,
        );
        assert_eq!(
            classified.message.string_at(""),
            Some("Model 'claude-sonnet-5' not found. Try 'us.host-current-sonnet-4-6-profile' instead")
        );
        assert_eq!(classified.server_refused_model, Some(true));
        assert!(classified.source_gaps.is_empty());
    }

    #[test]
    fn native_not_found_and_permission_denials_set_server_refused_only_on_source_match() {
        let model = TrimmedModelSource::from_projection(&model(
            &"custom-model".encode_utf16().collect::<Vec<_>>(),
        ))
        .unwrap();
        let not_found = native_provider_error(
            404,
            r#"{"type":"error","error":{"type":"not_found_error","message":"model: custom-model"}}"#,
            LlmError::ModelUnavailable,
        );
        let classified = classify(&model, &not_found);
        assert_eq!(classified.kind, NativeModelValidationFailureKind::NotFound);
        assert_eq!(classified.server_refused_model, Some(true));

        let permission = native_provider_error(
            403,
            r#"{"type":"error","error":{"type":"permission_error","message":"model: custom-model"}}"#,
            LlmError::PermissionDenied {
                message: "provider denied model".into(),
            },
        );
        let classified = classify(&model, &permission);
        assert_eq!(
            classified.kind,
            NativeModelValidationFailureKind::PermissionDenied
        );
        assert!(classified.permission_denied);
        assert_eq!(classified.server_refused_model, Some(true));

        let unrelated_404 = native_provider_error(
            404,
            r#"{"type":"error","error":{"type":"not_found_error","message":"resource missing"}}"#,
            LlmError::ModelUnavailable,
        );
        let unrelated_classified = classify(&model, &unrelated_404);
        assert_eq!(
            unrelated_classified.kind,
            NativeModelValidationFailureKind::NotFound
        );
        assert_eq!(
            unrelated_classified.server_refused_model, None,
            "qRe requires the model: marker in addition to 404/not_found_error"
        );
    }

    #[test]
    fn native_credits_and_server_refusal_use_full_body_and_exact_rate_limit_headers() {
        let source_model = TrimmedModelSource::from_projection(&model(
            &"custom-model".encode_utf16().collect::<Vec<_>>(),
        ))
        .unwrap();
        let error = native_provider_error(
            429,
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"billing limit","details":{"error_code":"credits_required","disabled_reason":"out_of_credits"}}}"#,
            LlmError::RateLimited {
                retry_after: None,
                scope: None,
            },
        );
        let classified = classify(&source_model, &error);
        assert_eq!(classified.credits_required, Some(true));
        assert_eq!(classified.server_refused_model, Some(true));
        assert!(classified.source_gaps.is_empty());
        assert!(classified.retryable);

        let body_decides_false = native_provider_error(
            429,
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"billing limit","details":{"error_code":"credits_required","disabled_reason":"unknown"}}}"#,
            LlmError::RateLimited {
                retry_after: None,
                scope: None,
            },
        );
        let classified = classify(&source_model, &body_decides_false);
        assert_eq!(classified.credits_required, Some(true));
        assert_eq!(classified.server_refused_model, None);
        assert!(classified.source_gaps.is_empty());

        let representative_claim = native_provider_error_with_headers(
            429,
            r#"{"message":"billing limit","error":{"type":"rate_limit_error","message":"billing limit","details":{"error_code":"credits_required","disabled_reason":"out_of_credits"}}}"#,
            LlmError::RateLimited {
                retry_after: None,
                scope: None,
            },
            Some("claimed"),
            None,
        );
        let classified = classify(&source_model, &representative_claim);
        assert_eq!(classified.credits_required, Some(true));
        assert_eq!(classified.server_refused_model, None);

        let disabled_reason_header = native_provider_error_with_headers(
            429,
            r#"{"message":"billing limit","error":{"type":"rate_limit_error","message":"billing limit","details":{"error_code":"credits_required","disabled_reason":"out_of_credits"}}}"#,
            LlmError::RateLimited {
                retry_after: None,
                scope: None,
            },
            None,
            Some("unknown"),
        );
        let classified = classify(&source_model, &disabled_reason_header);
        assert_eq!(classified.credits_required, Some(true));
        assert_eq!(classified.server_refused_model, None);
    }
}
