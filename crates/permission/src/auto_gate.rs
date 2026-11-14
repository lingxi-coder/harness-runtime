//! Auto-mode **availability gate** — 1:1 port of claude-code 2.1.207's
//! `P0` / `One` / `Jce` / `Bpa` / `dUe` / `jqt` cluster.
//!
//! Auto mode (the classifier-driven auto-accept mode, [`PermissionMode::Auto`])
//! is only reachable when three conditions hold: it is not disabled by the
//! `disableAutoMode` settings killswitch, the local denial circuit-breaker has
//! not tripped, and the active model supports it. This module ports that gate as
//! pure functions so both the boot mode-load path (claude-code `xms` silent
//! downgrade) and the live `set_permission_mode` path (claude-code `Nle`
//! `setPermissionModeWithGuards` rejection) share one authoritative decision.
//!
//! # Source of truth (claude-code v2.1.207, `strings -n 8` of the binary)
//!
//! ```js
//! // killswitch — reads merged settings at BOTH positions (top-level + permissions):
//! function Bpa(){let e=Mi()||{};return e.disableAutoMode==="disable"||e.permissions?.disableAutoMode==="disable"}
//! // circuit-breaker latch:
//! function q6r(){return f5e.circuitBroken}
//! // provider opt-in — VESTIGIAL in 2.1.207 (always true; the CLAUDE_CODE_ENABLE_AUTO_MODE
//! // opt-in was removed, so One()'s "provider" branch is unreachable dead code):
//! function jqt(e){if(e==="firstParty"||e==="anthropicAws")return!0;return!0}
//! // model gate:
//! function dUe(e){let t=ao(e),r=xn();if(!jqt(r))return!1;
//!   if(t.includes("claude-3-")||t==="claude-opus-4-0"||t==="claude-opus-4-1"||t==="claude-opus-4-5"||t==="claude-sonnet-4-0"||t==="claude-sonnet-4-5"||t==="claude-haiku-4-5")return!1;
//!   if(r!=="firstParty"&&r!=="anthropicAws"&&(t==="claude-opus-4-6"||t==="claude-sonnet-4-6"||t.includes("haiku")))return!1;
//!   return!0}
//! // availability (short-circuit order: breaker, settings, model):
//! function P0(){if(q6r())return!1;if(Bpa())return!1;if(!dUe(wi()))return!1;return!0}
//! // denial reason (precedence order: settings, breaker, provider, model):
//! function One(){if(Bpa())return"settings";if(q6r())return"circuit-breaker";if(!jqt(xn()))return"provider";if(!dUe(wi()))return"model";return null}
//! // user-facing message map:
//! function Jce(e){switch(e){
//!   case"settings":       return"auto mode disabled by settings";
//!   case"circuit-breaker":return"auto mode is unavailable for your plan";
//!   case"provider":       return"auto mode requires CLAUDE_CODE_ENABLE_AUTO_MODE=1";
//!   case"model":          return"auto mode unavailable for this model"}}
//! // silent downgrade at mode load:
//! //   xms: if(t==="auto"&&!P0())return"default"
//! // live set-mode rejection:
//! //   Nle: if(e==="auto"&&!P0()){let o=One();return{ok:!1,error:`Cannot set permission mode to auto: ${Jce(o)}`}}
//! ```
//!
//! ## Re-audited against 2.1.270 (2026-09-13)
//!
//! The cluster was re-read in the 2.1.270 binary. The availability decision and
//! its message map are now:
//!
//! ```js
//! function aC(){if(WSe())return!1;if(eqe())return!1;if(!Xce(nt()))return!1;return!0}   // P0
//! function R8(){if(eqe())return"settings";if(WSe())return"circuit-breaker";
//!               if(!Xce(nt()))return"model";return AFn()}                              // One
//! function KW(e){switch(e){                                                            // Jce
//!   case"settings":       return"auto mode disabled by settings";
//!   case"circuit-breaker":return"auto mode is unavailable for your plan";
//!   case"fast-mode":      return"auto mode unavailable while fast mode is on \xB7 run /fast off";
//!   case"model":          return"auto mode unavailable for this model"}}
//! function iBe(){let e=R8();return e!==null?KW(e):"auto mode is unavailable right now"}
//! function fae(e){return e.includes("claude-3-")||e==="claude-opus-4-0"||e==="claude-opus-4-1"
//!   ||e==="claude-opus-4-5"||e==="claude-sonnet-4-0"||e==="claude-sonnet-4-5"||e==="claude-haiku-4-5"}
//! function Xce(e){let n=je(e),r=He();if(fae(n))return!1;                                // dUe
//!   if(r!=="firstParty"&&!XD(r)&&(n==="claude-opus-4-6"||n==="claude-sonnet-4-6"||n.includes("haiku")))return!1;
//!   return!0}
//! ```
//!
//! Two deltas against the 2.1.207 shape this module was written from:
//!
//! * **`jqt` is gone, and with it the `provider` reason.** `Xce` no longer
//!   consults a provider opt-in, and `KW` has no `provider` case; the string
//!   `auto mode requires CLAUDE_CODE_ENABLE_AUTO_MODE=1` occurs ZERO times in
//!   the 2.1.270 binary (the env-var NAME survives only inside settings
//!   allowlists). `AutoGateDenialReason::Provider` and the vestigial
//!   `provider_allows_auto_mode` are therefore DELETED rather than kept
//!   unreachable — an unreachable variant that prints copy the oracle no longer
//!   has is a divergence in the port's own direction.
//! * **A `fast-mode` reason was ADDED.** `R8`'s tail is `AFn()` — the
//!   `fastModeBreakerReason` latch — whose message is
//!   `auto mode unavailable while fast mode is on · run /fast off`, and `iBe`
//!   adds the null-reason fallback `auto mode is unavailable right now`. This
//!   port has a fast mode (`/fast`, `OrchestratorHandle::set_fast_mode`) but
//!   does not feed a breaker latch into this gate, and inventing an input that
//!   nothing sets would add a variant no code path can reach. NOT PORTED,
//!   deliberately; wire the latch first, then add the reason.
//!
//! The model deny-list itself is unchanged from 2.1.207 EXCEPT that the
//! "counts as first-party" test is `XD` (`anthropicAws` **or**
//! `anthropicGoogleCloud`), which this module had as `anthropicAws` alone —
//! see [`is_anthropic_managed_provider`].
//!
//! ## Documented omissions vs the binary (no substrate in this build)
//! - **Statsig remote-disable** (`tengu_auto_mode_config.enabled==="disabled"`,
//!   read by the runtime `AVr`/`verifyAutoModeGateAccess`): `LingXi` has no
//!   Statsig substrate (consistent with the omissions documented in
//!   [`crate::cli_mode`]). The circuit-breaker input here is fed ONLY by the
//!   already-ported LOCAL denial-tracking breaker
//!   ([`crate::denial_tracking::DenialTrackingState::is_circuit_broken`]); the
//!   remote-config latch is inert-by-default.
//! - Service-owned override maps, model knowledge and application-profile
//!   metadata remain the routing caller's responsibility. The static 2.1.318
//!   `et`/`Dy` identity fold is applied by [`model_supports_auto_mode`] itself,
//!   so every boot and live caller also handles dated/cloud/context-wrapped IDs.

use crate::mode::PermissionMode;
use regex::Regex;
use std::sync::LazyLock;

// Qd/IOs shape recognition (2.1.318, native offsets 258141342/258142026).
// Matching the complete trailer avoids mistaking a date for a minor version.
static AUTO_MODEL_SHAPE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"^claude-([a-z]+)-([0-9]{1,2})(?:-([0-9]{1,2}))?",
        r"((?:-fast|-latest)?(?:-v[0-9]{1,3}@[0-9]{8}|[-@][0-9]{8})?",
        r"(?:-v[0-9]{1,3}(?::[0-9]{1,3})?)?)$"
    ))
    .expect("native model shape")
});
static AUTO_LEGACY_MODEL_SHAPE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"^claude-([0-9]{1,2})(?:-([0-9]{1,2}))?-([a-z]+)",
        r"((?:-fast|-latest)?(?:-v[0-9]{1,3}@[0-9]{8}|[-@][0-9]{8})?",
        r"(?:-v[0-9]{1,3}(?::[0-9]{1,3})?)?)$"
    ))
    .expect("native legacy model shape")
});
static AUTO_MODEL_DOT_VERSION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(claude-(?:[a-z]+-)?[0-9]{1,2})\.([0-9])").expect("native dotted model version")
});
static AUTO_MODEL_DATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"-[0-9]{8}$").expect("native model date"));

fn native_legacy_four_fallback(value: &str, family: &str) -> bool {
    value.match_indices(family).any(|(offset, _)| {
        let tail = &value.as_bytes()[offset + family.len()..];
        !(tail.first() == Some(&b'-')
            && tail.get(1).is_some_and(u8::is_ascii_digit)
            && !tail.get(2).is_some_and(u8::is_ascii_digit))
    })
}

/// Native static `et`/`Dy` identity fold used before the auto model gate.
/// Resolved routing/overrides remain the caller's responsibility; arbitrary
/// non-Claude model names retain Native's lowercase/date-only fallback.
/// This covers canonical IDs, dated IDs, context markers, cloud transports,
/// dotted version spellings and recognized legacy aliases such as Opus 4.
#[must_use]
pub fn canonical_auto_mode_model(model: &str) -> String {
    let lower = model.to_lowercase();
    let value = AUTO_MODEL_DOT_VERSION.replace_all(&lower, "$1-$2");
    let trimmed = lingxi_core::host::effort::trim_js_whitespace(&value);
    let trimmed = trimmed
        .strip_suffix("[1m]")
        .or_else(|| trimmed.strip_suffix("[2m]"))
        .unwrap_or(trimmed);
    let mut candidate = trimmed.rsplit('/').next().unwrap_or(trimmed);
    if let Some(tail) = candidate.strip_prefix("anthropic.") {
        candidate = tail;
    } else if let Some((region, tail)) = candidate.split_once(".anthropic.") {
        if ["us", "eu", "apac", "jp", "au", "us-gov", "global"].contains(&region) {
            candidate = tail;
        }
    }
    if !trimmed
        .chars()
        .any(lingxi_core::host::effort::javascript_whitespace)
    {
        let current = AUTO_MODEL_SHAPE.captures(candidate);
        let legacy = AUTO_LEGACY_MODEL_SHAPE.captures(candidate);
        if let Some((captures, version_first)) = current
            .map(|c| (c, false))
            .or_else(|| legacy.map(|c| (c, true)))
        {
            let (family, major, minor) = if version_first {
                (
                    &captures[3],
                    &captures[1],
                    captures.get(2).map(|m| m.as_str()),
                )
            } else {
                (
                    &captures[1],
                    &captures[2],
                    captures.get(3).map(|m| m.as_str()),
                )
            };
            let major: u8 = major.parse().expect("bounded decimal major");
            let minor = minor.map(|n| n.parse::<u8>().expect("bounded decimal minor"));
            let normalized = if version_first {
                format!(
                    "claude-{major}{}{family}",
                    minor.map_or_else(|| "-".into(), |n| format!("-{n}-"))
                )
            } else {
                let minor = if major == 4 && matches!(family, "opus" | "sonnet") {
                    Some(minor.unwrap_or(0))
                } else {
                    minor
                };
                format!(
                    "claude-{family}-{major}{}",
                    minor.map_or_else(String::new, |n| format!("-{n}"))
                )
            };
            if lingxi_core::host::model_capabilities::is_known_model_id(&normalized)
                || ["claude-3-opus", "claude-3-sonnet", "claude-3-haiku"]
                    .contains(&normalized.as_str())
            {
                return normalized;
            }
            // Qd recognized an uncatalogued Claude family/version: Native KG
            // preserves its base rather than guessing a nearby catalog entry.
            let tail = captures.get(4).expect("trailer capture").start();
            return candidate[..tail].to_owned();
        }
    }
    // Native BG's fallback order matters when more than one family appears.
    for family in [
        "claude-fable-5-1",
        "claude-fable-5",
        "claude-mythos-5-1",
        "claude-mythos-5",
        "claude-opus-5-5",
        "claude-opus-5",
        "claude-opus-4-8",
        "claude-opus-4-7",
        "claude-opus-4-6",
        "claude-opus-4-5",
        "claude-opus-4-1",
        "claude-opus-4",
        "claude-sonnet-5-5",
        "claude-sonnet-5",
        "claude-sonnet-4-6",
        "claude-sonnet-4-5",
        "claude-sonnet-4",
        "claude-haiku-5-5",
        "claude-haiku-4-5",
        "claude-3-7-sonnet",
        "claude-3-5-sonnet",
        "claude-3-5-haiku",
        "claude-3-opus",
        "claude-3-sonnet",
        "claude-3-haiku",
    ] {
        if matches!(family, "claude-opus-4" | "claude-sonnet-4") {
            if native_legacy_four_fallback(&value, family) {
                return format!("{family}-0");
            }
        } else if value.contains(family) {
            return family.to_owned();
        }
    }
    AUTO_MODEL_DATE.replace(&value, "").into_owned()
}

/// Why auto mode is unavailable — 1:1 with claude-code `One()`'s four return
/// tags. Precedence (highest first): [`Self::Settings`] → [`Self::CircuitBreaker`]
/// → [`Self::Model`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoGateDenialReason {
    /// `disableAutoMode == "disable"` at either settings position (`Bpa()`).
    Settings,
    /// The local denial circuit-breaker has tripped (`q6r()`).
    CircuitBreaker,
    /// The active model does not support auto mode (`dUe()` deny-list).
    Model,
}

impl AutoGateDenialReason {
    /// The byte-exact user-facing message — 1:1 with claude-code `Jce(e)`.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            AutoGateDenialReason::Settings => "auto mode disabled by settings",
            AutoGateDenialReason::CircuitBreaker => "auto mode is unavailable for your plan",
            AutoGateDenialReason::Model => "auto mode unavailable for this model",
        }
    }
}

/// The inputs the gate reads — the pure projection of claude-code's ambient
/// `Bpa()`/`q6r()`/`wi()`/`xn()` reads, so the decision is exhaustively testable
/// without settings IO, a live breaker, or a provider registry.
#[derive(Debug, Clone)]
pub struct AutoGateInputs {
    /// `Bpa()`: the `disableAutoMode` settings killswitch (either position).
    pub disabled_by_settings: bool,
    /// `q6r()`: the local denial circuit-breaker has tripped. (Statsig
    /// remote-disable is a documented omission; only the local breaker feeds
    /// this.)
    pub circuit_broken: bool,
    /// `wi()`: the active main-loop model id. Its static identity decorations
    /// are normalized by the gate; callers supply their resolved route/override.
    pub model: String,
    /// `xn()`: the active provider (`"firstParty"` / `"anthropicAws"` / other).
    pub provider: String,
}

/// `dUe(e)` — does the active model support auto mode? Ports the binary's exact
/// deny-list:
/// - Off for `claude-3-*` and the legacy 4-x line
///   (`opus-4-0`/`4-1`/`4-5`, `sonnet-4-0`/`4-5`, `haiku-4-5`) on EVERY provider.
/// - Additionally off, on any provider OTHER than `firstParty`/`anthropicAws`,
///   for `claude-opus-4-6`, `claude-sonnet-4-6`, and anything containing
///   `haiku`, except for `claude-haiku-5-5` (2.1.318 `KP` at 261385150).
/// - Otherwise on.
#[must_use]
pub fn model_supports_auto_mode(model: &str, provider: &str) -> bool {
    // Boot and live mode callers can supply a dated/cloud/context-suffixed ID.
    // Native KP always calls et before applying any exact canonical comparisons.
    let canonical = canonical_auto_mode_model(model);
    let model = canonical.as_str();
    // Shared exclusion list (all providers).
    if model.contains("claude-3-")
        || model == "claude-opus-4-0"
        || model == "claude-opus-4-1"
        || model == "claude-opus-4-5"
        || model == "claude-sonnet-4-0"
        || model == "claude-sonnet-4-5"
        || model == "claude-haiku-4-5"
    {
        return false;
    }
    // Non-1P (bedrock/vertex/gateway/…) additionally exclude the 4-6 pair and
    // every haiku except 5.5. The "counts as first-party" family is `UI` —
    // [`is_anthropic_managed_provider`] — NOT `anthropicAws` alone.
    if provider != "firstParty"
        && !is_anthropic_managed_provider(provider)
        && (model == "claude-opus-4-6"
            || model == "claude-sonnet-4-6"
            || (model.contains("haiku") && model != "claude-haiku-5-5"))
    {
        return false;
    }
    true
}

/// `XD(e)` — the Anthropic-operated provider family, 2.1.270
/// (`src_166316371.js` @27030):
///
/// ```js
/// function XD(e=He()){return e==="anthropicAws"||e==="anthropicGoogleCloud"}
/// ```
///
/// 🚨 This port previously spelled the second arm of [`model_supports_auto_mode`]
/// as `provider != "anthropicAws"`, which was the 2.1.207 reading and is one
/// provider short. On `anthropicGoogleCloud` the omission is user-visible and
/// wrong in the RESTRICTIVE direction: a session on `claude-sonnet-4-6` (or any
/// haiku) there was told `auto mode unavailable for this model` and refused
/// entry to Auto mode, while upstream lets it in.
#[must_use]
pub fn is_anthropic_managed_provider(provider: &str) -> bool {
    provider == "anthropicAws" || provider == "anthropicGoogleCloud"
}

/// `P0()` — is auto mode available? Short-circuits in the binary's order:
/// circuit-breaker, then settings killswitch, then model gate.
#[must_use]
pub fn auto_mode_available(inputs: &AutoGateInputs) -> bool {
    if inputs.circuit_broken {
        return false;
    }
    if inputs.disabled_by_settings {
        return false;
    }
    if !model_supports_auto_mode(&inputs.model, &inputs.provider) {
        return false;
    }
    true
}

/// `One()` — the denial reason (or `None` when auto mode IS available). NOTE the
/// precedence order differs from [`auto_mode_available`]'s short-circuit order:
/// settings → circuit-breaker → model, exactly as the binary's
/// `One()` — settings → circuit-breaker → model. (2.1.270's `R8` has a fourth
/// tail, `AFn()`'s `fast-mode`; see the module doc for why it is not ported.)
#[must_use]
pub fn auto_mode_denial_reason(inputs: &AutoGateInputs) -> Option<AutoGateDenialReason> {
    if inputs.disabled_by_settings {
        return Some(AutoGateDenialReason::Settings);
    }
    if inputs.circuit_broken {
        return Some(AutoGateDenialReason::CircuitBreaker);
    }
    if !model_supports_auto_mode(&inputs.model, &inputs.provider) {
        return Some(AutoGateDenialReason::Model);
    }
    None
}

/// Apply the gate at mode-load — 1:1 with claude-code `xms`
/// (`if(t==="auto"&&!P0())return"default"`), additionally returning the
/// [`AutoGateDenialReason`] so a caller can surface it (the boot notice /
/// `kickOutOfAutoIfNeeded` notification). When `mode` is not `Auto`, or auto
/// mode is available, the mode passes through unchanged with `None`.
#[must_use]
pub fn apply_auto_mode_gate(
    mode: PermissionMode,
    inputs: &AutoGateInputs,
) -> (PermissionMode, Option<AutoGateDenialReason>) {
    if mode == PermissionMode::Auto && !auto_mode_available(inputs) {
        // `One()` always yields Some here (auto_mode_available is false ⇒ at
        // least one denial branch fires); fall back defensively to Model.
        let reason = auto_mode_denial_reason(inputs).unwrap_or(AutoGateDenialReason::Model);
        return (PermissionMode::Default, Some(reason));
    }
    (mode, None)
}

/// The byte-exact `set_permission_mode` rejection string — 1:1 with claude-code
/// `Nle`: ``Cannot set permission mode to auto: ${Jce(o)}``.
#[must_use]
pub fn cannot_set_auto_message(reason: AutoGateDenialReason) -> String {
    format!("Cannot set permission mode to auto: {}", reason.message())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(model: &str, provider: &str) -> AutoGateInputs {
        AutoGateInputs {
            disabled_by_settings: false,
            circuit_broken: false,
            model: model.to_string(),
            provider: provider.to_string(),
        }
    }

    #[test]
    fn jce_messages_are_byte_exact() {
        assert_eq!(
            AutoGateDenialReason::Settings.message(),
            "auto mode disabled by settings"
        );
        assert_eq!(
            AutoGateDenialReason::CircuitBreaker.message(),
            "auto mode is unavailable for your plan"
        );
        assert_eq!(
            AutoGateDenialReason::Model.message(),
            "auto mode unavailable for this model"
        );
    }

    #[test]
    fn model_gate_shared_exclusions_deny_every_provider() {
        for provider in ["firstParty", "anthropicAws", "bedrock", "vertex"] {
            for model in [
                "claude-3-5-sonnet-20241022",
                "claude-3-opus-20240229",
                "claude-opus-4-0",
                "claude-opus-4-1",
                "claude-opus-4-5",
                "claude-sonnet-4-0",
                "claude-sonnet-4-5",
                "claude-haiku-4-5",
            ] {
                assert!(
                    !model_supports_auto_mode(model, provider),
                    "{model} on {provider} must be denied (shared exclusion)"
                );
            }
        }
    }

    #[test]
    fn model_gate_opus_4_6_and_sonnet_4_6_provider_scoped() {
        // Allowed on firstParty / anthropicAws.
        assert!(model_supports_auto_mode("claude-opus-4-6", "firstParty"));
        assert!(model_supports_auto_mode("claude-opus-4-6", "anthropicAws"));
        assert!(model_supports_auto_mode("claude-sonnet-4-6", "firstParty"));
        assert!(model_supports_auto_mode(
            "claude-sonnet-4-6",
            "anthropicAws"
        ));
        // Denied on any other provider.
        assert!(!model_supports_auto_mode("claude-opus-4-6", "bedrock"));
        assert!(!model_supports_auto_mode("claude-opus-4-6", "vertex"));
        assert!(!model_supports_auto_mode("claude-sonnet-4-6", "gateway"));
    }

    /// 🚨 The gate's "counts as first-party" test is `XD` — `anthropicAws` OR
    /// `anthropicGoogleCloud` — not `anthropicAws` alone. With only the first
    /// arm, a 2.1.270-supported model on `anthropicGoogleCloud` was refused
    /// entry to Auto mode with `auto mode unavailable for this model`. The
    /// tests above all used `anthropicAws`, so nothing was red.
    #[test]
    fn anthropic_google_cloud_is_in_the_first_party_family() {
        assert!(is_anthropic_managed_provider("anthropicAws"));
        assert!(is_anthropic_managed_provider("anthropicGoogleCloud"));
        for provider in ["firstParty", "bedrock", "vertex", "gateway", "foundry"] {
            assert!(
                !is_anthropic_managed_provider(provider),
                "{provider} is not in `XD`"
            );
        }
        for model in ["claude-opus-4-6", "claude-sonnet-4-6", "claude-haiku-4-6"] {
            assert!(
                model_supports_auto_mode(model, "anthropicGoogleCloud"),
                "{model} must be allowed on anthropicGoogleCloud"
            );
            // The same model on a provider OUTSIDE `XD` is still denied, so the
            // fix widens exactly one family and nothing else.
            assert!(!model_supports_auto_mode(model, "vertex"), "{model}");
        }
        // The shared exclusion list still binds inside the family.
        assert!(!model_supports_auto_mode(
            "claude-haiku-4-5",
            "anthropicGoogleCloud"
        ));
        assert!(!model_supports_auto_mode(
            "claude-opus-4-5",
            "anthropicGoogleCloud"
        ));
    }

    #[test]
    fn model_gate_haiku_substring_denied_off_first_party() {
        // Any haiku (that is not in the shared exclusion) is denied off 1P.
        assert!(!model_supports_auto_mode("claude-haiku-4-6", "bedrock"));
        assert!(!model_supports_auto_mode("some-haiku-model", "vertex"));
        // But allowed on firstParty / anthropicAws (not in shared exclusion).
        assert!(model_supports_auto_mode("claude-haiku-4-6", "firstParty"));
        assert!(model_supports_auto_mode("claude-haiku-4-6", "anthropicAws"));
    }

    #[test]
    fn model_gate_current_models_supported() {
        for provider in ["firstParty", "anthropicAws", "bedrock", "vertex"] {
            assert!(model_supports_auto_mode("claude-sonnet-5", provider));
            assert!(model_supports_auto_mode("claude-opus-4-7", provider));
            assert!(model_supports_auto_mode("claude-opus-4-8", provider));
        }
    }

    #[test]
    fn model_gate_matches_318_raw_and_canonical_model_oracle() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/permission_modes_2_1_318.json"
        ))
        .unwrap();
        for row in fixture["models"].as_array().unwrap() {
            let model = row["model"].as_str().unwrap();
            let provider = row["provider"].as_str().unwrap();
            assert_eq!(
                model_supports_auto_mode(model, provider),
                row["expected"].as_bool().unwrap(),
                "{model} on {provider}"
            );
        }
    }

    #[test]
    fn model_identity_matches_318_native_static_fold() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/permission_modes_2_1_318.json"
        ))
        .unwrap();
        for row in fixture["normalization"].as_array().unwrap() {
            let input = row["input"].as_str().unwrap();
            assert_eq!(
                canonical_auto_mode_model(input),
                row["expected"].as_str().unwrap(),
                "{input}"
            );
        }
    }

    #[test]
    fn available_requires_all_three() {
        // All good → available.
        assert!(auto_mode_available(&inputs(
            "claude-sonnet-5",
            "firstParty"
        )));
        // Settings kill.
        let mut i = inputs("claude-sonnet-5", "firstParty");
        i.disabled_by_settings = true;
        assert!(!auto_mode_available(&i));
        // Breaker.
        let mut i = inputs("claude-sonnet-5", "firstParty");
        i.circuit_broken = true;
        assert!(!auto_mode_available(&i));
        // Model.
        assert!(!auto_mode_available(&inputs(
            "claude-sonnet-4-5",
            "firstParty"
        )));
    }

    #[test]
    fn denial_reason_precedence_settings_first() {
        // All denials active → settings wins (One() precedence).
        let mut i = inputs("claude-sonnet-4-5", "firstParty"); // model unsupported
        i.disabled_by_settings = true;
        i.circuit_broken = true;
        assert_eq!(
            auto_mode_denial_reason(&i),
            Some(AutoGateDenialReason::Settings)
        );
    }

    #[test]
    fn denial_reason_circuit_breaker_before_model() {
        let mut i = inputs("claude-sonnet-4-5", "firstParty"); // model unsupported
        i.circuit_broken = true;
        assert_eq!(
            auto_mode_denial_reason(&i),
            Some(AutoGateDenialReason::CircuitBreaker)
        );
    }

    /// 2.1.270's `R8` reports only `settings` / `circuit-breaker` / `model`
    /// (plus the unported `fast-mode` tail). A model-only denial is `Model`;
    /// there is no `provider` tag left to confuse it with, and the string the
    /// deleted one printed occurs ZERO times in the binary.
    #[test]
    fn denial_reason_for_an_unsupported_model_is_model() {
        let i = inputs("claude-opus-4-6", "bedrock"); // model unsupported off-1P
        assert_eq!(
            auto_mode_denial_reason(&i),
            Some(AutoGateDenialReason::Model)
        );
        // The full message map, byte-exact against `KW`.
        assert_eq!(
            AutoGateDenialReason::Settings.message(),
            "auto mode disabled by settings"
        );
        assert_eq!(
            AutoGateDenialReason::CircuitBreaker.message(),
            "auto mode is unavailable for your plan"
        );
        assert_eq!(
            AutoGateDenialReason::Model.message(),
            "auto mode unavailable for this model"
        );
    }

    #[test]
    fn denial_reason_none_when_available() {
        assert_eq!(
            auto_mode_denial_reason(&inputs("claude-sonnet-5", "firstParty")),
            None
        );
    }

    #[test]
    fn gate_downgrades_auto_to_default_when_unavailable() {
        let mut i = inputs("claude-sonnet-5", "firstParty");
        i.disabled_by_settings = true;
        let (mode, reason) = apply_auto_mode_gate(PermissionMode::Auto, &i);
        assert_eq!(mode, PermissionMode::Default);
        assert_eq!(reason, Some(AutoGateDenialReason::Settings));
    }

    #[test]
    fn gate_passes_auto_through_when_available() {
        let (mode, reason) = apply_auto_mode_gate(
            PermissionMode::Auto,
            &inputs("claude-sonnet-5", "firstParty"),
        );
        assert_eq!(mode, PermissionMode::Auto);
        assert_eq!(reason, None);
    }

    #[test]
    fn gate_leaves_non_auto_modes_untouched() {
        // Even with the gate closed, a non-auto mode is never rewritten.
        let mut i = inputs("claude-sonnet-4-5", "bedrock");
        i.disabled_by_settings = true;
        for mode in [
            PermissionMode::Default,
            PermissionMode::Plan,
            PermissionMode::AcceptEdits,
            PermissionMode::BypassPermissions,
            PermissionMode::DontAsk,
        ] {
            let (out, reason) = apply_auto_mode_gate(mode, &i);
            assert_eq!(out, mode);
            assert_eq!(reason, None);
        }
    }

    #[test]
    fn cannot_set_message_is_byte_exact() {
        assert_eq!(
            cannot_set_auto_message(AutoGateDenialReason::Settings),
            "Cannot set permission mode to auto: auto mode disabled by settings"
        );
        assert_eq!(
            cannot_set_auto_message(AutoGateDenialReason::Model),
            "Cannot set permission mode to auto: auto mode unavailable for this model"
        );
    }
}
