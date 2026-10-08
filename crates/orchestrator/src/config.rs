//! Orchestrator runtime configuration.
//!
//! `MAX_TURNS_DEFAULT = 0` means **UNBOUNDED**, matching claude-code: `maxTurns`
//! is an optional CLI/SDK option (`--max-turns`) that is `undefined` by default,
//! and the turn cap is enforced only when it is truthy
//! (`query.ts:1705` `if (maxTurns && nextTurnCount > maxTurns)`). So a plain
//! interactive REPL or a `claude -p` headless run is uncapped unless the user
//! passes `--max-turns N`. Override at construction via
//! `OrchestratorConfig { max_turns, .. }`; the model's own `end_turn` plus the
//! token budget are the natural terminators.

use serde::{Deserialize, Serialize};

/// Default value for [`OrchestratorConfig::max_turns`]. **`0` = UNBOUNDED**
/// (parity with claude-code's optional, default-unset `maxTurns`). A previous
/// LingXi build locked this at 30; the parity audit flagged that as a divergence
/// (a legitimate >30-turn interactive loop hard-errored), so it is now unbounded
/// by default and capped only when a caller sets a non-zero `max_turns`.
pub const MAX_TURNS_DEFAULT: u32 = 0;

/// Default model identifier. The actual model lives in user settings or
/// CLI flags (M3-01 + M5-12); this value is only used when the embedder
/// constructs an orchestrator with `OrchestratorConfig::default()` for
/// tests.
pub const DEFAULT_MODEL: &str = "claude-opus-4-8";

/// Main-query `querySource` for the CLI TUI, stdio REPL, and `--print`.
pub const QUERY_SOURCE_REPL_MAIN_THREAD: &str = "repl_main_thread";

/// Main-query `querySource` for SDK / bridge / transport hosts.
pub const QUERY_SOURCE_SDK: &str = "sdk";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModRenderSurface {
    Terminal,
    Desktop,
    Mobile,
    Vscode,
}

impl ModRenderSurface {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Terminal => "terminal",
            Self::Desktop => "desktop",
            Self::Mobile => "mobile",
            Self::Vscode => "vscode",
        }
    }
}

fn default_query_source() -> String {
    QUERY_SOURCE_REPL_MAIN_THREAD.to_string()
}

fn default_max_structured_output_retries() -> i64 {
    5
}

/// Claude Code 2.1.245 `E_`: collapse `agent:custom:<id>` to `agent:custom`.
#[must_use]
pub fn sanitize_query_source(raw: &str) -> &str {
    if raw.starts_with("agent:custom:") {
        "agent:custom"
    } else {
        raw
    }
}

/// Runtime configuration for [`crate::ConversationOrchestrator`].
//
// Independent feature/auth flags — claude-code carries these as separate
// booleans too (interactive vs print, the otk/token-budget gates, the resolved
// subscription flags); collapsing them into a state enum would obscure the 1:1
// mapping, so we suppress `struct_excessive_bools` (as `anthropic-oauth`'s
// resolver does for the same reason).
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchestratorConfig {
    /// Maximum number of turns before the loop aborts with
    /// [`crate::OrchestratorError::MaxTurnsReached`]. **`0` = UNBOUNDED** (the
    /// default, [`MAX_TURNS_DEFAULT`]) — the cap is enforced only when this is
    /// non-zero, mirroring claude-code's truthy `if (maxTurns && …)` check. Set
    /// a positive value (e.g. from `--max-turns N`) to impose a ceiling.
    pub max_turns: u32,
    /// Validation attempts within one StructuredOutput query, independent of
    /// the ordinary turn ceiling.
    #[serde(default = "default_max_structured_output_retries")]
    pub max_structured_output_retries: i64,
    /// This query explicitly requires the configured StructuredOutput capture
    /// tool. A registry entry with that name does not activate schema policy.
    #[serde(default)]
    pub structured_output_enabled: bool,

    /// Active model identifier (passed verbatim to
    /// `AnthropicProvider::messages_create_non_stream`).
    pub model: String,

    /// Opus-fallback model (claude-code `--fallback-model`). When `Some(id)`,
    /// the batched turn loop routes its primary API call through the
    /// fallback-aware path so a consecutive-529 gate on a non-custom Opus
    /// primary model can surface `llm_runtime::LlmError::Overloaded` with
    /// fallback triggered; the turn loop then switches the session model to
    /// `id`, warns the user, and re-issues against it
    /// (1:1 with claude-code `query.ts:894-948`).
    ///
    /// `None` (the parity default) is a STRICT no-op: the primary call keeps
    /// using the plain `messages_create` seam (fallback disabled), so the locked
    /// turn-loop fixtures are byte-unaffected. claude-code restricts
    /// `--fallback-model` to `--print`/non-interactive mode; the CLI mirrors that
    /// guard (M5-12 `argv.rs`).
    #[serde(default)]
    pub fallback_model: Option<String>,

    /// Optional system prompt override. `None` means the default
    /// claude-code-equivalent system prompt is assembled (M5-03 wires
    /// the dynamic assembly; M5-02 leaves this `None` and the API call
    /// sends NO system prompt — the model receives only `messages`).
    pub system_prompt_override: Option<String>,

    /// Normal queries announce typed context attachments. Native bare/inline
    /// callers retain the additional-context prefix by selecting `Inline`.
    #[serde(default)]
    pub context_rendering: lingxi_core::host::instructions::InstructionRendering,

    /// Explicit native bare composition: omit ambient context and attachment
    /// producers while retaining task completion delivery through its owner.
    #[serde(default)]
    pub bare: bool,

    /// CLI `--exclude-dynamic-system-prompt-sections`. When `true`, the
    /// per-machine `env_block` (cwd / env info / git status / OS / shell) is
    /// OMITTED from the assembled system prompt and instead emitted in the
    /// first-user-message context reminder (`additional_context_message`), so
    /// the static system prompt is identical across machines/users and stays
    /// prompt-cacheable. `false` (the default) keeps the env block in the
    /// system prompt — byte-identical to before this field existed (only
    /// applies with the default assembled prompt; ignored under
    /// `system_prompt_override`).
    #[serde(default)]
    pub exclude_dynamic_system_prompt_sections: bool,

    /// When `true`, M5-12 CLI binary wires
    /// [`permission::InteractivePromptingGate`] over real stdin /
    /// stderr; when `false` (default), it wires
    /// [`crate::test_support::NoOpPermissionGate`]. The orchestrator
    /// itself doesn't read this flag — the `perms: Arc<dyn PermissionGate>`
    /// constructor argument decides; this field is the CLI's source of
    /// truth for which gate to construct. (M5-05)
    #[serde(default)]
    pub interactive_permissions: bool,

    /// Whether the main session should follow interactive CLI semantics for
    /// prompt assembly, session flags, and request metadata. This is distinct
    /// from [`Self::interactive_permissions`]: a host can surface permission
    /// prompts remotely while still needing headless/SDK session semantics.
    #[serde(default)]
    pub interactive_session: bool,

    #[serde(default)]
    pub mod_render_surface: Option<ModRenderSurface>,

    /// Trusted Projects inputs supplied by the session owner. These facts
    /// include source provenance and must never be accepted from config JSON.
    /// Missing facts remain unknown until the composition root resolves them.
    #[serde(skip)]
    pub mod_projects_consent: hooks::mods::ProjectsConsentFacts,

    /// Claude Code 2.1.245 `querySource` for the main query generator.
    ///
    /// CLI TUI / stdio REPL / `--print` → [`QUERY_SOURCE_REPL_MAIN_THREAD`].
    /// SDK / bridge / transport hosts → [`QUERY_SOURCE_SDK`]. The previous
    /// port hardcoded `"user"`, which is not in the 2.1.245 allowlist
    /// (`repl_main_thread*` + `sdk` + agent/compact/hook auxiliaries).
    #[serde(default = "default_query_source")]
    pub query_source: String,

    /// Native query fork origin. `btw` can emit local fallback notices even
    /// when its query source is not `side_question`.
    #[serde(default)]
    pub fork_origin: Option<String>,

    /// Claude Code 2.1.245 `print` (`-p` / `--print`). Distinct from
    /// [`Self::interactive_session`]: an SDK session is non-interactive but
    /// is not print mode.
    #[serde(default)]
    pub print: bool,

    /// Claude Code 2.1.245 `process.stdout.isTTY??!1` (`tengu_api_success.isTTY`).
    /// Independent of [`Self::print`]: `-p` in a terminal is `print=true` and
    /// `is_tty=true`. Default `false` for SDK / bridge / mobile / tests.
    #[serde(default)]
    pub is_tty: bool,

    /// If `Some(id)`, the orchestrator was started via `--resume <id>` or
    /// `/resume <id>` (M5-08) and must replay messages from the on-disk
    /// JSONL before running the first turn. The same `id` is re-used for
    /// new appends so the chain continues uninterrupted. `None` = fresh
    /// session.
    #[serde(default)]
    pub resume_session_id: Option<uuid::Uuid>,

    /// A1: gate for the 8k→64k output-token escalation retry (TS gate
    /// `tengu_otk_slot_v1`, 3P default `false` — not validated on
    /// Bedrock/Vertex). When `false` (the parity default), the loop performs
    /// ONLY the multi-turn "resume directly" nudge recovery on a `max_tokens`
    /// `stop_reason`; the single-shot escalation to
    /// [`crate::turn_loop::ESCALATED_MAX_TOKENS`] is skipped.
    ///
    /// The override is consumed by the next request through
    /// `messages_create`; a separate recovery latch guarantees a
    /// single 64k retry per max-token episode before normal continuation
    /// nudges resume.
    #[serde(default)]
    pub escalate_max_output_tokens: bool,

    /// A3: feature gate for token-budget auto-continuation (TS `feature('TOKEN_BUDGET')`).
    /// Defaults `false` for parity — TS gates the whole `+500k` continuation
    /// block. When `false` (or [`Self::token_budget`] is `None`), the turn
    /// loop's end-of-turn budget check is a NO-OP and the loop stops at the
    /// first `end_turn`, exactly as before. Set BOTH this flag `true` AND
    /// [`Self::token_budget`] to `Some(n)` to enable the continuation behaviour.
    #[serde(default)]
    pub enable_token_budget: bool,

    /// A3: the active per-turn output-token budget (e.g. `Some(500_000)` for
    /// `"+500k"`). `None` (the parity default) disables continuation. Callers
    /// may populate this from
    /// [`crate::token_budget::parse_token_budget`] applied to the user prompt,
    /// or set it explicitly. Only honoured when [`Self::enable_token_budget`]
    /// is also `true`.
    #[serde(default)]
    pub token_budget: Option<u64>,

    /// Construction-time OAuth subscription snapshot used by the session's
    /// model UI. Provider retry gates read the service's live subscription slot
    /// independently of this value.
    #[serde(default)]
    pub is_subscriber: bool,

    /// Construction-time enterprise-tier snapshot retained in session config.
    /// Provider execution resolves the live tier from its shared subscription
    /// slot, rather than accepting subscription flags on a model request.
    #[serde(default)]
    pub is_enterprise: bool,
    /// Whether this route may carry claude-code's FIRST-PARTY beta headers.
    ///
    /// Gates the context-hint negotiation
    /// (`compaction::context_hint::create_context_hint_controller`), whose
    /// oracle returns no controller at all without it. Custom Anthropic-wire
    /// gateways understand the stable Messages schema but must never inherit a
    /// private first-party beta, which is the same rule
    /// `service::direct_anthropic_api_route` enforces one layer down.
    ///
    /// `false` by default, so the negotiation is inert unless a composition
    /// root opts in — on top of the controller's own env gate, which is also
    /// off by default because the ORACLE's `tengu_hazel_osprey` is.
    #[serde(default)]
    pub include_first_party_betas: bool,
    /// Which upstream this session talks to, for the error copy that names
    /// where to look when trouble persists (oracle `xn()` feeding `hpo()`).
    ///
    /// `None` (the default) renders the capacity clause without its suffix —
    /// see `api_error_copy::capacity_fallback`. Supplied by a composition root,
    /// which is the only layer that knows the resolved provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_route: Option<crate::api_error_copy::ErrorRouteTag>,
    /// Whether the Anthropic credential came from OUTSIDE the app — the
    /// `ANTHROPIC_API_KEY` env var or an `apiKeyHelper` script.
    ///
    /// Selects between `Invalid API key · Fix external API key` and
    /// `Not logged in · Please run /login` when the server rejects the
    /// credential (oracle: `e1().source === "ANTHROPIC_API_KEY" ||
    /// === "apiKeyHelper"`). `false` — the default — yields the /login copy,
    /// which is the right advice for a stored or OAuth credential.
    ///
    /// Set from `llm_runtime::auth::anthropic::AuthSource` at the composition
    /// root. Started life as a `bool`; widened because the org-disabled copy
    /// tells an env-var user and an `apiKeyHelper` user to unset DIFFERENT
    /// things, which a single flag cannot express.
    #[serde(default)]
    pub credential_origin: crate::api_error_copy::CredentialOrigin,
    /// Whether a claude.ai OAuth access token is present — oracle `zv()`
    /// (`ms()?.accessToken != null`). With an account already signed in,
    /// unsetting `ANTHROPIC_API_KEY` suffices; without one the user must also
    /// run `/login`.
    #[serde(default)]
    pub has_oauth_token: bool,

    /// OUTSTYLE.2: the active output-style name from `settings.outputStyle`
    /// (TS types it `z.string()`). `None` / `"default"` → no style section;
    /// a builtin name (`Explanatory` / `Learning`) injects its
    /// `# Output Style: <name>` section into the assembled system prompt via
    /// [`outputstyles::resolve_builtin_output_style`]. Populated at the
    /// composition root from merged settings.
    #[serde(default)]
    pub output_style: Option<String>,

    /// OUTSTYLE.3: directories searched for CUSTOM output styles (`*.md` files
    /// with `name`/`description`/`keepCodingInstructions` frontmatter + a body),
    /// in INCREASING priority — e.g.
    /// `[~/.lingxi/output-styles, <cwd>/.lingxi/output-styles]` so a project
    /// style overrides a user one, and both override the builtins
    /// ([`outputstyles::resolve_output_style`]). EMPTY (the default) means
    /// builtin-only resolution — byte-identical to before, so non-desktop hosts
    /// and tests are unaffected; the desktop composition root populates it.
    #[serde(default)]
    pub output_style_dirs: Vec<std::path::PathBuf>,

    /// Optional cost ceiling in NANO-USD (claude-code `maxBudgetUsd`, in USD; the
    /// nano-USD unit matches [`cost::CostTracker`]). `None` (the default) = no
    /// cap. When set, the turn loop stops with
    /// [`crate::OrchestratorError::MaxBudgetReached`] once the session's
    /// cumulative cost reaches it — 1:1 with `QueryEngine.ts:972`
    /// `getTotalCost() >= maxBudgetUsd`. Enforced only when a `CostTracker` is
    /// wired (the cap cannot be enforced without cost tracking); a headless
    /// `--max-budget` run is the primary consumer.
    #[serde(default)]
    pub max_budget_nano_usd: Option<u64>,

    /// Dormant gate for the `TRANSCRIPT_CLASSIFIER` feature (claude-code
    /// `feature('TRANSCRIPT_CLASSIFIER')`, an ant-internal GrowthBook/Statsig
    /// flag that is OFF in every external build). It guards the auto-mode
    /// classifier's `PermissionDenied`-hook retry path: only when this is `true`
    /// AND the deny came from a [`lingxi_core::host::permission_gate::PermissionDecisionSource::Classifier`]
    /// source does the turn loop honour a `PermissionDenied` hook's
    /// `{retry: true}` by pushing the verbatim `isMeta` retry message
    /// (`toolExecution.ts:1075-1101`).
    ///
    /// `false` (the parity default) is byte-identical to claude-code's external
    /// build: there is no LLM classifier
    /// ([`permission::classifier::is_classifier_permissions_enabled`] is also
    /// hardcoded `false`), so the retry message NEVER fires on the normal deny
    /// path. Exposed as a config bit (rather than the const) ONLY so the
    /// plumbing is unit-testable with the gate forced on — production code never
    /// sets it true.
    #[serde(default)]
    pub transcript_classifier_enabled: bool,

    /// Local refusal retry target. The configured cascade owns local hops;
    /// `server_refusal_fallback` supplies the separate native server-lane facts.
    #[serde(default)]
    pub refusal_fallback_model: Option<String>,
    /// Native model/feature facts resolved by the host. Server lane admission
    /// runs for each query; provider input cannot grant this authority.
    #[serde(default)]
    pub server_refusal_fallback: Option<lingxi_core::host::refusal_server::Policy>,

    /// Trusted policy snapshot used to validate a model returned by an
    /// admitted first-party server-fallback lane. This is resolved by the host
    /// from managed `policySettings`; it is deliberately absent from the
    /// serialized orchestrator config and must never be populated from the
    /// provider model catalog.
    #[serde(skip)]
    pub server_fallback_model_enforcement: Option<llm_runtime::model::allowlist::ModelEnforcement>,
    /// Regular settings `availableModels`, used only when managed enforcement
    /// is inactive. `None` means the host has no regular allowlist restriction.
    #[serde(skip)]
    pub server_fallback_regular_available_models: Option<Vec<String>>,
    /// Regular settings `modelOverrides`, for reverse-matching provider model
    /// IDs against their canonical allowlist IDs.
    #[serde(skip)]
    pub server_fallback_regular_model_overrides: std::collections::BTreeMap<String, String>,
    /// Concrete, credential-resolved default model used by the native exact
    /// default-model exception when a regular allowlist is active.
    #[serde(skip)]
    pub server_fallback_default_model: Option<String>,

    /// The refusal-fallback CASCADE: an ordered chain of models to try, in
    /// order, as each one refuses.
    ///
    /// Claude walks a chain rather than a single model, skipping stages that
    /// cannot be resolved or were already tried this episode
    /// (`crate::refusal_cascade`). This port's historical shape is the single
    /// [`Self::refusal_fallback_model`], which is exactly a one-element chain —
    /// so an EMPTY chain here means "use that field", and the two never
    /// disagree.
    ///
    /// Empty (the default) is a strict no-op: behaviour is byte-identical to
    /// before this field existed.
    #[serde(default)]
    pub refusal_fallback_chain: Vec<String>,

    /// R-P1d: the authenticated user's email, surfaced in the leading
    /// `additionalContext` meta message as
    /// `# userEmail\nThe user's email address is {email}.` — 1:1 with
    /// claude-code's `userContext.userEmail` (`pS`, sourced from
    /// `Pc()?.emailAddress`). Only emitted when `Some(_)` and non-empty,
    /// mirroring the `...email&&{userEmail:…}` spread.
    ///
    /// `None` (the parity default) omits the `# userEmail` entry entirely, so
    /// the additional-context message carries only `# instructions` (when present)
    /// and `# currentDate`. Populated at the composition root from the resolved
    /// OAuth/account profile.
    #[serde(default)]
    pub user_email: Option<String>,

    /// CLI `--plan-mode-instructions` (206 `options.planModeInstructions`): custom
    /// plan-mode workflow body. Honored only in `--print` mode (print-gated in
    /// init.rs). `None` = the default 5-phase workflow.
    #[serde(default)]
    pub plan_mode_instructions: Option<String>,

    /// `settings.json` `plansDirectory` (206 `iT`): custom directory for
    /// plan-mode plan files, relative to the project root. Threaded here from the
    /// merged settings at the composition root. When `Some(_)`,
    /// [`crate::ConversationOrchestrator::plans_dir`] resolves it against the
    /// session's project root with a within-root containment check (falling back
    /// to the default `<project-root>/.lingxi/plans/` on rejection). `None` (the
    /// default) keeps the default plans directory.
    #[serde(default)]
    pub plans_directory: Option<String>,
    /// The session's plan-file identity, shared with the permission policy that
    /// grants plan mode's write carve-out and with `ExitPlanMode`'s read-back.
    ///
    /// `None` (tests, and hosts that never wired one) falls back to deriving the
    /// path from the session id, which is what the port did before 2.1.266's
    /// random slugs. Set it and this is the ONLY derivation: the path the model
    /// is told to write, the path the gate allows and the path the tool reads
    /// are the same string by construction.
    ///
    /// Runtime-only: never serialized with the rest of the config.
    #[serde(skip)]
    pub plan_files: Option<std::sync::Arc<lingxi_core::host::plan_files::PlanFileMatcher>>,

    /// (2.1.212) The session's resolved reasoning-effort LEVEL string
    /// (`low`/`medium`/`high`/`xhigh`/`max`), sourced from CLI `--effort`
    /// (already normalized by the CLI). When `Some(_)`, every REAL assistant
    /// transcript line records it as a top-level `effort` field — 1:1 with
    /// claude-code 2.1.212, which spreads `...effort!==void 0&&{effort}` (the
    /// `Y4n(effort).level`) onto the in-memory assistant message object that is
    /// persisted verbatim into the on-disk record.
    ///
    /// `None` (the parity default) omits the `effort` field on every line,
    /// matching claude's `!==void 0` guard — so sessions without an explicit
    /// effort keep byte-identical transcripts.
    #[serde(default)]
    pub effort: Option<String>,
    /// Explicit initial Ultracode switch, independent of the effort level.
    #[serde(default)]
    pub ultracode: bool,

    /// Remote feature-flag cadence for Ultracode maintenance reminders.
    /// The environment override is resolved inside `tool-workflow` and wins.
    #[serde(default)]
    pub ultracode_feature_flag_cadence: Option<u32>,

    /// Product-default Ultracode cadence. `None` falls back to the oracle's
    /// hardcoded ten non-meta turns.
    #[serde(default)]
    pub ultracode_product_default_cadence: Option<u32>,

    /// `settings.workflowKeywordTriggerEnabled`.
    #[serde(default)]
    pub workflow_keyword_trigger_enabled: bool,

    /// (gap218 #43) Whether an IN-PLACE resume (`resume_session`, the bridge /
    /// desktop hot-resume surface) may adopt the resumed agent's frontmatter
    /// `model` — the hot-path twin of claude-code `NQe`'s
    /// `if(!bC()&&i.model&&i.model!=="inherit")` gate, where `!bC()` is
    /// `!userSpecifiedModel`. The composition root owns `--model`, so it sets
    /// this to `!default_model_explicit`; the orchestrator then resolves the
    /// agent's `AgentModel` (alias → wire id) and applies it to the session on
    /// resume, exactly as the COLD-resume path already does at the root
    /// (engine-desktop `lib.rs`).
    ///
    /// `false` (the parity default) is byte-identical to before this field
    /// existed: the hot resume passes `model_override: None` and never overrides
    /// the session model — so a root that does not opt in (or a user who DID pass
    /// `--model`, where the root sets this `false`) is unchanged. Only set `true`
    /// when the user did NOT specify a model, so an explicit `--model` is never
    /// overridden by agent frontmatter.
    #[serde(default)]
    pub apply_resumed_agent_model: bool,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            max_turns: MAX_TURNS_DEFAULT,
            max_structured_output_retries: default_max_structured_output_retries(),
            structured_output_enabled: false,
            model: DEFAULT_MODEL.to_string(),
            fallback_model: None,
            system_prompt_override: None,
            context_rendering: lingxi_core::host::instructions::InstructionRendering::Announced,
            bare: false,
            exclude_dynamic_system_prompt_sections: false,
            interactive_permissions: false,
            interactive_session: false,
            mod_render_surface: None,
            mod_projects_consent: hooks::mods::ProjectsConsentFacts::default(),
            query_source: default_query_source(),
            fork_origin: None,
            print: false,
            is_tty: false,
            resume_session_id: None,
            escalate_max_output_tokens: false,
            enable_token_budget: false,
            token_budget: None,
            is_subscriber: false,
            is_enterprise: false,
            include_first_party_betas: false,
            error_route: None,
            credential_origin: crate::api_error_copy::CredentialOrigin::Other,
            has_oauth_token: false,
            output_style: None,
            output_style_dirs: Vec::new(),
            max_budget_nano_usd: None,
            transcript_classifier_enabled: false,
            refusal_fallback_model: None,
            server_refusal_fallback: None,
            server_fallback_model_enforcement: None,
            server_fallback_regular_available_models: None,
            server_fallback_regular_model_overrides: std::collections::BTreeMap::new(),
            server_fallback_default_model: None,
            refusal_fallback_chain: Vec::new(),
            user_email: None,
            plan_mode_instructions: None,
            plans_directory: None,
            plan_files: None,
            effort: None,
            ultracode: false,
            ultracode_feature_flag_cadence: None,
            ultracode_product_default_cadence: None,
            workflow_keyword_trigger_enabled: false,
            apply_resumed_agent_model: false,
        }
    }
}

impl OrchestratorConfig {
    /// The refusal-fallback chain to walk, in order.
    ///
    /// An empty [`Self::refusal_fallback_chain`] falls back to the historical
    /// single [`Self::refusal_fallback_model`], which is exactly a one-element
    /// chain — so the default path is byte-identical to before the cascade
    /// existed. Both the main-thread loop and the subagent spawner read the
    /// chain through here, so the two cannot drift apart on that rule.
    #[must_use]
    pub fn refusal_chain(&self) -> Vec<String> {
        if self.refusal_fallback_chain.is_empty() {
            self.refusal_fallback_model.clone().into_iter().collect()
        } else {
            self.refusal_fallback_chain.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_max_turns_is_unbounded() {
        // Parity: claude-code's `maxTurns` is unset by default (unbounded);
        // `0` is the LingXi sentinel for "no cap".
        assert_eq!(OrchestratorConfig::default().max_turns, 0);
        assert_eq!(MAX_TURNS_DEFAULT, 0);
    }

    #[test]
    fn default_model_is_locked_string() {
        assert_eq!(OrchestratorConfig::default().model, "claude-opus-4-8");
    }

    #[test]
    fn default_system_prompt_override_is_none() {
        assert!(
            OrchestratorConfig::default()
                .system_prompt_override
                .is_none()
        );
    }

    #[test]
    fn projects_facts_are_host_owned_and_cannot_be_injected_through_config_json() {
        let mut cfg = OrchestratorConfig::default();
        cfg.mod_projects_consent.default_host_sticky_latch = Some(true);
        cfg.mod_projects_consent.projects_env = Some(true);
        let mut encoded = serde_json::to_value(&cfg).unwrap();
        assert!(encoded.get("mod_projects_consent").is_none());
        encoded["mod_projects_consent"] = serde_json::json!({
            "default_host_sticky_latch": true,
            "projects_env": true,
            "session_mcp_signal": true,
            "feature_result": {"value": true, "source": "Payload"},
            "growthbook_used_non_default_host": false
        });
        let decoded: OrchestratorConfig = serde_json::from_value(encoded).unwrap();
        let facts = decoded.mod_projects_consent;
        assert!(facts.default_host_sticky_latch.is_none());
        assert!(facts.projects_env.is_none());
        assert!(facts.session_mcp_signal.is_none());
        assert!(facts.feature_result.is_none());
        assert!(facts.growthbook_used_non_default_host.is_none());
    }

    #[test]
    fn config_round_trips_through_json() {
        let cfg = OrchestratorConfig {
            max_turns: 5,
            model: "x".into(),
            fallback_model: Some("claude-sonnet-4-6".into()),
            system_prompt_override: Some("custom".into()),
            context_rendering: lingxi_core::host::instructions::InstructionRendering::Announced,
            bare: false,
            exclude_dynamic_system_prompt_sections: false,
            interactive_permissions: true,
            interactive_session: true,
            mod_render_surface: Some(ModRenderSurface::Desktop),
            mod_projects_consent: hooks::mods::ProjectsConsentFacts::default(),
            query_source: QUERY_SOURCE_SDK.to_string(),
            fork_origin: None,
            print: true,
            is_tty: true,
            resume_session_id: None,
            escalate_max_output_tokens: true,
            enable_token_budget: true,
            token_budget: Some(500_000),
            is_subscriber: true,
            is_enterprise: true,
            include_first_party_betas: false,
            error_route: None,
            credential_origin: crate::api_error_copy::CredentialOrigin::Other,
            has_oauth_token: false,
            output_style: Some("Explanatory".into()),
            output_style_dirs: vec![std::path::PathBuf::from("/home/u/.lingxi/output-styles")],
            max_budget_nano_usd: Some(5_000_000_000),
            transcript_classifier_enabled: true,
            refusal_fallback_model: Some("claude-sonnet-4-6".into()),
            server_refusal_fallback: None,
            server_fallback_model_enforcement: None,
            server_fallback_regular_available_models: None,
            server_fallback_regular_model_overrides: std::collections::BTreeMap::new(),
            server_fallback_default_model: None,
            refusal_fallback_chain: Vec::new(),
            user_email: Some("u@example.com".into()),
            plan_mode_instructions: Some("MY BODY".into()),
            plans_directory: Some("docs/plans".into()),
            plan_files: None,
            effort: Some("high".into()),
            ultracode: true,
            ultracode_feature_flag_cadence: Some(12),
            ultracode_product_default_cadence: Some(10),
            workflow_keyword_trigger_enabled: true,
            apply_resumed_agent_model: true,
        };
        let s = serde_json::to_string(&cfg).unwrap();
        let back: OrchestratorConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(back.max_turns, 5);
        assert_eq!(back.model, "x");
        assert_eq!(back.fallback_model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(back.system_prompt_override.as_deref(), Some("custom"));
        assert!(back.interactive_permissions);
        assert!(back.interactive_session);
        assert_eq!(back.query_source, QUERY_SOURCE_SDK);
        assert!(back.print);
        assert!(back.is_tty);
        assert!(back.resume_session_id.is_none());
        assert!(back.escalate_max_output_tokens);
        assert!(back.enable_token_budget);
        assert_eq!(back.token_budget, Some(500_000));
        assert!(back.is_subscriber);
        assert!(back.is_enterprise);
        assert_eq!(back.output_style.as_deref(), Some("Explanatory"));
        assert_eq!(
            back.output_style_dirs,
            vec![std::path::PathBuf::from("/home/u/.lingxi/output-styles")]
        );
        assert_eq!(back.max_budget_nano_usd, Some(5_000_000_000));
        assert!(back.transcript_classifier_enabled);
        assert_eq!(
            back.refusal_fallback_model.as_deref(),
            Some("claude-sonnet-4-6")
        );
        assert_eq!(back.user_email.as_deref(), Some("u@example.com"));
        assert_eq!(back.plan_mode_instructions.as_deref(), Some("MY BODY"));
        assert_eq!(back.plans_directory.as_deref(), Some("docs/plans"));
        assert!(back.ultracode);
        assert_eq!(back.ultracode_feature_flag_cadence, Some(12));
        assert_eq!(back.ultracode_product_default_cadence, Some(10));
        assert!(back.workflow_keyword_trigger_enabled);
        assert!(back.apply_resumed_agent_model);
    }

    #[test]
    fn default_apply_resumed_agent_model_is_false() {
        assert!(!OrchestratorConfig::default().apply_resumed_agent_model);
    }

    #[test]
    fn default_plan_mode_instructions_is_none() {
        assert!(
            OrchestratorConfig::default()
                .plan_mode_instructions
                .is_none()
        );
    }

    #[test]
    fn default_fallback_model_is_none() {
        assert!(OrchestratorConfig::default().fallback_model.is_none());
    }

    #[test]
    fn default_refusal_fallback_model_is_none() {
        // Finding #80: the parity default is a strict no-op — a `refusal`
        // response keeps today's terminal/Continue behavior.
        assert!(
            OrchestratorConfig::default()
                .refusal_fallback_model
                .is_none()
        );
    }

    #[test]
    fn default_subscription_flags_are_false() {
        // The parity default: no subscription resolved → byte-identical to the
        // pre-wiring `is_subscriber = false` / `is_enterprise = false` stub.
        let cfg = OrchestratorConfig::default();
        assert!(!cfg.is_subscriber);
        assert!(!cfg.is_enterprise);
    }

    #[test]
    fn subscription_flags_default_when_absent_from_json() {
        // `#[serde(default)]` — a pre-subscription persisted config must
        // deserialize with both flags `false`.
        let s = r#"{"max_turns":7,"model":"m"}"#;
        let back: OrchestratorConfig = serde_json::from_str(s).unwrap();
        assert!(!back.is_subscriber);
        assert!(!back.is_enterprise);
    }

    #[test]
    fn fallback_model_defaults_when_absent_from_json() {
        // `#[serde(default)]` — a config JSON without `fallback_model` must
        // deserialize with `None` (back-compat with pre-fallback persisted cfgs).
        let s = r#"{"max_turns":7,"model":"m"}"#;
        let back: OrchestratorConfig = serde_json::from_str(s).unwrap();
        assert!(back.fallback_model.is_none());
        assert_eq!(back.model, "m");
    }

    #[test]
    fn default_token_budget_disabled() {
        let cfg = OrchestratorConfig::default();
        assert!(!cfg.enable_token_budget);
        assert!(cfg.token_budget.is_none());
    }

    #[test]
    fn default_escalate_max_output_tokens_is_false() {
        assert!(!OrchestratorConfig::default().escalate_max_output_tokens);
    }

    #[test]
    fn default_interactive_permissions_is_false() {
        assert!(!OrchestratorConfig::default().interactive_permissions);
    }

    #[test]
    fn default_interactive_session_is_false() {
        assert!(!OrchestratorConfig::default().interactive_session);
    }

    #[test]
    fn default_query_source_is_repl_main_thread_and_print_is_false() {
        let cfg = OrchestratorConfig::default();
        assert_eq!(cfg.query_source, QUERY_SOURCE_REPL_MAIN_THREAD);
        assert!(!cfg.print);
        assert!(!cfg.is_tty);
        assert_eq!(
            sanitize_query_source("agent:custom:reviewer"),
            "agent:custom"
        );
        assert_eq!(
            sanitize_query_source("repl_main_thread"),
            "repl_main_thread"
        );
        assert_eq!(sanitize_query_source("sdk"), "sdk");
        assert_eq!(sanitize_query_source(""), "");
    }

    #[test]
    fn query_source_and_print_default_when_absent_from_json() {
        let s = r#"{"max_turns":7,"model":"m"}"#;
        let back: OrchestratorConfig = serde_json::from_str(s).unwrap();
        assert_eq!(back.query_source, QUERY_SOURCE_REPL_MAIN_THREAD);
        assert!(!back.print);
        assert!(!back.is_tty);
    }

    #[test]
    fn default_resume_session_id_is_none() {
        assert!(OrchestratorConfig::default().resume_session_id.is_none());
    }

    #[test]
    fn resume_session_id_round_trips_through_json() {
        let sid = uuid::Uuid::from_bytes([7u8; 16]);
        let cfg = OrchestratorConfig {
            resume_session_id: Some(sid),
            ..OrchestratorConfig::default()
        };
        let s = serde_json::to_string(&cfg).unwrap();
        let back: OrchestratorConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(back.resume_session_id, Some(sid));
    }
}
