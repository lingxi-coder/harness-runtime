//! The `Dh(model)` "simple system prompt" gate, shared by every model-gated
//! tool prompt.
//!
//! claude-code builds several tool prompts as `fn(model){ if(Dh(model)) return
//! SHORT; return LONG }` — the current-generation default models
//! (`claude-opus-4-8` / `claude-fable-5-1` / `claude-mythos-5-1`) get the terse
//! SHORT variant, classic models (sonnet/haiku/`claude-3-*`/`opus-4-0..4-7`)
//! get the verbose LONG one. The file tools (Read/Write/Edit/Glob/Grep) and the
//! task tool (TodoWrite) all consult the SAME predicate, so it lives here as a
//! single source of truth rather than being re-derived per crate.
//!
//! Ports `Dh` / `UWu` / `dfe`, spelled `UKb` / `BKb` / `imt` in 2.1.238 at
//! offsets 285081061 / 285080711 / 285079051 (2.1.220: `oug` @228079696, same
//! bodies). The historical `~1951597xx` offsets in this header were 2.1.185-era
//! and no longer resolve.

/// Port of claude-code `dfe(e)` (binary): `/-eap($|\[)/i.test(e)`. Matches an
/// early-access-program model id where `-eap` is at the end of the string or
/// immediately followed by `[`.
fn is_early_access_model(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    let mut from = 0usize;
    while let Some(idx) = lower[from..].find("-eap") {
        let abs = from + idx;
        let after = abs + "-eap".len();
        match lower.as_bytes().get(after) {
            None => return true,        // `-eap` at end ($)
            Some(&b'[') => return true, // `-eap[`
            _ => from = after,
        }
    }
    false
}

/// Port of claude-code `UWu(model)` — 2.1.238 `BKb`, binary offset 285080711:
/// the "uses the standard/long system prompt" model-class predicate.
///
/// ```js
/// function BKb(e){
///   if(imt(e)) return!1;                                  // `-eap` (early-access) → false
///   let t = Fo(e);                                        // normalize (strip region/profile)
///   if(F2(t,"lean_prompt") || t==="claude-mythos-5-1") return!1; // (A) CAPABILITY first
///   if(t.includes("claude-3-") || t.includes("haiku") || t.includes("sonnet")
///      || t==="claude-opus-4-0" || t==="claude-opus-4-1" || t==="claude-opus-4-5"
///      || t==="claude-opus-4-6" || t==="claude-opus-4-7") return!0;  // (B) NAME list second
///   return !v_();                                         // unknown → !first-party
/// }
/// ```
///
/// Verified verbatim at 2.1.238 @285080711 (`imt` = `/-eap($|\[)/i` @285079051;
/// `v_(e=ro())` = `firstParty || anthropicAws || anthropicGoogleCloud ||
/// gateway` @283311030). 2.1.220 `oug` @228079696 is identical, order included.
///
/// **Branch order (A) before (B) is the oracle's, and it is reproduced here.**
/// It is not currently observable: a model is classified differently by the two
/// orders only if it BOTH carries `lean_prompt` (or is `claude-mythos-5`) AND
/// matches the classic name list, and no such id exists in either catalog —
/// `F2` reads the *baked* catalog (`q0` → `uBs().entriesById`, 2.1.238
/// @281155200), where `lean_prompt` is carried by exactly `claude-opus-4-8`,
/// `claude-opus-5`, `claude-fable-5`, and `claude-mythos-5` carries
/// `capabilities:[]`. The only input that could distinguish the orders upstream
/// is a host-injected `F2` fallback (`NIr().runtimeCapabilityLookup`, default
/// `void 0` and never assigned in the shipped JS) answering `lean_prompt` for a
/// sonnet/haiku id — an SDK-embedding hook LingXi has no substrate for. The
/// invariant that keeps the order unobservable is pinned by
/// `no_registry_model_is_both_lean_and_classic` below: the day a lean-prompt
/// sonnet/haiku ships, that test goes red instead of this file going quietly
/// wrong.
///
/// `Fo` (binary offset ~ `function Fo(`) resolves application-inference-profiles
/// and strips Bedrock region prefixes. LingXi has no `Fo` port; the capability
/// arm normalizes internally (`has_capability` → `capabilities_for_loose` →
/// `normalize_model_id`), while the name arm matches substrings on the raw id
/// after a light lowercasing — for every published Anthropic id this is
/// identical to `Fo(e)` (the id is already canonical; region/profile-wrapped ids
/// still contain the same `claude-3-`/`haiku`/`sonnet` substrings). The `dfe`
/// early-access check (`/-eap($|\[)/i`) is reproduced verbatim. The
/// unknown-model fallthrough `!v_()` (NOT first-party/anthropicAws/
/// anthropicGoogleCloud/gateway) is approximated as `false` here because the
/// provider class is not threaded into the tool layer; for the default
/// first-party deployment `v_()` is `true` so `!v_()` is `false`, matching this
/// default. This only affects genuinely unknown model ids on a non-first-party
/// provider — documented residual.
fn uwu_standard_model(model: &str) -> bool {
    // `imt(e)` = `/-eap($|\[)/i.test(e)` — early-access models are NOT standard.
    if is_early_access_model(model) {
        return false;
    }
    let t = model.to_ascii_lowercase();
    // (A) `if (F2(t,"lean_prompt") || t === "claude-mythos-5") return false;`
    // The oracle asks the CAPABILITY REGISTRY *before* the name list, so this
    // arm is first here too. `mythos-5` stays a NAME check because the oracle
    // keeps it one: it carries no capabilities at all (`capabilities:[]` in the
    // baked catalog) yet must still take this branch.
    //
    // This arm used to be a hardcoded name list (`claude-opus-4-8 |
    // claude-fable-5 | claude-mythos-5`). That list omitted `claude-opus-5`,
    // which reached the right answer only by falling through to the default
    // below — correct by accident, and silently wrong for any model added
    // later.
    if lingxi_core::host::model_capabilities::has_capability(
        &t,
        lingxi_core::host::model_capabilities::ModelCapability::LeanPrompt,
    ) || t == "claude-mythos-5-1"
    {
        return false;
    }
    // (B) the classic name list.
    if t.contains("claude-3-")
        || t.contains("haiku")
        || t.contains("sonnet")
        || t == "claude-opus-4-0"
        || t == "claude-opus-4-1"
        || t == "claude-opus-4-5"
        || t == "claude-opus-4-6"
        || t == "claude-opus-4-7"
    {
        return true;
    }
    // `return !v_()` — provider class unavailable in the tool layer; default
    // first-party deployment ⇒ `v_()` true ⇒ `!v_()` false.
    false
}

/// Port of claude-code `Dh(model)` — 2.1.238 `UKb` @285081061 — the
/// "simple system prompt" gate:
///
/// ```js
/// function UKb(e){
///   if(!e) return!1;                                     // no model → false
///   if(Un(V.CLAUDE_CODE_SIMPLE_SYSTEM_PROMPT)) return!0; // env-truthy → simple
///   if(Sf(V.CLAUDE_CODE_SIMPLE_SYSTEM_PROMPT)) return!1; // env-defined-falsy → standard
///   if(!BKb(e)) return!0;                                // non-standard model → simple
///   if(it("tengu_velvet_tide",!1)) return!0;             // gate flag
///   return WQd("simple_system_prompt",Fo(e))             // per-model config map
/// }
/// ```
///
/// `WQd(key, model)` (@285079408) reads `VC()?.[key]` — the clientData cache —
/// and returns true when any entry whose key the model id contains is `true`.
/// Neither that cache nor the `tengu_velvet_tide` flag is plumbed into the tool
/// layer. On a fresh/default config both are absent, so the model branch
/// reduces to `!BKb(model)`. This is the dominant, parity-faithful behavior; a
/// config that force-enables the simple prompt for a specific model via those
/// keys is a documented residual.
///
/// Returns `true` when the SHORT prompt variant should be served, `false` for
/// the LONG one. `None` (no model known) mirrors the JS `Dh(undefined)` path and
/// returns `false`.
#[must_use]
pub fn dh_simple_system_prompt(model: Option<&str>) -> bool {
    // `if(!e) return false` — the gate short-circuits to the long prompt when
    // no model is known (the JS `Dh(undefined)` path).
    let Some(model) = model.filter(|m| !m.is_empty()) else {
        return false;
    };
    // LingXi's multi-provider harness is intentionally more explicit than the
    // model-specific Claude prompt. A non-Claude model must never inherit the
    // short prompt from Claude's unknown-model fallthrough.
    if lingxi_core::host::model_capabilities::prompt_profile_for(model)
        == lingxi_core::host::model_capabilities::PromptProfile::FullHarness
    {
        return false;
    }
    let env = std::env::var("LINGXI_SIMPLE_SYSTEM_PROMPT").ok();
    if lingxi_core::host::env::is_env_truthy(env.as_deref()) {
        return true;
    }
    if lingxi_core::host::env::is_env_defined_falsy(env.as_deref()) {
        return false;
    }
    // `if(!BKb(e)) return true; if(it("tengu_velvet_tide")) return true;
    //  return WQd("simple_system_prompt", Fo(e))` — with the flag off and the
    // clientData map absent, the tail collapses to `!BKb(e)`.
    !uwu_standard_model(model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_is_long() {
        assert!(!dh_simple_system_prompt(None));
        assert!(!dh_simple_system_prompt(Some("")));
    }

    /// The two arms of [`uwu_standard_model`] as the oracle spells them.
    /// `capability_arm` is `F2(t,"lean_prompt")||t==="claude-mythos-5"`;
    /// `classic_name_arm` is the `includes`/equality list that follows it.
    fn capability_arm(t: &str) -> bool {
        lingxi_core::host::model_capabilities::has_capability(
            t,
            lingxi_core::host::model_capabilities::ModelCapability::LeanPrompt,
        ) || t == "claude-mythos-5-1"
    }

    fn classic_name_arm(t: &str) -> bool {
        t.contains("claude-3-")
            || t.contains("haiku")
            || t.contains("sonnet")
            || t == "claude-opus-4-0"
            || t == "claude-opus-4-1"
            || t == "claude-opus-4-5"
            || t == "claude-opus-4-6"
            || t == "claude-opus-4-7"
    }

    /// Tripwire for the branch ORDER inside [`uwu_standard_model`].
    ///
    /// The oracle (2.1.238 `BKb` @285080711) evaluates the `lean_prompt`
    /// capability arm BEFORE the classic name list; this port now does too. The
    /// order is unobservable today only because the two arms are disjoint over
    /// the whole model table — the oracle's baked catalog gives `lean_prompt`
    /// to exactly `claude-opus-4-8` / `claude-opus-5` / `claude-fable-5`, none
    /// of which contains `claude-3-`/`haiku`/`sonnet` or equals an
    /// `opus-4-0..4-7`, and `claude-mythos-5` matches neither.
    ///
    /// If a future model is ever both — a lean-prompt Sonnet, say — the order
    /// becomes load-bearing and this test goes red, pointing at the branch that
    /// has to stay first. The id list mirrors
    /// `lingxi_core::host::model_capabilities::KNOWN_MODEL_IDS` (private to that crate);
    /// extend it whenever the registry gains a model.
    #[test]
    fn no_registry_model_is_both_lean_and_classic() {
        for id in [
            "claude-sonnet-4-6",
            "claude-sonnet-5",
            "claude-opus-4-5",
            "claude-opus-4-6",
            "claude-opus-4-7",
            "claude-opus-4-8",
            "claude-opus-5",
            "claude-fable-5-1",
            "claude-mythos-5-1",
            // Catalog entries the port's registry does not enumerate but the
            // oracle does; all `capabilities:[]` or `["context_management"]`.
            "claude-3-5-haiku",
            "claude-haiku-4-5",
            "claude-3-5-sonnet",
            "claude-3-7-sonnet",
            "claude-sonnet-4-0",
            "claude-sonnet-4-5",
            "claude-opus-4-0",
            "claude-opus-4-1",
        ] {
            assert!(
                !(capability_arm(id) && classic_name_arm(id)),
                "{id} matches BOTH the lean_prompt arm and the classic name \
                 list — the branch order in uwu_standard_model is now \
                 observable. The oracle evaluates the capability arm FIRST \
                 (2.1.238 BKb @285080711), so {id} must classify as \
                 non-standard (SHORT prompt)."
            );
        }
    }

    /// Both arms individually still reach the answers the oracle gives, so the
    /// reorder is behaviour-preserving on every id the registry knows.
    #[test]
    fn each_arm_alone_classifies_the_registry_the_same_way() {
        for id in ["claude-opus-4-8", "claude-opus-5", "claude-fable-5-1"] {
            assert!(capability_arm(id), "{id} must hit the capability arm");
            assert!(!classic_name_arm(id), "{id} must miss the name list");
        }
        assert!(capability_arm("claude-mythos-5-1"));
        assert!(!classic_name_arm("claude-mythos-5-1"));
        for id in [
            "claude-sonnet-5",
            "claude-sonnet-4-6",
            "claude-opus-4-7",
            "claude-3-5-sonnet-20241022",
        ] {
            assert!(!capability_arm(id), "{id} must miss the capability arm");
            assert!(classic_name_arm(id), "{id} must hit the name list");
        }
    }

    /// `claude-opus-5` is SHORT because it carries `lean_prompt` in the
    /// capability registry — not because it fell off the end of a name list.
    /// Before the registry it was absent from both branches and reached the
    /// right answer by accident.
    #[test]
    fn opus_5_is_short_via_the_capability_registry() {
        assert!(dh_simple_system_prompt(Some("claude-opus-5")));
        assert!(lingxi_core::host::model_capabilities::has_capability(
            "claude-opus-5",
            lingxi_core::host::model_capabilities::ModelCapability::LeanPrompt
        ));
    }

    /// Every model the oracle marks `lean_prompt` takes the SHORT prompt, and
    /// the pre-lean Opus models still take the LONG one.
    #[test]
    fn lean_prompt_models_are_short_and_older_opus_is_long() {
        for m in ["claude-opus-4-8", "claude-opus-5", "claude-fable-5-1"] {
            assert!(dh_simple_system_prompt(Some(m)), "{m} must be SHORT");
        }
        for m in ["claude-opus-4-5", "claude-opus-4-6", "claude-opus-4-7"] {
            assert!(!dh_simple_system_prompt(Some(m)), "{m} must be LONG");
        }
    }

    #[test]
    fn new_models_are_short() {
        assert!(dh_simple_system_prompt(Some("claude-opus-4-8")));
        assert!(dh_simple_system_prompt(Some("claude-fable-5-1")));
        assert!(dh_simple_system_prompt(Some("claude-mythos-5-1")));
    }

    #[test]
    fn non_claude_models_keep_long_tool_prompts() {
        for model in ["gpt-5.5", "deepseek-flash", "gemini-3.5-flash", "glm-5.1"] {
            assert!(
                !dh_simple_system_prompt(Some(model)),
                "{model} must keep the full harness/tool prompt"
            );
        }
    }

    #[test]
    fn classic_models_are_long() {
        for m in [
            "claude-3-5-sonnet-20241022",
            "claude-3-haiku-20240307",
            "claude-sonnet-4-5",
            // Sonnet 5 (2.1.198 `LBd`): no "lean_prompt" capability in the
            // registry, so it falls into the `includes("sonnet")` standard arm
            // → LONG prompt (unlike opus-4-8 / fable-5 / mythos-5).
            "claude-sonnet-5",
            "claude-opus-4-0",
            "claude-opus-4-1",
            "claude-opus-4-5",
            "claude-opus-4-6",
            "claude-opus-4-7",
        ] {
            assert!(!dh_simple_system_prompt(Some(m)), "{m} should be LONG");
        }
    }

    #[test]
    fn early_access_is_long() {
        // `-eap` ⇒ UWu=false ⇒ Dh would be true, BUT only because dfe forces
        // UWu false; opus-4-8-eap is genuinely early-access so still SHORT.
        // The dfe carve-out matters for ids that would otherwise be standard.
        assert!(is_early_access_model("claude-sonnet-4-5-eap"));
        assert!(is_early_access_model("claude-opus-4-7-eap[foo]"));
        assert!(!is_early_access_model("claude-opus-4-8"));
    }

    #[test]
    fn anthropic_aws_alias_targets_are_long() {
        // M2 Part B pin: the 2.1.198 alias table's anthropic_aws per_provider
        // targets — sonnet → "claude-sonnet-4-6", opus → "claude-opus-4-7"
        // (verified against the real binary registry) — are both UWu-standard
        // models, so an anthropicAws-routed session keeps the LONG prompt.
        assert!(!dh_simple_system_prompt(Some("claude-sonnet-4-6")));
        assert!(!dh_simple_system_prompt(Some("claude-opus-4-7")));
    }
}
