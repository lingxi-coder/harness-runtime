//! The `OO()` todo/task-tool model gate.
//!
//! claude-code 2.1.263 stopped advertising the five todo/task-tracking tools
//! (`TodoWrite`, `TaskCreate`, `TaskGet`, `TaskUpdate`, `TaskList`) on the
//! current model generation. Its CHANGELOG entry (chunk `src_168628323.js`
//! @113894) reads:
//!
//! > Todo/task-tracking tools (TaskCreate/Get/Update/List, TodoWrite) are no
//! > longer available on Opus 4.8, Sonnet 5, Fable 5, Mythos 5, and newer
//! > models; set `CLAUDE_CODE_ENABLE_TODO_TOOLS=1` to bring them back
//!
//! The gate itself, verbatim from `src_160988549.js` @1995502:
//!
//! ```js
//! var Zao=[["opus",[4,8]],["sonnet",[5]],["fable",[5]],["mythos",[5]]],
//!     ERe=[XS,UE,mG,WE,kT],
//!     elo="tengu_rosy_wren";
//! function tlo(e){return!s$e(e,Zao)}
//! function OO(){
//!   if(Ja()||QDn())return!0;
//!   let e=J$e();
//!   if(e===void 0||tlo(e))return!0;
//!   if(a.CLAUDE_CODE_ENABLE_TODO_TOOLS===!0)return!0;
//!   return H(elo,!1)===!0
//! }
//! function h3(){return X_()&&OO()}
//! ```
//!
//! Resolved terms:
//! - `Ja()` = `_t()||Jh()!==null` (`src_158095655.js` @674937) — a background
//!   session (`E6()==="bg"`, itself `CLAUDE_CODE_SESSION_KIND==="bg"` @674824)
//!   or an active bg takeover.
//! - `QDn()` = `host.launchOptions.todoToolsOptIn()` (`src_156627426.js`
//!   @78682), set at `src_160988549.js` @3434970 by `ZDn(ERe.some(ue))` — the
//!   user named one of the five tools in `--tools`/`--allowedTools`.
//! - `J$e()` = `m1().mainLoopCanonical?.()` (`src_158095655.js` @260166),
//!   registered as `Dx(()=>HR(rt()))` @349424 — the **session** main-loop model
//!   id, canonicalised, read live (so `/model` moves the gate mid-session).
//!   Note this is the main-loop model, never the caller's: a subagent on an
//!   older model does not get the tools back.
//! - `CLAUDE_CODE_ENABLE_TODO_TOOLS` is declared `I.bool()`
//!   (`src_156866368.js`) — plain env-truthiness, compared `=== true`. This is
//!   deliberately NOT the `I.triBool()` shape that `CLAUDE_CODE_ENABLE_TASKS`
//!   uses, so it does not share `is_env_defined_falsy` with
//!   `is_todo_v2_enabled`.
//! - `H("tengu_rosy_wren", false)` is a GrowthBook gate defaulting to **false**,
//!   ported through `telemetry::flag_bool`. No fetcher is wired in production,
//!   so the snapshot is empty and the read returns its default — byte-identical
//!   to a GrowthBook-absent oracle, and the same reasoning
//!   `telemetry::feature_flags` already documents for its other callers. Going
//!   through the real helper rather than a hardcoded `false` is what lets
//!   `telemetry::test_set_flag` exercise the arm.
//!
//! `h3()` (`X_() && OO()`) is not spelled here because `X_()` — the
//! `LINGXI_ENABLE_TASKS` half — lives with the tools in `tool-task`. Callers
//! compose the two.

use lingxi_core::host::env::is_env_truthy;

use crate::tool_trait::ToolStaticContext;

/// `Lks` — 2.1.270's ALLOWLIST of model ids that still get the todo/task tools.
///
/// 2.1.268 replaced the 2.1.263 family+threshold DENYLIST (`Zao`) with this
/// exact-id set. The port still carried the denylist, so every model added since
/// kept the tools instead of losing them.
///
/// Oracle `lM()` (`src_169588164.js`):
/// ```js
/// Lks=new Set(["claude-3-opus","claude-3-sonnet","claude-3-haiku","claude-3-5-sonnet",
///   "claude-3-5-haiku","claude-3-7-sonnet","claude-opus-4-0","claude-opus-4-1",
///   "claude-opus-4-5","claude-opus-4-6","claude-opus-4-7","claude-sonnet-4-0",
///   "claude-sonnet-4-5","claude-sonnet-4-6","claude-haiku-4-5"])
/// ```
const ALLOWED_MODEL_IDS: &[&str] = &[
    "claude-3-opus",
    "claude-3-sonnet",
    "claude-3-haiku",
    "claude-3-5-sonnet",
    "claude-3-5-haiku",
    "claude-3-7-sonnet",
    "claude-opus-4-0",
    "claude-opus-4-1",
    "claude-opus-4-5",
    "claude-opus-4-6",
    "claude-opus-4-7",
    "claude-sonnet-4-0",
    "claude-sonnet-4-5",
    "claude-sonnet-4-6",
    "claude-haiku-4-5",
];

/// `$ks` — a Bedrock application-inference-profile id names no model, so it can
/// never be matched against the allowlist and is let through.
const INFERENCE_PROFILE_MARKER: &str = "application-inference-profile";

/// Does `model` look like an Anthropic model id at all?
///
/// ⛔ LingXi is a multi-provider product, so "not in the allowlist" is the
/// NORMAL case, not the edge case: a DeepSeek/Kimi/GLM id would fail an
/// exact-set lookup and silently lose TaskCreate/TodoWrite. The oracle never has
/// to think about this because every id it sees is Anthropic's.
///
/// So the allowlist only DECIDES for ids that are recognisably Anthropic
/// (`claude-*`). Anything else takes the same exit as `e===void 0` upstream:
/// unrecognised ⇒ keep the tools. Pinned by
/// `a_non_anthropic_model_keeps_the_tools`.
fn is_anthropic_model_id(model: &str) -> bool {
    model.starts_with("claude-")
}

/// The env escape hatch, port-renamed from `CLAUDE_CODE_ENABLE_TODO_TOOLS`.
pub const ENABLE_TODO_TOOLS_ENV: &str = "LINGXI_ENABLE_TODO_TOOLS";

/// Pure core of [`todo_tools_enabled`] — the `OO()` body with its four
/// ambient reads (`Ja()`, `QDn()`, the env var, the gate flag) lifted into
/// parameters.
///
/// `main_loop_model` is `None` for the JS `e === void 0` path, which is an
/// *enable*: an unknown model keeps the tools.
/// 2.1.270 has NO trailing feature-flag term: `lM()` ends at the env check, so
/// `gate_flag` (the old `tengu_rosy_wren`) is gone.
#[must_use]
pub fn todo_tools_enabled_inner(
    main_loop_model: Option<&str>,
    background_session: bool,
    opt_in: bool,
    env_enable: bool,
) -> bool {
    // `if(yl()||bVn())return!0`
    if background_session || opt_in {
        return true;
    }
    // `let e=Lze(); if(e===void 0||$ks(e)||Fks(e))return!0`
    match main_loop_model {
        // Unknown model ⇒ keep the tools (`e===void 0`).
        None => return true,
        Some(model) => {
            // Bedrock inference profile: names no model, so it can never match.
            if model.contains(INFERENCE_PROFILE_MARKER) {
                return true;
            }
            // The allowlist only decides for Anthropic ids; anything else is
            // "unrecognised" and keeps the tools (see `is_anthropic_model_id`).
            if !is_anthropic_model_id(model) {
                return true;
            }
            if ALLOWED_MODEL_IDS.contains(&model) {
                return true;
            }
        }
    }
    // `return a.CLAUDE_CODE_ENABLE_TODO_TOOLS===!0`
    env_enable
}

/// `OO()` — whether the todo/task tool family is advertised at all.
///
/// The model comes from [`ToolStaticContext::main_loop_model`], which
/// `ToolRegistry::available_tools` fills from the registry's session-scoped
/// main-loop model. That is deliberately the **session** model (the port's
/// `J$e()`) and never the calling agent's: a subagent running an older model
/// does not get the tools back, matching the oracle.
#[must_use]
pub fn todo_tools_enabled(ctx: &ToolStaticContext) -> bool {
    todo_tools_enabled_for_model(ctx.main_loop_model.as_deref())
}

/// `OO()` for a caller that already holds the canonical main-loop model and has
/// no [`ToolStaticContext`] — the reminder producer, which runs off the session
/// rather than off the tool registry.
#[must_use]
pub fn todo_tools_enabled_for_model(main_loop_model: Option<&str>) -> bool {
    todo_tools_enabled_inner(
        main_loop_model,
        lingxi_core::host::env::is_bg_session(),
        lingxi_core::host::session_flags::todo_tools_opt_in(),
        is_env_truthy(std::env::var(ENABLE_TODO_TOOLS_ENV).ok().as_deref()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on(model: &str) -> bool {
        todo_tools_enabled_inner(Some(model), false, false, false)
    }

    /// Every id in the 2.1.270 allowlist keeps the tools.
    #[test]
    fn allowlisted_models_keep_the_tools() {
        for model in ALLOWED_MODEL_IDS {
            assert!(on(model), "{model} is allowlisted and must keep the tools");
        }
    }

    /// 2.1.268 flipped this from a family+threshold DENYLIST to an exact-id
    /// ALLOWLIST. Under the old denylist every Anthropic model NOT named in the
    /// table kept the tools, so each of these was wrongly enabled.
    #[test]
    fn anthropic_models_outside_the_allowlist_lose_the_tools() {
        for model in [
            "claude-opus-4-8",
            "claude-sonnet-5",
            "claude-fable-5",
            "claude-mythos-5",
            "claude-opus-5",
            "claude-haiku-5",
            // Not in the set even though its family IS: only exact ids match.
            "claude-sonnet-4-7",
            "claude-3-5-opus",
        ] {
            assert!(!on(model), "{model} is not allowlisted and must lose them");
        }
    }

    /// ⛔ The multi-provider exit. An exact-id allowlist copied verbatim would
    /// strip TaskCreate/TodoWrite from EVERY non-Anthropic model, because none
    /// of them can ever be in an Anthropic-only set. Unrecognised ids take the
    /// same branch as `e === void 0` upstream: keep the tools.
    #[test]
    fn a_non_anthropic_model_keeps_the_tools() {
        for model in [
            "deepseek-chat",
            "kimi-k2",
            "glm-4-plus",
            "gpt-5.4",
            "gemini-3-pro",
            "qwen-max",
            "",
        ] {
            assert!(
                on(model),
                "{model:?} is not an Anthropic id and must keep the tools"
            );
        }
    }

    /// `$ks` — a Bedrock application-inference-profile id names no model.
    #[test]
    fn an_inference_profile_keeps_the_tools() {
        assert!(on(
            "arn:aws:bedrock:us-east-1:1234:application-inference-profile/abc"
        ));
    }

    /// `e === void 0`.
    #[test]
    fn an_unknown_model_keeps_the_tools() {
        assert!(todo_tools_enabled_inner(None, false, false, false));
    }

    /// The three escape hatches ahead of / behind the model check.
    #[test]
    fn background_opt_in_and_env_each_re_enable_a_gated_model() {
        let gated = "claude-sonnet-5";
        assert!(!on(gated), "precondition: this model is gated");
        assert!(
            todo_tools_enabled_inner(Some(gated), true, false, false),
            "bg session"
        );
        assert!(
            todo_tools_enabled_inner(Some(gated), false, true, false),
            "opt-in"
        );
        assert!(
            todo_tools_enabled_inner(Some(gated), false, false, true),
            "env"
        );
    }

    /// The allowlist is the oracle's, verbatim — a drifted entry silently
    /// changes which models get the tools.
    #[test]
    fn the_allowlist_matches_the_oracle_set() {
        assert_eq!(ALLOWED_MODEL_IDS.len(), 15);
        for expected in [
            "claude-3-opus",
            "claude-3-sonnet",
            "claude-3-haiku",
            "claude-3-5-sonnet",
            "claude-3-5-haiku",
            "claude-3-7-sonnet",
            "claude-opus-4-0",
            "claude-opus-4-1",
            "claude-opus-4-5",
            "claude-opus-4-6",
            "claude-opus-4-7",
            "claude-sonnet-4-0",
            "claude-sonnet-4-5",
            "claude-sonnet-4-6",
            "claude-haiku-4-5",
        ] {
            assert!(
                ALLOWED_MODEL_IDS.contains(&expected),
                "{expected} missing from the allowlist"
            );
        }
    }
}
