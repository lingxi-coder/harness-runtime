//! Thinking-mode model predicates and per-request thinking configuration.
//!
//! Ports claude-code's thinking gating from `services/api/claude.ts:1596-1630`
//! (`modelSupportsThinking`, `modelSupportsAdaptiveThinking`,
//! `getMaxThinkingTokensForModel`) onto the orchestrator's request-build path.
//!
//! ## Canonical-name handling
//!
//! These predicates compare the model's **canonical** name (date suffix stripped)
//! for either a `claude-3-` prefix (thinking support) or exact membership in the
//! adaptive-thinking set. The canonical names this module must distinguish —
//! `claude-opus-4-8`, `claude-opus-4-7`, `claude-fable-5-1`, `claude-mythos-5-1`,
//! `claude-sonnet-4-6` — are finer-grained than `compaction`'s
//! `firstPartyNameToCanonical` (which collapses `claude-opus-4-8` → `claude-opus-4`).
//! We therefore carry a dedicated [`canonical`] helper here that preserves the
//! full family-version so the exact-equality adaptive set is byte-faithful to the
//! v2.1.183 binary.

/// Reduce a full model id to its canonical family-version name (date suffix
/// stripped), preserving the precision the thinking predicates require.
///
/// claude-code's `firstPartyNameToCanonical` maps e.g.
/// `claude-opus-4-8-20260115` → `claude-opus-4-8`. We recognize each known
/// family-version as a substring and return it; for an unrecognized id we strip
/// a trailing `-YYYYMMDD` date (and any `[1m]`-style suffix) and return the
/// lowercased remainder. The exact value only matters for the known set —
/// unknown ids fall through to the firstParty default in both predicates.
#[must_use]
pub fn canonical(model: &str) -> String {
    let name = model.to_lowercase();
    // Most specific first. These are the family-versions the predicates key on.
    const KNOWN: &[&str] = &[
        "claude-fable-5-1",
        "claude-mythos-5-1",
        "claude-mythos-preview",
        "claude-opus-4-8",
        "claude-opus-4-7",
        "claude-opus-4-6",
        "claude-opus-4-5",
        "claude-opus-4-1",
        "claude-opus-4-0",
        // sonnet-5 before the sonnet-4-x arms (2.1.198 registry; mutually
        // exclusive substrings — "claude-sonnet-4-5" does NOT contain it).
        "claude-sonnet-5",
        "claude-sonnet-4-6",
        "claude-sonnet-4-5",
        "claude-sonnet-4-0",
        "claude-haiku-4-5",
    ];
    for k in KNOWN {
        if name.contains(k) {
            return (*k).to_string();
        }
    }
    // `claude-opus-4` / `claude-sonnet-4` (undated family) after the dotted ones.
    for k in ["claude-opus-4", "claude-sonnet-4", "claude-haiku-4"] {
        if name.contains(k) {
            return k.to_string();
        }
    }
    // claude-3-* families — preserve the `claude-3-` prefix for the thinking gate.
    for k in [
        "claude-3-7-sonnet",
        "claude-3-5-sonnet",
        "claude-3-5-haiku",
        "claude-3-opus",
        "claude-3-sonnet",
        "claude-3-haiku",
    ] {
        if name.contains(k) {
            return k.to_string();
        }
    }
    name
}

/// `modelSupportsThinking` (firstParty path): every model whose canonical name
/// does NOT start with `claude-3-` supports thinking.
///
/// fable-5 / mythos-5 / opus-4-8 / sonnet-4-6 / haiku-4-5 → `true`;
/// the claude-3 families → `false`. Unknown ids default to `true`.
#[must_use]
pub fn model_supports_thinking(model: &str) -> bool {
    !canonical(model).starts_with("claude-3-")
}

/// `modelSupportsAdaptiveThinking` — EXACT v2.1.183 binary list (canonical
/// equality, NOT substring `.includes`).
///
/// `true` for `{claude-fable-5-1, claude-mythos-5-1, claude-opus-4-8, claude-opus-4-7,
/// claude-opus-4-6, claude-sonnet-4-6}`; `false` for the explicit non-adaptive
/// set `{claude-3-*, claude-opus-4-0, claude-opus-4-1, claude-opus-4-5,
/// claude-sonnet-4-0, claude-sonnet-4-5, claude-haiku-4-5}`; any other
/// (unknown) canonical defaults to `true` (firstParty default).
#[must_use]
pub fn model_supports_adaptive_thinking(model: &str) -> bool {
    let c = canonical(model);
    // Explicit TRUE set. 2.1.198 `Vit`: sonnet-5 resolves TRUE via the registry
    // capability check `lB(n,"adaptive_thinking")` (its capabilities include
    // "adaptive_thinking"); folded into the static TRUE set here.
    const ADAPTIVE: &[&str] = &[
        "claude-fable-5-1",
        "claude-mythos-5-1",
        "claude-opus-4-8",
        "claude-opus-4-7",
        "claude-opus-4-6",
        "claude-sonnet-5",
        "claude-sonnet-4-6",
    ];
    if ADAPTIVE.contains(&c.as_str()) {
        return true;
    }
    // Explicit FALSE set.
    const NON_ADAPTIVE: &[&str] = &[
        "claude-opus-4-0",
        "claude-opus-4-1",
        "claude-opus-4-5",
        "claude-sonnet-4-0",
        "claude-sonnet-4-5",
        "claude-haiku-4-5",
        // bare family fall-throughs that resolve to a 4.0-class model
        "claude-opus-4",
        "claude-sonnet-4",
        "claude-haiku-4",
    ];
    if NON_ADAPTIVE.contains(&c.as_str()) {
        return false;
    }
    if c.starts_with("claude-3-") {
        return false;
    }
    // Unknown (incl. mythos-preview, future ids) → firstParty default.
    true
}

/// `rhn` — the v2.1.185 temperature-gate model set (binary `function rhn`
/// @195164989). When thinking is disabled, claude-code sends `temperature:1`
/// ONLY for these models; for every OTHER model — including the default
/// `claude-opus-4-8`, `claude-opus-4-7`, `claude-fable-5-1`, `claude-mythos-5-1`,
/// and any unknown id — it omits the `temperature` field entirely.
///
/// Binary: substring match on `claude-3-` plus canonical equality on the
/// explicit 4.x list `{opus-4-0, opus-4-1, opus-4-5, opus-4-6, sonnet-4-0,
/// sonnet-4-5, sonnet-4-6, haiku-4-5}`. NOTE this is a DISTINCT set from
/// [`model_supports_adaptive_thinking`] (which puts opus-4-8/4-7/fable-5/
/// mythos-5 in its TRUE set) — do not conflate them.
#[must_use]
pub fn model_sends_temperature(model: &str) -> bool {
    let c = canonical(model);
    if c.starts_with("claude-3-") {
        return true;
    }
    const TEMP: &[&str] = &[
        "claude-opus-4-0",
        "claude-opus-4-1",
        "claude-opus-4-5",
        "claude-opus-4-6",
        "claude-sonnet-4-0",
        "claude-sonnet-4-5",
        "claude-sonnet-4-6",
        "claude-haiku-4-5",
        // bare family fall-throughs resolve to a 4.0-class id, all in the TRUE set
        "claude-opus-4",
        "claude-sonnet-4",
        "claude-haiku-4",
    ];
    TEMP.contains(&c.as_str())
}

/// Session/request thinking configuration, mirroring claude-code's resolved
/// `thinking` intent before it is rendered into the Anthropic `thinking` field.
///
/// `Default` is [`ThinkingConfig::Adaptive`] — claude-code's default for
/// adaptive-capable models (`alwaysThinkingEnabled` true by default).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub enum ThinkingConfig {
    /// Provider-owned automatic mode. No thinking/reasoning override is sent.
    Automatic,
    /// Thinking is off — no `thinking` field, and `temperature:1` is sent.
    Disabled,
    /// Adaptive thinking — the model decides depth (default).
    #[default]
    Adaptive,
    /// Fixed thinking budget cap.
    Enabled {
        /// Requested maximum thinking-budget tokens.
        budget_tokens: u32,
    },
}

/// `true` when the named env var is truthy under the strict claude-code
/// allowlist (`1`/`true`/`yes`/`on`). Shared gate for
/// `LINGXI_DISABLE_THINKING` / `LINGXI_DISABLE_ADAPTIVE_THINKING`.
#[must_use]
pub fn is_thinking_env_disabled(name: &str) -> bool {
    platform_api::env::is_env_truthy(std::env::var(name).ok().as_deref())
}

/// Session-level "thinking is on" gate: the session [`ThinkingConfig`] is not
/// `Disabled` AND the `LINGXI_DISABLE_THINKING` kill switch is off. Drives
/// both the `reasoning` field (via [`reasoning_for_request`]) and the
/// thinking-off `temperature:1` rule in `ApiService::build_request`.
#[must_use]
pub fn session_thinking_active(thinking: ThinkingConfig) -> bool {
    !matches!(
        thinking,
        ThinkingConfig::Disabled | ThinkingConfig::Automatic
    ) && !is_thinking_env_disabled("LINGXI_DISABLE_THINKING")
}

/// Resolve the boot SESSION [`ThinkingConfig`] from the `MAX_THINKING_TOKENS`
/// env var, the `--max-thinking-tokens` CLI flag (`cli_budget`), and the
/// `alwaysThinkingEnabled` setting (`always_thinking`) — byte-mirroring
/// claude-code's boot thinking resolution (binary v2.1.207 `qIe()` +
/// the `wn` request-build arm):
///
/// ```js
/// let gm = qIe(), lp = gm !== false ? {type:"adaptive"} : {type:"disabled"};
/// // (the --thinking flag override omitted here — a separate unwired surface)
/// let wn = process.env.MAX_THINKING_TOKENS
///     ? hp(process.env.MAX_THINKING_TOKENS)   // shared int-env parse (`hp`)
///     : a.maxThinkingTokens;                 // the --max-thinking-tokens flag
/// if (wn !== void 0) {
///     if (wn > 0)  lp = {type:"enabled", budgetTokens: wn};   // pre-empts adaptive
///     else if (wn === 0) lp = {type:"disabled"};
/// }
/// // qIe(): if(env) return hp(env)>0;
/// //        if(settings.alwaysThinkingEnabled===false) return false; return true
/// ```
///
/// The env value is parsed by claude-code's shared `hp` helper
/// ([`platform_api::env::parse_int_env`]), which since 2.1.211 accepts scientific
/// notation and digit-group separators in addition to plain `parseInt` values.
///
/// A positive env/flag budget PRE-EMPTS adaptive thinking (claude-code sets the
/// fixed `enabled`+`budgetTokens` config before the adaptive default applies);
/// `0` hard-disables; a `NaN`/non-positive env value disables (`qIe` returns
/// `hp(env) > 0` = false). When neither env nor flag pins a budget the
/// gate falls to `alwaysThinkingEnabled === false ? disabled : adaptive`. The
/// `--thinking` is folded by [`session_thinking_from_cli`], which preserves the
/// budget precedence implemented here.
///
/// The env var name `MAX_THINKING_TOKENS` is kept UNPREFIXED — claude-code's
/// name carries no `CLAUDE_`/`ANTHROPIC_` prefix, and this repo keeps such
/// names verbatim.
#[must_use]
pub fn session_thinking_from_env(
    cli_budget: Option<u32>,
    always_thinking: Option<bool>,
) -> ThinkingConfig {
    // `process.env.MAX_THINKING_TOKENS ? … : …` — an empty value is falsy and
    // falls through to the flag; any non-empty value is truthy and is parsed.
    if let Some(raw) = std::env::var("MAX_THINKING_TOKENS")
        .ok()
        .filter(|s| !s.is_empty())
    {
        let wn = platform_api::env::parse_int_env(&raw);
        return if wn > 0.0 {
            ThinkingConfig::Enabled {
                budget_tokens: u32::try_from(wn as u64).unwrap_or(u32::MAX),
            }
        } else {
            // wn <= 0 or NaN → disabled (`qIe`: `hp(env) > 0` is false).
            ThinkingConfig::Disabled
        };
    }
    // env unset → `wn = a.maxThinkingTokens` (the `--max-thinking-tokens` flag).
    if let Some(budget) = cli_budget {
        return if budget > 0 {
            ThinkingConfig::Enabled {
                budget_tokens: budget,
            }
        } else {
            // budget == 0 → `wn === 0` → disabled.
            ThinkingConfig::Disabled
        };
    }
    // env + flag both unset → `qIe()`: `alwaysThinkingEnabled === false` off,
    // else on (adaptive default for adaptive-capable models).
    if always_thinking == Some(false) {
        return ThinkingConfig::Disabled;
    }
    ThinkingConfig::Adaptive
}

/// Resolve the hidden `--thinking` session override while preserving Claude's
/// `MAX_THINKING_TOKENS` / `--max-thinking-tokens` precedence. A fixed budget
/// still wins because it is more specific; otherwise `enabled` is an alias for
/// adaptive thinking and `disabled` turns thinking off.
#[must_use]
pub fn session_thinking_from_cli(
    cli_mode: Option<&str>,
    cli_budget: Option<u32>,
    always_thinking: Option<bool>,
) -> ThinkingConfig {
    let configured = session_thinking_from_env(cli_budget, always_thinking);
    let has_env_budget = std::env::var("MAX_THINKING_TOKENS")
        .ok()
        .is_some_and(|value| !value.is_empty());
    if has_env_budget || cli_budget.is_some() {
        return configured;
    }
    match cli_mode {
        Some("disabled") => ThinkingConfig::Disabled,
        Some("enabled" | "adaptive") => ThinkingConfig::Adaptive,
        _ => configured,
    }
}

/// Resolve the SESSION [`ThinkingConfig`] into the per-request
/// [`crate::ReasoningConfig`] for `model` — the exact logic
/// `ApiService::build_request` applies to every main-loop AND subagent request
/// (claude.ts:1596-1630), extracted so the compaction side-query path can
/// inherit the SAME session thinking configuration (cc 2.1.198 "Subagents +
/// compaction inherit extended thinking config"; binary: the summarizer call
/// passes `thinkingConfig: mXt(r)` — the session `options.thinkingConfig` —
/// @216945141/@216926189).
///
/// The byte-faithful claude-code thinking shape (Adaptive default, canonical
/// max-output budget cap) is Anthropic-specific. The OpenAI/Gemini codecs
/// mistranslate `Adaptive` to a forced `effort="high"` / `thinkingBudget=0`,
/// so it must NOT be applied to non-Claude models: those only honor an
/// EXPLICIT fixed budget (the provider applies its own reasoning default
/// otherwise).
#[must_use]
pub fn reasoning_for_request(
    thinking: ThinkingConfig,
    model: &str,
    max_tokens: Option<u32>,
) -> Option<crate::ReasoningConfig> {
    if matches!(thinking, ThinkingConfig::Automatic) {
        return None;
    }
    if !session_thinking_active(thinking) {
        return None;
    }
    if crate::model::context_window::is_claude_family(model) {
        // Claude path — unchanged from claude-code.
        if !model_supports_thinking(model) {
            return None;
        }
        // Binary gate (2.1.208 closure `Xr` @223939748):
        //   vr = ut(CLAUDE_CODE_DISABLE_ADAPTIVE_THINKING)
        //        && (f.includes("opus-4-6") || f.includes("sonnet-4-6"))
        //   emit {type:"adaptive"} when  bqt(u) && !vr        (In === void 0)
        // where `f = co(model)` is the resolved model name and `bqt` is
        // `model_supports_adaptive_thinking`. The kill switch therefore ONLY
        // suppresses adaptive for opus-4-6 / sonnet-4-6 — every OTHER
        // adaptive-capable model (opus-4-8, opus-4-7, fable-5, mythos-5,
        // sonnet-5) keeps adaptive regardless of the env. (`x7o(model)` — the
        // per-model `thinkingTypeOverrides` map — is empty by default, so
        // `In === void 0` always holds and the gate reduces to `bqt(u) && !vr`.)
        let name = model.to_lowercase();
        let vr = is_thinking_env_disabled("LINGXI_DISABLE_ADAPTIVE_THINKING")
            && (name.contains("opus-4-6") || name.contains("sonnet-4-6"));
        if !vr && model_supports_adaptive_thinking(model) {
            return Some(crate::ReasoningConfig::Adaptive);
        }
        let mut budget = crate::model::context_window::max_thinking_tokens_for_model(model);
        if let ThinkingConfig::Enabled { budget_tokens } = thinking {
            budget = budget_tokens;
        }
        // An automatically selected manual budget must fit the caller's output
        // ceiling. Do not invent an invalid sub-minimum budget or increase the
        // ceiling. Explicit manual settings still reach SDK validation.
        if matches!(thinking, ThinkingConfig::Adaptive)
            && max_tokens.is_some_and(|limit| limit <= lingxi_llm_client::providers::anthropic::request_policy::MIN_MANUAL_THINKING_TOKENS)
        { return None; }
        // budget_tokens must stay strictly below max_tokens.
        budget = budget.min(max_tokens.unwrap_or(u32::MAX).saturating_sub(1));
        Some(crate::ReasoningConfig::Enabled {
            budget_tokens: budget,
        })
    } else {
        // Non-Claude: only honor an EXPLICIT fixed budget.
        match thinking {
            ThinkingConfig::Enabled { budget_tokens } => {
                let budget = budget_tokens.min(max_tokens.unwrap_or(u32::MAX).saturating_sub(1));
                Some(crate::ReasoningConfig::Enabled {
                    budget_tokens: budget,
                })
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_strips_date_and_preserves_family_version() {
        assert_eq!(canonical("claude-opus-4-8-20260115"), "claude-opus-4-8");
        assert_eq!(canonical("claude-sonnet-4-6-20251114"), "claude-sonnet-4-6");
        assert_eq!(canonical("claude-haiku-4-5-20251001"), "claude-haiku-4-5");
        assert_eq!(canonical("claude-fable-5-1"), "claude-fable-5-1");
        assert_eq!(canonical("claude-3-5-haiku-20241022"), "claude-3-5-haiku");
        // bare opus-4 family (no dotted minor) → the undated family name
        assert_eq!(canonical("claude-opus-4-20250514"), "claude-opus-4");
    }

    #[test]
    fn supports_thinking_excludes_claude_3() {
        for m in [
            "claude-fable-5-1",
            "claude-opus-4-8-20260115",
            "claude-sonnet-4-6-20251114",
            "claude-haiku-4-5-20251001",
        ] {
            assert!(model_supports_thinking(m), "{m} should support thinking");
        }
        for m in [
            "claude-3-opus-20240229",
            "claude-3-5-sonnet-20241022",
            "claude-3-5-haiku-20241022",
            "claude-3-7-sonnet-20250219",
        ] {
            assert!(
                !model_supports_thinking(m),
                "{m} should NOT support thinking"
            );
        }
    }

    #[test]
    fn sonnet_5_canonical_adaptive_and_no_temperature() {
        // 2.1.198: canonical preserves claude-sonnet-5 (incl. dated ids), it is
        // adaptive-thinking (registry capability), and it is NOT in the sIn
        // temperature set (temperature omitted when thinking is off).
        assert_eq!(canonical("claude-sonnet-5"), "claude-sonnet-5");
        assert_eq!(canonical("claude-sonnet-5-20260203"), "claude-sonnet-5");
        // Neighbor ids must NOT resolve to sonnet-5.
        assert_eq!(canonical("claude-sonnet-4-5-20250929"), "claude-sonnet-4-5");
        assert_eq!(canonical("claude-3-5-sonnet-20241022"), "claude-3-5-sonnet");
        assert!(model_supports_adaptive_thinking("claude-sonnet-5"));
        assert!(model_supports_thinking("claude-sonnet-5"));
        assert!(!model_sends_temperature("claude-sonnet-5"));
        // sonnet-4-6 keeps its (distinct) behavior: adaptive AND temperature.
        assert!(model_sends_temperature("claude-sonnet-4-6"));
    }

    #[test]
    fn adaptive_thinking_exact_binary_list() {
        for m in [
            "claude-fable-5-1",
            "claude-mythos-5-1",
            "claude-sonnet-5",
            "claude-opus-4-8-20260115",
            "claude-opus-4-7-20251201",
            "claude-opus-4-6-20260101",
            "claude-sonnet-4-6-20251114",
        ] {
            assert!(model_supports_adaptive_thinking(m), "{m} adaptive=true");
        }
        for m in [
            "claude-opus-4-0-20250514",
            "claude-opus-4-1-20250805",
            "claude-opus-4-5-20251101",
            "claude-sonnet-4-0-20250514",
            "claude-sonnet-4-5-20250929",
            "claude-haiku-4-5-20251001",
            "claude-3-5-haiku-20241022",
            "claude-3-opus-20240229",
        ] {
            assert!(!model_supports_adaptive_thinking(m), "{m} adaptive=false");
        }
        // Unknown → firstParty default true.
        assert!(model_supports_adaptive_thinking("claude-some-future-9"));
    }

    #[test]
    fn thinking_config_default_is_adaptive() {
        assert_eq!(ThinkingConfig::default(), ThinkingConfig::Adaptive);
    }

    #[test]
    fn adaptive_disable_env_only_narrows_opus46_and_sonnet46() {
        // 2.1.208 gate: vr = ut(CLAUDE_CODE_DISABLE_ADAPTIVE_THINKING)
        //   && (f.includes("opus-4-6")||f.includes("sonnet-4-6")); adaptive fires
        // when bqt(u) && !vr. The kill switch must NOT suppress adaptive for any
        // adaptive-capable model OTHER than opus-4-6 / sonnet-4-6.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _clear = EnvGuard::unset("LINGXI_DISABLE_THINKING");
        let _g = EnvGuard::set("LINGXI_DISABLE_ADAPTIVE_THINKING", "1");

        // Default model (opus-4-8) + other adaptive models keep adaptive.
        for model in [
            "claude-opus-4-8-20260115",
            "claude-opus-4-7-20251201",
            "claude-fable-5-1",
            "claude-mythos-5-1",
            "claude-sonnet-5",
        ] {
            assert_eq!(
                reasoning_for_request(ThinkingConfig::Adaptive, model, Some(64_000)),
                Some(crate::ReasoningConfig::Adaptive),
                "{model}: DISABLE_ADAPTIVE must NOT suppress adaptive (not opus-4-6/sonnet-4-6)"
            );
        }

        // opus-4-6 / sonnet-4-6 ARE narrowed by the env → fixed budget, not adaptive.
        for model in ["claude-opus-4-6-20260101", "claude-sonnet-4-6-20251114"] {
            let r = reasoning_for_request(ThinkingConfig::Adaptive, model, Some(64_000));
            assert!(
                matches!(r, Some(crate::ReasoningConfig::Enabled { .. })),
                "{model}: DISABLE_ADAPTIVE must suppress adaptive → fixed budget, got {r:?}"
            );
        }
    }

    #[test]
    fn adaptive_stays_adaptive_when_env_unset() {
        // Same models, env NOT set → all keep adaptive (baseline).
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _clear = EnvGuard::unset("LINGXI_DISABLE_THINKING");
        let _g = EnvGuard::unset("LINGXI_DISABLE_ADAPTIVE_THINKING");
        for model in [
            "claude-opus-4-8-20260115",
            "claude-opus-4-6-20260101",
            "claude-sonnet-4-6-20251114",
        ] {
            assert_eq!(
                reasoning_for_request(ThinkingConfig::Adaptive, model, Some(64_000)),
                Some(crate::ReasoningConfig::Adaptive),
                "{model}: env unset → adaptive"
            );
        }
    }

    // ---- MAX_THINKING_TOKENS / --max-thinking-tokens / alwaysThinkingEnabled ----
    //
    // `session_thinking_from_env` reads the process-global `MAX_THINKING_TOKENS`
    // env var, so every test here serializes on one lock and restores the prior
    // value (cargo runs a crate's tests in parallel).
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Restores the prior value on drop. Does NOT take [`ENV_LOCK`] itself —
    /// the caller must already hold it.
    ///
    /// Folding the lock into this guard looks tidier and DEADLOCKS: most tests
    /// here take `ENV_LOCK` explicitly and then build a guard, so an internal
    /// lock re-enters a non-reentrant `Mutex` and the whole module hangs. The
    /// exclusion is enforced by every caller taking the lock, which is now
    /// true of all of them.
    struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
    }
    impl EnvGuard {
        fn set(key: &'static str, val: &str) -> Self {
            let prev = std::env::var(key).ok();
            std::env::set_var(key, val);
            Self { key, prev }
        }
        fn unset(key: &'static str) -> Self {
            let prev = std::env::var(key).ok();
            std::env::remove_var(key);
            Self { key, prev }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[test]
    fn max_thinking_env_positive_forces_fixed_budget() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::set("MAX_THINKING_TOKENS", "50000");
        // A positive env budget PRE-EMPTS adaptive, even with no flag / setting.
        assert_eq!(
            session_thinking_from_env(None, None),
            ThinkingConfig::Enabled {
                budget_tokens: 50_000
            }
        );
        // parseInt semantics: leading digits, stop at first non-digit.
        let _g2 = EnvGuard::set("MAX_THINKING_TOKENS", "12000abc");
        assert_eq!(
            session_thinking_from_env(None, None),
            ThinkingConfig::Enabled {
                budget_tokens: 12_000
            }
        );
    }

    #[test]
    fn max_thinking_env_zero_or_nan_disables() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::set("MAX_THINKING_TOKENS", "0");
        assert_eq!(
            session_thinking_from_env(None, None),
            ThinkingConfig::Disabled
        );
        // NaN (parseInt("abc",10) is NaN, NaN>0 = false ⇒ qIe returns false).
        let _g2 = EnvGuard::set("MAX_THINKING_TOKENS", "abc");
        assert_eq!(
            session_thinking_from_env(None, None),
            ThinkingConfig::Disabled
        );
        // A negative value is likewise non-positive ⇒ disabled.
        let _g3 = EnvGuard::set("MAX_THINKING_TOKENS", "-5");
        assert_eq!(
            session_thinking_from_env(None, None),
            ThinkingConfig::Disabled
        );
    }

    #[test]
    fn max_thinking_env_empty_falls_through_to_flag() {
        let _lock = ENV_LOCK.lock().unwrap();
        // An empty env value is falsy → `wn = a.maxThinkingTokens` (the flag).
        let _g = EnvGuard::set("MAX_THINKING_TOKENS", "");
        assert_eq!(
            session_thinking_from_env(Some(1_234), None),
            ThinkingConfig::Enabled {
                budget_tokens: 1_234
            }
        );
    }

    #[test]
    fn max_thinking_flag_honored_when_env_unset() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::unset("MAX_THINKING_TOKENS");
        // flag > 0 → fixed budget (pre-empts adaptive).
        assert_eq!(
            session_thinking_from_env(Some(30_000), None),
            ThinkingConfig::Enabled {
                budget_tokens: 30_000
            }
        );
        // flag == 0 → disabled (`wn === 0`).
        assert_eq!(
            session_thinking_from_env(Some(0), None),
            ThinkingConfig::Disabled
        );
    }

    #[test]
    fn always_thinking_setting_governs_when_no_budget() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::unset("MAX_THINKING_TOKENS");
        // No env, no flag: alwaysThinkingEnabled===false ⇒ disabled.
        assert_eq!(
            session_thinking_from_env(None, Some(false)),
            ThinkingConfig::Disabled
        );
        // true / unset ⇒ the adaptive default.
        assert_eq!(
            session_thinking_from_env(None, Some(true)),
            ThinkingConfig::Adaptive
        );
        assert_eq!(
            session_thinking_from_env(None, None),
            ThinkingConfig::Adaptive
        );
    }

    #[test]
    fn cli_thinking_mode_overrides_setting_without_a_budget() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::unset("MAX_THINKING_TOKENS");
        assert_eq!(
            session_thinking_from_cli(Some("disabled"), None, Some(true)),
            ThinkingConfig::Disabled
        );
        assert_eq!(
            session_thinking_from_cli(Some("enabled"), None, Some(false)),
            ThinkingConfig::Adaptive
        );
        assert_eq!(
            session_thinking_from_cli(Some("adaptive"), None, Some(false)),
            ThinkingConfig::Adaptive
        );
    }

    #[test]
    fn explicit_thinking_budget_pre_empts_cli_mode() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::unset("MAX_THINKING_TOKENS");
        assert_eq!(
            session_thinking_from_cli(Some("disabled"), Some(4096), None),
            ThinkingConfig::Enabled {
                budget_tokens: 4096
            }
        );
    }

    #[test]
    fn env_budget_pre_empts_flag_and_setting() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::set("MAX_THINKING_TOKENS", "7777");
        // env wins over both the flag (0) and alwaysThinkingEnabled=false.
        assert_eq!(
            session_thinking_from_env(Some(0), Some(false)),
            ThinkingConfig::Enabled {
                budget_tokens: 7_777
            }
        );
    }

    /// MAX_THINKING_TOKENS is parsed by the shared `hp` helper (2.1.211+), so a
    /// scientific-notation or digit-separator budget now resolves to `Enabled`.
    #[test]
    fn max_thinking_tokens_accepts_scientific_and_separators() {
        // Take ENV_LOCK like every sibling test: `EnvGuard` restores the value
        // but provides no exclusion, so without this a concurrent reader saw
        // MAX_THINKING_TOKENS mid-change ("left: 10000, right: 12000").
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        {
            let _g = EnvGuard::set("MAX_THINKING_TOKENS", "1e4");
            assert_eq!(
                session_thinking_from_env(None, None),
                ThinkingConfig::Enabled {
                    budget_tokens: 10_000
                }
            );
        }
        {
            let _g = EnvGuard::set("MAX_THINKING_TOKENS", "12_000");
            assert_eq!(
                session_thinking_from_env(None, None),
                ThinkingConfig::Enabled {
                    budget_tokens: 12_000
                }
            );
        }
        {
            // Non-integer scientific value => NaN => disabled.
            let _g = EnvGuard::set("MAX_THINKING_TOKENS", "1.5e0");
            assert_eq!(
                session_thinking_from_env(None, None),
                ThinkingConfig::Disabled
            );
        }
    }
}
