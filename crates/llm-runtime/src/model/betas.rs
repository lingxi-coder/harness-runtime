//! `anthropic-beta` header constants + the per-model / per-request beta gate.
//!
//! **Source of truth**: the v2.1.185 binary's `xLr`/`e5` assembler (the
//! minified `getBetas(model)` path), reverse-engineered byte-for-byte against
//! `/opt/homebrew/.../claude-code-darwin-arm64/claude`. Earlier revisions of
//! this module assembled the header from `Provider × Endpoint` ONLY and emitted
//! the full constant set on every request — which both **over-emitted**
//! request-conditional betas (`web-search`, `structured-outputs`, `context-1m`,
//! `effort`, `task-budgets`, `fast-mode`, `advisor-tool`; over-emitting can
//! 400), emitted one **fabricated** beta (`token-efficient-tools-2026-03-28`,
//! 0 occurrences in the binary), and **missed** two real ones
//! (`thinking-token-count-2026-05-13`, `mid-conversation-system-2026-04-07`).
//!
//! The binary computes the set with `e5(model)` — a per-model, per-capability,
//! per-request gate. This module ports that gate for the first-party
//! (`Provider::Anthropic`) path LingXi actually drives, mapping each predicate
//! to LingXi state:
//!
//! | binary predicate | meaning | LingXi mapping |
//! |---|---|---|
//! | `isHaiku` | model id has `haiku` | [`is_haiku`] |
//! | `gkt(e)` | model supports thinking | firstParty ⇒ `!claude-3-` ([`thinking_capable`]) |
//! | `KWu(e)` | ctx-management capable | firstParty ⇒ `!claude-3-` ([`context_management_capable`]) |
//! | `T_(e)` | 1M context active | model id contains `[1m]` ([`context_1m_active`]) |
//! | `nhn(e)` | mid-conversation-system | model not in the older-exclusion list ([`mid_conversation_system`]) |
//! | `BO()` | experimental betas on | `!CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS` ([`BetaContext::experimental_on`]) |
//! | `kr()` | `!isInteractive` (SDK) | [`BetaContext::interactive`] |
//! | `ZAn()` | `showThinkingSummaries` | [`BetaContext::show_thinking_summaries`] |
//!
//! **Bounded divergences (documented, not gaps):**
//! - **oauth-2025-04-20** stays driven by the subscriber flag through
//!   [`apply_beta_header_with_auth`] (the binary's `VEe` push uses `iE()`
//!   token-presence; LingXi already threads `is_subscriber`, so the existing
//!   mechanism is preserved rather than duplicated).
//! - **cache-editing beta/session latch** is intentionally absent here. Claude
//!   Code arms cache editing only with an additional once-per-session beta
//!   latch plus cross-call pinned state; LingXi's request mutator remains
//!   fail-closed in `ApiService::should_use_cache_editing()` until that full
//!   protocol is wired, so this assembler emits no cache-editing-specific beta.
//! - The per-feature betas `effort` / `task-budgets` / `advisor-tool` /
//!   `web-search` / `structured-outputs` are emitted by the binary ONLY when the
//!   request carries that param (they live outside `xLr`, added at the
//!   request-build layer). LingXi's first-party `messages.create` body never
//!   carries those keys, so they are correctly omitted. `fast-mode` rides only
//!   when the request body sets `speed: "fast"` ([`BetaContext::fast_mode`]).
//! - `narration_summaries` (`summarize-connector-text`) is behind the
//!   `pewter_owl_header` server flag (default off) — never emitted.
//!
//! Constants are emitted in `xLr` declaration order and comma-joined with no
//! spaces, matching the binary.

#![forbid(unsafe_code)]

// ---- beta constants (declaration order = the binary's `qS(...)` registry) ----

/// Core claude-code feature gate (`J1e`).
pub const CLAUDE_CODE_BETA: &str = "claude-code-20250219";
/// Interleaved-thinking output (`pYe`).
pub const INTERLEAVED_THINKING: &str = "interleaved-thinking-2025-05-14";
/// 1M-token context window (`$7`, gated by `T_`).
pub const CONTEXT_1M: &str = "context-1m-2025-08-07";
/// Context-management endpoint (`X1e`).
pub const CONTEXT_MANAGEMENT: &str = "context-management-2025-06-27";
/// Structured-output JSON-schema mode (`bQ`, `tengu_tool_pear` flag-gated).
pub const STRUCTURED_OUTPUTS: &str = "structured-outputs-2025-12-15";
/// First-party web-search tool gating (`Xvt`, vertex/foundry only).
pub const WEB_SEARCH: &str = "web-search-2025-03-05";
/// Advanced tool-use semantics (`fRr`, 1P only).
pub const ADVANCED_TOOL_USE_1P: &str = "advanced-tool-use-2025-11-20";
/// Tool-search tool (`Qvt`, Vertex / Bedrock).
pub const TOOL_SEARCH_TOOL_3P: &str = "tool-search-tool-2025-10-19";
/// Effort hint header (`mYe`, per-request `output_config.effort`).
pub const EFFORT: &str = "effort-2025-11-24";
/// Per-task token budget enforcement (`wun`, per-request `output_config.task_budget`).
pub const TASK_BUDGETS: &str = "task-budgets-2026-03-13";
/// Prompt-caching scope control (`fYe`, experimental).
pub const PROMPT_CACHING_SCOPE: &str = "prompt-caching-scope-2026-01-05";
/// Context-hint negotiation (`_9i`, per-request `context_hint` body key).
///
/// Gated OFF by default at the caller — the oracle's `tengu_hazel_osprey` is
/// false in the binary AND server-delivered as false, so this header only ever
/// goes out when a host explicitly opts in.
pub const CONTEXT_HINT: &str = "context-hint-2026-04-09";
/// Fast-mode / `speed: "fast"` (`AYe`, per-request).
pub const FAST_MODE: &str = "fast-mode-2026-02-01";
/// Redact thinking-block output (`Zvt`, experimental ∧ thinking ∧ interactive).
pub const REDACT_THINKING: &str = "redact-thinking-2026-02-12";
/// Thinking-token-count (`Run`, experimental ∧ thinking ∧ firstParty).
///
/// NEW vs the prior static module — the binary emits this on every first-party
/// thinking request; LingXi previously missed it.
pub const THINKING_TOKEN_COUNT: &str = "thinking-token-count-2026-05-13";
/// Mid-conversation system messages (`q7`, `nhn`-gated by model).
///
/// NEW vs the prior static module — the binary emits this for opus-4-8 /
/// fable-5 / mythos-5 (and unknown first-party models); LingXi previously
/// missed it.
pub const MID_CONVERSATION_SYSTEM: &str = "mid-conversation-system-2026-04-07";
/// Connector-text summarization (`ewt`, `pewter_owl_header` flag — never emitted).
pub const SUMMARIZE_CONNECTOR_TEXT: &str = "summarize-connector-text-2026-03-13";
/// AFK transcript-classifier mode (`PH`).
pub const AFK_MODE: &str = "afk-mode-2026-01-31";
/// Advisor tool integration (`ARr`, per-request advisor tool).
pub const ADVISOR_TOOL: &str = "advisor-tool-2026-03-01";
/// OAuth bearer-token auth (`VEe`, subscriber-driven via [`apply_beta_header_with_auth`]).
pub const OAUTH: &str = "oauth-2025-04-20";

// ---- Per-provider / per-endpoint policy whitelists --------------------------

/// `count_tokens` keeps only these (the binary's `ERr` set, applied on the
/// first-party `beta.messages.countTokens` path: `e5(model).filter(ERr.has)`).
///
/// Four entries — includes [`OAUTH`], which the prior 3-entry `Vertex`-labelled
/// whitelist omitted. The binary applies `ERr` on the first-party count_tokens
/// path, not just Vertex.
pub const COUNT_TOKENS_ALLOWED: &[&str] = &[
    CLAUDE_CODE_BETA,
    INTERLEAVED_THINKING,
    CONTEXT_MANAGEMENT,
    OAUTH,
];

/// Bedrock requires these to ride in `extraBodyParams`, NOT the header
/// (the binary's `bRr` set: `e5(model).filter(n => !bRr.has(n))`).
pub const BEDROCK_EXTRA_PARAMS_HEADERS: &[&str] =
    &[INTERLEAVED_THINKING, CONTEXT_1M, TOOL_SEARCH_TOOL_3P];

// ---- Provider × Endpoint matrix --------------------------------------------

/// API provider routing target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// Direct Anthropic API (the binary's `firstParty`).
    Anthropic,
    /// Google Cloud Vertex AI Anthropic models.
    Vertex,
    /// AWS Bedrock Anthropic models.
    Bedrock,
}

/// Endpoint kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    /// Non-streaming `POST /v1/messages`.
    MessagesCreate,
    /// Streaming `POST /v1/messages` (`stream: true`).
    MessagesCreateStream,
    /// `POST /v1/messages/count_tokens`.
    CountTokens,
}

/// Per-request inputs to the beta gate (the binary's `xLr(model)` closure
/// state). Built at the call site from the prepared request + session state.
#[derive(Debug, Clone)]
pub struct BetaContext {
    /// Resolved request model id (e.g. `claude-opus-4-8`).
    pub model: String,
    /// `!isInteractive` is the binary's `kr()`; this is `isInteractive`. Gates
    /// `redact-thinking`. Defaults to `true` (the interactive TUI is LingXi's
    /// dominant path).
    pub interactive: bool,
    /// The binary's `ZAn()` (`settings.showThinkingSummaries`). When `true`,
    /// `redact-thinking` is suppressed. Defaults to `false`.
    pub show_thinking_summaries: bool,
    /// The request body sets `speed: "fast"`. Gates `fast-mode`. Defaults
    /// to `false`.
    pub fast_mode: bool,
    /// The request body sets `output_config.effort`. Gates the [`EFFORT`] beta.
    /// Defaults to `false`.
    pub effort: bool,
    /// Request uses ToolSearch/deferred schemas. Appends the first-party
    /// `advanced-tool-use` beta after the ordinary model betas.
    pub tool_search: bool,
    /// The request body sets `context_hint`. Gates the [`CONTEXT_HINT`] beta.
    /// Defaults to `false`.
    pub context_hint: bool,
}

impl BetaContext {
    /// Context for `model` with the faithful external-default gates
    /// (interactive TUI, no thinking summaries, standard speed).
    #[must_use]
    pub fn for_model(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            interactive: true,
            show_thinking_summaries: false,
            fast_mode: false,
            effort: false,
            tool_search: false,
            context_hint: false,
        }
    }

    /// Builder: set the interactive flag (the binary's `isInteractive`).
    #[must_use]
    pub fn with_interactive(mut self, interactive: bool) -> Self {
        self.interactive = interactive;
        self
    }

    /// Builder: set `showThinkingSummaries` (the binary's `ZAn()`).
    #[must_use]
    pub fn with_show_thinking_summaries(mut self, on: bool) -> Self {
        self.show_thinking_summaries = on;
        self
    }

    /// Builder: set `fast_mode` (request body `speed: "fast"`).
    #[must_use]
    pub fn with_fast_mode(mut self, on: bool) -> Self {
        self.fast_mode = on;
        self
    }

    /// Builder: set `effort` (request body `output_config.effort`).
    #[must_use]
    pub fn with_effort(mut self, on: bool) -> Self {
        self.effort = on;
        self
    }

    /// Builder: set dynamic ToolSearch usage for this request.
    #[must_use]
    pub fn with_tool_search(mut self, on: bool) -> Self {
        self.tool_search = on;
        self
    }

    /// Set [`Self::context_hint`] — the request body carries `context_hint`.
    #[must_use]
    pub fn with_context_hint(mut self, on: bool) -> Self {
        self.context_hint = on;
        self
    }

    /// The binary's `BO()` — experimental betas enabled. `RLr()` is always true
    /// for the first-party path; `$Be()` is `CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS`
    /// (HIPAA compliance taint is out of single-process scope here).
    fn experimental_on(&self) -> bool {
        !env_truthy("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS")
    }
}

/// Normalize a model id for the substring gates (lowercase; the binary's `Fo`).
fn norm(model: &str) -> String {
    model.to_ascii_lowercase()
}

/// The binary's `isHaiku` — model id contains `haiku`.
fn is_haiku(model: &str) -> bool {
    norm(model).contains("haiku")
}

/// The binary's `gkt(e)` for the first-party path: thinking-capable unless a
/// `claude-3-*` model.
fn thinking_capable(model: &str) -> bool {
    !norm(model).contains("claude-3-")
}

/// The binary's `KWu(e)` for the first-party path: context-management-capable
/// unless a `claude-3-*` model.
fn context_management_capable(model: &str) -> bool {
    !norm(model).contains("claude-3-")
}

/// The binary's `T_(e)` — 1M context active iff the model id carries `[1m]`.
fn context_1m_active(model: &str) -> bool {
    norm(model).contains("[1m]")
}

/// The binary's `nhn(e)` for the first-party path: mid-conversation-system is
/// emitted for every first-party model EXCEPT the enumerated older ones
/// (returns `false` for `claude-3-*` and the explicit pre-4.8 list; `true` for
/// fable-5 / mythos-5 / opus-4-8 and any other first-party model).
fn mid_conversation_system(model: &str) -> bool {
    let m = norm(model);
    if m.contains("claude-3-") {
        return false;
    }
    const OLDER: &[&str] = &[
        "opus-4-0",
        "opus-4-1",
        "opus-4-5",
        "opus-4-6",
        "opus-4-7",
        "sonnet-4-0",
        "sonnet-4-5",
        "sonnet-4-6",
        "sonnet-5",
        "haiku-4-5",
    ];
    !OLDER.iter().any(|older| m.contains(older))
}

/// Whether `var` is set to a non-empty, non-`0`/`false` value.
fn env_truthy(var: &str) -> bool {
    match std::env::var(var) {
        Ok(v) => {
            let v = v.trim();
            !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false")
        }
        Err(_) => false,
    }
}

/// Build the first-party (`xLr(model)`) ordered beta set for messages.create.
fn first_party_betas(ctx: &BetaContext) -> Vec<&'static str> {
    let model = &ctx.model;
    let exp = ctx.experimental_on();
    let gkt = thinking_capable(model);
    let mut betas: Vec<&'static str> = Vec::new();

    // 1. claude-code (unless Haiku).
    if !is_haiku(model) {
        betas.push(CLAUDE_CODE_BETA);
    }
    // (2. oauth `VEe` is appended by apply_beta_header_with_auth — subscriber-driven.)
    // 3. context-1m, only when the model id carries `[1m]`.
    if context_1m_active(model) {
        betas.push(CONTEXT_1M);
    }
    // 4. interleaved-thinking, model thinking-capable ∧ not env-disabled.
    if !env_truthy("DISABLE_INTERLEAVED_THINKING") && gkt {
        betas.push(INTERLEAVED_THINKING);
    }
    // 5. redact-thinking: experimental ∧ thinking ∧ interactive ∧ !summaries.
    if exp && gkt && ctx.interactive && !ctx.show_thinking_summaries {
        betas.push(REDACT_THINKING);
    }
    // 6. thinking-token-count: experimental ∧ thinking ∧ firstParty.
    if exp && gkt {
        betas.push(THINKING_TOKEN_COUNT);
    }
    // 7. narration_summaries (`pewter_owl_header` flag, default off) — skipped.
    // 8. context-management: !$Be ∧ KWu(model).
    if exp && context_management_capable(model) {
        betas.push(CONTEXT_MANAGEMENT);
    }
    // 9. structured-outputs (`tengu_tool_pear` flag, default off) — skipped.
    // 10. web-search (vertex/foundry only) — skipped on first-party.
    // 11. prompt-caching-scope: experimental.
    if exp {
        betas.push(PROMPT_CACHING_SCOPE);
    }
    // 12. mid-conversation-system: nhn(model).
    if mid_conversation_system(model) {
        betas.push(MID_CONVERSATION_SYSTEM);
    }
    // (ANTHROPIC_BETAS env append is honored by apply_beta_header's merge.)
    // Per-feature: fast-mode when the request sets speed:"fast".
    if ctx.fast_mode {
        betas.push(FAST_MODE);
    }
    // Per-feature: effort when the request sets output_config.effort.
    if ctx.effort {
        betas.push(EFFORT);
    }
    // Per-feature: context-hint when the request sets the `context_hint` key.
    if ctx.context_hint {
        betas.push(CONTEXT_HINT);
    }
    // claude.ts appends the provider-specific tool-search header after the
    // ordinary model/request beta list once `useToolSearch` is resolved.
    if ctx.tool_search && exp {
        betas.push(ADVANCED_TOOL_USE_1P);
    }
    betas
}

/// Build the `anthropic-beta` header value for `provider` + `endpoint` + the
/// per-request [`BetaContext`].
///
/// Ports the binary's `e5(model)`: the first-party ordered set from
/// [`first_party_betas`], then provider/endpoint narrowing:
/// - **`CountTokens`** keeps only [`COUNT_TOKENS_ALLOWED`] (the binary's `ERr`).
/// - **`Bedrock`** drops [`BEDROCK_EXTRA_PARAMS_HEADERS`] (the binary's `bRr`),
///   which move to `extraBodyParams`.
/// - **`Vertex`** drops the 1P-only [`ADVANCED_TOOL_USE_1P`] (not emitted on the
///   first-party path here anyway).
#[must_use]
pub fn assemble_beta_header(provider: Provider, endpoint: Endpoint, ctx: &BetaContext) -> String {
    let mut betas = first_party_betas(ctx);

    // count_tokens: intersect with the ERr whitelist (any provider).
    if matches!(endpoint, Endpoint::CountTokens) {
        betas.retain(|b| COUNT_TOKENS_ALLOWED.contains(b));
    }

    match provider {
        Provider::Anthropic => {}
        Provider::Vertex => {
            betas.retain(|b| *b != ADVANCED_TOOL_USE_1P);
            if ctx.tool_search && ctx.experimental_on() {
                betas.push(TOOL_SEARCH_TOOL_3P);
            }
        }
        Provider::Bedrock => {
            betas.retain(|b| {
                !BEDROCK_EXTRA_PARAMS_HEADERS.contains(b) && *b != ADVANCED_TOOL_USE_1P
            });
        }
    }

    betas.join(",")
}

/// Beta identifiers Bedrock requires in the request body's
/// `anthropic_beta` array rather than the HTTP header.
#[must_use]
pub fn bedrock_extra_body_betas(ctx: &BetaContext) -> Vec<String> {
    let mut betas: Vec<String> = first_party_betas(ctx)
        .into_iter()
        .filter(|beta| BEDROCK_EXTRA_PARAMS_HEADERS.contains(beta))
        .map(str::to_string)
        .collect();
    if ctx.tool_search && ctx.experimental_on() {
        betas.push(TOOL_SEARCH_TOOL_3P.to_string());
    }
    betas
}

/// Inject the assembled `anthropic-beta` header into a prepared request,
/// merging with any pre-existing value (e.g. an auth-injected `oauth-2025-04-20`).
///
/// Pre-existing values are preserved first in their original order; assembled
/// entries not already present are appended in declaration order; duplicates
/// are dropped. See the auth-layer note on [`apply_beta_header_with_auth`].
pub fn apply_beta_header(
    request: &mut crate::ProviderRequest,
    provider: Provider,
    endpoint: Endpoint,
    ctx: &BetaContext,
) {
    lingxi_llm_client::providers::anthropic::request_policy::merge_beta_header(
        &mut request.headers,
        &[assemble_beta_header(provider, endpoint, ctx)],
    );
}

/// Variant of [`apply_beta_header`] that also appends `oauth-2025-04-20` when
/// `is_oauth_subscriber` is `true`.
///
/// **Parity:** mirrors `betas.ts`'s subscriber push
/// (`if (isClaudeAISubscriber()) betaHeaders.push(OAUTH_BETA_HEADER)`).
/// Under OAuth subscriber auth, `oauth-2025-04-20` is appended as one element of
/// the comma-joined list — never as a clobbering insert; the dedup pass keeps it
/// at most once. For non-subscriber routes this behaves like [`apply_beta_header`].
pub fn apply_beta_header_with_auth(
    request: &mut crate::ProviderRequest,
    provider: Provider,
    endpoint: Endpoint,
    ctx: &BetaContext,
    is_oauth_subscriber: bool,
) {
    apply_beta_header_with_auth_and_custom(
        request,
        provider,
        endpoint,
        ctx,
        is_oauth_subscriber,
        &[],
    );
}

/// Auth-aware beta assembly with an explicit, host-validated CLI beta list.
/// Keeping the custom values as an argument prevents process environment state
/// from leaking them onto a custom Anthropic-wire-compatible provider route.
pub fn apply_beta_header_with_auth_and_custom(
    request: &mut crate::ProviderRequest,
    provider: Provider,
    endpoint: Endpoint,
    ctx: &BetaContext,
    is_oauth_subscriber: bool,
    custom_betas: &[String],
) {
    apply_beta_header(request, provider, endpoint, ctx);

    // CLI `--betas`: API-key-only, first-party messages.create additions. The
    // caller passes values only for an AnthropicFirstParty route; this layer
    // still excludes OAuth and non-Anthropic/count endpoints.
    if !is_oauth_subscriber
        && matches!(provider, Provider::Anthropic)
        && matches!(
            endpoint,
            Endpoint::MessagesCreate | Endpoint::MessagesCreateStream
        )
        && !custom_betas.is_empty()
    {
        lingxi_llm_client::providers::anthropic::request_policy::merge_beta_header(
            &mut request.headers,
            custom_betas,
        );
    }

    if is_oauth_subscriber {
        lingxi_llm_client::providers::anthropic::request_policy::merge_beta_header(
            &mut request.headers,
            &[OAUTH.to_string()],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 17 live constants are byte-exact against the binary's `qS(...)` registry.
    #[test]
    fn beta_constants_match_binary_byte_for_byte() {
        assert_eq!(CLAUDE_CODE_BETA, "claude-code-20250219");
        assert_eq!(INTERLEAVED_THINKING, "interleaved-thinking-2025-05-14");
        assert_eq!(CONTEXT_1M, "context-1m-2025-08-07");
        assert_eq!(CONTEXT_MANAGEMENT, "context-management-2025-06-27");
        assert_eq!(STRUCTURED_OUTPUTS, "structured-outputs-2025-12-15");
        assert_eq!(WEB_SEARCH, "web-search-2025-03-05");
        assert_eq!(ADVANCED_TOOL_USE_1P, "advanced-tool-use-2025-11-20");
        assert_eq!(TOOL_SEARCH_TOOL_3P, "tool-search-tool-2025-10-19");
        assert_eq!(EFFORT, "effort-2025-11-24");
        assert_eq!(CONTEXT_HINT, "context-hint-2026-04-09");
        assert_eq!(TASK_BUDGETS, "task-budgets-2026-03-13");
        assert_eq!(PROMPT_CACHING_SCOPE, "prompt-caching-scope-2026-01-05");
        assert_eq!(FAST_MODE, "fast-mode-2026-02-01");
        assert_eq!(REDACT_THINKING, "redact-thinking-2026-02-12");
        assert_eq!(THINKING_TOKEN_COUNT, "thinking-token-count-2026-05-13");
        assert_eq!(
            MID_CONVERSATION_SYSTEM,
            "mid-conversation-system-2026-04-07"
        );
        assert_eq!(ADVISOR_TOOL, "advisor-tool-2026-03-01");
        assert_eq!(OAUTH, "oauth-2025-04-20");
    }

    /// `token-efficient-tools` was fabricated — it must NOT be a live constant
    /// nor appear in any assembled header. (Regression lock for the over-emit fix.)
    #[test]
    fn fabricated_token_efficient_tools_is_gone() {
        let h = assemble_beta_header(
            Provider::Anthropic,
            Endpoint::MessagesCreate,
            &BetaContext::for_model("claude-opus-4-8"),
        );
        assert!(
            !h.contains("token-efficient-tools"),
            "fabricated token-efficient-tools must never be emitted; got: {h}"
        );
    }

    /// Default first-party Opus-4.8 (interactive, experimental on) emits exactly
    /// the `xLr` core set in declaration order — the ~7-beta faithful set, NOT
    /// the prior static 14.
    #[test]
    fn anthropic_opus48_emits_faithful_core_set() {
        // Ensure experimental gate is on for this assertion.
        if env_truthy("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS")
            || env_truthy("DISABLE_INTERLEAVED_THINKING")
        {
            return;
        }
        let h = assemble_beta_header(
            Provider::Anthropic,
            Endpoint::MessagesCreate,
            &BetaContext::for_model("claude-opus-4-8"),
        );
        let expected = [
            CLAUDE_CODE_BETA,
            INTERLEAVED_THINKING,
            REDACT_THINKING,
            THINKING_TOKEN_COUNT,
            CONTEXT_MANAGEMENT,
            PROMPT_CACHING_SCOPE,
            MID_CONVERSATION_SYSTEM,
        ]
        .join(",");
        assert_eq!(h, expected, "faithful core set mismatch");
    }

    /// The previously over-emitted request-conditional betas must NOT appear on
    /// a default first-party messages.create (no web-search tool, no schema, no
    /// effort/task-budget/advisor, no `[1m]`, standard speed).
    #[test]
    fn over_emitted_betas_are_gone_by_default() {
        if env_truthy("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS") {
            return;
        }
        let h = assemble_beta_header(
            Provider::Anthropic,
            Endpoint::MessagesCreate,
            &BetaContext::for_model("claude-opus-4-8"),
        );
        for gone in [
            WEB_SEARCH,
            STRUCTURED_OUTPUTS,
            CONTEXT_1M,
            EFFORT,
            TASK_BUDGETS,
            FAST_MODE,
            ADVISOR_TOOL,
            TOOL_SEARCH_TOOL_3P,
            ADVANCED_TOOL_USE_1P,
        ] {
            assert!(
                !h.split(',').any(|p| p == gone),
                "{gone} must not be emitted by default; got: {h}"
            );
        }
    }

    /// Haiku drops `claude-code-20250219` (the binary's `!isHaiku` gate).
    #[test]
    fn haiku_drops_claude_code_beta() {
        let h = assemble_beta_header(
            Provider::Anthropic,
            Endpoint::MessagesCreate,
            &BetaContext::for_model("claude-haiku-4-5"),
        );
        assert!(
            !h.split(',').any(|p| p == CLAUDE_CODE_BETA),
            "Haiku must not carry claude-code beta; got: {h}"
        );
    }

    /// `mid-conversation-system` is gated by model: emitted for opus-4-8 /
    /// fable-5, NOT for the enumerated older models (sonnet-4-6, opus-4-7).
    #[test]
    fn mid_conversation_system_is_model_gated() {
        assert!(mid_conversation_system("claude-opus-4-8"));
        assert!(mid_conversation_system("claude-fable-5-1"));
        assert!(mid_conversation_system("claude-mythos-5-1"));
        // 2.1.201 gate: claude-sonnet-5 is in the OLDER return-false branch
        // (`n==="claude-sonnet-5"||n==="claude-haiku-4-5")return!1`), so the
        // beta does NOT ride. Contains-hazard lock: the "sonnet-5" exclude must
        // match "claude-sonnet-5" but NOT "sonnet-4-5"/"sonnet-4-6".
        assert!(!mid_conversation_system("claude-sonnet-5"));
        assert!(!mid_conversation_system("claude-sonnet-4-6"));
        assert!(!mid_conversation_system("claude-sonnet-4-5"));
        assert!(!mid_conversation_system("claude-opus-4-7"));
        assert!(!mid_conversation_system("claude-3-5-sonnet"));
    }

    /// `context-1m` rides only when the model id carries `[1m]`.
    #[test]
    fn context_1m_only_when_model_marked() {
        if env_truthy("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS") {
            return;
        }
        let off = assemble_beta_header(
            Provider::Anthropic,
            Endpoint::MessagesCreate,
            &BetaContext::for_model("claude-sonnet-4-6"),
        );
        assert!(!off.split(',').any(|p| p == CONTEXT_1M));
        let on = assemble_beta_header(
            Provider::Anthropic,
            Endpoint::MessagesCreate,
            &BetaContext::for_model("claude-sonnet-4-6[1m]"),
        );
        assert!(on.split(',').any(|p| p == CONTEXT_1M), "got: {on}");
    }

    /// `redact-thinking` rides only in interactive mode without thinking summaries.
    #[test]
    fn redact_thinking_gated_on_interactive() {
        if env_truthy("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS") {
            return;
        }
        let interactive = assemble_beta_header(
            Provider::Anthropic,
            Endpoint::MessagesCreate,
            &BetaContext::for_model("claude-opus-4-8"),
        );
        assert!(interactive.split(',').any(|p| p == REDACT_THINKING));

        let headless = assemble_beta_header(
            Provider::Anthropic,
            Endpoint::MessagesCreate,
            &BetaContext::for_model("claude-opus-4-8").with_interactive(false),
        );
        assert!(
            !headless.split(',').any(|p| p == REDACT_THINKING),
            "got: {headless}"
        );

        let summarized = assemble_beta_header(
            Provider::Anthropic,
            Endpoint::MessagesCreate,
            &BetaContext::for_model("claude-opus-4-8").with_show_thinking_summaries(true),
        );
        assert!(
            !summarized.split(',').any(|p| p == REDACT_THINKING),
            "got: {summarized}"
        );
    }

    /// `fast-mode` rides only when the request sets speed:"fast".
    #[test]
    fn fast_mode_gated_on_request() {
        let off = assemble_beta_header(
            Provider::Anthropic,
            Endpoint::MessagesCreate,
            &BetaContext::for_model("claude-opus-4-8"),
        );
        assert!(!off.split(',').any(|p| p == FAST_MODE));
        let on = assemble_beta_header(
            Provider::Anthropic,
            Endpoint::MessagesCreate,
            &BetaContext::for_model("claude-opus-4-8").with_fast_mode(true),
        );
        assert!(on.split(',').any(|p| p == FAST_MODE), "got: {on}");
    }

    /// count_tokens keeps only the `ERr` whitelist (claude-code,
    /// interleaved-thinking, context-management) — never thinking-token-count,
    /// redact-thinking, prompt-caching-scope, mid-conversation-system.
    #[test]
    fn count_tokens_filters_to_err_whitelist() {
        if env_truthy("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS")
            || env_truthy("DISABLE_INTERLEAVED_THINKING")
        {
            return;
        }
        let h = assemble_beta_header(
            Provider::Anthropic,
            Endpoint::CountTokens,
            &BetaContext::for_model("claude-opus-4-8"),
        );
        let expected = [CLAUDE_CODE_BETA, INTERLEAVED_THINKING, CONTEXT_MANAGEMENT].join(",");
        assert_eq!(h, expected, "count_tokens must equal the ERr-filtered set");
        for excluded in [
            REDACT_THINKING,
            THINKING_TOKEN_COUNT,
            PROMPT_CACHING_SCOPE,
            MID_CONVERSATION_SYSTEM,
        ] {
            assert!(
                !h.split(',').any(|p| p == excluded),
                "{excluded} leaked into count_tokens: {h}"
            );
        }
    }

    /// COUNT_TOKENS_ALLOWED is exactly the binary's 4-entry `ERr` set
    /// (now including OAUTH).
    #[test]
    fn count_tokens_whitelist_is_err_set() {
        assert_eq!(COUNT_TOKENS_ALLOWED.len(), 4);
        for b in [
            CLAUDE_CODE_BETA,
            INTERLEAVED_THINKING,
            CONTEXT_MANAGEMENT,
            OAUTH,
        ] {
            assert!(
                COUNT_TOKENS_ALLOWED.contains(&b),
                "{b} missing from ERr whitelist"
            );
        }
    }

    #[test]
    fn bedrock_extra_params_headers_is_exactly_three() {
        assert_eq!(BEDROCK_EXTRA_PARAMS_HEADERS.len(), 3);
        assert!(BEDROCK_EXTRA_PARAMS_HEADERS.contains(&INTERLEAVED_THINKING));
        assert!(BEDROCK_EXTRA_PARAMS_HEADERS.contains(&CONTEXT_1M));
        assert!(BEDROCK_EXTRA_PARAMS_HEADERS.contains(&TOOL_SEARCH_TOOL_3P));
    }

    /// Bedrock drops the `bRr` header-only entries (interleaved-thinking etc.).
    #[test]
    fn bedrock_excludes_b_rr_entries() {
        let h = assemble_beta_header(
            Provider::Bedrock,
            Endpoint::MessagesCreate,
            &BetaContext::for_model("claude-opus-4-8"),
        );
        for excluded in BEDROCK_EXTRA_PARAMS_HEADERS {
            assert!(
                !h.split(',').any(|p| p == *excluded),
                "Bedrock must not carry {excluded}; got: {h}"
            );
        }
    }

    /// The `context_hint` body key must actually light its beta header.
    ///
    /// Without this, deleting the push compiles and every other test passes —
    /// the request would carry the body key and no header, which the server
    /// rejects as an unknown field rather than negotiating.
    #[test]
    fn context_hint_body_lights_the_beta_header() {
        let model = "claude-opus-4-6";
        let off = first_party_betas(&BetaContext::for_model(model));
        assert!(
            !off.contains(&CONTEXT_HINT),
            "no `context_hint` in the body ⇒ no beta: {off:?}"
        );
        let on = first_party_betas(&BetaContext::for_model(model).with_context_hint(true));
        assert!(
            on.contains(&CONTEXT_HINT),
            "`context_hint` in the body ⇒ the beta rides along: {on:?}"
        );
    }

    #[test]
    fn tool_search_uses_provider_specific_beta_locations() {
        if env_truthy("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS") {
            return;
        }
        let ctx = BetaContext::for_model("claude-sonnet-4-5").with_tool_search(true);
        let first_party = assemble_beta_header(Provider::Anthropic, Endpoint::MessagesCreate, &ctx);
        assert!(first_party
            .split(',')
            .any(|beta| beta == ADVANCED_TOOL_USE_1P));

        let vertex = assemble_beta_header(Provider::Vertex, Endpoint::MessagesCreate, &ctx);
        assert!(vertex.split(',').any(|beta| beta == TOOL_SEARCH_TOOL_3P));
        assert!(!vertex.split(',').any(|beta| beta == ADVANCED_TOOL_USE_1P));

        let bedrock = bedrock_extra_body_betas(&ctx);
        assert!(bedrock.iter().any(|beta| beta == TOOL_SEARCH_TOOL_3P));
        assert!(!bedrock.iter().any(|beta| beta == ADVANCED_TOOL_USE_1P));
    }

    /// A pre-existing `anthropic-beta` value (e.g. auth-injected oauth) is
    /// preserved and the assembled set appended after it (no clobber, no dup).
    #[test]
    fn apply_beta_header_preserves_existing_values() {
        let mut req = crate::ProviderRequest::post_json(
            "https://api.anthropic.com/v1/messages",
            serde_json::json!({"model": "claude-opus-4-8", "max_tokens": 1024}),
        );
        req.headers
            .insert("anthropic-beta".to_string(), "oauth-2025-04-20".to_string());

        apply_beta_header(
            &mut req,
            Provider::Anthropic,
            Endpoint::MessagesCreate,
            &BetaContext::for_model("claude-opus-4-8"),
        );

        let value = req.headers.get("anthropic-beta").expect("header present");
        assert!(value.starts_with("oauth-2025-04-20,"), "got: {value}");
        assert_eq!(
            value
                .split(',')
                .filter(|p| *p == "oauth-2025-04-20")
                .count(),
            1
        );
    }

    /// Subscriber path appends oauth exactly once alongside the model betas.
    #[test]
    fn oauth_beta_appended_for_subscriber() {
        let mut req = crate::ProviderRequest::post_json(
            "https://api.anthropic.com/v1/messages",
            serde_json::json!({"model": "claude-opus-4-8", "max_tokens": 1024}),
        );
        apply_beta_header_with_auth(
            &mut req,
            Provider::Anthropic,
            Endpoint::MessagesCreate,
            &BetaContext::for_model("claude-opus-4-8"),
            true,
        );
        let value = req.headers.get("anthropic-beta").expect("header present");
        assert!(
            value.split(',').any(|p| p == CLAUDE_CODE_BETA),
            "got: {value}"
        );
        assert!(value.split(',').any(|p| p == OAUTH), "got: {value}");
        assert_eq!(value.split(',').filter(|p| *p == OAUTH).count(), 1);
    }

    /// Non-subscriber path does NOT add oauth.
    #[test]
    fn oauth_beta_absent_for_non_subscriber() {
        let mut req = crate::ProviderRequest::post_json(
            "https://api.anthropic.com/v1/messages",
            serde_json::json!({"model": "claude-opus-4-8", "max_tokens": 1024}),
        );
        apply_beta_header_with_auth(
            &mut req,
            Provider::Anthropic,
            Endpoint::MessagesCreate,
            &BetaContext::for_model("claude-opus-4-8"),
            false,
        );
        let value = req.headers.get("anthropic-beta").expect("header present");
        assert!(!value.split(',').any(|p| p == OAUTH), "got: {value}");
    }

    #[test]
    fn custom_cli_betas_apply_to_stream_and_are_rejected_for_oauth() {
        let make = || {
            crate::ProviderRequest::post_json(
                "https://api.anthropic.com/v1/messages",
                serde_json::json!({"model": "claude-opus-4-8", "max_tokens": 1024}),
            )
        };
        let custom = vec!["example-beta-1".to_string(), "example-beta-2".to_string()];

        let mut api_key_req = make();
        apply_beta_header_with_auth_and_custom(
            &mut api_key_req,
            Provider::Anthropic,
            Endpoint::MessagesCreateStream,
            &BetaContext::for_model("claude-opus-4-8"),
            false,
            &custom,
        );
        let header = api_key_req.headers["anthropic-beta"].as_str();
        assert!(header.split(',').any(|part| part == "example-beta-1"));
        assert!(header.split(',').any(|part| part == "example-beta-2"));

        let mut oauth_req = make();
        apply_beta_header_with_auth_and_custom(
            &mut oauth_req,
            Provider::Anthropic,
            Endpoint::MessagesCreateStream,
            &BetaContext::for_model("claude-opus-4-8"),
            true,
            &custom,
        );
        let header = oauth_req.headers["anthropic-beta"].as_str();
        assert!(!header.split(',').any(|part| part == "example-beta-1"));
        assert!(header.split(',').any(|part| part == OAUTH));
    }

    /// `apply_beta_header` on a clean request equals `assemble_beta_header`.
    #[test]
    fn apply_beta_header_inserts_assembled_header() {
        let mut req = crate::ProviderRequest::post_json(
            "https://api.anthropic.com/v1/messages",
            serde_json::json!({"model": "claude-opus-4-8", "max_tokens": 1024}),
        );
        assert!(!req.headers.contains_key("anthropic-beta"));
        let ctx = BetaContext::for_model("claude-opus-4-8");
        apply_beta_header(
            &mut req,
            Provider::Anthropic,
            Endpoint::MessagesCreate,
            &ctx,
        );
        let expected = assemble_beta_header(Provider::Anthropic, Endpoint::MessagesCreate, &ctx);
        assert_eq!(
            req.headers.get("anthropic-beta").map(String::as_str),
            Some(expected.as_str())
        );
    }
}
