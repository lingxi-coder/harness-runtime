//! SKILLEXEC.3 (model scope): resolve a skill's `model:` frontmatter against the
//! session's current main-loop model, carrying the `[1m]` 1M-context suffix over
//! when the target family supports it.
//!
//! 1:1 port of claude-code `resolveSkillModelOverride`
//! (`src/utils/model/model.ts:523-536`) and the helpers it leans on:
//!   - `has1mContext` / `modelSupports1M` (`src/utils/context.ts:35-49`),
//!   - the alias-resolution subset of `parseUserSpecifiedModel`
//!     (`src/utils/model/model.ts:445-505`) reachable from this seam.
//!
//! ## Why `[1m]` must be carried over
//! A skill author writing `model: opus` means "use opus-class reasoning" — not
//! "downgrade to 200K". If the user is on `opus[1m]` and invokes a skill with a
//! bare `model: opus`, passing the alias through unchanged would drop the
//! effective window from 1M to 200K and trip autocompact at ~23% apparent usage.
//! So when the CURRENT model carries `[1m]`, the SKILL model does NOT, and the
//! skill's resolved family supports 1M, we re-append `[1m]`.
//!
//! ## DRIFT NOTES (deliberate, documented deferrals vs. the full TS)
//! - `model_supports_1m` mirrors the LITERAL TS check (`claude-sonnet-4` /
//!   `opus-4-6` substrings). When the catalog default opus moves past 4-6 this
//!   check intentionally stops re-appending `[1m]` for the bare `opus` alias —
//!   exactly as the unchanged TS would, until its `@[MODEL LAUNCH]` pattern is
//!   bumped. Keep this aligned with `compaction::context_window`.
//! - the family default ids (`opus`→`claude-opus-4-8`, …) mirror
//!   `agent::model_resolution::family_default_id` + orchestrator `DEFAULT_MODEL`.
//!   Opus is provider-aware (firstParty→4-8, Bedrock/Vertex/Foundry→4-6).
//!   Keep them aligned when the catalog versions change.
//! - the ant-model registry, the legacy Opus 4.0/4.1 first-party remap, and the
//!   Foundry deployment-id passthrough branches of `parseUserSpecifiedModel` are
//!   NOT modeled — none is reachable from a skill `model:` string in this port.

use std::env;

use lingxi_core::host::env::is_env_truthy;

/// Resolve `skill_model` against `current_model`, carrying `[1m]` over when the
/// target family supports it. 1:1 with `resolveSkillModelOverride`
/// (`model.ts:523-536`).
#[must_use]
pub fn resolve_skill_model_override(skill_model: &str, current_model: &str) -> String {
    // Already explicitly 1M, or the current model is not 1M → pass through.
    if has_1m_context(skill_model) || !has_1m_context(current_model) {
        return skill_model.to_string();
    }
    // The current model is 1M but the skill model is a bare alias / id without
    // the suffix: re-append `[1m]` iff the resolved family supports it.
    if model_supports_1m(&parse_user_specified_model(skill_model)) {
        return format!("{skill_model}[1m]");
    }
    skill_model.to_string()
}

/// Mirrors `is1mContextDisabled` (`utils/context.ts:31-33`).
fn is_1m_context_disabled() -> bool {
    is_env_truthy(env::var("CLAUDE_CODE_DISABLE_1M_CONTEXT").ok().as_deref())
}

/// `true` if `model` carries an explicit `[1m]` suffix (case-insensitive),
/// unless 1M context is disabled. Mirrors `has1mContext` (`context.ts:35-40`).
fn has_1m_context(model: &str) -> bool {
    if is_1m_context_disabled() {
        return false;
    }
    // TS uses /\[1m\]/i — a case-insensitive substring of the literal "[1m]".
    model.to_lowercase().contains("[1m]")
}

/// `true` if the canonical model family supports 1M context (sonnet-4 family,
/// sonnet-5, or opus-4-6), unless 1M context is disabled. Mirrors
/// `modelSupports1M` (2.1.198 registry: `claude-sonnet-5` is natively 1M and
/// carries `supports_1m_beta:!0` — `sonnet-5[1m]` is a valid suffixed id).
fn model_supports_1m(model: &str) -> bool {
    if is_1m_context_disabled() {
        return false;
    }
    let canonical = canonical_name(model);
    canonical.contains("claude-sonnet-4")
        || canonical.contains("claude-sonnet-5")
        || canonical.contains("opus-4-6")
}

/// Family default concrete id for a bare alias. Mirrors `getDefault*Model()`
/// resolution; see the module DRIFT NOTE on keeping the catalog aligned.
fn family_default_id(family_lower: &str) -> Option<&'static str> {
    match family_lower {
        "opus" => Some("claude-opus-4-8"),
        // 2.1.198 alias table: sonnet.default = "claude-sonnet-5" (was 4-6).
        "sonnet" => Some("claude-sonnet-5"),
        "haiku" => Some("claude-haiku-4-5"),
        _ => None,
    }
}

/// The Sonnet family default id for non-firstParty (3P) providers
/// (`getModelStrings().sonnet45`, canonical `claude-sonnet-4-5-20250929`,
/// `configs.ts:45`). See `agent::model_resolution::SONNET_3P_DEFAULT_ID` — kept
/// in sync.
///
/// NB: for THIS seam the choice is a pure-internal no-op — the resolved id only
/// feeds `model_supports_1m`, and both `claude-sonnet-4-6` and
/// `claude-sonnet-4-5-20250929` canonicalize to a `claude-sonnet-4*` family that
/// supports 1M, so `resolve_skill_model_override`'s OUTPUT is byte-identical
/// either way. The provider-aware branch is mirrored anyway to keep the two
/// `parseUserSpecifiedModel` ports structurally identical (matching claude's
/// `getDefaultSonnetModel`) and prevent future drift.
const SONNET_3P_DEFAULT_ID: &str = "claude-sonnet-4-5-20250929";

/// `getDefault*Model()` (`model.ts`): the `ANTHROPIC_DEFAULT_{OPUS,SONNET,HAIKU}_MODEL`
/// env override (when non-empty) wins, else the built-in family default. The
/// skill `model:` alias path MUST route through these so a custom default flows
/// through, matching claude's `resolveSkillModelOverride` → `parseUserSpecifiedModel`
/// → `getDefault*Model()`. Mirrors `agent::model_resolution::env_default`.
///
/// Used for opus/haiku, whose defaults do NOT diverge by provider. Sonnet is
/// provider-aware — see [`default_sonnet_model`].
fn env_default(env_var: &str, family_lower: &str) -> String {
    if let Some(v) = env::var(env_var).ok().filter(|s| !s.is_empty()) {
        return v;
    }
    family_default_id(family_lower)
        .expect("env_default called with a known family")
        .to_string()
}

/// `getDefaultSonnetModel()` (`model.ts:118-128`): env override (non-empty) wins;
/// else provider-aware — `claude-sonnet-4-5-20250929` for non-firstParty
/// (Bedrock/Vertex/Foundry), `claude-sonnet-5` for firstParty (2.1.198 alias
/// table). Mirrors `agent::model_resolution::get_default_sonnet_model`.
fn default_sonnet_model() -> String {
    if let Some(v) = env::var("ANTHROPIC_DEFAULT_SONNET_MODEL")
        .ok()
        .filter(|s| !s.is_empty())
    {
        return v;
    }
    if api_provider_is_first_party() {
        return family_default_id("sonnet")
            .expect("sonnet is a known family")
            .to_string();
    }
    SONNET_3P_DEFAULT_ID.to_string()
}

/// The Opus family default id for the Foundry provider. Per the 2.1.207 alias
/// table (`opus.per_provider = {bedrock/vertex/mantle/anthropic_aws: 4-8,
/// foundry: 4-6, gateway: 4-7}`, default 4-8), Bedrock/Vertex joined the 4-8
/// default and ONLY Foundry stays on 4-6. Mirrors
/// `agent::model_resolution::OPUS_FOUNDRY_DEFAULT_ID`.
const OPUS_FOUNDRY_DEFAULT_ID: &str = "claude-opus-4-6";

/// `getDefaultOpusModel()` (`db()`/`nJe()` in 2.1.207): env override (non-empty)
/// wins; else the alias table resolves `COn("opus", provider) ?? opus48` — only
/// Foundry (`claude-opus-4-6`) diverges from the `claude-opus-4-8` default;
/// firstParty/Bedrock/Vertex all get 4-8. Mirrors
/// `agent::model_resolution::get_default_opus_model`.
fn default_opus_model() -> String {
    if let Some(v) = env::var("ANTHROPIC_DEFAULT_OPUS_MODEL")
        .ok()
        .filter(|s| !s.is_empty())
    {
        return v;
    }
    if api_provider_is_foundry() {
        return OPUS_FOUNDRY_DEFAULT_ID.to_string();
    }
    family_default_id("opus")
        .expect("opus is a known family")
        .to_string()
}

/// `getAPIProvider() === 'foundry'`: precedence `bedrock > foundry > … > vertex >
/// firstParty`, so Foundry iff `CLAUDE_CODE_USE_BEDROCK` falsy AND
/// `CLAUDE_CODE_USE_FOUNDRY` env-truthy. Mirrors
/// `agent::model_resolution::api_provider_is_foundry`.
fn api_provider_is_foundry() -> bool {
    !is_env_truthy(env::var("CLAUDE_CODE_USE_BEDROCK").ok().as_deref())
        && is_env_truthy(env::var("CLAUDE_CODE_USE_FOUNDRY").ok().as_deref())
}

/// `getAPIProvider() === 'firstParty'` (`providers.ts:6-13`): first-party iff none
/// of `CLAUDE_CODE_USE_BEDROCK` / `_VERTEX` / `_FOUNDRY` is env-truthy (strict
/// allowlist). Mirrors `agent::model_resolution::api_provider_is_first_party`.
fn api_provider_is_first_party() -> bool {
    !is_env_truthy(env::var("CLAUDE_CODE_USE_BEDROCK").ok().as_deref())
        && !is_env_truthy(env::var("CLAUDE_CODE_USE_VERTEX").ok().as_deref())
        && !is_env_truthy(env::var("CLAUDE_CODE_USE_FOUNDRY").ok().as_deref())
}

/// Resolve a user/skill-specified model string to a concrete id — the
/// alias-resolution subset of `parseUserSpecifiedModel` (`model.ts:445-505`)
/// that this seam can reach. Bare family aliases (`opus` / `sonnet` / `haiku` /
/// `opusplan`) map to their default id (preserving a trailing `[1m]`); every
/// other string passes through with only `[1m]` normalized. See module DRIFT
/// NOTE for the unmodeled branches.
fn parse_user_specified_model(model_input: &str) -> String {
    let trimmed = model_input.trim();
    let normalized = trimmed.to_lowercase();
    let has_1m_tag = has_1m_context(&normalized);
    let base = strip_1m_suffix(&normalized);
    let suffix = if has_1m_tag { "[1m]" } else { "" };

    match base.as_str() {
        // `opusplan` → Sonnet default (Opus only in plan mode), 1:1 with TS.
        // Routed through the env-aware (and provider-aware, #18) helpers so
        // `ANTHROPIC_DEFAULT_*_MODEL` overrides + the 3P Sonnet default take effect
        // on the skill `model:` path.
        "opusplan" | "sonnet" => format!("{}{suffix}", default_sonnet_model()),
        "haiku" => format!(
            "{}{suffix}",
            env_default("ANTHROPIC_DEFAULT_HAIKU_MODEL", "haiku")
        ),
        // #2: Opus is provider-aware in 2.1.193 (firstParty→4-8, 3P→4-6).
        "opus" => format!("{}{suffix}", default_opus_model()),
        _ => {
            // Non-alias: preserve the original case, normalizing only `[1m]`.
            if has_1m_tag {
                format!("{}[1m]", strip_1m_suffix(trimmed))
            } else {
                trimmed.to_string()
            }
        }
    }
}

/// Strip a single trailing `[1m]` (case-insensitive) and trim. Mirrors the TS
/// `replace(/\[1m]$/i, '').trim()`.
fn strip_1m_suffix(s: &str) -> String {
    if s.to_lowercase().ends_with("[1m]") {
        s[..s.len() - 4].trim().to_string()
    } else {
        s.trim().to_string()
    }
}

/// Resolve a full model id to a shorter canonical family name. Faithful copy of
/// `compaction::context_window::canonical_name` (ports `firstPartyNameToCanonical`,
/// `model.ts:217-270`); the Bedrock-ARN indirection is a no-op for our substring
/// checks, so it is folded in.
fn canonical_name(model: &str) -> String {
    let name = model.to_lowercase();
    // Order matters: check more specific versions first (4-6 before 4-5 before 4).
    if name.contains("claude-opus-4-6") {
        return "claude-opus-4-6".to_string();
    }
    if name.contains("claude-opus-4-5") {
        return "claude-opus-4-5".to_string();
    }
    if name.contains("claude-opus-4-1") {
        return "claude-opus-4-1".to_string();
    }
    if name.contains("claude-opus-4") {
        return "claude-opus-4".to_string();
    }
    // sonnet-5 before the sonnet-4-x arms (2.1.198; mutually exclusive —
    // "claude-sonnet-4-5" does NOT contain "sonnet-5").
    if name.contains("claude-sonnet-5") {
        return "claude-sonnet-5".to_string();
    }
    if name.contains("claude-sonnet-4-6") {
        return "claude-sonnet-4-6".to_string();
    }
    if name.contains("claude-sonnet-4-5") {
        return "claude-sonnet-4-5".to_string();
    }
    if name.contains("claude-sonnet-4") {
        return "claude-sonnet-4".to_string();
    }
    if name.contains("claude-haiku-4-5") {
        return "claude-haiku-4-5".to_string();
    }
    if name.contains("claude-3-7-sonnet") {
        return "claude-3-7-sonnet".to_string();
    }
    if name.contains("claude-3-5-sonnet") {
        return "claude-3-5-sonnet".to_string();
    }
    if name.contains("claude-3-5-haiku") {
        return "claude-3-5-haiku".to_string();
    }
    if name.contains("claude-3-opus") {
        return "claude-3-opus".to_string();
    }
    if name.contains("claude-3-sonnet") {
        return "claude-3-sonnet".to_string();
    }
    if name.contains("claude-3-haiku") {
        return "claude-3-haiku".to_string();
    }
    // Fall back to the lowercased input when no pattern matches (the TS regex
    // only narrows the unmatched case; substring checks are equivalent here).
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- env-var serialization (#18 provider-aware Sonnet default) ----------
    //
    // CLAUDE_CODE_USE_{BEDROCK,VERTEX,FOUNDRY} / ANTHROPIC_DEFAULT_SONNET_MODEL
    // mutate process-global env. cargo runs tests in parallel within a crate, so
    // env-mutating tests share one Mutex and restore on Drop.
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
    }
    impl EnvGuard {
        fn set(key: &'static str, val: &str) -> Self {
            let prev = env::var(key).ok();
            env::set_var(key, val);
            Self { key, prev }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => env::set_var(self.key, v),
                None => env::remove_var(self.key),
            }
        }
    }

    /// Clear all three provider env vars (restored on Drop), pinning firstParty.
    fn clear_provider_env() -> [EnvGuard; 3] {
        let mk = |k: &'static str| {
            let g = EnvGuard {
                key: k,
                prev: env::var(k).ok(),
            };
            env::remove_var(k);
            g
        };
        [
            mk("CLAUDE_CODE_USE_BEDROCK"),
            mk("CLAUDE_CODE_USE_VERTEX"),
            mk("CLAUDE_CODE_USE_FOUNDRY"),
        ]
    }

    #[test]
    fn skill_model_already_1m_passes_through() {
        // The skill explicitly asked for 1M → never touched.
        assert_eq!(
            resolve_skill_model_override("claude-sonnet-4-6[1m]", "claude-opus-4-6[1m]"),
            "claude-sonnet-4-6[1m]"
        );
    }

    #[test]
    fn current_model_not_1m_passes_through() {
        // Session is plain 200K → no [1m] to carry over → unchanged.
        assert_eq!(
            resolve_skill_model_override("claude-sonnet-4-6", "claude-opus-4-6"),
            "claude-sonnet-4-6"
        );
        assert_eq!(
            resolve_skill_model_override("opus", "claude-opus-4-6"),
            "opus"
        );
    }

    #[test]
    fn carries_1m_when_current_is_1m_and_family_supports_it() {
        // Full id whose family supports 1M → re-append [1m].
        assert_eq!(
            resolve_skill_model_override("claude-sonnet-4-6", "claude-opus-4-6[1m]"),
            "claude-sonnet-4-6[1m]"
        );
    }

    #[test]
    fn bare_alias_resolves_then_carries_1m() {
        // `sonnet` resolves to the Sonnet default (supports 1M) → [1m] re-appended,
        // matching the TS `parseUserSpecifiedModel` pre-resolution step. The
        // resolution reads provider + ANTHROPIC_DEFAULT_SONNET_MODEL env (#18), so
        // pin firstParty with no override under the ENV_LOCK to keep it hermetic.
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = clear_provider_env();
        let _o = EnvGuard {
            key: "ANTHROPIC_DEFAULT_SONNET_MODEL",
            prev: env::var("ANTHROPIC_DEFAULT_SONNET_MODEL").ok(),
        };
        env::remove_var("ANTHROPIC_DEFAULT_SONNET_MODEL");
        assert_eq!(
            resolve_skill_model_override("sonnet", "claude-opus-4-6[1m]"),
            "sonnet[1m]"
        );
    }

    #[test]
    fn family_without_1m_variant_does_not_carry() {
        // haiku has no 1M variant → downgrade stands, no [1m] re-appended.
        assert_eq!(
            resolve_skill_model_override("haiku", "claude-opus-4-6[1m]"),
            "haiku"
        );
        assert_eq!(
            resolve_skill_model_override("claude-haiku-4-5", "claude-sonnet-4-6[1m]"),
            "claude-haiku-4-5"
        );
    }

    #[test]
    fn sonnet_5_supports_1m_carry() {
        // 2.1.198: claude-sonnet-5 is a 1M family → the session's [1m] carries
        // over (sonnet-5[1m] is a valid suffixed id in the binary).
        assert_eq!(
            resolve_skill_model_override("claude-sonnet-5", "claude-opus-4-6[1m]"),
            "claude-sonnet-5[1m]"
        );
        assert!(model_supports_1m("claude-sonnet-5"));
        // Hazard lock: neighbors resolve through their own arms.
        assert!(model_supports_1m("claude-sonnet-4-5-20250929"));
        assert!(!model_supports_1m("claude-haiku-4-5"));
    }

    #[test]
    fn parse_resolves_known_aliases() {
        // The `sonnet`/`opusplan` arms are provider-aware (#18) → pin firstParty
        // under the ENV_LOCK to assert the 1P ids deterministically.
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = clear_provider_env();
        assert_eq!(parse_user_specified_model("opus"), "claude-opus-4-8");
        // 2.1.198: the bare `sonnet` alias (and `opusplan` outside plan mode)
        // resolves to claude-sonnet-5 on firstParty.
        assert_eq!(parse_user_specified_model("sonnet"), "claude-sonnet-5");
        assert_eq!(parse_user_specified_model("haiku"), "claude-haiku-4-5");
        assert_eq!(parse_user_specified_model("opusplan"), "claude-sonnet-5");
        // Unknown / full id passes through (case preserved).
        assert_eq!(
            parse_user_specified_model("claude-sonnet-4-6"),
            "claude-sonnet-4-6"
        );
        assert_eq!(parse_user_specified_model("My-Custom-ID"), "My-Custom-ID");
    }

    // ---- #18: 3P-provider Sonnet default on the skill `model:` path ----------

    #[test]
    fn parse_sonnet_alias_is_3p_default_on_bedrock() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = clear_provider_env();
        let _b = EnvGuard::set("CLAUDE_CODE_USE_BEDROCK", "1");
        assert_eq!(
            parse_user_specified_model("sonnet"),
            "claude-sonnet-4-5-20250929"
        );
        // `opusplan` resolves to the Sonnet default → same 3P id.
        assert_eq!(
            parse_user_specified_model("opusplan"),
            "claude-sonnet-4-5-20250929"
        );
    }

    #[test]
    fn parse_opus_alias_is_provider_aware_2_1_207() {
        // 2.1.207 opus alias table: default 4-8, per_provider foundry → 4-6,
        // bedrock/vertex → 4-8. Only Foundry diverges.
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = clear_provider_env();
        assert_eq!(parse_user_specified_model("opus"), "claude-opus-4-8");
        {
            let _b = EnvGuard::set("CLAUDE_CODE_USE_BEDROCK", "1");
            assert_eq!(parse_user_specified_model("opus"), "claude-opus-4-8");
        }
        {
            let _v = EnvGuard::set("CLAUDE_CODE_USE_VERTEX", "1");
            assert_eq!(parse_user_specified_model("opus"), "claude-opus-4-8");
        }
        {
            let _f = EnvGuard::set("CLAUDE_CODE_USE_FOUNDRY", "1");
            assert_eq!(parse_user_specified_model("opus"), "claude-opus-4-6");
            // Bedrock outranks Foundry in the provider precedence chain.
            let _b = EnvGuard::set("CLAUDE_CODE_USE_BEDROCK", "1");
            assert_eq!(parse_user_specified_model("opus"), "claude-opus-4-8");
        }
    }

    #[test]
    fn parse_sonnet_alias_is_1p_default_on_vertex_when_env_override_set() {
        // ANTHROPIC_DEFAULT_SONNET_MODEL beats the 3P provider branch.
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = clear_provider_env();
        let _v = EnvGuard::set("CLAUDE_CODE_USE_VERTEX", "1");
        let _o = EnvGuard::set("ANTHROPIC_DEFAULT_SONNET_MODEL", "custom-sonnet-id");
        assert_eq!(parse_user_specified_model("sonnet"), "custom-sonnet-id");
    }

    #[test]
    fn skill_override_output_is_provider_invariant() {
        // The provider-aware 3P Sonnet id is a pure-internal discriminator here:
        // resolve_skill_model_override's OUTPUT is byte-identical on firstParty and
        // on a 3P provider, because both Sonnet defaults canonicalize to a
        // `claude-sonnet-4*` family that supports 1M.
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = clear_provider_env();
        let fp = resolve_skill_model_override("sonnet", "claude-opus-4-6[1m]");
        let _b = EnvGuard::set("CLAUDE_CODE_USE_BEDROCK", "1");
        let tp = resolve_skill_model_override("sonnet", "claude-opus-4-6[1m]");
        assert_eq!(fp, "sonnet[1m]");
        assert_eq!(fp, tp);
    }
}
