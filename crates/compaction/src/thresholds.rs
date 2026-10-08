//! Verified constants from the claude-code reference (see spec §13.2), plus the
//! pure threshold math and token-warning state machine ported from
//! `src/services/compact/autoCompact.ts:30-158`
//! (`getEffectiveContextWindowSize`, `getAutoCompactThreshold`,
//! `calculateTokenWarningState`, `isAutoCompactEnabled`).
//!
//! ## Divergences from TS
//!
//! - `is_auto_compact_enabled` takes the config flag as a `bool` parameter
//!   rather than calling `getGlobalConfig().autoCompactEnabled`. The
//!   `compaction` crate does not depend on a config crate, so the caller reads
//!   `GlobalConfig` and passes the resolved `auto_compact_enabled` flag. The two
//!   env gates (`DISABLE_COMPACT`, `DISABLE_AUTO_COMPACT`) are read here.
//! - `calculate_token_warning_state` takes `auto_compact_enabled: bool` for the
//!   same reason (TS calls `isAutoCompactEnabled()` twice internally).

use crate::context_window::{context_window_for_model, max_output_tokens_for_model};

/// Buffer tokens before the autocompact threshold kicks in.
pub const AUTOCOMPACT_BUFFER_TOKENS: u64 = 13_000;
/// Buffer tokens before warning-level threshold.
pub const WARNING_THRESHOLD_BUFFER_TOKENS: u64 = 20_000;
/// Buffer tokens before error-level threshold.
pub const ERROR_THRESHOLD_BUFFER_TOKENS: u64 = 20_000;
/// Buffer tokens kept available after a manual compact request.
pub const MANUAL_COMPACT_BUFFER_TOKENS: u64 = 3_000;
/// Maximum output tokens budgeted for the autocompact summary call.
///
/// Deliberately 32_000, above the oracle's `avp = 20_000` cap: the reserve then
/// covers the full Claude max-output tier, so autocompact keeps the headroom a
/// summary of a long, tool-heavy context actually needs. Every threshold below
/// derives from this reserve.
pub const MAX_OUTPUT_TOKENS_FOR_SUMMARY: u64 = 32_000;
/// Maximum consecutive autocompact failures before the circuit breaker trips.
///
/// claude-code v2.1.183 `jho = 3` (`bin/claude.exe` offset 203009353).
pub const MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES: u32 = 3;
/// Turns-since-previous-compact ceiling below which a fresh compact counts as a
/// "rapid refill" (the context refilled to the limit within this many turns).
///
/// claude-code v2.1.183 `Who = 3` (`bin/claude.exe` offset 203009353).
pub const RAPID_REFILL_TURN_WINDOW: u32 = 3;
/// Consecutive rapid-refill count at which the thrashing breaker trips.
///
/// claude-code v2.1.183 `f6n = 3` (`bin/claude.exe` offset 203009353).
pub const MAX_CONSECUTIVE_RAPID_REFILLS: u32 = 3;

/// Byte-exact thrashing message surfaced (on the reactive PTL path) when the
/// rapid-refill breaker trips.
///
/// 1:1 with claude-code v2.1.183 `Rho` (`bin/claude.exe` offset 203009521).
/// `${Who}` / `${f6n}` are both `3` (the constants are interpolated at module
/// init), so the literal carries the resolved `3`s.
pub const RAPID_REFILL_THRASHING_MESSAGE: &str = "Autocompact is thrashing: the context refilled to the limit within 3 turns of the previous compact, 3 times in a row. A file being read or a tool output is likely too large for the context window. Try reading in smaller chunks, or use /clear to start fresh.";
/// Maximum number of recent files restored into the post-compact prompt.
pub const POST_COMPACT_MAX_FILES_TO_RESTORE: usize = 5;
/// Total token budget shared across post-compact file restoration.
pub const POST_COMPACT_TOKEN_BUDGET: u64 = 50_000;
/// Per-file token budget for post-compact file restoration.
pub const POST_COMPACT_MAX_TOKENS_PER_FILE: u64 = 5_000;
/// Per-skill token budget for post-compact skill restoration.
pub const POST_COMPACT_MAX_TOKENS_PER_SKILL: u64 = 5_000;
/// Total token budget shared across post-compact skill restoration.
pub const POST_COMPACT_SKILLS_TOKEN_BUDGET: u64 = 25_000;
/// Maximum prompt-too-long retry attempts before giving up.
pub const MAX_PTL_RETRIES: u32 = 3;
/// Maximum streaming retries for the compaction summary call.
pub const MAX_COMPACT_STREAMING_RETRIES: u32 = 2;

/// Which compaction layer was applied during an iteration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CompactionLayer {
    /// Drop oldest messages (cheap, no LLM).
    Snip,
    /// Time-based clearing of large tool results.
    Microcompact,
    /// Cached microcompact path (wired in a later plan).
    CachedMicrocompact,
    /// Read-time context-collapse projection (default-off).
    ContextCollapse,
    /// LLM-driven summarization.
    Autocompact,
    /// Partial autocompact path.
    PartialAutocompact,
}

/// Why compaction was triggered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CompactionReason {
    /// Approaching the model's context window.
    TokenLimit,
    /// The user explicitly requested a compact.
    ManualRequest,
    /// Server reported the prompt was too long.
    PromptTooLong,
    /// Microcompact warned that recent results were too large.
    MicrocompactWarn,
}

/// Per-agent tracking state used to coordinate autocompact across iterations.
///
/// Mirrors the `autoCompactTracking` object claude-code threads through
/// `autoCompactIfNeeded` (`bin/claude.exe`, the `{compacted, turnId,
/// turnCounter, consecutiveFailures, consecutiveRapidRefills}` shape set at
/// offset 202919683).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct AutoCompactTrackingState {
    /// Whether autocompact has run (`compacted`). Set `true` when a compact
    /// ran; consulted by the rapid-refill breaker and the per-turn
    /// `turnCounter++` increment.
    pub compacted: bool,
    /// Turns since the previous compact (`turnCounter`): reset to `0` when a
    /// compact runs, incremented each turn thereafter while `compacted`.
    pub turn_counter: u32,
    /// Identifier of the turn that last compacted (`turnId`).
    pub turn_id: String,
    /// How many autocompact attempts have failed in a row
    /// (`consecutiveFailures`).
    pub consecutive_failures: u32,
    /// How many compacts in a row each occurred within
    /// [`RAPID_REFILL_TURN_WINDOW`] turns of the previous one
    /// (`consecutiveRapidRefills`). Drives the thrashing breaker.
    pub consecutive_rapid_refills: u32,
    /// SC-04: the failure detail of the compaction attempt that was supposed to
    /// rescue THIS API call, mirroring the oracle's
    /// `precomputeOutcome.kind==="failed" ? precomputeOutcome.compactFailure : undefined`
    /// (cc-238.js @228721216). Consumed once by the prompt-too-long surface,
    /// which renders it through `Fol` (`Prompt is too long · automatic
    /// compaction failed: …`) instead of the bare `Prompt is too long`.
    ///
    /// Transient loop state, never persisted: `#[serde(skip)]` keeps the resume
    /// runtime-metadata wire shape byte-identical.
    #[serde(skip)]
    pub last_compact_failure_detail: Option<String>,
}

/// The rapid-refill (thrashing) count for `state`: how many consecutive
/// compacts have each occurred within [`RAPID_REFILL_TURN_WINDOW`] turns of the
/// previous one.
///
/// 1:1 with `kho` (`bin/claude.exe` offset 203004820):
/// ```text
/// function kho(e){
///   return e?.compacted===!0 && e.turnCounter<Who
///     ? (e?.consecutiveRapidRefills ?? 0) + 1
///     : 0
/// }
/// ```
/// When the previous compact ran (`compacted`) AND the context refilled within
/// `Who` turns (`turn_counter < RAPID_REFILL_TURN_WINDOW`), the running rapid-
/// refill count is incremented; otherwise it resets to `0`. The thrashing
/// breaker trips when this reaches [`MAX_CONSECUTIVE_RAPID_REFILLS`].
#[must_use]
pub fn rapid_refill_count(state: &AutoCompactTrackingState) -> u32 {
    if state.compacted && state.turn_counter < RAPID_REFILL_TURN_WINDOW {
        state.consecutive_rapid_refills.saturating_add(1)
    } else {
        0
    }
}

/// Where the auto-compact window came from.
///
/// 1:1 with the `source` field of claude-code's `N8` / `resolveAutoCompactWindow`
/// (cc-238.js @286413238). [`Self::as_str`] returns the oracle's wire spellings,
/// which the `/autocompact` status renderer `sgT` switches on.
///
/// Precedence (the whole `N8` chain, in order):
/// `env → settings → clientdata → experiment → model-default → unknown-model → auto`.
/// The 2.1.220 twin `aY` stops at `model-default → auto`; the `unknown-model`
/// arm is new in 2.1.238 (`source:"unknown-model"` has 0 hits in the 2.1.220
/// binary), which is why this taxonomy exists at all — the port previously
/// resolved a bare `u64` with no source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoCompactWindowSource {
    /// `"env"` — pinned by `LINGXI_AUTO_COMPACT_WINDOW`.
    Env,
    /// `"settings"` — pinned by the `autoCompactWindow` setting.
    Settings,
    /// `"clientdata"` — the server-pushed client-data window.
    ///
    /// Unreachable in LingXi: `TyS` reads the `rowan_thicket` client-data cache,
    /// which has no Rust equivalent (same deferral as the GrowthBook branches
    /// already documented in `llm-runtime/src/model/context_window.rs`).
    ClientData,
    /// `"experiment"` — the `gRa` experiment override.
    ///
    /// Unreachable in LingXi for the same reason as [`Self::ClientData`].
    Experiment,
    /// `"model-default"` — a per-model default window.
    ///
    /// The 2.1.261 `Pko` restricted-window set, Sonnet 5's ordinary CLI table
    /// default, or a registered native-1M model with auto-compact enabled.
    ModelDefault,
    /// `"unknown-model"` (NEW in 2.1.238) — auto-compact will hold the session
    /// inside the window it *assumes* for a model this build does not recognize.
    ///
    /// Paired with a one-shot user-facing notice, [`unknown_model_window_notice`]
    /// (`Pk0` @306646044) — see there for the copy and where it is emitted.
    ///
    /// The *enforcement* half of the oracle's branch is deliberately absent:
    /// this arm returns the bare model window, same as [`Self::Auto`], so
    /// nothing is clamped. That is a LingXi divergence with a reason —
    /// multi-provider support means an unrecognized model is the NORMAL case
    /// here, and clamping every third-party model to an assumed window would
    /// truncate sessions the provider was happy to serve. The notice says only
    /// what is true of this port: the window is an assumption.
    UnknownModel,
    /// `"auto"` — no override applied.
    Auto,
}

impl AutoCompactWindowSource {
    /// The oracle's wire spelling for this source.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Env => "env",
            Self::Settings => "settings",
            Self::ClientData => "clientdata",
            Self::Experiment => "experiment",
            Self::ModelDefault => "model-default",
            Self::UnknownModel => "unknown-model",
            Self::Auto => "auto",
        }
    }
}

/// The `{window, configured, source}` triple `N8` returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedAutoCompactWindow {
    /// The window actually applied — `min(model_window, configured)`.
    pub window: u64,
    /// The window the source asked for, before the model clamp. Equal to
    /// [`Self::window`] unless the model's own window is smaller (the oracle
    /// renders that gap as ` · capped to N by model`).
    pub configured: u64,
    /// Which branch of `N8` produced it.
    pub source: AutoCompactWindowSource,
}

/// Env kill-switch for the `unknown-model` branch.
///
/// `CLAUDE_CODE_DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT` upstream, renamed to
/// the `LINGXI_` prefix like every other window/threshold knob in this module
/// (`LINGXI_AUTO_COMPACT_WINDOW`, `LINGXI_MAX_CONTEXT_TOKENS`, …).
pub const DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT_ENV: &str =
    "LINGXI_DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT";

/// `true` when this build has no real window for `model` and
/// [`context_window_for_model`] is therefore returning its assumed default.
///
/// The oracle's predicate is `!jpt(e,n)` — `ICd(canonical)`, "is the canonical
/// name in the model registry". LingXi's registry split is:
///
/// * non-Claude ids resolve through the catalog-fed
///   `llm_runtime::model::model_limits` registry — the SAME
///   lookup [`context_window_for_model`] uses to decide whether it knows the
///   model, so `lookup(...).is_none()` is exactly "unrecognized" here, with no
///   second table to drift out of sync;
/// * Claude-family ids have no "known id" set at all (`canonical_name` falls
///   back to the raw string), so they are treated as recognized. Conservative
///   on purpose: the port never warns about a `claude-*` id it might well know.
fn model_window_is_assumed(model: &str) -> bool {
    !llm_runtime::model::context_window::is_claude_family(model)
        && llm_runtime::model::model_limits::lookup(model).is_none()
}

/// Resolve the auto-compact window together with the source that produced it.
///
/// 1:1 with `N8` (cc-238.js @286413238) for the branches LingXi can decide:
///
/// ```text
/// function N8(e,t,r=Ox()){ let n=Fo(e),o=OR(e,r);
///   if(process.env.CLAUDE_CODE_AUTO_COMPACT_WINDOW){…return{window:Math.min(o,c),configured:c,source:"env"}}
///   if(t!==void 0) return{window:Math.min(o,t),configured:t,source:"settings"};
///   … clientdata … experiment … model-default …
///   if(iO()&&!V.CLAUDE_CODE_DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT&&!vyS(e,r)&&!C1r(e)&&!jpt(e,n))
///     return{window:o,configured:o,source:"unknown-model"};
///   return{window:o,configured:o,source:"auto"} }
/// ```
///
/// The 1M guard `vyS(e,r)` (`[1m]` suffix, or the 1M beta header on a
/// 1M-capable model) is folded into `model_window >= 1_000_000`: every path that
/// satisfies `vyS` is a path on which [`context_window_for_model`] already
/// returned `1e6`, so this reads the same derivation instead of duplicating the
/// suffix/beta tables. `C1r(e)` (an unresolved Bedrock
/// `application-inference-profile`) has no port analogue.
///
/// The env branch shares the 2.1.261 `GS` / `kte` numeric parser and 100k–1M
/// clamp through [`resolve_auto_compact_env_window`].
#[must_use]
pub fn resolve_auto_compact_window(
    model: &str,
    betas: &[String],
    settings_window: Option<u64>,
    auto_compact_enabled: bool,
) -> ResolvedAutoCompactWindow {
    let model_window = context_window_for_model(model, betas);

    if let Ok(raw) = std::env::var("LINGXI_AUTO_COMPACT_WINDOW") {
        if let Some(configured) = resolve_auto_compact_env_window(&raw) {
            return ResolvedAutoCompactWindow {
                window: model_window.min(configured),
                configured,
                source: AutoCompactWindowSource::Env,
            };
        }
    }

    if let Some(configured) = settings_window {
        return ResolvedAutoCompactWindow {
            window: model_window.min(configured),
            configured,
            source: AutoCompactWindowSource::Settings,
        };
    }

    // Clientdata / experiment inputs are unavailable in this crate. Resolve
    // the checked-in ordinary CLI model defaults before the fallback source.
    if let Some(configured) =
        model_default_compact_window(model, model_window, auto_compact_enabled)
    {
        return ResolvedAutoCompactWindow {
            window: model_window.min(configured),
            configured,
            source: AutoCompactWindowSource::ModelDefault,
        };
    }

    let source = if auto_compact_enabled
        && !env_truthy(DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT_ENV)
        && model_window < 1_000_000
        && model_window_is_assumed(model)
    {
        AutoCompactWindowSource::UnknownModel
    } else {
        AutoCompactWindowSource::Auto
    };

    ResolvedAutoCompactWindow {
        window: model_window,
        configured: model_window,
        source,
    }
}

/// The locally decidable 2.1.261 `GS` model-default branches. `model_window`
/// already incorporates explicit 1M opt-ins and the existing model registry.
/// Remote Cowork / local-agent table rows and served clientdata belong to
/// their hosts; the ordinary CLI uses Sonnet 5's table default of 1M.
fn model_default_compact_window(model: &str, model_window: u64, enabled: bool) -> Option<u64> {
    let canonical = llm_runtime::model::thinking::canonical(model);
    // The shared thinking canonicalizer predates Opus 5; preserve the same
    // provider/date suffix matching that the context-window registry applies.
    let canonical = if canonical.contains("claude-opus-5") {
        "claude-opus-5"
    } else {
        canonical.as_str()
    };
    let restricted_default = matches!(
        canonical,
        "claude-sonnet-4-6" | "claude-opus-4-6" | "claude-opus-4-8" | "claude-opus-5"
    );
    // 2.1.261 `qL` / `YL`: a registered native-1M model, not an arbitrary
    // model that obtained a 1M window through a suffix or beta header.
    let native_1m = matches!(
        canonical,
        "claude-sonnet-5"
            | "claude-opus-4-7"
            | "claude-opus-4-8"
            | "claude-opus-5"
            | "claude-fable-5-1"
            | "claude-mythos-5-1"
            | "claude-mythos-preview"
    );
    let disabled_1m = is_1m_context_disabled();
    // `d<1e6 && (Pko.has(o) || Iko(...))` is intentionally independent of
    // auto-compact enabled. A 1M opt-in skips this 200k branch entirely.
    if model_window < 1_000_000 && (restricted_default || disabled_1m && native_1m) {
        return Some(200_000);
    }
    if !enabled {
        return None;
    }
    // `Rko` is gated by auto-compact enabled; the CLI row is a configured
    // 1M even if another input made the model's actual window smaller.
    if canonical == "claude-sonnet-5" {
        return Some(1_000_000);
    }
    (model_window >= 1_000_000 && native_1m && !disabled_1m).then_some(model_window)
}

/// Resolve the explicit window knob like Claude Code 2.1.261 `GS` / `kte`:
/// parse integer env notation, reject nonpositive/NaN, then clamp to 100k–1M.
/// Exposed so the `/autocompact` status uses the same value as the runtime.
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn resolve_auto_compact_env_window(raw: &str) -> Option<u64> {
    let value = lingxi_core::host::env::parse_int_env(raw);
    (!value.is_nan() && value > 0.0).then(|| value.clamp(100_000.0, 1_000_000.0) as u64)
}

/// `Gpe()` — `CLAUDE_CODE_DISABLE_1M_CONTEXT`.
///
/// Read here rather than borrowed from `llm_runtime::model::context_window`
/// because that crate's `is_1m_context_disabled` is private; the spelling is
/// the un-rebranded one the port already honours there, so the two agree.
fn is_1m_context_disabled() -> bool {
    env_truthy("CLAUDE_CODE_DISABLE_1M_CONTEXT")
}

/// `oc(e)` (@283762505) — `vf(e).replace(".0","")`, where `vf` is
/// `Intl.NumberFormat("en-US", compact-for-≥1000).format(e).toLowerCase()`.
/// `200000` -> `200k`, `1000000` -> `1m`, `1500000` -> `1.5m`.
///
/// Twin of `commands::core::autocompact`'s private `format_tokens_compact`;
/// duplicated rather than shared because that one is a crate-private helper of
/// a slash-command handler and `compaction` must not depend on `commands`.
fn format_window_tokens(n: u64) -> String {
    fn trim(v: f64) -> String {
        let s = format!("{v:.1}");
        s.strip_suffix(".0").map_or(s.clone(), str::to_string)
    }
    #[allow(clippy::cast_precision_loss)]
    if n >= 1_000_000 {
        format!("{}m", trim(n as f64 / 1_000_000.0))
    } else if n >= 1_000 {
        format!("{}k", trim(n as f64 / 1_000.0))
    } else {
        n.to_string()
    }
}

/// SC-06 — the one-shot notice for an [`AutoCompactWindowSource::UnknownModel`]
/// window. `Pk0` (cc-238.js @306646044), wrapped by `Fby` (@306646018):
///
/// ```js
/// function Pk0(e,t,r){ let{source:n,window:o}=N8(e,t,r);
///   if(n!=="unknown-model") return null;
///   let i=BCd(e), s=V.CLAUDE_CODE_MAX_CONTEXT_TOKENS;
///   if(i&&s!==void 0&&s>0) return null;
///   let a=o<1e6, l=[];
///   if(!Gpe()&&a) l.push("append [1m] to the model name for 1M");
///   if(i) l.push("set CLAUDE_CODE_MAX_CONTEXT_TOKENS to its real window");
///   let c=l.length>0?`If the model accepts ${a?"more":"less"}, ${l.join(", or ")}; to make it recognized, `
///                   :"To make it recognized, ";
///   return `"${e}" is not a model this version of Claude Code recognizes, so auto-compact will keep this session within ${oc(o)} tokens (the context window it assumes). ${c}map it in the modelOverrides setting or update Claude Code; CLAUDE_CODE_DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT=1 restores the previous wait-for-the-API behavior.` }
/// ```
///
/// `Fby` exists only to catch a throw and log `"unknown-model notice failed"`;
/// nothing here can throw, so the wrapper collapses into this function.
///
/// # Rebrands, and the one clause that is dropped
///
/// * `Claude Code` -> [`branding::PRODUCT_NAME`];
///   `CLAUDE_CODE_MAX_CONTEXT_TOKENS` -> `LINGXI_MAX_CONTEXT_TOKENS`;
///   `CLAUDE_CODE_DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT` ->
///   [`DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT_ENV`].
/// * **`map it in the modelOverrides setting or ` is DROPPED.** LingXi has no
///   window-overrides setting — `modelOverrides` in
///   `llm-runtime/src/model/allowlist.rs` is an unrelated Anthropic-id ->
///   provider-id map for the allowlist gate. Rendering the clause would tell
///   the user to do something that cannot be done, which is worse than a
///   shorter notice. Everything else is byte-for-byte.
/// * `LINGXI_MAX_CONTEXT_TOKENS` is itself honoured only under `USER_TYPE=ant`
///   (`llm_runtime::model::context_window`, a pre-existing documented
///   divergence), so that remedy can be inert. The probe below still reads the
///   RAW var, like the oracle: if the user has already set it, repeating the
///   advice is noise regardless of whether the gate lets it through.
///
/// `betas` / `settings_window` / `auto_compact_enabled` are threaded straight
/// into [`resolve_auto_compact_window`] so the notice can never disagree with
/// the window it describes.
#[must_use]
pub fn unknown_model_window_notice(
    model: &str,
    betas: &[String],
    settings_window: Option<u64>,
    auto_compact_enabled: bool,
) -> Option<String> {
    let resolved = resolve_auto_compact_window(model, betas, settings_window, auto_compact_enabled);
    if resolved.source != AutoCompactWindowSource::UnknownModel {
        return None;
    }
    // `BCd(e)` — "this is not a `claude-*` model", i.e. the one case where a
    // real context window can be stated by hand.
    let is_non_claude = !llm_runtime::model::context_window::is_claude_family(model);
    let max_context_tokens_set = std::env::var("LINGXI_MAX_CONTEXT_TOKENS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .is_some_and(|n| n > 0);
    if is_non_claude && max_context_tokens_set {
        return None;
    }

    let accepts_more = resolved.window < 1_000_000;
    let mut remedies: Vec<&str> = Vec::new();
    if !is_1m_context_disabled() && accepts_more {
        remedies.push("append [1m] to the model name for 1M");
    }
    if is_non_claude {
        remedies.push("set LINGXI_MAX_CONTEXT_TOKENS to its real window");
    }
    let lead = if remedies.is_empty() {
        "To make it recognized, ".to_string()
    } else {
        format!(
            "If the model accepts {}, {}; to make it recognized, ",
            if accepts_more { "more" } else { "less" },
            remedies.join(", or ")
        )
    };

    let product = branding::PRODUCT_NAME;
    Some(format!(
        "\"{model}\" is not a model this version of {product} recognizes, so \
         auto-compact will keep this session within {} tokens (the context \
         window it assumes). {lead}update {product}; \
         {DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT_ENV}=1 restores the previous \
         wait-for-the-API behavior.",
        format_window_tokens(resolved.window)
    ))
}

/// One-shot guard for [`unknown_model_window_notice`].
///
/// The oracle emits its notice exactly once, from the REPL launcher
/// (@306693668), before the first turn. The port cannot emit it there: LingXi's
/// model registry (`llm_runtime::model::model_limits`) is populated at
/// CATALOG-ASSEMBLY time, which happens AFTER the launcher — so at the oracle's
/// emit point every non-Claude model still looks unrecognized, including the
/// ones the catalog is about to describe exactly. Upstream has no such window;
/// `ICd` reads a table compiled into the binary.
///
/// So the port latches it instead and emits on the first window resolution of
/// the session, which is the first turn — same information, same once, and by
/// then the registry answers correctly.
static UNKNOWN_MODEL_NOTICE_SHOWN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// [`unknown_model_window_notice`], but only the FIRST time it would fire in
/// this process. Returns `None` on every later call.
#[must_use]
pub fn unknown_model_window_notice_once(
    model: &str,
    betas: &[String],
    settings_window: Option<u64>,
    auto_compact_enabled: bool,
) -> Option<String> {
    if UNKNOWN_MODEL_NOTICE_SHOWN.load(std::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    let notice = unknown_model_window_notice(model, betas, settings_window, auto_compact_enabled)?;
    // Latched only when a notice was actually produced, so a session that
    // starts on a recognized model and later switches still gets one.
    if UNKNOWN_MODEL_NOTICE_SHOWN.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    Some(notice)
}

/// Test-only reset for [`UNKNOWN_MODEL_NOTICE_SHOWN`].
#[cfg(test)]
fn reset_unknown_model_notice_latch() {
    UNKNOWN_MODEL_NOTICE_SHOWN.store(false, std::sync::atomic::Ordering::Relaxed);
}

/// Returns the context window size minus the max output tokens reserved for the
/// compaction summary.
///
/// Mirrors `getEffectiveContextWindowSize` (`autoCompact.ts:33-49`) — the
/// oracle's `USe(e,t)`, which is `N8(e,n).window − min(max_output, avp)`. The
/// window half is delegated to [`resolve_auto_compact_window`] so the runtime
/// and window-source reporting share one derivation.
///
/// `settings_window` is `None` here: the port has no writable
/// `autoCompactWindow` setting (its only knob is `LINGXI_AUTO_COMPACT_WINDOW`),
/// as `commands/core/src/autocompact.rs` already documents.
#[must_use]
pub fn effective_context_window_size(model: &str, betas: &[String]) -> u64 {
    let reserved_tokens_for_summary =
        max_output_tokens_for_model(model).min(MAX_OUTPUT_TOKENS_FOR_SUMMARY);

    resolve_auto_compact_window(model, betas, None, is_auto_compact_enabled(true))
        .window
        .saturating_sub(reserved_tokens_for_summary)
}

/// Returns the token count at which autocompact should fire.
///
/// Mirrors `getAutoCompactThreshold` (`autoCompact.ts:72-91`):
/// `effective − AUTOCOMPACT_BUFFER_TOKENS`, honoring the
/// `LINGXI_AUTOCOMPACT_PCT_OVERRIDE` env override (a float in `(0, 100]` yields
/// `floor(effective × pct/100)`, then `min`'d with the buffer-based threshold).
#[must_use]
pub fn auto_compact_threshold(model: &str, betas: &[String]) -> u64 {
    let effective_context_window = effective_context_window_size(model, betas);
    let autocompact_threshold = effective_context_window.saturating_sub(AUTOCOMPACT_BUFFER_TOKENS);

    if let Ok(raw) = std::env::var("LINGXI_AUTOCOMPACT_PCT_OVERRIDE") {
        if let Some(parsed) = parse_float_prefix(&raw) {
            if parsed.is_finite() && parsed > 0.0 && parsed <= 100.0 {
                // Math.floor(effective * (pct / 100)).
                #[allow(
                    clippy::cast_precision_loss,
                    clippy::cast_sign_loss,
                    clippy::cast_possible_truncation
                )]
                let percentage_threshold =
                    (effective_context_window as f64 * (parsed / 100.0)).floor() as u64;
                return percentage_threshold.min(autocompact_threshold);
            }
        }
    }

    autocompact_threshold
}

/// Snapshot of where the current token usage sits relative to every threshold.
///
/// Mirrors the object returned by `calculateTokenWarningState`
/// (`autoCompact.ts:93-145`).
// Mirrors the flat object returned by TS `calculateTokenWarningState`: four
// independent threshold-crossing flags. Collapsing them into enums would diverge
// from the byte-faithful field set and the serde wire shape, so the bool flags
// are intentional here.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TokenWarningState {
    /// Percentage of the threshold still available, clamped to `>= 0`
    /// (`Math.round` parity).
    pub percent_left: u8,
    /// `token_usage >= threshold − WARNING_THRESHOLD_BUFFER_TOKENS`.
    pub is_above_warning_threshold: bool,
    /// `token_usage >= threshold − ERROR_THRESHOLD_BUFFER_TOKENS`.
    pub is_above_error_threshold: bool,
    /// Autocompact is enabled AND `token_usage >= auto_compact_threshold`.
    pub is_above_auto_compact_threshold: bool,
    /// `token_usage >= blocking_limit` (the hard manual-compact ceiling).
    pub is_at_blocking_limit: bool,
}

/// Computes the [`TokenWarningState`] for `token_usage` against `model`.
///
/// Mirrors `calculateTokenWarningState` (`autoCompact.ts:93-145`). The
/// `auto_compact_enabled` flag is supplied by the caller (see module docs);
/// `LINGXI_BLOCKING_LIMIT_OVERRIDE` is honored here.
#[must_use]
pub fn calculate_token_warning_state(
    token_usage: u64,
    model: &str,
    betas: &[String],
    auto_compact_enabled: bool,
) -> TokenWarningState {
    let autocompact_threshold = auto_compact_threshold(model, betas);
    let threshold = if auto_compact_enabled {
        autocompact_threshold
    } else {
        effective_context_window_size(model, betas)
    };

    // Math.max(0, Math.round(((threshold - tokenUsage) / threshold) * 100)).
    let percent_left = percent_left_of(threshold, token_usage);

    let warning_threshold = threshold.saturating_sub(WARNING_THRESHOLD_BUFFER_TOKENS);
    let error_threshold = threshold.saturating_sub(ERROR_THRESHOLD_BUFFER_TOKENS);

    let is_above_warning_threshold = token_usage >= warning_threshold;
    let is_above_error_threshold = token_usage >= error_threshold;

    let is_above_auto_compact_threshold =
        auto_compact_enabled && token_usage >= autocompact_threshold;

    // 2.1.261 `UTn` uses the full model window for the blocking ceiling,
    // independently of a smaller configured auto-compact window.
    let actual_context_window = context_window_for_model(model, betas)
        .saturating_sub(max_output_tokens_for_model(model).min(MAX_OUTPUT_TOKENS_FOR_SUMMARY));
    let default_blocking_limit = actual_context_window.saturating_sub(MANUAL_COMPACT_BUFFER_TOKENS);

    // Allow override for testing (positive integer wins, else the default).
    let blocking_limit = std::env::var("LINGXI_BLOCKING_LIMIT_OVERRIDE")
        .ok()
        .and_then(|raw| parse_positive_u64(&raw))
        .unwrap_or(default_blocking_limit);

    let is_at_blocking_limit = token_usage >= blocking_limit;

    TokenWarningState {
        percent_left,
        is_above_warning_threshold,
        is_above_error_threshold,
        is_above_auto_compact_threshold,
        is_at_blocking_limit,
    }
}

/// Whether autocompact is enabled, honoring the two env kill-switches and the
/// caller-supplied config flag.
///
/// Mirrors `isAutoCompactEnabled` (`autoCompact.ts:147-158`):
/// `DISABLE_COMPACT` and `DISABLE_AUTO_COMPACT` (truthy) force `false`;
/// otherwise the result is `config_auto_compact_enabled`. The config flag is a
/// parameter because the `compaction` crate has no config-crate dependency (see
/// module docs).
#[must_use]
pub fn is_auto_compact_enabled(config_auto_compact_enabled: bool) -> bool {
    if env_truthy("DISABLE_COMPACT") {
        return false;
    }
    // Allow disabling just auto-compact (keeps manual /compact working).
    if env_truthy("DISABLE_AUTO_COMPACT") {
        return false;
    }
    config_auto_compact_enabled
}

/// `Math.max(0, Math.round(((threshold - usage) / threshold) * 100))`, clamped
/// to `u8`. Returns `0` when `threshold == 0` (avoids div-by-zero; TS would
/// yield `NaN → 0` via `Math.max(0, …)` semantics for our usage).
fn percent_left_of(threshold: u64, usage: u64) -> u8 {
    if threshold == 0 {
        return 0;
    }
    if usage >= threshold {
        return 0;
    }
    #[allow(clippy::cast_precision_loss)]
    let ratio = (threshold - usage) as f64 / threshold as f64;
    // JS Math.round: round half away from zero for positive values → (x + 0.5).floor().
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let rounded = (ratio * 100.0 + 0.5).floor() as i64;
    // `clamp(0, 100)` guarantees the value fits in `u8`, so `try_from` never
    // fails; this avoids the (false-positive) sign-loss cast.
    u8::try_from(rounded.clamp(0, 100)).unwrap_or(0)
}

/// Read an env var and apply `isEnvTruthy` ([`lingxi_core::host::env::is_env_truthy`])
/// semantics (`1` / `true` / `yes` / `on`, case-insensitive, trimmed).
fn env_truthy(name: &str) -> bool {
    lingxi_core::host::env::is_env_truthy(std::env::var(name).ok().as_deref())
}

/// Shared integer env coercion (`tl`), including scientific/grouped notation.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn parse_positive_u64(raw: &str) -> Option<u64> {
    let value = lingxi_core::host::env::parse_int_env(raw);
    (!value.is_nan() && value > 0.0).then_some(value as u64)
}

/// JS `parseFloat` accepts the longest valid decimal prefix, unlike Rust's
/// whole-string parser. Infinity is irrelevant to the `(0, 100]` caller gate.
fn parse_float_prefix(raw: &str) -> Option<f64> {
    static PREFIX: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PREFIX
        .get_or_init(|| {
            regex::Regex::new(r"^[+-]?(?:[0-9]+\.?[0-9]*|\.[0-9]+)(?:[eE][+-]?[0-9]+)?")
                .expect("valid decimal prefix regex")
        })
        .find(raw.trim_start())?
        .as_str()
        .parse()
        .ok()
}

/// Test-only: serializes every test in this crate that reads or writes the
/// process environment.
///
/// Crate-visible on purpose. It used to live inside this module's `tests`, so
/// it only serialized the threshold tests — while `token_warning_banner`'s
/// tests read `DISABLE_COMPACT` without taking it and intermittently observed
/// the value a threshold test had set. **A lock only protects the tests that
/// actually take it.**
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    use super::ENV_LOCK;

    const ENV_VARS: &[&str] = &[
        "LINGXI_AUTO_COMPACT_WINDOW",
        "LINGXI_AUTOCOMPACT_PCT_OVERRIDE",
        "LINGXI_BLOCKING_LIMIT_OVERRIDE",
        "LINGXI_MAX_CONTEXT_TOKENS",
        "LINGXI_MAX_OUTPUT_TOKENS",
        "CLAUDE_CODE_DISABLE_1M_CONTEXT",
        "USER_TYPE",
        "DISABLE_COMPACT",
        "DISABLE_AUTO_COMPACT",
        DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT_ENV,
    ];

    /// Snapshot the env vars this module manipulates, clear them, run `body`,
    /// then restore. Holds [`ENV_LOCK`] for the duration so tests don't race on
    /// the shared process environment.
    fn with_clean_env(body: impl FnOnce()) {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let saved: Vec<(&str, Option<String>)> = ENV_VARS
            .iter()
            .map(|&k| (k, std::env::var(k).ok()))
            .collect();
        for &k in ENV_VARS {
            std::env::remove_var(k);
        }
        body();
        for (k, v) in saved {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
    }

    // sonnet-4-6, no betas, no env overrides:
    //   context_window = 200_000, max_output = 32_000,
    //   reserved = min(32_000, 32_000) = 32_000 → effective = 168_000.
    const MODEL: &str = "claude-sonnet-4-6-20251001";
    const EFFECTIVE: u64 = 168_000;
    const AUTOCOMPACT: u64 = EFFECTIVE - AUTOCOMPACT_BUFFER_TOKENS; // 155_000

    /// A model this build has no real window for resolves through the
    /// `unknown-model` branch `N8` gained in 2.1.238 — and the window it
    /// reports is unchanged, so routing `effective_context_window_size` through
    /// the resolver is source-only.
    #[test]
    fn sc06_unrecognized_model_resolves_to_the_unknown_model_source() {
        with_clean_env(|| {
            // Non-Claude id absent from the catalog registry: the port is
            // ASSUMING MODEL_CONTEXT_WINDOW_DEFAULT for it.
            const UNKNOWN: &str = "mysteryprovider/mystery-9-turbo";
            let r = resolve_auto_compact_window(UNKNOWN, &[], None, true);
            assert_eq!(r.source, AutoCompactWindowSource::UnknownModel);
            assert_eq!(
                r.window,
                crate::context_window::MODEL_CONTEXT_WINDOW_DEFAULT
            );
            assert_eq!(r.configured, r.window);

            // The kill switch restores the previous behavior (source `auto`).
            std::env::set_var(DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT_ENV, "1");
            assert_eq!(
                resolve_auto_compact_window(UNKNOWN, &[], None, true).source,
                AutoCompactWindowSource::Auto
            );
            std::env::remove_var(DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT_ENV);

            // `iO()` gate: auto-compact off ⇒ never `unknown-model`.
            assert_eq!(
                resolve_auto_compact_window(UNKNOWN, &[], None, false).source,
                AutoCompactWindowSource::Auto
            );

            // A recognized 200k Claude model uses its checked-in default.
            assert_eq!(
                resolve_auto_compact_window(MODEL, &[], None, true).source,
                AutoCompactWindowSource::ModelDefault
            );

            // `vyS(e,r)`: a 1M opt-in is never called unrecognized.
            assert_eq!(
                resolve_auto_compact_window("mysteryprovider/mystery-9[1m]", &[], None, true)
                    .source,
                AutoCompactWindowSource::Auto
            );

            // The window an unrecognized model reports is the same one the
            // pre-taxonomy code returned.
            assert_eq!(
                effective_context_window_size(UNKNOWN, &[]),
                crate::context_window::MODEL_CONTEXT_WINDOW_DEFAULT
                    - max_output_tokens_for_model(UNKNOWN).min(MAX_OUTPUT_TOKENS_FOR_SUMMARY)
            );
        });
    }

    /// SC-06 — the unknown-model notice (`Pk0`). Every branch of the remedy
    /// list, and the two early returns.
    #[test]
    fn sc06_unknown_model_notice_copy() {
        with_clean_env(|| {
            reset_unknown_model_notice_latch();
            const UNKNOWN: &str = "mysteryprovider/mystery-9-turbo";

            // Non-Claude, under 1M, nothing disabled ⇒ BOTH remedies.
            let notice = unknown_model_window_notice(UNKNOWN, &[], None, true)
                .expect("an unrecognized model must produce a notice");
            assert_eq!(
                notice,
                format!(
                    "\"{UNKNOWN}\" is not a model this version of {} recognizes, \
                     so auto-compact will keep this session within 200k tokens \
                     (the context window it assumes). If the model accepts more, \
                     append [1m] to the model name for 1M, or set \
                     LINGXI_MAX_CONTEXT_TOKENS to its real window; to make it \
                     recognized, update {}; \
                     LINGXI_DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT=1 restores \
                     the previous wait-for-the-API behavior.",
                    branding::PRODUCT_NAME,
                    branding::PRODUCT_NAME
                )
            );

            // `Gpe()` drops the `[1m]` remedy, leaving one item — and therefore
            // NO `, or ` separator.
            std::env::set_var("CLAUDE_CODE_DISABLE_1M_CONTEXT", "1");
            let one = unknown_model_window_notice(UNKNOWN, &[], None, true).expect("notice");
            assert!(
                one.contains(
                    "If the model accepts more, set LINGXI_MAX_CONTEXT_TOKENS to its real window; \
                     to make it recognized, "
                ),
                "{one}"
            );
            assert!(!one.contains("append [1m]"));
            std::env::remove_var("CLAUDE_CODE_DISABLE_1M_CONTEXT");

            // A model the build DOES recognize never gets a notice.
            assert_eq!(unknown_model_window_notice(MODEL, &[], None, true), None);
            // Neither does an unknown one once the enforcement kill switch is on
            // (the source falls back to `auto`).
            std::env::set_var(DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT_ENV, "1");
            assert_eq!(unknown_model_window_notice(UNKNOWN, &[], None, true), None);
            std::env::remove_var(DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT_ENV);

            // `if(i && s !== void 0 && s > 0) return null` — the user already
            // stated the real window, so the advice would be noise.
            std::env::set_var("LINGXI_MAX_CONTEXT_TOKENS", "300000");
            assert_eq!(unknown_model_window_notice(UNKNOWN, &[], None, true), None);
            std::env::set_var("LINGXI_MAX_CONTEXT_TOKENS", "0");
            assert!(
                unknown_model_window_notice(UNKNOWN, &[], None, true).is_some(),
                "a non-positive value is not a stated window"
            );
            std::env::remove_var("LINGXI_MAX_CONTEXT_TOKENS");
        });
    }

    /// The notice fires ONCE per process — the latch that stands in for the
    /// oracle's launcher-time emit.
    #[test]
    fn sc06_unknown_model_notice_is_one_shot() {
        with_clean_env(|| {
            reset_unknown_model_notice_latch();
            const UNKNOWN: &str = "mysteryprovider/mystery-9-turbo";
            // A recognized model must NOT burn the latch.
            assert_eq!(
                unknown_model_window_notice_once(MODEL, &[], None, true),
                None
            );
            assert!(unknown_model_window_notice_once(UNKNOWN, &[], None, true).is_some());
            assert_eq!(
                unknown_model_window_notice_once(UNKNOWN, &[], None, true),
                None,
                "the second call is silent"
            );
            reset_unknown_model_notice_latch();
        });
    }

    /// `oc(e)` — the compact token formatter the notice renders the window with.
    #[test]
    fn sc06_window_token_formatting() {
        assert_eq!(format_window_tokens(200_000), "200k");
        assert_eq!(format_window_tokens(1_000_000), "1m");
        assert_eq!(format_window_tokens(1_500_000), "1.5m");
        assert_eq!(format_window_tokens(999), "999");
        assert_eq!(format_window_tokens(128_000), "128k");
    }

    /// `N8`'s env → settings precedence, and the byte-exact `source` spellings
    /// the `/autocompact` status renderer switches on.
    #[test]
    fn sc06_window_source_precedence_and_wire_spellings() {
        with_clean_env(|| {
            std::env::set_var("LINGXI_AUTO_COMPACT_WINDOW", "150000");
            let r = resolve_auto_compact_window(MODEL, &[], Some(120_000), true);
            assert_eq!(
                r.source,
                AutoCompactWindowSource::Env,
                "env outranks settings"
            );
            assert_eq!(r.window, 150_000);
            assert_eq!(r.configured, 150_000);
            std::env::remove_var("LINGXI_AUTO_COMPACT_WINDOW");

            let r = resolve_auto_compact_window(MODEL, &[], Some(120_000), true);
            assert_eq!(r.source, AutoCompactWindowSource::Settings);
            assert_eq!(r.window, 120_000);

            // `Math.min(o, t)` — the model window caps the configured one.
            let r = resolve_auto_compact_window(MODEL, &[], Some(900_000), true);
            assert_eq!(r.window, 200_000, "clamped to the model window");
            assert_eq!(r.configured, 900_000, "configured keeps the raw ask");
        });

        assert_eq!(AutoCompactWindowSource::Env.as_str(), "env");
        assert_eq!(AutoCompactWindowSource::Settings.as_str(), "settings");
        assert_eq!(AutoCompactWindowSource::ClientData.as_str(), "clientdata");
        assert_eq!(AutoCompactWindowSource::Experiment.as_str(), "experiment");
        assert_eq!(
            AutoCompactWindowSource::ModelDefault.as_str(),
            "model-default"
        );
        assert_eq!(
            AutoCompactWindowSource::UnknownModel.as_str(),
            "unknown-model"
        );
        assert_eq!(AutoCompactWindowSource::Auto.as_str(), "auto");
    }

    #[test]
    fn oracle_261_cli_model_defaults_preserve_native_1m_windows() {
        with_clean_env(|| {
            for (model, expected) in [
                ("claude-sonnet-4-6", 200_000),
                ("claude-opus-4-6", 200_000),
                ("claude-opus-4-8", 1_000_000),
                ("claude-opus-5", 1_000_000),
                ("claude-opus-5-20260901", 1_000_000),
                ("claude-sonnet-5", 1_000_000),
            ] {
                let resolved = resolve_auto_compact_window(model, &[], None, true);
                assert_eq!(
                    resolved.source,
                    AutoCompactWindowSource::ModelDefault,
                    "{model}"
                );
                assert_eq!(resolved.window, expected, "{model}");
                assert_eq!(resolved.configured, expected, "{model}");
            }
            let sonnet_disabled = resolve_auto_compact_window("claude-sonnet-5", &[], None, false);
            assert_eq!(sonnet_disabled.source, AutoCompactWindowSource::Auto);
            assert_eq!(sonnet_disabled.window, 1_000_000);
        });
    }

    #[test]
    fn oracle_261_explicit_1m_on_non_native_models_skips_restricted_default() {
        with_clean_env(|| {
            for model in ["claude-sonnet-4-6", "claude-opus-4-6"] {
                let with_suffix =
                    resolve_auto_compact_window(&format!("{model}[1m]"), &[], None, true);
                let with_beta = resolve_auto_compact_window(
                    model,
                    &[crate::CONTEXT_1M_BETA_HEADER.into()],
                    None,
                    true,
                );
                for resolved in [with_suffix, with_beta] {
                    assert_eq!(resolved.source, AutoCompactWindowSource::Auto, "{model}");
                    assert_eq!(resolved.window, 1_000_000, "{model}");
                    assert_eq!(resolved.configured, 1_000_000, "{model}");
                }
            }
        });
    }

    #[test]
    fn oracle_261_disable_1m_restores_200k_model_default_even_when_auto_disabled() {
        with_clean_env(|| {
            std::env::set_var("CLAUDE_CODE_DISABLE_1M_CONTEXT", "1");
            for model in [
                "claude-opus-4-8",
                "claude-opus-5",
                "claude-sonnet-5",
                "claude-fable-5-1",
            ] {
                for enabled in [false, true] {
                    let resolved = resolve_auto_compact_window(model, &[], None, enabled);
                    assert_eq!(
                        resolved.source,
                        AutoCompactWindowSource::ModelDefault,
                        "{model}"
                    );
                    assert_eq!(resolved.window, 200_000, "{model}");
                    assert_eq!(resolved.configured, 200_000, "{model}");
                }
            }
        });
    }

    #[test]
    fn effective_and_autocompact_baseline() {
        with_clean_env(|| {
            assert_eq!(effective_context_window_size(MODEL, &[]), EFFECTIVE);
            assert_eq!(auto_compact_threshold(MODEL, &[]), AUTOCOMPACT);
        });
    }

    #[test]
    fn opus_4_8_bedrock_id_uses_the_same_auto_compact_boundary() {
        // Claude Code 2.1.217 fixed Opus 4.8 Bedrock sessions never reaching
        // auto-compact. Provider-shaped IDs must canonicalize to the same 1M
        // window / 64k output tier as the first-party model id.
        with_clean_env(|| {
            let first_party = "claude-opus-4-8";
            let bedrock = "us.anthropic.claude-opus-4-8-v1:0";
            assert_eq!(
                effective_context_window_size(bedrock, &[]),
                effective_context_window_size(first_party, &[])
            );
            assert_eq!(
                auto_compact_threshold(bedrock, &[]),
                auto_compact_threshold(first_party, &[])
            );
        });
    }

    #[test]
    fn is_auto_compact_enabled_env_gates() {
        with_clean_env(|| {
            assert!(is_auto_compact_enabled(true));
            assert!(!is_auto_compact_enabled(false));

            std::env::set_var("DISABLE_COMPACT", "1");
            assert!(!is_auto_compact_enabled(true));
            std::env::remove_var("DISABLE_COMPACT");

            std::env::set_var("DISABLE_AUTO_COMPACT", "true");
            assert!(!is_auto_compact_enabled(true));
            std::env::remove_var("DISABLE_AUTO_COMPACT");

            // Non-truthy values do not disable.
            std::env::set_var("DISABLE_COMPACT", "0");
            assert!(is_auto_compact_enabled(true));
        });
    }

    // --- TokenWarningState field boundaries (autocompact enabled) ---------- //
    // threshold = AUTOCOMPACT = 155_000.
    //   warning  threshold = 155_000 - 20_000 = 135_000
    //   error    threshold = 155_000 - 20_000 = 135_000
    //   autocompact        = 155_000
    //   blocking limit     = effective(168_000) - 3_000 = 165_000

    #[test]
    fn warning_threshold_boundaries() {
        with_clean_env(|| {
            // just below warning
            let below = calculate_token_warning_state(134_999, MODEL, &[], true);
            assert!(!below.is_above_warning_threshold);
            // exactly at warning
            let at = calculate_token_warning_state(135_000, MODEL, &[], true);
            assert!(at.is_above_warning_threshold);
            // above
            let above = calculate_token_warning_state(150_000, MODEL, &[], true);
            assert!(above.is_above_warning_threshold);
        });
    }

    #[test]
    fn error_threshold_boundaries() {
        with_clean_env(|| {
            assert!(
                !calculate_token_warning_state(134_999, MODEL, &[], true).is_above_error_threshold
            );
            assert!(
                calculate_token_warning_state(135_000, MODEL, &[], true).is_above_error_threshold
            );
            assert!(
                calculate_token_warning_state(160_000, MODEL, &[], true).is_above_error_threshold
            );
        });
    }

    #[test]
    fn auto_compact_threshold_boundaries_enabled() {
        with_clean_env(|| {
            assert!(
                !calculate_token_warning_state(AUTOCOMPACT - 1, MODEL, &[], true)
                    .is_above_auto_compact_threshold
            );
            assert!(
                calculate_token_warning_state(AUTOCOMPACT, MODEL, &[], true)
                    .is_above_auto_compact_threshold
            );
            assert!(
                calculate_token_warning_state(AUTOCOMPACT + 5_000, MODEL, &[], true)
                    .is_above_auto_compact_threshold
            );
        });
    }

    #[test]
    fn auto_compact_threshold_always_false_when_disabled() {
        with_clean_env(|| {
            // Even above the autocompact threshold, the flag is false when
            // autocompact is disabled. The active threshold is then `effective`.
            let st = calculate_token_warning_state(AUTOCOMPACT + 5_000, MODEL, &[], false);
            assert!(!st.is_above_auto_compact_threshold);
        });
    }

    #[test]
    fn blocking_limit_boundaries() {
        with_clean_env(|| {
            // blocking limit = effective(168_000) - MANUAL_COMPACT_BUFFER(3_000) = 165_000.
            let limit = EFFECTIVE - MANUAL_COMPACT_BUFFER_TOKENS;
            assert_eq!(limit, 165_000);
            assert!(
                !calculate_token_warning_state(limit - 1, MODEL, &[], true).is_at_blocking_limit
            );
            assert!(calculate_token_warning_state(limit, MODEL, &[], true).is_at_blocking_limit);
            assert!(
                calculate_token_warning_state(limit + 1, MODEL, &[], true).is_at_blocking_limit
            );
        });
    }

    // --- percent_left rounding parity -------------------------------------- //

    #[test]
    fn percent_left_rounding_and_clamp() {
        // Math.round((threshold - usage)/threshold*100), clamped >= 0.
        // threshold = 155_000.
        with_clean_env(|| {
            // usage = 0 → 100%.
            assert_eq!(
                calculate_token_warning_state(0, MODEL, &[], true).percent_left,
                100
            );
            // usage == threshold → 0%.
            assert_eq!(
                calculate_token_warning_state(AUTOCOMPACT, MODEL, &[], true).percent_left,
                0
            );
            // usage > threshold → clamped to 0.
            assert_eq!(
                calculate_token_warning_state(AUTOCOMPACT + 100_000, MODEL, &[], true).percent_left,
                0
            );
            // half-way: usage = 77_500 → remaining 77_500/155_000 = 0.5 → round → 50.
            assert_eq!(
                calculate_token_warning_state(77_500, MODEL, &[], true).percent_left,
                50
            );
        });
    }

    #[test]
    fn percent_left_rounds_half_up() {
        // Construct a fraction that rounds half away from zero (JS Math.round).
        // threshold = 200 (via override), usage = 99 → (200-99)/200*100 = 50.5 → 51.
        with_clean_env(|| {
            std::env::set_var("LINGXI_AUTOCOMPACT_PCT_OVERRIDE", "100");
            // pct=100 makes autocompact_threshold = min(floor(effective*1.0), effective-buffer)
            //   = min(168_000, 155_000) = 155_000. Not what we want for a tiny threshold.
            // Instead use the helper directly for the rounding invariant.
            std::env::remove_var("LINGXI_AUTOCOMPACT_PCT_OVERRIDE");
            assert_eq!(super::percent_left_of(200, 99), 51);
            // 50.4 → 50: usage=101 → (200-101)/200*100 = 49.5 → 50.
            assert_eq!(super::percent_left_of(200, 101), 50);
            // threshold 0 → 0.
            assert_eq!(super::percent_left_of(0, 0), 0);
        });
    }

    // --- env overrides ----------------------------------------------------- //

    #[test]
    fn oracle_261_window_env_uses_numeric_parser_and_clamps_100k_to_1m() {
        with_clean_env(|| {
            for (raw, expected) in [
                ("50000", 100_000),
                ("150_000", 150_000),
                ("+2e5", 200_000),
                ("9000000", 1_000_000),
            ] {
                std::env::set_var("LINGXI_AUTO_COMPACT_WINDOW", raw);
                let resolved = resolve_auto_compact_window(MODEL, &[], None, true);
                assert_eq!(resolved.configured, expected, "{raw}");
                assert_eq!(resolved.window, expected.min(200_000), "{raw}");
            }
        });
    }

    #[test]
    fn oracle_261_blocking_limit_uses_full_model_window() {
        with_clean_env(|| {
            std::env::set_var("LINGXI_AUTO_COMPACT_WINDOW", "100000");
            assert!(!calculate_token_warning_state(100_000, MODEL, &[], true).is_at_blocking_limit);
            assert!(calculate_token_warning_state(177_000, MODEL, &[], true).is_at_blocking_limit);
            std::env::set_var("LINGXI_BLOCKING_LIMIT_OVERRIDE", "150_000");
            assert!(!calculate_token_warning_state(149_999, MODEL, &[], true).is_at_blocking_limit);
            assert!(calculate_token_warning_state(150_000, MODEL, &[], true).is_at_blocking_limit);
        });
    }

    #[test]
    fn auto_compact_window_clamps_context() {
        with_clean_env(|| {
            // 50k is floored to the 100k minimum; reserve 32k → effective 68k.
            std::env::set_var("LINGXI_AUTO_COMPACT_WINDOW", "50000");
            assert_eq!(effective_context_window_size(MODEL, &[]), 68_000);
            // A clamp larger than the real window is a no-op (min picks the smaller).
            std::env::set_var("LINGXI_AUTO_COMPACT_WINDOW", "999999");
            assert_eq!(effective_context_window_size(MODEL, &[]), EFFECTIVE);
            // Invalid / zero values are ignored.
            std::env::set_var("LINGXI_AUTO_COMPACT_WINDOW", "0");
            assert_eq!(effective_context_window_size(MODEL, &[]), EFFECTIVE);
            std::env::set_var("LINGXI_AUTO_COMPACT_WINDOW", "abc");
            assert_eq!(effective_context_window_size(MODEL, &[]), EFFECTIVE);
        });
    }

    #[test]
    fn autocompact_pct_override() {
        with_clean_env(|| {
            // pct=10 → floor(168_000 * 0.10) = 16_800; min(16_800, 155_000) = 16_800.
            std::env::set_var("LINGXI_AUTOCOMPACT_PCT_OVERRIDE", "10");
            assert_eq!(auto_compact_threshold(MODEL, &[]), 16_800);
            // pct=100 → floor(168_000) = 168_000; min(168_000, 155_000) = 155_000.
            std::env::set_var("LINGXI_AUTOCOMPACT_PCT_OVERRIDE", "100");
            assert_eq!(auto_compact_threshold(MODEL, &[]), AUTOCOMPACT);
            // Out of range (>100) ignored.
            std::env::set_var("LINGXI_AUTOCOMPACT_PCT_OVERRIDE", "150");
            assert_eq!(auto_compact_threshold(MODEL, &[]), AUTOCOMPACT);
            // Zero / negative ignored.
            std::env::set_var("LINGXI_AUTOCOMPACT_PCT_OVERRIDE", "0");
            assert_eq!(auto_compact_threshold(MODEL, &[]), AUTOCOMPACT);
        });
    }

    #[test]
    fn oracle_261_percent_override_accepts_js_parse_float_prefix() {
        with_clean_env(|| {
            for raw in ["10percent", "  +1e1%", "10e+"] {
                std::env::set_var("LINGXI_AUTOCOMPACT_PCT_OVERRIDE", raw);
                assert_eq!(auto_compact_threshold(MODEL, &[]), 16_800, "{raw}");
            }
            std::env::set_var("LINGXI_AUTOCOMPACT_PCT_OVERRIDE", ".5suffix");
            assert_eq!(auto_compact_threshold(MODEL, &[]), 840);
        });
    }

    #[test]
    fn blocking_limit_override() {
        with_clean_env(|| {
            std::env::set_var("LINGXI_BLOCKING_LIMIT_OVERRIDE", "100000");
            let below = calculate_token_warning_state(99_999, MODEL, &[], true);
            assert!(!below.is_at_blocking_limit);
            let at = calculate_token_warning_state(100_000, MODEL, &[], true);
            assert!(at.is_at_blocking_limit);
            // Invalid override falls back to the default (165_000).
            std::env::set_var("LINGXI_BLOCKING_LIMIT_OVERRIDE", "notanumber");
            let st = calculate_token_warning_state(100_000, MODEL, &[], true);
            assert!(!st.is_at_blocking_limit);
        });
    }

    #[test]
    fn disabled_autocompact_uses_effective_as_threshold() {
        with_clean_env(|| {
            // When disabled, threshold = effective(168_000), so percent_left at
            // usage 0 is 100 and at usage=168_000 is 0.
            assert_eq!(
                calculate_token_warning_state(0, MODEL, &[], false).percent_left,
                100
            );
            assert_eq!(
                calculate_token_warning_state(EFFECTIVE, MODEL, &[], false).percent_left,
                0
            );
            // warning threshold = 168_000 - 20_000 = 148_000.
            assert!(
                !calculate_token_warning_state(147_999, MODEL, &[], false)
                    .is_above_warning_threshold
            );
            assert!(
                calculate_token_warning_state(148_000, MODEL, &[], false)
                    .is_above_warning_threshold
            );
        });
    }

    // --- #54 rapid-refill (thrashing) breaker ------------------------------ //

    #[test]
    fn rapid_refill_count_zero_when_not_previously_compacted() {
        // `compacted=false` → kho returns 0 regardless of the other fields.
        let state = AutoCompactTrackingState {
            compacted: false,
            turn_counter: 0,
            consecutive_rapid_refills: 5,
            ..Default::default()
        };
        assert_eq!(rapid_refill_count(&state), 0);
    }

    #[test]
    fn rapid_refill_count_zero_when_turn_counter_at_or_above_window() {
        // turnCounter >= Who(3) → not a rapid refill → reset to 0.
        let state = AutoCompactTrackingState {
            compacted: true,
            turn_counter: 3,
            consecutive_rapid_refills: 2,
            ..Default::default()
        };
        assert_eq!(rapid_refill_count(&state), 0);
        let state4 = AutoCompactTrackingState {
            turn_counter: 4,
            ..state
        };
        assert_eq!(rapid_refill_count(&state4), 0);
    }

    #[test]
    fn rapid_refill_count_increments_within_window() {
        // compacted && turnCounter < Who → consecutiveRapidRefills + 1.
        // First rapid refill: prev count 0 → 1.
        let state0 = AutoCompactTrackingState {
            compacted: true,
            turn_counter: 0,
            consecutive_rapid_refills: 0,
            ..Default::default()
        };
        assert_eq!(rapid_refill_count(&state0), 1);
        // Second: prev count 1 → 2.
        let state1 = AutoCompactTrackingState {
            consecutive_rapid_refills: 1,
            turn_counter: 1,
            ..state0.clone()
        };
        assert_eq!(rapid_refill_count(&state1), 2);
        // Third: prev count 2 → 3 = MAX_CONSECUTIVE_RAPID_REFILLS → breaker trips.
        let state2 = AutoCompactTrackingState {
            consecutive_rapid_refills: 2,
            turn_counter: 2,
            ..state0
        };
        let count = rapid_refill_count(&state2);
        assert_eq!(count, 3);
        assert!(count >= MAX_CONSECUTIVE_RAPID_REFILLS, "breaker trips at 3");
    }

    #[test]
    fn rapid_refill_constants_match_binary() {
        // jho=3, Who=3, f6n=3.
        assert_eq!(MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES, 3);
        assert_eq!(RAPID_REFILL_TURN_WINDOW, 3);
        assert_eq!(MAX_CONSECUTIVE_RAPID_REFILLS, 3);
    }

    #[test]
    fn rapid_refill_thrashing_message_is_byte_exact() {
        // Byte-exact `Rho` (bin/claude.exe offset 203009521), with ${Who}/${f6n}
        // resolved to 3.
        assert_eq!(
            RAPID_REFILL_THRASHING_MESSAGE,
            "Autocompact is thrashing: the context refilled to the limit within 3 turns of the previous compact, 3 times in a row. A file being read or a tool output is likely too large for the context window. Try reading in smaller chunks, or use /clear to start fresh."
        );
    }
}
