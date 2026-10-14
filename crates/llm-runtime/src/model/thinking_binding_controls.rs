//! Detached Native 2.1.295 TBC state-machine candidate.
//!
//! Native's `yD` records a beta in the conversation's `sent` set while building
//! the request, before transport/response success. `J_` reads that state and
//! the independent `rejected` set. This helper models those steps separately
//! from actual request beta-list/header membership.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderRoute {
    FirstParty,
    Foundry,
    Bedrock,
    Vertex,
    /// A resolved non-first-party provider with no TBC producer branch.
    Other,
}

/// Native 2.1.295's `rS` static beta identity allowlist. When `vh()` is false,
/// Native keeps only these custom CLI additions; this is independent from
/// `Rkr()`'s earlier HIPAA-wide suppression.
pub const NATIVE_STATIC_BETA_ALLOWLIST: &[&str] = &[
    "claude-code-20250219",
    "interleaved-thinking-2025-05-14",
    "context-1m-2025-08-07",
    "context-management-2025-06-27",
    "structured-outputs-2025-12-15",
    "web-search-2025-03-05",
    "effort-2025-11-24",
    "tool-search-tool-2025-10-19",
    "afk-mode-2026-01-31",
    "dangerous-tool-use-2026-09-03",
    "fallback-credit-2026-06-01",
    "mid-conversation-system-2026-04-07",
    "thinking-token-count-2026-05-13",
    "thinking-binding-controls-2026-08-01",
    "thinking-display-updates-2026-08-18",
];

/// Native's environment getter parses this setting through `M.bool`, so only
/// its recognized true tokens disable experimental betas. This deliberately
/// shares the exact ECMAScript-trimmed parser used by structured-output policy.
pub fn native_disable_experimental_betas(value: Option<&std::ffi::OsStr>) -> bool {
    value
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(crate::structured_output::bool_value)
}

/// Read the Native B6 environment input through the source `M.bool` parser.
/// Callers that also handle generic providers should first apply their own
/// provider boundary before using this Native-only value.
pub(crate) fn native_experimental_betas_disabled() -> bool {
    native_disable_experimental_betas(
        std::env::var_os("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS").as_deref(),
    )
}

/// Native B6 combines the typed environment gate with the local HIPAA taint.
pub(crate) fn native_b6_disabled() -> bool {
    native_b6_disabled_for(
        native_experimental_betas_disabled(),
        lingxi_core::host::compliance_taints::is_tainted("hipaa"),
    )
}

fn native_b6_disabled_for(experimental_betas_disabled: bool, hipaa_tainted: bool) -> bool {
    experimental_betas_disabled || hipaa_tainted
}

/// Preserve the ordinary main request's source `thinkingConfig.type` before
/// wire projection can erase Disabled/Automatic. Automatic is unresolved here.
pub(crate) fn native_request_type_disabled(
    thinking: crate::model::thinking::ThinkingConfig,
) -> Option<bool> {
    match thinking {
        crate::model::thinking::ThinkingConfig::Disabled => Some(true),
        crate::model::thinking::ThinkingConfig::Adaptive
        | crate::model::thinking::ThinkingConfig::Enabled { .. } => Some(false),
        crate::model::thinking::ThinkingConfig::Automatic => None,
    }
}

/// Apply Native `Rkr` then `vh` behavior to host-supplied CLI beta additions.
/// FirstParty/Foundry with experimental betas enabled keep all additions;
/// third-party Native families and `B6` requests keep only `rS`. HIPAA drops
/// the entire additions list before the static filter. Generic compatible
/// routes are excluded by the caller because they have no Native provider ID.
pub fn native_cli_beta_additions(
    provider: ProviderRoute,
    experimental_betas_disabled: bool,
    hipaa_tainted: bool,
    additions: &[String],
) -> Vec<String> {
    if hipaa_tainted {
        return Vec::new();
    }
    let u6 = matches!(provider, ProviderRoute::FirstParty | ProviderRoute::Foundry);
    if u6 && !experimental_betas_disabled {
        return additions.to_vec();
    }
    additions
        .iter()
        .filter(|beta| NATIVE_STATIC_BETA_ALLOWLIST.contains(&beta.as_str()))
        .cloned()
        .collect()
}

/// Only a selected non-empty API key proves Native `h3()` false in this Host
/// snapshot. Other auth kinds and empty/missing credentials stay unknown.
pub(crate) fn first_party_h3_from_selected_credential(
    credential: Option<&crate::Credential>,
) -> Option<bool> {
    match credential {
        Some(crate::Credential::ApiKey(key)) if !key.is_empty() => Some(false),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Branch {
    /// Native's `Kn && vh()` arm (`U6 && !B6`).
    FirstPartyFamily,
    /// Native's `Kn && !B6()` Bedrock/Vertex model arm.
    BedrockOrVertex,
    /// No arm assigned `kc`; Native initialized it to false.
    NotEntered,
    /// Local facts do not establish which arm Native would take.
    Unobserved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationEffect {
    /// The `yD(nt, uB)` call was not reached.
    NotAttempted,
    /// `yD` added `uB` to `nt.sent` during request construction.
    RegisteredBeforeDispatch,
    /// `yD` ran but did nothing because `uB` was already rejected.
    RefusedByConversationState,
    /// The local facts do not establish whether `yD` ran or changed the set.
    Unobserved,
}

#[derive(Debug, Clone, Copy)]
pub struct Facts<'a> {
    pub provider: Option<ProviderRoute>,
    /// Exact Native condition `r.type === "disabled" && r.mechanical === true`.
    pub mechanically_disabled: Option<bool>,
    pub request_type_disabled: Option<bool>,
    pub simulated_proxy_usage: Option<bool>,
    pub experimental_betas_disabled: Option<bool>,
    pub hipaa_tainted: Option<bool>,
    /// Native `h3()` for the selected first-party route.
    pub first_party_h3: Option<bool>,
    /// Native `KXt()`, consulted only if first-party `h3()` is true.
    pub first_party_kxt_eligible: Option<bool>,
    /// Native `sl()`, consulted by `$nn(Fnn())` in the first-party-family arm.
    pub first_party_endpoint_eligible: Option<bool>,
    pub bedrock_base_url_override_defined: Option<bool>,
    pub vertex_base_url_override_defined: Option<bool>,
    /// Exact `We(model)` canonical identity, not an unobserved alias.
    pub bedrock_native_canonical_model: Option<&'a str>,
    /// Pre-request Native conversation state for this exact beta.
    /// Native calls this set `sent`; `yD` updates it before actual dispatch.
    pub sent_before: Option<bool>,
    pub rejected_before: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Evaluation {
    pub branch: Branch,
    pub registration: RegistrationEffect,
    /// Native `nt.sent` after the possible `yD` call; not transport success.
    pub sent_after_registration: Option<bool>,
    /// Native `kc`: `J_(nt, uB)` only when a producer branch is entered;
    /// otherwise `false` from its local initializer.
    pub kc: Option<bool>,
    /// Native cleanup predicate for the current `ut` beta list: `!kc && !U6`.
    /// This is separate from SDK/request-wire membership.
    pub remove_from_current_beta_list: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlannedWireMembership {
    Present,
    Absent,
    Unobserved,
}

fn tri_and(left: Option<bool>, right: Option<bool>) -> Option<bool> {
    match (left, right) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}

fn tri_or(left: Option<bool>, right: Option<bool>) -> Option<bool> {
    match (left, right) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    }
}

fn tri_not(value: Option<bool>) -> Option<bool> {
    value.map(|value| !value)
}

fn first_party_auth_gate(facts: &Facts<'_>) -> Option<bool> {
    match facts.first_party_h3 {
        Some(false) => Some(true),
        Some(true) => facts.first_party_kxt_eligible,
        None if facts.first_party_kxt_eligible == Some(true) => Some(true),
        None => None,
    }
}

pub fn evaluate_kn(facts: &Facts<'_>) -> Option<bool> {
    let provider = facts.provider?;
    let auth_gate = if provider == ProviderRoute::FirstParty {
        first_party_auth_gate(facts)
    } else {
        Some(true) // Native `!h3()` short-circuits on non-first-party routes.
    };
    tri_and(
        tri_and(
            tri_not(facts.mechanically_disabled),
            tri_not(facts.simulated_proxy_usage),
        ),
        auth_gate,
    )
}

pub fn bedrock_model_supported(native_canonical_model: Option<&str>) -> Option<bool> {
    let model = native_canonical_model?.to_ascii_lowercase();
    if model.contains("application-inference-profile") {
        return None;
    }
    if model.contains("claude-3-") {
        return Some(false);
    }
    const DENIED: &[&str] = &[
        "claude-opus-4-0",
        "claude-opus-4-1",
        "claude-opus-4-5",
        "claude-opus-4-6",
        "claude-sonnet-4-0",
        "claude-sonnet-4-5",
        "claude-sonnet-4-6",
        "claude-haiku-4-5",
    ];
    Some(!DENIED.iter().any(|name| model == *name))
}

fn beta_membership(sent: Option<bool>, rejected: Option<bool>) -> Option<bool> {
    if rejected == Some(true) || sent == Some(false) {
        Some(false)
    } else if sent == Some(true) && rejected == Some(false) {
        Some(true)
    } else {
        None
    }
}

fn apply_y_d(
    sent_before: Option<bool>,
    rejected_before: Option<bool>,
    call_y_d: Option<bool>,
) -> (RegistrationEffect, Option<bool>) {
    match call_y_d {
        Some(false) => (RegistrationEffect::NotAttempted, sent_before),
        Some(true) if rejected_before == Some(true) => {
            (RegistrationEffect::RefusedByConversationState, sent_before)
        }
        Some(true) if rejected_before == Some(false) => {
            (RegistrationEffect::RegisteredBeforeDispatch, Some(true))
        }
        Some(true) => (
            RegistrationEffect::Unobserved,
            if sent_before == Some(true) {
                Some(true)
            } else {
                None
            },
        ),
        None if rejected_before == Some(true) => (RegistrationEffect::Unobserved, sent_before),
        None => (
            RegistrationEffect::Unobserved,
            if sent_before == Some(true) {
                Some(true)
            } else {
                None
            },
        ),
    }
}

/// Evaluate Native's branch, `yD` registration effect, and `kc = J_(...)`.
/// This does not represent a successful HTTP send and does not inspect headers.
pub fn evaluate(facts: &Facts<'_>) -> Evaluation {
    let Some(provider) = facts.provider else {
        return Evaluation {
            branch: Branch::Unobserved,
            registration: RegistrationEffect::Unobserved,
            sent_after_registration: None,
            kc: None,
            remove_from_current_beta_list: None,
        };
    };
    let u6 = matches!(provider, ProviderRoute::FirstParty | ProviderRoute::Foundry);
    let beta_blocked = tri_or(facts.experimental_betas_disabled, facts.hipaa_tainted);
    let kn = evaluate_kn(facts);

    let branch = match (kn, beta_blocked) {
        (Some(false), _) | (_, Some(true)) => Branch::NotEntered,
        (None, _) | (_, None) => Branch::Unobserved,
        (Some(true), Some(false)) if u6 => Branch::FirstPartyFamily,
        (Some(true), Some(false)) => {
            let route_eligible = match provider {
                ProviderRoute::Bedrock => tri_and(
                    tri_not(facts.bedrock_base_url_override_defined),
                    bedrock_model_supported(facts.bedrock_native_canonical_model),
                ),
                ProviderRoute::Vertex => tri_not(facts.vertex_base_url_override_defined),
                ProviderRoute::Other => Some(false),
                ProviderRoute::FirstParty | ProviderRoute::Foundry => unreachable!(),
            };
            match route_eligible {
                Some(true) => Branch::BedrockOrVertex,
                Some(false) => Branch::NotEntered,
                None => Branch::Unobserved,
            }
        }
    };

    let call_y_d = match branch {
        Branch::FirstPartyFamily => {
            let endpoint_eligible = match provider {
                ProviderRoute::FirstParty => facts.first_party_endpoint_eligible,
                ProviderRoute::Foundry => Some(false), // `sl()` requires firstParty.
                _ => unreachable!(),
            };
            tri_and(tri_not(facts.request_type_disabled), endpoint_eligible)
        }
        Branch::BedrockOrVertex => tri_not(facts.request_type_disabled),
        Branch::NotEntered => Some(false),
        Branch::Unobserved => None,
    };
    let (registration, sent_after_registration) =
        apply_y_d(facts.sent_before, facts.rejected_before, call_y_d);
    let kc = match branch {
        Branch::FirstPartyFamily | Branch::BedrockOrVertex => {
            beta_membership(sent_after_registration, facts.rejected_before)
        }
        Branch::NotEntered => Some(false),
        Branch::Unobserved => None,
    };
    let remove_from_current_beta_list = if u6 { Some(false) } else { tri_not(kc) };
    Evaluation {
        branch,
        registration,
        sent_after_registration,
        kc,
        remove_from_current_beta_list,
    }
}

/// Actual wire-header presence is a separate observation from Native `kc`.
/// This helper must not be used as the SDK's `...beta_requested` fact or as
/// Native `Tp`, both of which require `kc == Some(true)`.
pub fn planned_wire_membership(
    kc: Option<bool>,
    final_host_request_has_tbc_beta: Option<bool>,
) -> PlannedWireMembership {
    match (kc, final_host_request_has_tbc_beta) {
        (Some(true), _) | (_, Some(true)) => PlannedWireMembership::Present,
        (Some(false), Some(false)) => PlannedWireMembership::Absent,
        _ => PlannedWireMembership::Unobserved,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn facts(provider: ProviderRoute) -> Facts<'static> {
        Facts {
            provider: Some(provider),
            mechanically_disabled: Some(false),
            request_type_disabled: Some(false),
            simulated_proxy_usage: Some(false),
            experimental_betas_disabled: Some(false),
            hipaa_tainted: Some(false),
            first_party_h3: None,
            first_party_kxt_eligible: None,
            first_party_endpoint_eligible: None,
            bedrock_base_url_override_defined: None,
            vertex_base_url_override_defined: None,
            bedrock_native_canonical_model: None,
            sent_before: Some(false),
            rejected_before: Some(false),
        }
    }

    #[test]
    fn custom_cli_additions_follow_native_rkr_vh_and_static_allowlist() {
        let additions = vec![
            "context-1m-2025-08-07".to_owned(),
            "thinking-binding-controls-2026-08-01".to_owned(),
            "not-in-native-static-list".to_owned(),
        ];
        assert_eq!(
            native_cli_beta_additions(ProviderRoute::FirstParty, false, false, &additions),
            additions
        );
        assert_eq!(
            native_cli_beta_additions(ProviderRoute::Foundry, false, false, &additions),
            additions
        );
        assert_eq!(
            native_cli_beta_additions(ProviderRoute::Foundry, true, false, &additions),
            additions[..2]
        );
        assert_eq!(
            native_cli_beta_additions(ProviderRoute::FirstParty, true, false, &additions),
            additions[..2]
        );
        assert_eq!(
            native_cli_beta_additions(ProviderRoute::Bedrock, false, false, &additions),
            additions[..2]
        );
        assert_eq!(
            native_cli_beta_additions(ProviderRoute::Vertex, false, false, &additions),
            additions[..2]
        );
        for provider in [
            ProviderRoute::FirstParty,
            ProviderRoute::Foundry,
            ProviderRoute::Bedrock,
            ProviderRoute::Vertex,
        ] {
            assert!(native_cli_beta_additions(provider, false, true, &additions).is_empty());
        }
    }

    #[test]
    fn native_b6_disable_env_uses_native_typed_boolean_parser() {
        for (value, expected) in [
            ("true", true),
            ("1", true),
            ("yes", true),
            ("on", true),
            ("false", false),
            ("0", false),
            ("no", false),
            ("off", false),
            ("wat", false),
            ("", false),
        ] {
            assert_eq!(
                native_disable_experimental_betas(Some(OsStr::new(value))),
                expected,
                "Native M.bool parses {value:?} as {expected}"
            );
        }
        assert!(!native_disable_experimental_betas(None));

        let additions = vec![
            "context-1m-2025-08-07".to_owned(),
            "tool-search-tool-2025-10-19".to_owned(),
            "not-in-native-static-list".to_owned(),
        ];
        for raw_value in ["false", "0", "wat", ""] {
            let disabled = native_disable_experimental_betas(Some(OsStr::new(raw_value)));
            assert_eq!(
                native_cli_beta_additions(ProviderRoute::FirstParty, disabled, false, &additions,),
                additions,
                "Native M.bool leaves the beta gate enabled for {raw_value:?}"
            );
        }
        for raw_value in ["true", "1", "yes", "on"] {
            assert_eq!(
                native_cli_beta_additions(
                    ProviderRoute::FirstParty,
                    native_disable_experimental_betas(Some(OsStr::new(raw_value))),
                    false,
                    &additions,
                ),
                additions[..2],
                "Native M.bool disables experimental additions for {raw_value:?}"
            );
        }
    }

    #[test]
    fn native_b6_combines_environment_and_local_hipaa_gates() {
        assert!(!native_b6_disabled_for(false, false));
        assert!(native_b6_disabled_for(true, false));
        assert!(native_b6_disabled_for(false, true));
    }

    #[test]
    fn main_thinking_config_sidecar_keeps_native_type_before_wire_projection() {
        use crate::model::thinking::ThinkingConfig;

        assert_eq!(
            native_request_type_disabled(ThinkingConfig::Disabled),
            Some(true)
        );
        assert_eq!(
            native_request_type_disabled(ThinkingConfig::Adaptive),
            Some(false)
        );
        assert_eq!(
            native_request_type_disabled(ThinkingConfig::Enabled { budget_tokens: 64 }),
            Some(false)
        );
        assert_eq!(
            native_request_type_disabled(ThinkingConfig::Automatic),
            None
        );
    }

    #[test]
    fn only_nonempty_selected_api_key_proves_first_party_h3_false() {
        assert_eq!(
            first_party_h3_from_selected_credential(Some(&crate::Credential::ApiKey(
                "redacted-key".to_owned()
            ))),
            Some(false)
        );
        assert_eq!(
            first_party_h3_from_selected_credential(Some(
                &crate::Credential::ApiKey(String::new())
            )),
            None
        );
        assert_eq!(
            first_party_h3_from_selected_credential(Some(&crate::Credential::BearerToken(
                "token".to_owned()
            ))),
            None
        );
        assert_eq!(first_party_h3_from_selected_credential(None), None);
    }

    #[test]
    fn add_registers_before_dispatch_and_sets_kc() {
        let mut f = facts(ProviderRoute::Bedrock);
        f.bedrock_base_url_override_defined = Some(false);
        f.bedrock_native_canonical_model = Some("claude-opus-4-7");
        let result = evaluate(&f);
        assert_eq!(result.branch, Branch::BedrockOrVertex);
        assert_eq!(
            result.registration,
            RegistrationEffect::RegisteredBeforeDispatch
        );
        assert_eq!(result.sent_after_registration, Some(true));
        assert_eq!(result.kc, Some(true));
    }

    #[test]
    fn ordinary_disabled_keeps_prior_nonrejected_membership_without_reregistration() {
        let mut f = facts(ProviderRoute::Bedrock);
        f.request_type_disabled = Some(true);
        f.bedrock_base_url_override_defined = Some(false);
        f.bedrock_native_canonical_model = Some("claude-opus-4-7");
        f.sent_before = Some(true);
        let result = evaluate(&f);
        assert_eq!(result.registration, RegistrationEffect::NotAttempted);
        assert_eq!(result.sent_after_registration, Some(true));
        assert_eq!(result.kc, Some(true));
        assert_eq!(
            planned_wire_membership(result.kc, Some(false)),
            PlannedWireMembership::Present
        );
    }

    #[test]
    fn ordinary_disabled_without_prior_membership_does_not_set_kc() {
        let mut f = facts(ProviderRoute::Bedrock);
        f.request_type_disabled = Some(true);
        f.bedrock_base_url_override_defined = Some(false);
        f.bedrock_native_canonical_model = Some("claude-opus-4-7");
        let result = evaluate(&f);
        assert_eq!(result.registration, RegistrationEffect::NotAttempted);
        assert_eq!(result.kc, Some(false));
        assert_eq!(
            planned_wire_membership(result.kc, Some(false)),
            PlannedWireMembership::Absent
        );
    }

    #[test]
    fn foundry_runs_j_but_sl_prevents_a_new_registration() {
        let mut f = facts(ProviderRoute::Foundry);
        f.sent_before = Some(true);
        let result = evaluate(&f);
        assert_eq!(result.branch, Branch::FirstPartyFamily);
        assert_eq!(result.registration, RegistrationEffect::NotAttempted);
        assert_eq!(result.kc, Some(true));
    }

    #[test]
    fn rejection_wins_over_registration_and_membership() {
        let mut f = facts(ProviderRoute::Vertex);
        f.vertex_base_url_override_defined = Some(false);
        f.sent_before = Some(true);
        f.rejected_before = Some(true);
        let result = evaluate(&f);
        assert_eq!(
            result.registration,
            RegistrationEffect::RefusedByConversationState
        );
        assert_eq!(result.kc, Some(false));
        assert_eq!(result.remove_from_current_beta_list, Some(true));
    }

    #[test]
    fn mechanical_gate_skips_j_even_when_beta_was_previously_registered() {
        let mut f = facts(ProviderRoute::Bedrock);
        f.mechanically_disabled = Some(true);
        f.bedrock_base_url_override_defined = Some(false);
        f.bedrock_native_canonical_model = Some("claude-opus-4-7");
        f.sent_before = Some(true);
        let result = evaluate(&f);
        assert_eq!(result.branch, Branch::NotEntered);
        assert_eq!(result.kc, Some(false));
        assert_eq!(result.remove_from_current_beta_list, Some(true));
    }

    #[test]
    fn unresolved_profile_and_first_party_entitlement_stay_unknown() {
        let mut dynamic = facts(ProviderRoute::Bedrock);
        dynamic.bedrock_base_url_override_defined = Some(false);
        dynamic.bedrock_native_canonical_model = Some("application-inference-profile/abc");
        assert_eq!(evaluate(&dynamic).branch, Branch::Unobserved);

        let mut first_party = facts(ProviderRoute::FirstParty);
        first_party.first_party_h3 = None;
        first_party.first_party_endpoint_eligible = Some(true);
        assert_eq!(evaluate(&first_party).kc, None);
    }

    #[test]
    fn known_kxt_opens_first_party_gate_without_resolving_h3() {
        let mut first_party = facts(ProviderRoute::FirstParty);
        first_party.first_party_h3 = None;
        first_party.first_party_kxt_eligible = Some(true);
        first_party.first_party_endpoint_eligible = Some(true);

        let result = evaluate(&first_party);
        assert_eq!(result.branch, Branch::FirstPartyFamily);
        assert_eq!(
            result.registration,
            RegistrationEffect::RegisteredBeforeDispatch
        );
        assert_eq!(result.kc, Some(true));
    }

    #[test]
    fn kc_and_actual_request_beta_are_distinct_observations() {
        assert_eq!(
            planned_wire_membership(Some(false), Some(true)),
            PlannedWireMembership::Present
        );
        assert_eq!(
            planned_wire_membership(None, Some(false)),
            PlannedWireMembership::Unobserved
        );
    }
}
