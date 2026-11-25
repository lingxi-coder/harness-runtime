//! Route admission and endpoint identity for Claude compatibility policy.

use crate::dispatch_header::{
    DispatchAttempt, DispatchFailure, DispatchFallback, DispatchHeaderState, DISPATCH_ID_HEADER,
};
use crate::model::telemetry;
use crate::model::user_agent::user_agent;
use crate::service::anthropic::foundry::native_foundry_model_key;
use crate::service::request::{query_source_category, QuerySourceCategory};
use crate::service::ApiService;
use crate::{LlmError, LlmRequest, PromptCacheQuerySource};

const NATIVE_ANTHROPIC_FIRST_PARTY_BASE_URL: &str = "https://api.anthropic.com";

pub(in crate::service) fn native_custom_beta_provider_route(
    provider_id: &crate::ProviderId,
    protocol: crate::ProtocolFamily,
) -> Option<crate::model::thinking_binding_controls::ProviderRoute> {
    use crate::model::thinking_binding_controls::ProviderRoute;

    match (provider_id, protocol) {
        (crate::ProviderId::AnthropicFirstParty, crate::ProtocolFamily::AnthropicMessages) => {
            Some(ProviderRoute::FirstParty)
        }
        (crate::ProviderId::BedrockClaude, crate::ProtocolFamily::BedrockClaude) => {
            Some(ProviderRoute::Bedrock)
        }
        (crate::ProviderId::VertexClaude, crate::ProtocolFamily::VertexClaude) => {
            Some(ProviderRoute::Vertex)
        }
        (crate::ProviderId::FoundryClaude, crate::ProtocolFamily::FoundryClaude) => {
            Some(ProviderRoute::Foundry)
        }
        _ => None,
    }
}

/// Native `tf()` accepts a missing or empty ANTHROPIC_BASE_URL, or a
/// configured URL whose WHATWG-normalized host is exactly api.anthropic.com.
/// Non-default ports remain part of URL.host and therefore do not match.
pub(in crate::service) fn native_tbc_first_party_tf(
    configured_base_url: Option<&std::ffi::OsStr>,
) -> bool {
    let Some(configured_base_url) = configured_base_url else {
        return true;
    };
    let value = configured_base_url.to_string_lossy();
    if value.is_empty() {
        return true;
    }
    url::Url::parse(&value)
        .ok()
        .is_some_and(|url| url.host_str() == Some(FIRST_PARTY_API_HOST) && url.port().is_none())
}

/// Prefer the resolved profile URL because it is the endpoint this request will
/// actually use. The canonical API host is the Native default, so leave it
/// unconfigured when the process env is absent; that preserves the separate
/// unknown `KXt()` branch. The process value is only a fallback when no route
/// profile was available to the service.
pub(in crate::service) fn native_tbc_effective_base_url(
    selected_profile_base_url: Option<&str>,
    process_base_url: Option<&std::ffi::OsStr>,
) -> Option<String> {
    if let Some(base_url) = selected_profile_base_url {
        if native_tbc_first_party_tf(Some(std::ffi::OsStr::new(base_url))) {
            // An explicit canonical env value is equivalent to the selected
            // canonical route and takes Native KXt's defined-URL branch. An
            // env value for another host cannot override the URL this request
            // actually selected.
            if process_base_url.is_some_and(|value| !value.is_empty())
                && process_base_url.is_some_and(|value| native_tbc_first_party_tf(Some(value)))
            {
                return process_base_url.map(|value| value.to_string_lossy().into_owned());
            }
            return None;
        }
        return Some(base_url.to_owned());
    }
    process_base_url.map(|value| value.to_string_lossy().into_owned())
}

/// Native `M.str()` trims the process environment string and returns undefined
/// for an empty result. KXt's early path checks this parsed value, not whether
/// the raw environment variable exists.
pub(in crate::service) fn native_tbc_first_party_base_url_is_defined(
    configured_base_url: Option<&std::ffi::OsStr>,
) -> bool {
    configured_base_url.is_some_and(|value| {
        !value
            .to_string_lossy()
            .trim_matches(native_js_trim_whitespace)
            .is_empty()
    })
}

/// Native `KXt()` returns true immediately when its parsed ANTHROPIC_BASE_URL
/// is defined and `tf()` passed. With no parsed ANTHROPIC_BASE_URL, its
/// remaining `bd()/h3()` and selected-profile checks are not available here,
/// so keep the result unknown instead of treating the canonical default
/// endpoint as account evidence.
pub(in crate::service) fn native_tbc_first_party_kxt(
    parsed_base_url_defined: bool,
    tf: bool,
) -> Option<bool> {
    if !tf {
        Some(false)
    } else if parsed_base_url_defined {
        Some(true)
    } else {
        None
    }
}

/// Native `sl()` is `Pe()==firstParty && (abe() || tf())`. The process env
/// overrides the captured managed setting only when its triBool parser returns
/// a value; empty and invalid values parse as undefined and use the fallback.
pub(in crate::service) fn native_tbc_first_party_assume(
    process_assume: Option<&std::ffi::OsStr>,
    captured_managed_assume: Option<bool>,
) -> Option<bool> {
    process_assume
        .and_then(native_tbc_first_party_assume_tri_bool)
        .or(captured_managed_assume)
}

fn native_tbc_first_party_assume_tri_bool(value: &std::ffi::OsStr) -> Option<bool> {
    let normalized = value
        .to_string_lossy()
        .trim_matches(native_js_trim_whitespace)
        .to_ascii_lowercase();
    match normalized.as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn native_js_trim_whitespace(value: char) -> bool {
    matches!(
        value,
        '\u{0009}'
            | '\u{000A}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

pub(in crate::service) fn native_tbc_first_party_sl(
    tf: bool,
    assume: Option<bool>,
) -> Option<bool> {
    match (tf, assume) {
        (true, _) | (_, Some(true)) => Some(true),
        (false, Some(false)) => Some(false),
        (false, None) => None,
    }
}

pub(in crate::service) fn apply_native_first_party_base_url_assumption(
    facts: &mut crate::model::classifier_model_policy::ClassifierModelRouteFacts,
    assume_first_party_base_url: bool,
) {
    if facts.provider.as_deref() == Some("firstParty") {
        facts.native_base_url |= assume_first_party_base_url;
    }
}

#[cfg(test)]
mod native_first_party_base_url_assumption_tests {
    use super::*;
    use crate::model::classifier_model_policy::ClassifierModelRouteFacts;

    #[test]
    fn explicit_host_assumption_is_limited_to_native_first_party_route() {
        let mut first_party = ClassifierModelRouteFacts {
            provider: Some("firstParty".into()),
            ..Default::default()
        };
        apply_native_first_party_base_url_assumption(&mut first_party, true);
        assert!(first_party.native_base_url);

        let mut bedrock = ClassifierModelRouteFacts {
            provider: Some("bedrock".into()),
            ..Default::default()
        };
        apply_native_first_party_base_url_assumption(&mut bedrock, true);
        assert!(!bedrock.native_base_url);

        let mut already_native = ClassifierModelRouteFacts {
            provider: Some("firstParty".into()),
            native_base_url: true,
            ..Default::default()
        };
        apply_native_first_party_base_url_assumption(&mut already_native, false);
        assert!(already_native.native_base_url);
    }

    #[test]
    fn tbc_tf_uses_native_url_host_and_default_port_normalization() {
        use std::ffi::OsStr;

        assert!(native_tbc_first_party_tf(None));
        assert!(native_tbc_first_party_tf(Some(OsStr::new(""))));
        assert!(native_tbc_first_party_tf(Some(OsStr::new(
            "https://api.anthropic.com"
        ))));
        assert!(native_tbc_first_party_tf(Some(OsStr::new(
            "https://api.anthropic.com:443"
        ))));
        assert!(!native_tbc_first_party_tf(Some(OsStr::new(
            "https://api.anthropic.com:8443"
        ))));
        assert!(!native_tbc_first_party_tf(Some(OsStr::new(
            "https://proxy.example.test"
        ))));
        assert!(!native_tbc_first_party_tf(Some(OsStr::new("   "))));
        assert!(!native_tbc_first_party_tf(Some(OsStr::new("not a URL"))));
    }

    #[test]
    fn tbc_first_party_sl_and_kxt_keep_their_native_separate_gates() {
        use std::ffi::OsStr;

        // An absent env URL makes tf/sl true, but does not resolve KXt's
        // profile/account branch while H has no h3/bd snapshot.
        let default_tf = native_tbc_first_party_tf(None);
        assert_eq!(native_tbc_first_party_sl(default_tf, None), Some(true));
        assert_eq!(native_tbc_first_party_kxt(false, default_tf), None);

        let empty_url = Some(OsStr::new(""));
        let empty_tf = native_tbc_first_party_tf(empty_url);
        assert_eq!(native_tbc_first_party_sl(empty_tf, None), Some(true));
        assert!(!native_tbc_first_party_base_url_is_defined(empty_url));
        assert_eq!(native_tbc_first_party_kxt(false, empty_tf), None);

        let whitespace_url = Some(OsStr::new("   "));
        let whitespace_tf = native_tbc_first_party_tf(whitespace_url);
        assert!(!whitespace_tf);
        assert!(!native_tbc_first_party_base_url_is_defined(whitespace_url));
        assert_eq!(
            native_tbc_first_party_kxt(false, whitespace_tf),
            Some(false)
        );

        // A canonical configured URL takes KXt's source-local true branch.
        let canonical_url = Some(OsStr::new("https://api.anthropic.com"));
        let canonical_tf = native_tbc_first_party_tf(canonical_url);
        assert_eq!(native_tbc_first_party_sl(canonical_tf, None), Some(true));
        assert!(native_tbc_first_party_base_url_is_defined(canonical_url));
        assert_eq!(native_tbc_first_party_kxt(true, canonical_tf), Some(true));

        // An assumption can keep sl true while an invalid/custom tf makes
        // KXt false. The two facts must not be collapsed into one boolean.
        let custom_tf = native_tbc_first_party_tf(Some(OsStr::new("https://proxy.example.test")));
        assert_eq!(native_tbc_first_party_sl(custom_tf, Some(true)), Some(true));
        assert_eq!(native_tbc_first_party_kxt(true, custom_tf), Some(false));
        assert_eq!(
            native_tbc_first_party_sl(custom_tf, Some(false)),
            Some(false)
        );
        assert_eq!(native_tbc_first_party_sl(custom_tf, None), None);
    }

    #[test]
    fn tbc_first_party_assumption_matches_native_tri_bool_and_nullish_fallback() {
        use std::ffi::OsStr;

        for value in ["1", "true", "TRUE", "yes", "On", "\u{FEFF} true \u{2000}"] {
            assert_eq!(
                native_tbc_first_party_assume_tri_bool(OsStr::new(value)),
                Some(true),
                "expected {value:?} to parse as true"
            );
        }
        for value in ["0", "false", "FALSE", "no", "Off", "\u{2000} no \u{FEFF}"] {
            assert_eq!(
                native_tbc_first_party_assume_tri_bool(OsStr::new(value)),
                Some(false),
                "expected {value:?} to parse as false"
            );
        }

        for value in ["", "   ", "maybe", "00", "enabled"] {
            assert_eq!(
                native_tbc_first_party_assume(Some(OsStr::new(value)), Some(true)),
                Some(true),
                "undefined parse should use captured fallback for {value:?}"
            );
            assert_eq!(
                native_tbc_first_party_assume(Some(OsStr::new(value)), Some(false)),
                Some(false),
                "undefined parse should use captured fallback for {value:?}"
            );
        }
        assert_eq!(native_tbc_first_party_assume(None, Some(true)), Some(true));
        assert_eq!(
            native_tbc_first_party_assume(None, Some(false)),
            Some(false)
        );
        assert_eq!(native_tbc_first_party_assume(None, None), None);

        // Parsed env values take precedence over the captured managed setting.
        assert_eq!(
            native_tbc_first_party_assume(Some(OsStr::new("0")), Some(true)),
            Some(false)
        );
        assert_eq!(
            native_tbc_first_party_assume(Some(OsStr::new("yes")), Some(false)),
            Some(true)
        );
    }
}

fn native_js_env_truthy(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|value| !value.is_empty())
}

fn native_bare_flag_before_separator(
    arguments: impl IntoIterator<Item = std::ffi::OsString>,
) -> bool {
    for argument in arguments {
        if argument == std::ffi::OsStr::new("--") {
            break;
        }
        if argument == std::ffi::OsStr::new("--bare") {
            return true;
        }
    }
    false
}

fn native_prompt_cache_bare_mode_for_inputs(
    simple_mode: Option<&std::ffi::OsStr>,
    arguments: impl IntoIterator<Item = std::ffi::OsString>,
) -> bool {
    native_js_env_truthy(simple_mode) || native_bare_flag_before_separator(arguments)
}

pub(in crate::service) fn native_prompt_cache_bare_mode() -> bool {
    native_prompt_cache_bare_mode_for_inputs(
        std::env::var_os(branding::SIMPLE_ENV).as_deref(),
        std::env::args_os(),
    )
}

fn native_mctc_disabled_for_inputs(
    simple_mode: Option<&std::ffi::OsStr>,
    disable_attachments: Option<&std::ffi::OsStr>,
    arguments: impl IntoIterator<Item = std::ffi::OsString>,
) -> bool {
    native_js_env_truthy(disable_attachments)
        || native_prompt_cache_bare_mode_for_inputs(simple_mode, arguments)
}

pub(in crate::service) fn native_mctc_disabled_by_process_mode() -> bool {
    native_mctc_disabled_for_inputs(
        std::env::var_os(branding::SIMPLE_ENV).as_deref(),
        std::env::var_os(branding::DISABLE_ATTACHMENTS_ENV).as_deref(),
        std::env::args_os(),
    )
}

#[cfg(test)]
mod native_simple_and_route_beta_gate_tests {
    use super::*;
    use std::ffi::{OsStr, OsString};

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn native_simple_mode_and_bare_flag_match_source_truthiness_and_separator() {
        assert!(native_mctc_disabled_for_inputs(
            Some(OsStr::new("false")),
            None,
            args(&["lingxi"]),
        ));
        assert!(native_mctc_disabled_for_inputs(
            Some(OsStr::new("0")),
            None,
            args(&["lingxi"]),
        ));
        assert!(native_mctc_disabled_for_inputs(
            None,
            Some(OsStr::new("false")),
            args(&["lingxi"]),
        ));
        assert!(native_prompt_cache_bare_mode_for_inputs(
            None,
            args(&["lingxi", "--bare", "--", "--not-an-option"]),
        ));
        assert!(!native_prompt_cache_bare_mode_for_inputs(
            Some(OsStr::new("")),
            args(&["lingxi", "--", "--bare"]),
        ));
    }

    #[test]
    fn native_tbc_uses_the_selected_profile_endpoint_without_cross_request_state() {
        let profile_a = native_tbc_effective_base_url(
            Some("http://127.0.0.1:31001"),
            Some(OsStr::new("https://env-proxy.example.test")),
        )
        .expect("selected request route has a URL");
        let profile_b = native_tbc_effective_base_url(Some("http://127.0.0.1:31002"), None)
            .expect("selected request route has a URL");
        assert_eq!(profile_a, "http://127.0.0.1:31001");
        assert_eq!(profile_b, "http://127.0.0.1:31002");
        assert!(!native_tbc_first_party_tf(Some(OsStr::new(&profile_a))));
        assert!(!native_tbc_first_party_tf(Some(OsStr::new(&profile_b))));

        // The canonical resolved default keeps the no-env `KXt()` result
        // unknown instead of treating every SDK profile as explicit env input.
        let default_route = native_tbc_effective_base_url(Some("https://api.anthropic.com"), None);
        assert_eq!(default_route, None);
        assert_eq!(
            native_tbc_first_party_kxt(false, native_tbc_first_party_tf(None)),
            None
        );
        let explicit_canonical = native_tbc_effective_base_url(
            Some("https://api.anthropic.com"),
            Some(OsStr::new("https://api.anthropic.com")),
        )
        .expect("explicit canonical env is a defined Native URL");
        assert!(native_tbc_first_party_tf(Some(OsStr::new(
            &explicit_canonical
        ))));
        assert!(native_tbc_first_party_base_url_is_defined(Some(
            OsStr::new(&explicit_canonical)
        )));

        // If no route profile exists, preserve Native's empty and whitespace
        // environment distinctions: empty is the default; whitespace is set
        // but fails URL validation and is not a parsed URL for KXt.
        let empty = native_tbc_effective_base_url(None, Some(OsStr::new(""))).unwrap();
        assert!(native_tbc_first_party_tf(Some(OsStr::new(&empty))));
        assert!(!native_tbc_first_party_base_url_is_defined(Some(
            OsStr::new(&empty)
        )));
        let whitespace = native_tbc_effective_base_url(None, Some(OsStr::new("   "))).unwrap();
        assert!(!native_tbc_first_party_tf(Some(OsStr::new(&whitespace))));
        assert!(!native_tbc_first_party_base_url_is_defined(Some(
            OsStr::new(&whitespace)
        )));
    }
}

pub(in crate::service) fn native_anthropic_unix_socket_enabled() -> bool {
    std::env::var_os("ANTHROPIC_UNIX_SOCKET").is_some_and(|value| !value.is_empty())
}

pub(in crate::service) fn native_request_model_identity(request: &LlmRequest) -> Vec<u16> {
    request
        .input
        .model_source
        .as_ref()
        .and_then(lingxi_llm_client::protocol::JsonPayload::root_string_code_units)
        .unwrap_or_else(|| request.input.model.encode_utf16().collect())
}

pub(in crate::service) fn native_request_model_name_for_option_lookup(
    request: &LlmRequest,
) -> Option<String> {
    if let Some(source) = request.input.model_source.as_ref() {
        let units = source.root_string_code_units()?;
        return String::from_utf16(&units).ok();
    }
    Some(request.input.model.clone())
}

pub(in crate::service) fn native_context_1m_model_key(request: &LlmRequest) -> Vec<u16> {
    // Native `Fn(X.model)` strips only case-insensitive `[1m]` and `[2m]` tags.
    // The source projection preserves model_source UTF-16 units where present.
    native_foundry_model_key(&native_request_model_identity(request))
}

impl DispatchHeaderState {
    pub(in crate::service) fn for_query_source(query_source: Option<&str>) -> Self {
        Self {
            auxiliary: query_source_category(query_source) == Some(QuerySourceCategory::Auxiliary),
            ..Self::default()
        }
    }
}

/// The Native main retry ledger applies only to the exact Claude request
/// routes represented by Host. Compatible Anthropic codecs and unrelated
/// providers retain their existing retry behavior.
pub(in crate::service) fn is_native_main_anthropic_route(
    provider_id: &crate::ProviderId,
    protocol: crate::ProtocolFamily,
) -> bool {
    (provider_id == &crate::ProviderId::AnthropicFirstParty
        && protocol == crate::ProtocolFamily::AnthropicMessages)
        || (provider_id == &crate::ProviderId::BedrockClaude
            && protocol == crate::ProtocolFamily::BedrockClaude)
        || (provider_id == &crate::ProviderId::VertexClaude
            && protocol == crate::ProtocolFamily::VertexClaude)
        || (provider_id == &crate::ProviderId::FoundryClaude
            && protocol == crate::ProtocolFamily::FoundryClaude)
}

pub(in crate::service) fn native_adaptive_provider_default(
    provider_id: &crate::ProviderId,
    protocol: crate::ProtocolFamily,
) -> Option<bool> {
    match (provider_id, protocol) {
        (crate::ProviderId::AnthropicFirstParty, crate::ProtocolFamily::AnthropicMessages)
        | (crate::ProviderId::FoundryClaude, crate::ProtocolFamily::FoundryClaude) => Some(true),
        (crate::ProviderId::BedrockClaude, crate::ProtocolFamily::BedrockClaude)
        | (crate::ProviderId::VertexClaude, crate::ProtocolFamily::VertexClaude) => Some(false),
        _ => None,
    }
}

/// (cc 2.1.219) `S8s` — the dispatch-routing opt-in header name.
/// (cc 2.1.219) `Vtp` — the dispatch-routing opt-in header value.
/// `T1e`'s allow-list — the only host `Yd()` accepts as first-party.
const FIRST_PARTY_API_HOST: &str = "api.anthropic.com";

impl ApiService {
    /// Choose an Anthropic compatibility version without changing product UA.
    #[must_use]
    pub fn with_anthropic_compatible_version(mut self, version: impl Into<String>) -> Self {
        self.anthropic.anthropic_compatible_version = Some(version.into());
        self
    }

    fn anthropic_user_agent(&self) -> String {
        user_agent(
            &self.ua,
            self.anthropic
                .anthropic_compatible_version
                .as_deref()
                .unwrap_or(&self.version),
        )
    }

    /// Set the identity for the Anthropic `metadata.user_id` field. Builder-style;
    /// the default is `None` (no `metadata` object emitted).
    #[must_use]
    pub fn with_request_metadata(mut self, metadata: crate::RequestMetadata) -> Self {
        self.anthropic.request_metadata = Some(metadata);
        self
    }

    /// Opt into the Native Anthropic client headers and HTTP/1 field layout.
    /// The resolved provider, protocol and base URL must also pass first-party
    /// admission; custom gateways and other providers retain their own policy.
    #[must_use]
    pub fn with_anthropic_client_metadata(
        mut self,
        metadata: lingxi_llm_client::providers::anthropic::request_policy::AnthropicClientMetadata,
    ) -> Self {
        self.anthropic.anthropic_client_metadata = Some(metadata);
        self
    }

    /// Select the exact configured API base that is authorized for Native
    /// Anthropic client headers and HTTP/1 field layout. The selected model
    /// must still resolve to the first-party Anthropic Messages provider at
    /// this endpoint. A custom gateway profile without this explicit endpoint
    /// keeps its ordinary provider transport policy.
    #[must_use]
    pub fn with_native_anthropic_endpoint(mut self, api_base_url: impl Into<String>) -> Self {
        self.anthropic.native_anthropic_api_base_url = Some(api_base_url.into());
        self
    }

    /// A direct first-party Anthropic API route, resolved after profile/model
    /// selection. Custom Anthropic-wire gateways are deliberately excluded:
    /// they may understand the stable Messages schema, but must not inherit
    /// Claude Code's private first-party betas or fast tier.
    pub(in crate::service) fn direct_anthropic_api_route(
        prepared: &crate::PreparedLlmCall,
    ) -> bool {
        if prepared.route.resolved_route.provider_id != crate::ProviderId::AnthropicFirstParty
            || prepared.route.protocol != crate::ProtocolFamily::AnthropicMessages
        {
            return false;
        }
        url::Url::parse(&prepared.provider_request.url).is_ok_and(|url| {
            url.scheme() == "https"
                && url.host_str() == Some(FIRST_PARTY_API_HOST)
                && url.port().is_none()
        })
    }

    /// `true` for protocols that speak to Anthropic models (first-party or via
    /// Bedrock/Vertex). Only these get the `claude-cli/<ver>` User-Agent;
    /// OpenAI / Gemini / Azure / Copilot routes get a neutral UA so we don't
    /// announce ourselves as Anthropic's official CLI to third-party providers.
    pub(in crate::service) fn is_anthropic_family_protocol(
        protocol: &crate::ProtocolFamily,
    ) -> bool {
        protocol.is_anthropic_family()
    }

    /// Provider-aware User-Agent. Anthropic-family routes keep the byte-faithful
    /// `claude-cli/...` UA. Other routes get a neutral `LingXi-Code/<ver>` UA —
    /// but only when an authenticator hasn't already set one (e.g. Copilot's
    /// `User-Agent: LingXi-Code`), which avoids shipping two conflicting UA
    /// headers on a case-sensitive header map.
    pub(in crate::service) fn apply_user_agent(&self, prepared: &mut crate::PreparedLlmCall) {
        use lingxi_llm_client::providers::anthropic::request_policy::{
            apply_user_agent, UserAgentPolicy,
        };
        let anthropic = Self::is_anthropic_family_protocol(&prepared.route.protocol);
        let value = if anthropic {
            self.anthropic_user_agent()
        } else {
            format!("LingXi-Code/{}", self.version)
        };
        let policy = if anthropic {
            UserAgentPolicy::Replace(&value)
        } else {
            UserAgentPolicy::IfAbsent(&value)
        };
        apply_user_agent(&mut prepared.provider_request.headers, policy);
        if self.native_anthropic_endpoint_selected(prepared) {
            if let Some(metadata) = &self.anthropic.anthropic_client_metadata {
                if let Some(draft) = prepared.wire_draft.as_mut() {
                    draft.set_http1_header_layout(Some(
                        lingxi_llm_client::Http1HeaderLayout::NativeFetch,
                    ));
                }
                let mut metadata = metadata.clone();
                metadata.session_id = prepared.request_session_id.clone();
                lingxi_llm_client::providers::anthropic::request_policy::apply_client_metadata(
                    &mut prepared.provider_request.headers,
                    &metadata,
                );
            }
        }
    }

    pub(in crate::service) fn native_anthropic_endpoint_selected(
        &self,
        prepared: &crate::PreparedLlmCall,
    ) -> bool {
        // NativeFetch layout and client-identity headers need an explicit host
        // selection. Request-body policy is admitted separately below.
        if self.anthropic.native_anthropic_api_base_url.is_none()
            || self.anthropic.anthropic_client_metadata.is_none()
        {
            return false;
        }
        self.native_anthropic_request_selected(prepared)
    }

    pub(in crate::service) fn native_anthropic_request_selected(
        &self,
        prepared: &crate::PreparedLlmCall,
    ) -> bool {
        if prepared.route.protocol != crate::ProtocolFamily::AnthropicMessages
            || prepared.route.resolved_route.provider_id != crate::ProviderId::AnthropicFirstParty
        {
            return false;
        }
        let Some(profile) = self.client().prompt_cache_profile(
            &prepared.route.resolved_route.request_model,
            Some(&prepared.route.resolved_route.profile_name),
        ) else {
            return false;
        };
        let configured_base_url = self
            .anthropic
            .native_anthropic_api_base_url
            .as_deref()
            .unwrap_or(NATIVE_ANTHROPIC_FIRST_PARTY_BASE_URL);
        profile.base_url.trim_end_matches('/') == configured_base_url.trim_end_matches('/')
            && lingxi_llm_client::providers::anthropic::request_policy::messages_request_matches_base_url(
                configured_base_url,
                &prepared.provider_request.url,
            )
    }

    pub(in crate::service) fn native_anthropic_endpoint_fingerprint(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Option<String> {
        let configured_base_url = self
            .anthropic
            .native_anthropic_api_base_url
            .as_deref()
            .unwrap_or(NATIVE_ANTHROPIC_FIRST_PARTY_BASE_URL);
        let route = self.client().resolve_media_route(model, profile).ok()?.main;
        if route.provider_id != crate::ProviderId::AnthropicFirstParty
            || self.client().protocol_for_model(model, profile).ok()
                != Some(crate::ProtocolFamily::AnthropicMessages)
        {
            return None;
        }
        let selected_profile = self
            .client()
            .prompt_cache_profile(model, Some(&route.profile_name))?;
        (selected_profile.base_url.trim_end_matches('/')
            == configured_base_url.trim_end_matches('/'))
        .then(|| {
            lingxi_llm_client::files::provider_file_endpoint_fingerprint(&selected_profile.base_url)
        })
    }

    pub(in crate::service) fn native_api_system_route(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> bool {
        self.client().native_api_system_route(model, profile)
            && self
                .native_anthropic_endpoint_fingerprint(model, profile)
                .is_some()
    }

    pub(in crate::service) fn native_per_turn_effort_route(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> bool {
        self.native_api_system_route(model, profile)
            || self.client().protocol_for_model(model, profile).ok()
                == Some(crate::ProtocolFamily::FoundryClaude)
    }

    pub(in crate::service) fn native_main_thinking_policy_route(
        &self,
        model: &str,
        profile: Option<&str>,
        query_source: PromptCacheQuerySource<'_>,
    ) -> bool {
        let PromptCacheQuerySource::Named(source) = query_source else {
            return false;
        };
        if query_source_category(Some(source)) != Some(QuerySourceCategory::Main) {
            return false;
        }
        self.client()
            .resolve_media_route(model, profile)
            .ok()
            .is_some_and(|route| {
                self.client()
                    .protocol_for_model(model, Some(&route.main.profile_name))
                    .ok()
                    .is_some_and(|protocol| {
                        is_native_main_anthropic_route(&route.main.provider_id, protocol)
                            && native_adaptive_provider_default(&route.main.provider_id, protocol)
                                .is_some()
                    })
            })
    }

    /// (cc 2.1.219) Opt-in `anthropic-dispatch-id: v2s` resolver —
    /// `Mg.CLAUDE_CODE_DISPATCH_V2S ?? Ke("tengu_cedar_lattice", !1)`.
    ///
    /// `Mg` is `oMl(cfh, null)` with `cfh = {}` (2.1.220 @226176806): the
    /// getter loop over `Object.entries({})` never runs and the prototype is
    /// `null`, so `Mg.<ANY>` reads `undefined` and the nullish `??` ALWAYS
    /// falls through to the flag. `CLAUDE_CODE_DISPATCH_V2S` is therefore not
    /// a live switch in the shipped binary — the flag is the sole gate, and
    /// reading the env here would let the port enable (or disable) a header
    /// the oracle cannot.
    fn dispatch_v2s_opt_in() -> bool {
        // `::telemetry` = the flags crate (the unqualified name is the
        // local `crate::model::telemetry` emit module).
        ::telemetry::flag_bool("tengu_cedar_lattice", false)
    }

    /// (cc 2.1.219) `Ooe()` = `xn()==="firstParty" && Yd()` (2.1.220
    /// @227683488) — the header's provider gate has TWO halves.
    ///
    /// `xn()` is env-only (bedrock/foundry/…/else `"firstParty"`), which the
    /// port models as [`crate::ProviderId::AnthropicFirstParty`]. `Yd()` is the
    /// BASE-URL half and is independent of it: `_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL`
    /// (Native JS nonempty string truthiness) short-circuits to true, else
    /// `d6r()` requires the configured base to be unset or to parse to host
    /// `api.anthropic.com` (`T1e`; an unparseable URL is false). A first-party
    /// route pointed at an enterprise gateway must get NO header.
    pub(in crate::service) fn native_first_party_endpoint(
        prepared: &crate::PreparedLlmCall,
    ) -> bool {
        if prepared.route.resolved_route.provider_id != crate::ProviderId::AnthropicFirstParty {
            return false;
        }
        // Native compares URL.host, including non-default ports. `url::Url`
        // normalizes the default port, while a custom port stays explicit.
        url::Url::parse(&prepared.provider_request.url).is_ok_and(|url| {
            url.host_str().is_some_and(|host| match url.port() {
                Some(port) => format!("{host}:{port}") == FIRST_PARTY_API_HOST,
                None => host == FIRST_PARTY_API_HOST,
            })
        })
    }

    pub(in crate::service) fn dispatch_first_party(prepared: &crate::PreparedLlmCall) -> bool {
        if prepared.route.resolved_route.provider_id != crate::ProviderId::AnthropicFirstParty {
            return false;
        }
        if std::env::var("_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL")
            .ok()
            .is_some_and(|value| !value.is_empty())
        {
            return true;
        }
        Self::native_first_party_endpoint(prepared)
    }

    /// Native XMe/hYe: recovery uses v2p for the remainder of this query.
    pub(in crate::service) fn apply_dispatch_header(
        prepared: &mut crate::PreparedLlmCall,
        state: DispatchHeaderState,
    ) -> DispatchAttempt {
        let first_party = Self::dispatch_first_party(prepared);
        let value = state.header(
            first_party,
            ::telemetry::flag_bool("tengu_dreamy_frost", false),
            Self::dispatch_v2s_opt_in(),
        );
        if let Some(value) = value {
            lingxi_llm_client::providers::anthropic::request_policy::set_header(
                &mut prepared.provider_request.headers,
                DISPATCH_ID_HEADER,
                value,
            );
            tracing::debug!("[dispatch] sent {DISPATCH_ID_HEADER}={value}");
        }
        DispatchAttempt {
            value: value.map(str::to_owned),
            first_party,
        }
    }

    pub(in crate::service) fn note_dispatch_header_failure(
        state: &mut DispatchHeaderState,
        attempt: &DispatchAttempt,
        err: &LlmError,
        response: Option<(u16, bool)>,
    ) -> Option<DispatchFallback> {
        state.on_failure(
            attempt,
            DispatchFailure {
                status: response
                    .map(|(status, _)| status)
                    .or_else(|| Self::status_of(err)),
                connection: Self::is_dispatch_conn_err(err),
                declined: response.is_some_and(|(_, declined)| declined),
            },
        )
    }

    pub(in crate::service) async fn report_dispatch_fallback(
        &self,
        req: &LlmRequest,
        fallback: DispatchFallback,
        request_id: Option<&str>,
    ) {
        let what = fallback
            .status
            .map_or_else(|| "connection error".to_string(), |s| format!("HTTP {s}"));
        let previous = fallback.previous.as_deref().map_or_else(
            || format!("no {DISPATCH_ID_HEADER}"),
            |value| format!("{DISPATCH_ID_HEADER}={value}"),
        );
        tracing::warn!(
            "[dispatch] {what} with {previous}; retrying once with {DISPATCH_ID_HEADER}=v2p"
        );
        telemetry::emit_dispatch_header_fallback(
            &self.analytics,
            telemetry::DispatchFallbackEvent {
                model: &req.input.model,
                dispatch: fallback.previous.as_deref(),
                reason: fallback.reason,
                status: fallback.status,
                query_source: req.execution.query_source.as_deref(),
                request_id,
            },
        )
        .await;
        let delay = fallback.delay(rand::random::<f64>());
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
    }

    /// Native yYe checks APIConnectionError, including its timeout subclass.
    /// Body-phase x2 cause classification remains separate below.
    fn is_dispatch_conn_err(err: &LlmError) -> bool {
        matches!(
            err.classification_source(),
            LlmError::Transport { .. }
                | LlmError::TransportTimeout { .. }
                | LlmError::TlsCert { .. }
        )
    }
}
