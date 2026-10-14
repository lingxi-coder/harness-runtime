//! Detached helper draft for the Native 2.1.295 `bQn` model-validation boundary.
//!
//! This module does not perform policy, model-catalog, credential, or network
//! work. The current selection/resolver owner supplies those decisions and an
//! optional probe closure. The helper preserves the source model as exact
//! UTF-16, applies `bQn`'s local ordering, and owns only the source-validated
//! model-name cache/probe wait lifecycle.

use agent::model_resolution::ModelProviderKind;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
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
/// boolean means Native `yfo(model)` succeeded *and* an option's trimmed name
/// exactly matched this trimmed source name. `first_party_base_url` is the
/// source result `bi() || hostname === anthropic.com || hostname ends in
/// .anthropic.com || hostname appears in the imported first-party host list`;
/// endpoint parsing/host-list ownership stays with the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeModelValidationFacts {
    pub policy_allowed: bool,
    pub force_server_probe: bool,
    pub server_probe: bool,
    pub custom_model_option_matches: bool,
    pub picker_model_option_is_valid: bool,
    pub provider: ModelProviderKind,
    pub first_party_base_url: bool,
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

/// Source-shaped payload for `aF({model, max_tokens:1, maxRetries:0,
/// querySource:"model_validation", resendWithoutRefusedContext1m:true,
/// messages:[{role:"user",content:[{type:"text",text:"Hi",
/// cache_control:{type:"ephemeral"}}]}]})`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeModelProbePayload {
    pub model: TrimmedModelSource,
    pub max_tokens: u8,
    pub max_retries: u8,
    pub query_source: &'static str,
    pub resend_without_refused_context_1m: bool,
    pub user_text: &'static str,
    pub ephemeral_cache_control: bool,
}

impl NativeModelProbePayload {
    fn new(model: TrimmedModelSource) -> Self {
        Self {
            model,
            max_tokens: 1,
            max_retries: 0,
            query_source: "model_validation",
            resend_without_refused_context_1m: true,
            user_text: "Hi",
            ephemeral_cache_control: true,
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
        if facts.picker_model_option_is_valid {
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
            NativeModelValidationFailure::not_found(&model),
        ));
    }

    if !facts.server_probe {
        return Ok(accepted(
            model,
            ModelValidationFastPath::ServerProbeDisabled,
        ));
    }

    Ok(NativeModelValidationDecision::ProbeRequired(
        NativeModelProbePayload::new(model),
    ))
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

    /// Spawn the host-owned probe. Only successful eventual completion updates
    /// the validated-name cache, even when the caller's 15s wait has expired.
    /// Dropping `NativeModelProbeTask` detaches the Tokio task (JoinHandle's
    /// native behavior); only `abort_for_host_shutdown` cancels it.
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
        let handle = tokio::spawn(async move {
            let result = probe.await;
            if result.is_ok() {
                cache.insert(&model_key);
            }
            result
        });
        NativeModelProbeTask {
            model: payload.model.clone(),
            handle: Some(handle),
        }
    }
}

/// A still-owned validation operation. A wait timeout leaves the task alive;
/// callers can retain this handle, check it later, or explicitly abort during
/// host shutdown.
pub struct NativeModelProbeTask {
    model: TrimmedModelSource,
    handle: Option<JoinHandle<Result<(), LlmError>>>,
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
    /// Await the source 15s budget. `TimedOut` does not abort the probe and the
    /// task retains its handle for a later completion check.
    pub async fn wait(&mut self) -> NativeModelProbeWait {
        let Some(handle) = self.handle.as_mut() else {
            return NativeModelProbeWait::AlreadyReaped;
        };
        match tokio::time::timeout(NATIVE_MODEL_VALIDATION_WAIT, handle).await {
            Err(_) => NativeModelProbeWait::TimedOut,
            Ok(joined) => {
                self.handle.take();
                map_probe_join(self.model.clone(), joined)
            }
        }
    }

    /// Reap a completed operation without imposing another timeout. Returns
    /// `None` while the task remains active.
    pub async fn reap_if_finished(&mut self) -> Option<NativeModelProbeWait> {
        if !self.is_finished() {
            return None;
        }
        let handle = self.handle.take()?;
        Some(map_probe_join(self.model.clone(), handle.await))
    }

    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Cancellation is explicit and reserved for the host's shutdown path.
    /// The ordinary validation wait timeout never calls this method.
    pub fn abort_for_host_shutdown(&mut self) -> bool {
        let Some(handle) = self.handle.take() else {
            return false;
        };
        handle.abort();
        true
    }
}

fn map_probe_join(
    model: TrimmedModelSource,
    joined: Result<Result<(), LlmError>, tokio::task::JoinError>,
) -> NativeModelProbeWait {
    match joined {
        Ok(Ok(())) => NativeModelProbeWait::Validated,
        Ok(Err(error)) => {
            NativeModelProbeWait::Rejected(classify_native_probe_error(&model, &error))
        }
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
    TimedOut,
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
    /// Native `A` can derive these from provider headers/body flags, but the
    /// current `LlmError` carrier does not expose those facts. `None` means
    /// unknown; do not treat it as an affirmative or negative decision.
    pub credits_required: Option<bool>,
    pub server_refused_model: Option<bool>,
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
        }
    }

    fn not_found(model: &TrimmedModelSource) -> Self {
        let mut units = "Model '".encode_utf16().collect::<Vec<_>>();
        units.extend_from_slice(model.code_units());
        units.extend("' not found".encode_utf16());
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
        }
    }
}

/// Map known host SDK error facts to Native `A`'s model-validation categories.
/// Existing SDK source helpers supply the display projection and status; this
/// module does not inspect headers, call the network, or invent server-refusal
/// / credits-required facts that `LlmError` does not expose.
#[must_use]
pub fn classify_native_probe_error(
    model: &TrimmedModelSource,
    error: &LlmError,
) -> NativeModelValidationFailure {
    use llm_runtime::LlmError as E;

    let source_error = error.classification_source();
    let status = error.http_status();
    match source_error {
        E::ModelUnavailable => {
            let mut failure = NativeModelValidationFailure::not_found(model);
            failure.http_status = status;
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
        },
        E::PermissionDenied { .. } => {
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
                server_refused_model: None,
            }
        }
        _ if error.provider_error_text().is_some() => {
            let source_message = llm_runtime::error_display_source(error);
            let source_units = source_message
                .string_units("")
                .expect("error_display_source returns a root string projection");
            let message = native_api_error_message(source_units);
            NativeModelValidationFailure {
                kind: NativeModelValidationFailureKind::Api,
                message,
                retryable: status
                    .is_none_or(|status| status == 408 || status == 429 || status >= 500),
                not_found: false,
                not_allowed: false,
                permission_denied: false,
                authentication_failed: false,
                http_status: status,
                credits_required: None,
                server_refused_model: None,
            }
        }
        _ => {
            let source_message = llm_runtime::error_display_source(error);
            let detail = source_message
                .string_units("")
                .expect("error_display_source returns a root string projection");
            let mut units = "Unable to validate model: "
                .encode_utf16()
                .collect::<Vec<_>>();
            units.extend(detail);
            NativeModelValidationFailure {
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
            }
        }
    }
}

fn native_api_error_message(mut detail: Vec<u16>) -> Utf16JsonProjection {
    while detail
        .last()
        .is_some_and(|unit| matches!(*unit, 0x0021 | 0x002e | 0x003f | 0x2026))
    {
        detail.pop();
    }

    // `error_display_source` may already have chosen one of its `API error …`
    // renderings. Do not add a second API/status prefix in that case.
    let already_api_rendered = detail.starts_with(&"API error".encode_utf16().collect::<Vec<_>>());
    let mut units = if already_api_rendered {
        detail
    } else {
        let mut prefixed = "API error: ".encode_utf16().collect::<Vec<_>>();
        prefixed.extend(detail);
        prefixed
    };
    units.extend(" · model not changed".encode_utf16());
    projection_string(units)
}

fn projection_string(units: Vec<u16>) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("display is derived from exact UTF-16 code units")
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_llm_client::protocol::ProviderErrorText;
    use tokio::sync::oneshot;

    fn model(units: &[u16]) -> Utf16JsonProjection {
        Utf16JsonProjection::root_string(String::from_utf16_lossy(units), units.to_vec()).unwrap()
    }

    fn facts(provider: ModelProviderKind) -> NativeModelValidationFacts {
        NativeModelValidationFacts {
            policy_allowed: true,
            force_server_probe: false,
            server_probe: true,
            custom_model_option_matches: false,
            picker_model_option_is_valid: false,
            provider,
            first_party_base_url: true,
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
        let payload = NativeModelProbePayload::new(trimmed.clone());
        let (release, wait) = oneshot::channel::<()>();
        let mut task = cache.spawn_probe(&payload, async move {
            wait.await.map_err(|error| LlmError::Transport {
                message: error.to_string(),
            })?;
            Ok(())
        });

        tokio::task::yield_now().await;
        tokio::time::advance(NATIVE_MODEL_VALIDATION_WAIT).await;
        assert_eq!(task.wait().await, NativeModelProbeWait::TimedOut);
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

    #[test]
    fn provider_error_projection_keeps_unpaired_utf16_when_formatted() {
        let display = "429 bad �".to_owned();
        let text = ProviderErrorText::new(
            display,
            vec![
                b'4' as u16,
                b'2' as u16,
                b'9' as u16,
                b' ' as u16,
                b'b' as u16,
                b'a' as u16,
                b'd' as u16,
                b' ' as u16,
                0xd800,
            ],
        )
        .unwrap();
        let error = LlmError::ProviderErrorText {
            source: Box::new(LlmError::InvalidRequest {
                message: "provider rejected model".into(),
            }),
            text,
            native_anthropic_facts: None,
        };
        let source_model = TrimmedModelSource::from_projection(&model(&[b'x' as u16])).unwrap();
        let classified = classify_native_probe_error(&source_model, &error);
        let units = classified.message.string_units("").unwrap();
        assert!(units.windows(9).any(|window| window
            == [
                b'b' as u16,
                b'a' as u16,
                b'd' as u16,
                b' ' as u16,
                0xd800,
                b' ' as u16,
                0x00b7,
                b' ' as u16,
                b'm' as u16
            ]));
        assert_eq!(classified.http_status, Some(429));
        assert!(classified.retryable);
    }
}
