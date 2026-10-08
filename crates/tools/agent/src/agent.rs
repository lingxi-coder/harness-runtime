//! `AgentTool` — spawns a subagent via the M1 agent pool.
//!
//! Spec §4 Flow D + §7 line 491-499. claude-code source:
//! `claude-code/src/tools/AgentTool/AgentTool.tsx` + `built-in/*.ts`.
//!
//! **Architectural note (M4-05):** the existing M1 dep graph has
//! `lingxi-tasks` and `lingxi-agent` already depending on `lingxi-tools`,
//! which prevents this crate from depending on either. Subagent spawn and
//! `BudgetEnforcer` inheritance therefore land here as a tool *surface*
//! (locked schemas + locked error strings + locked telemetry events); the
//! actual wiring through `StateMachinePool::allocate` + parent
//! `Arc<BudgetEnforcer>` happens in `lingxi-coordinator` post-M5 when the
//! cycle issue is resolved by extracting `ToolRegistry` into a leaf crate.
//! All byte-locks (tool name, 6 subagent types, M3-05 denial format,
//! 3 telemetry events) are preserved.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use branding::{
    MAX_CONCURRENT_SUBAGENTS_ENV, MAX_SUBAGENTS_PER_SESSION_ENV, MAX_SUBAGENT_SPAWN_DEPTH_ENV,
};
use lingxi_core::host::budget::BudgetError;
use lingxi_core::host::fusion::{
    FusionActivation, FusionAgentSurface, FusionExecutor, FusionInheritance, FusionModelRef,
    FusionOrigin, FusionPanelMode, FusionPreparedSummary, FusionPreset, FusionProgress,
    FusionRequest, FusionRunControl, FusionRunFactsRecorder, FusionRunId, FusionRunIdentity,
    FusionRunRecorder, FusionRunRecorderFactory, FusionStage, FusionStatus, FusionSubmission,
    FusionTerminalCapability, PreparedFusionRun, FUSION_MAX_PANEL, FUSION_MIN_PANEL,
};
use lingxi_core::host::subagent_spawn::{
    AgentSpawnAdmission, AgentSpawnStart, StructuredOutputMode, SubagentInheritance,
    SubagentListingEntry, SubagentObservation, SubagentResult, SubagentSpawnObserver,
    SubagentSpawnRequest,
};
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use telemetry::pii::{PiiTagged, Verified};
use telemetry::sink::{AnalyticsValue, LogEventMetadata};
use telemetry::tengu::agent::{
    CACHE_EVICTION_HINT, SUBAGENT_OUTPUT_FLAGGED, TOOL_COMPLETED, TOOL_SELECTED, TOOL_TERMINATED,
};
use telemetry::tengu::tool::{AGENT_COMPLETED_M4_05, AGENT_FAILED, AGENT_STARTED};
use telemetry::AnalyticsBus;

use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, InterruptBehavior, PromptOptions, Tool, ToolCallResult, ToolError,
    ToolStaticContext,
};
use tool_api::BuiltinToolContext;

struct ModStartReceiptObserver(
    std::sync::Mutex<Option<tokio::sync::oneshot::Sender<foreground_task::StartReceipt>>>,
);

#[async_trait]
impl SubagentSpawnObserver for ModStartReceiptObserver {
    fn on_started(&self, event: &SubagentObservation) {
        if let SubagentObservation::Allocated {
            agent_id, model, ..
        } = event
        {
            if let Some(sender) = self.0.lock().unwrap().take() {
                let _ = sender.send((*agent_id, model.clone()));
            }
        }
    }

    async fn on_event(&self, _event: SubagentObservation) {}
}

/// Registry name of the subagent-spawning tool.
pub const AGENT_TOOL_NAME: &str = "Agent";

/// Four built-in subagent types — byte-aligned with upstream
/// `claude-code/src/tools/AgentTool/built-in/*.ts`.
///
/// ADVISORY ONLY. `AgentTool` no longer rejects a `subagent_type` outside this
/// list: the catalog-aware [`lingxi_core::host::subagent_spawn::SubagentSpawner`] resolves
/// any type (user/project catalog overrides built-ins; an unknown type →
/// `general-purpose`, matching claude-code's `effectiveType ?? GENERAL_PURPOSE`).
/// `tool-agent` cannot depend on the `agent` crate (cycle — see the module
/// header), so the canonical definitions live there; this literal is kept for
/// tests and documentation of the built-in set, not as a gate.
///
/// vs the 2.1.223 `rJe` roster: the phantom `verification` agent (stale
/// leaked-TS, 0-hit in every local oracle) was removed, and the Claude-branded
/// `claude-code-guide` + `claude` catch-all are deliberately excluded — the
/// multi-provider divergence (user-confirmed 2026-08-06).
pub const BUILTIN_SUBAGENT_TYPES: &[&str] =
    &["general-purpose", "Plan", "Explore", "statusline-setup"];

/// Prefix locked by M3-05 (`cost/src/budget.rs` budget-exceeded test fixtures).
/// Production constructs the full string via
/// `format!("Budget exceeded (${:.2}); stopped.", dollars)`.
pub const SUBAGENT_BUDGET_DENIED_PREFIX: &str = "Budget exceeded ($";

/// Built-in agents that run once and return a report — the parent never
/// `SendMessage`s back to continue them. claude-code `ONE_SHOT_BUILTIN_AGENT_TYPES`
/// (`constants.ts:9-12`). The model-facing `tool_result` for these skips the
/// `agentId`/SendMessage/`<usage>` trailer (`AgentTool.tsx:1356`), saving tokens.
pub const ONE_SHOT_BUILTIN_AGENT_TYPES: &[&str] = &["Explore", "Plan"];

/// claude `maxResultSizeChars` for the Agent tool (`AgentTool.tsx:229`): a flat
/// `100_000`, NOT the shared `MAX_TOOL_OUTPUT_LENGTH`.
const AGENT_MAX_RESULT_SIZE_CHARS: usize = 100_000;

/// `general-purpose` — byte-aligned with upstream `GENERAL_PURPOSE_AGENT.agentType`
/// (`built-in/generalPurposeAgent.ts:26`). The effective type when the caller
/// OMITS `subagent_type` (claude `subagent_type ?? GENERAL_PURPOSE_AGENT.agentType`,
/// AgentTool.tsx:322).
const GENERAL_PURPOSE_AGENT_TYPE: &str = "general-purpose";

/// Public Fusion agent type (not `fusion-panel`).
const FUSION_AGENT_TYPE: &str = "fusion";

const FUSION_WHEN_TO_USE: &str = "Parallel multi-model deliberation for complex code, task, plan, or review work. Returns the panels' anonymized answers plus an analysis of their consensus, contradictions, partial coverage, unique insights and blind spots — you then write the final answer yourself. About 4–5× the cost of a single agent.";

/// Prepended to the listing text in Fusion mode (`fusion.proactive`). The user
/// turned the mode on, so the cost trade-off is theirs; the guard against
/// trivial requests is what keeps the mode from spending on one-line answers.
const FUSION_PROACTIVE_WHEN_TO_USE: &str = "Fusion mode is ON: the user wants substantial work compared across models. Start with this agent, before answering or editing yourself, for any non-trivial coding, debugging, review, design or planning request. Skip it for simple questions, one-line edits, lookups and follow-ups that need no comparison, and never start a second run while one is pending.";

/// Appended to [`FUSION_WHEN_TO_USE`] only on hosts that can run implement
/// mode (`FusionAgentSurface::implement_available`).
const FUSION_IMPLEMENT_WHEN_TO_USE: &str = "Pass fusion_mode \"implement\" when the panels should each make the change themselves, in their own git worktree, with the host running the user's verification commands on every result; you then write the final change in the user's workspace, using the panels' patches as references. Several times the cost of analysis, so use it for changes worth comparing, and expect the user to be asked to confirm it unless they allow it automatically.";

/// 2.1.238 `Gri` (@285270080), NEW in 2.1.238 (0 hits in 2.1.220):
/// the message head used both by the Agent-tool prompt (as the tail of the
/// `subagent_type` sentence when the general-purpose agent is unavailable) and
/// by the spawn path when `subagent_type` is OMITTED and `p7f` says
/// general-purpose is not in the catalog.
const SUBAGENT_TYPE_REQUIRED_MESSAGE: &str =
    "subagent_type is required: the general-purpose agent is not available in this session";

/// The `prompt:` body shared by the binary's `c` (fork) and `f` (non-fork)
/// `Example usage:` blocks — identical bytes in both, hoisted so the two
/// renderings cannot drift.
const EXAMPLE_SHIP_AUDIT_PROMPT: &str = "Audit what's left before this branch can ship. Check: uncommitted changes, commits ahead of main, whether tests exist, whether the GrowthBook gate is wired up, whether CI-relevant files changed. Report a punch list — done vs. missing. Under 200 words.";

/// The `prompt:` body shared by the binary's `d` (code-reviewer) example and
/// the fork `c` block's third example.
const EXAMPLE_MIGRATION_REVIEW_PROMPT: &str = "Review migration 0042_user_schema.sql for safety. Context: we're adding a NOT NULL column to a 50M-row table. Existing rows get a backfill default. I want a second opinion on whether the backfill approach is safe under concurrent writes — I've checked locking behavior but want independent verification. Report: is this safe, and if not, what specifically breaks?";

/// 2.1.238 `p7f(agents, allowedAgentTypes)` (@292880236) — the
/// general-purpose-availability probe threaded into the Agent-tool prompt as
/// `generalPurposeAvailable` and consulted by the spawn path when
/// `subagent_type` is omitted:
///
/// ```js
/// function p7f(e,t){
///   let r=XQe(hRe.agentType), n=e.filter((s)=>XQe(s.agentType)===r),
///       o=(s)=>t?.includes(s.agentType)??!0, i=n.find((s)=>s.agentType===hRe.agentType);
///   if(i)return o(i);
///   return n.length===1&&o(n[0])
/// }
/// ```
///
/// `t` (`allowedAgentTypes`, the `att()` wildcard-rule allowlist) has no port
/// seam, so `o(...)` is the JS `?? !0` default — always true.
fn general_purpose_is_available(
    agents: &[lingxi_core::host::subagent_spawn::SubagentListingEntry],
) -> bool {
    let target = normalize_agent_type(GENERAL_PURPOSE_AGENT_TYPE);
    let matches = agents
        .iter()
        .filter(|a| normalize_agent_type(&a.agent_type) == target)
        .count();
    if agents
        .iter()
        .any(|a| a.agent_type == GENERAL_PURPOSE_AGENT_TYPE)
    {
        return true;
    }
    matches == 1
}

/// Input shape accepted by `AgentTool`.
///
/// Mirrors claude-code's `AgentTool` Zod schema (`AgentTool.tsx:82-101` +
/// type alias `:132-138`): `description` + `prompt` required; the rest
/// optional. The `.describe()` strings are lifted verbatim.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentToolInput {
    /// `description` — REQUIRED in TS (`z.string().describe('A short (3-5
    /// word) description of the task')`, AgentTool.tsx:83).
    pub description: String,
    /// `prompt` — REQUIRED (`z.string().describe('The task for the agent to
    /// perform')`, AgentTool.tsx:84).
    pub prompt: String,
    /// `subagent_type?` — optional (`z.string().optional()`, AgentTool.tsx:85).
    /// `None` (omitted) ⇒ the effective type is `general-purpose` (TS
    /// `subagent_type ?? GENERAL_PURPOSE_AGENT.agentType`, AgentTool.tsx:322).
    /// `Some(x)` (explicit) is validated against the agent listing in `call`: a
    /// known type spawns it, an unknown type is rejected with claude's
    /// "Agent type 'x' not found" error (AgentTool.tsx:345-354). The `Option`
    /// distinguishes omitted (→ general-purpose) from explicit (→ validated),
    /// exactly as claude's `?? ` does.
    #[serde(default)]
    pub subagent_type: Option<String>,
    /// Optional catalog model ID, configured alias, or profile-qualified model.
    #[serde(default)]
    pub model: Option<String>,
    /// Optional configured provider profile for the model override.
    #[serde(default)]
    pub model_profile: Option<String>,
    /// `run_in_background?` — optional (AgentTool.tsx:87). Carried; background
    /// dispatch is handled by the host runtime / coordinator.
    #[serde(default)]
    pub run_in_background: Option<bool>,
    /// `name?` — optional teammate name (AgentTool.tsx:94).
    #[serde(default)]
    pub name: Option<String>,
    /// `isolation?` — optional `'worktree' | 'remote'` (AgentTool.tsx:99).
    #[serde(default)]
    pub isolation: Option<String>,
    /// `cwd?` — optional absolute path to run the agent in (AgentTool.tsx:100).
    #[serde(default)]
    pub cwd: Option<String>,
    /// INTERNAL plumbing only — NOT part of the model-facing schema (claude-code
    /// has no `context_paths` field). Kept so internal callers/tests that seed
    /// context files keep working; defaults to empty so the model never sees it.
    #[serde(default)]
    pub context_paths: Vec<PathBuf>,
    /// Fusion preset: `"quality"` or `"fast"`. Ignored unless
    /// `subagent_type` is `"fusion"`.
    #[serde(default)]
    pub preset: Option<String>,
    /// Explicit Fusion panel models (`"profile:model"` or `"model"`).
    #[serde(default)]
    pub models: Option<Vec<String>>,
    /// Fusion scoring dimensions.
    #[serde(default)]
    pub dimensions: Option<Vec<String>>,
    /// Fusion panel cap override.
    #[serde(default)]
    pub max_panel: Option<u8>,
    /// Continue Fusion analysis when some panels fail.
    #[serde(default)]
    pub partial_ok: Option<bool>,
    /// Request cross-provider Fusion (Agent default is same-provider).
    #[serde(default)]
    pub cross_provider: Option<bool>,
    /// Fusion panel mode: `"analysis"` (default) or `"implement"`. Ignored
    /// unless `subagent_type` is `"fusion"`.
    #[serde(default)]
    pub fusion_mode: Option<String>,
}

/// claude `Agt()` (`AgentTool.tsx`): normalize a subagent-type candidate for the
/// fuzzy fallback match — lowercase, then strip all whitespace, dashes (Unicode
/// `Pd`), and underscores, so `"Explore"` / `"explore"` / `"ex-plore"` /
/// `"general_purpose"` collapse to a comparable key.
///
/// claude additionally applies `NFKC` first; agent-type names are ASCII (where
/// NFKC is the identity), so it is omitted here — add `unicode-normalization`
/// if non-ASCII custom agent types ever need compatibility folding.
fn normalize_agent_type(s: &str) -> String {
    s.chars()
        .flat_map(char::to_lowercase)
        .filter(|c| !(c.is_whitespace() || *c == '_' || is_pd_dash(*c)))
        .collect()
}

/// 2.1.238 `YLi` (@292880950): `function YLi(e){return e.join(", ")||"none"}`.
///
/// Every `Available agents: …` tail in 2.1.238 routes through this helper, so
/// an empty (or fully deny-filtered) catalog renders the word `none` instead of
/// an empty tail. 2.1.220 interpolated a bare `join(", ")` at each site.
fn render_available_agents(types: &[String]) -> String {
    let joined = types.join(", ");
    if joined.is_empty() {
        "none".to_string()
    } else {
        joined
    }
}

/// claude 2.1.238 `KH="web-fetch"` (@287971650) — the built-in web-fetch
/// agent's type label. Spelled locally so `tool-agent` need not depend on the
/// `agent` engine crate (cycle); kept in sync with
/// `agent::builtins::WEB_FETCH_AGENT_TYPE`.
const WEB_FETCH_AGENT_TYPE: &str = "web-fetch";

/// claude 2.1.238 `hdr(e)` (@290291941):
///
/// ```js
/// function hdr(e){return`Agent type '${e}' is unavailable because every tool it may use is denied by the current permission settings.`}
/// ```
///
/// Thrown from the spawn path alongside `de("subagent_launch",
/// "subagent_type_tools_denied")` (@292890415) when `mdr(agent, ctx)` says every
/// tool the agent may use is denied. 0 hits in 2.1.220 — new in 2.1.238.
fn agent_type_tools_denied_error(agent_type: &str) -> String {
    format!(
        "Agent type '{agent_type}' is unavailable because every tool it may use is denied by the current permission settings."
    )
}

/// claude `Agi()` (@287975693) `"coordinator"` arm, applied where coordinator
/// mode is actually live: `vyt()` never pushes `Hlr` in coordinator mode, so the
/// built-in `web-fetch` agent must not appear in the catalog `AgentTool`
/// advertises (or resolves against) for a coordinator session. The roster
/// function `agent::builtins::builtin_agent_definitions` cannot read coordinator
/// mode (it is a per-session `CoordinatorModeHandle`, not a process global), so
/// the arm is enforced here.
fn drop_coordinator_hidden_builtins(
    agents: &mut Vec<lingxi_core::host::subagent_spawn::SubagentListingEntry>,
    is_coordinator: bool,
) {
    if is_coordinator {
        agents.retain(|a| a.agent_type != WEB_FETCH_AGENT_TYPE);
    }
}

/// Decode a forwarded-subagent-message progress line (`--forward-subagent-text`,
/// 2.1.212). Returns the inner subagent message `Value` when `line` is a JSON
/// object carrying [`lingxi_core::host::subagent_spawn::FORWARD_SUBAGENT_MESSAGE_SENTINEL`]
/// (the pool spawner's sentinel wrapper); `None` for a plain nested-activity
/// line, which never parses as such an object.
fn decode_forward_subagent_message(line: &str) -> Option<Value> {
    let parsed: Value = serde_json::from_str(line).ok()?;
    parsed
        .get(lingxi_core::host::subagent_spawn::FORWARD_SUBAGENT_MESSAGE_SENTINEL)
        .cloned()
}

fn decode_forward_subagent_tombstone(line: &str) -> Option<(Value, bool)> {
    let parsed: Value = serde_json::from_str(line).ok()?;
    let payload = parsed.get(
        lingxi_core::host::subagent_spawn::FORWARD_SUBAGENT_SERVER_FALLBACK_TOMBSTONE_SENTINEL,
    )?;
    let message = payload.get("message")?.clone();
    let display_only = payload.get("display_only")?.as_bool()?;
    Some((message, display_only))
}

/// Unicode `Pd` (dash punctuation) membership test for [`normalize_agent_type`].
fn is_pd_dash(c: char) -> bool {
    matches!(
        c,
        '-' | '\u{058A}' | '\u{05BE}' | '\u{1400}' | '\u{1806}' | '\u{2010}'
            ..='\u{2015}'
                | '\u{2E17}'
                | '\u{2E1A}'
                | '\u{2E3A}'
                | '\u{2E3B}'
                | '\u{2E40}'
                | '\u{301C}'
                | '\u{3030}'
                | '\u{30A0}'
                | '\u{FE31}'
                | '\u{FE32}'
                | '\u{FE58}'
                | '\u{FE63}'
                | '\u{FF0D}'
    )
}

/// `uZc` (claude-code): the `name` regex body — first char a letter/digit, then
/// up to 63 more of letter / digit / underscore / hyphen (max 64 chars total).
/// Emitted verbatim as the wire `input_schema` `name.pattern` (zod-to-json-schema
/// `case"regex"` → `addPattern`) AND enforced at the tool boundary by
/// [`validate_agent_name`].
const AGENT_NAME_PATTERN: &str = "^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$";

/// `K9` (claude-code): the reserved agent name. `SendMessage` routes it to the
/// main conversation, so a spawned agent may not claim it.
const RESERVED_AGENT_NAME: &str = "main";

/// `.regex(uZc)` message — the byte-exact zod validation message for a name that
/// violates [`AGENT_NAME_PATTERN`].
const AGENT_NAME_REGEX_MESSAGE: &str = "name must start with a letter or digit and contain only letters, digits, underscores, or hyphens (max 64 chars)";

/// `.refine(t=>t!==K9)` message — byte-exact (em-dash is U+2014); the literal
/// `"main"` is [`RESERVED_AGENT_NAME`] interpolated as `${K9}`.
fn reserved_agent_name_message() -> String {
    format!(
        "\"{RESERVED_AGENT_NAME}\" is reserved \u{2014} SendMessage routes it to the main conversation"
    )
}

/// `uZc.test(name)` — hand-rolled `/^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$/` (no `regex`
/// crate dep): a non-empty name of at most 64 chars whose first char is ASCII
/// alnum and whose remainder is ASCII alnum / `_` / `-`. The char class is
/// ASCII-only, so any non-ASCII byte fails the class (and the match); operating
/// on bytes is therefore equivalent to JS's UTF-16 length counting.
fn matches_agent_name_pattern(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > 64 {
        return false;
    }
    if !bytes[0].is_ascii_alphanumeric() {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Port of the `name` zod chain `z.string().regex(uZc).refine(t=>t!==K9)` (the
/// binary's `exy` schema): validate a spawned-agent name at the tool boundary.
/// Returns the byte-exact zod message on failure — the `.regex` message first
/// (zod evaluates `.regex` before `.refine`), then the reserved-name message.
/// `Ok(())` when the name is well-formed and not reserved.
fn validate_agent_name(name: &str) -> Result<(), String> {
    if !matches_agent_name_pattern(name) {
        return Err(AGENT_NAME_REGEX_MESSAGE.to_string());
    }
    if name == RESERVED_AGENT_NAME {
        return Err(reserved_agent_name_message());
    }
    let normalized = name.to_ascii_lowercase();
    if normalized == "main" || normalized == "team-lead" || reserved_agent_id_shape(&normalized) {
        return Err("name must not be a reserved recipient (\"main\" or \"team-lead\", in any spelling) or have the shape of an agent id — those already address an agent directly".into());
    }
    Ok(())
}

/// Oracle Mb: `^a(?:[\\w-]{1,63}-)?[0-9a-f]{16}$`, after recipient normalization.
fn reserved_agent_id_shape(name: &str) -> bool {
    let Some(rest) = name.strip_prefix('a') else {
        return false;
    };
    let hex = match rest.rsplit_once('-') {
        Some((prefix, suffix))
            if !prefix.is_empty()
                && prefix.len() <= 63
                && prefix
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')) =>
        {
            suffix
        }
        Some(_) => return false,
        None => rest,
    };
    hex.len() == 16
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Current model-selection policy advertised to ordinary Agent calls.
/// Per-call choices precede definition/default choices; deployment policy may
/// replace the complete model/profile selection.
pub(crate) const AGENT_MODEL_PARAM_DESCRIPTION: &str = "Optional catalog model ID, configured alias, or profile-qualified model (\"profile/model\") for this agent. Takes precedence over the agent definition's model and the configured default subagent model. A configured deployment model override can replace this model and profile. If omitted, uses the agent definition's model, else the configured default or the parent model. Unqualified selections prefer the parent's provider profile unless model_profile is specified. Ignored for subagent_type: \"fork\" — forks always inherit the parent model and profile.";
const AGENT_MODEL_PROFILE_PARAM_DESCRIPTION: &str = "Optional configured provider profile for the model selection; requires model. Omit it to prefer the parent's profile for an unqualified model, or specify the profile in model as \"profile/model\". Ignored for subagent_type: \"fork\" — forks always inherit the parent model and profile.";

/// The coordinator-mode suffix `G4o()` appends to
/// [`AGENT_MODEL_PARAM_DESCRIPTION`] (`xi()?…:""`, @3573581 / @3573661).
///
/// Both arms begin with a leading space — the binary concatenates them onto the
/// base sentence with `+`, not through a separator.
const AGENT_MODEL_PARAM_COORDINATOR_FORCED_SUFFIX: &str =
    " Unavailable on this session: this parameter is ignored — do not set it.";
const AGENT_MODEL_PARAM_COORDINATOR_SUFFIX: &str = " Set this only when EXPLICITLY asked by the user for a specific model, never because the task seems small, simple, or cheap; otherwise omit it so the worker uses the default (the session model, unless a default subagent model is configured).";

/// Require coordinator workers to inherit the session model and profile.
#[must_use]
pub(crate) fn coordinator_forces_worker_inherit_model() -> bool {
    lingxi_core::host::env::is_env_truthy(
        std::env::var(branding::COORDINATOR_FORCE_WORKER_INHERIT_MODEL_ENV)
            .ok()
            .as_deref(),
    )
}

/// Disable per-call model and provider-profile selection for subagents.
#[must_use]
pub(crate) fn subagent_model_forced() -> bool {
    lingxi_core::host::env::is_env_truthy(
        std::env::var(branding::SUBAGENT_MODEL_FORCE_ENV)
            .ok()
            .as_deref(),
    )
}

static AGENT_INPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "properties": {
            "description": {
                "type": "string",
                "description": "A short (3-5 word) description of the task"
            },
            "prompt": {
                "type": "string",
                "description": "The task for the agent to perform"
            },
            "subagent_type": {
                "type": "string",
                "description": "The type of specialized agent to use for this task"
            },
            "model": {
                "type": "string",
                "description": AGENT_MODEL_PARAM_DESCRIPTION
            },
            "model_profile": {
                "type": "string",
                "description": AGENT_MODEL_PROFILE_PARAM_DESCRIPTION
            },
            "run_in_background": {
                "type": "boolean",
                "description": "Agents run in the background by default; you will be notified when one completes. Set to false only when your very next action depends on this agent's result and nothing else could usefully happen while it runs — otherwise leave it in the background so the user can hand you other work."
            },
            "name": {
                "type": "string",
                // `z.string().regex(uZc)` → zod-to-json-schema `addPattern`
                // (`case"regex"`), so the wire input_schema carries the regex body
                // verbatim (`uZc=/^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$/`). The
                // `.refine(t => t !== "main")` reserved-name check has NO JSON
                // Schema equivalent and is enforced at the tool boundary in
                // `call` (see `AGENT_NAME_PATTERN` / `validate_agent_name`).
                "pattern": AGENT_NAME_PATTERN,
                "description": "Name for the spawned agent. Makes it addressable via SendMessage({to: name}) while running."
            },
            "isolation": {
                "type": "string",
                "enum": ["worktree", "remote"],
                "description": "Isolation mode. \"worktree\" creates a temporary git worktree so the agent works on an isolated copy of the repo. \"remote\" launches the agent in a remote cloud environment (always runs in background; availability is gated)."
            },
            "cwd": {
                "type": "string",
                "description": "Absolute path to run the agent in. Overrides the working directory for all filesystem and shell operations within this agent. Mutually exclusive with isolation: \"worktree\"."
            },
            "preset": {
                "type": "string",
                "enum": ["quality", "fast"],
                "description": "Fusion preset when subagent_type is \"fusion\". Ignored for other agent types."
            },
            "models": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Explicit Fusion panel models (\"profile:model\" or \"model\") when subagent_type is \"fusion\"."
            },
            "dimensions": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Fusion scoring dimensions when subagent_type is \"fusion\"."
            },
            "max_panel": {
                "type": "integer",
                "minimum": 2,
                "maximum": 8,
                "description": "Fusion panel cap when subagent_type is \"fusion\"."
            },
            "partial_ok": {
                "type": "boolean",
                "description": "Continue Fusion analysis when some panels fail."
            },
            "cross_provider": {
                "type": "boolean",
                "description": "Allow Fusion panels to leave the parent provider. Agent default is same-provider."
            },
            "fusion_mode": {
                "type": "string",
                "enum": ["analysis", "implement"],
                "description": "Fusion panel mode when subagent_type is \"fusion\". \"analysis\" (default): read-only panels answer the task. \"implement\": each panel changes the code in its own git worktree and the host runs the configured verification commands on every result. Ignored for other agent types."
            }
        },
        "required": ["description", "prompt"],
        "allOf": [{
            "if": { "required": ["model_profile"] },
            "then": { "required": ["model"] }
        }]
    })
});

/// claude `V4o` — the Agent tool's `outputSchema`, a union of the result shapes
/// `call` can return.
///
/// ```js
/// q4o = D2n().extend({status:k("completed"),prompt:s(),worktreePath:s().optional(),worktreeBranch:s().optional()})
/// V4o = Ne([q4o, {status:k("async_launched"), …}, {status:k("remote_launched"), …}])
/// ```
///
/// This is NOT advertised to the model. Its one consumer is the PostToolUse
/// hook path: when a hook returns `updatedToolOutput`, the replacement is
/// validated against this schema and DISCARDED on mismatch, keeping the
/// original result (`turn_loop.rs`, `e.outputSchema?.safeParse(...)`). Without
/// it, any shape a hook returned was substituted unchecked.
///
/// The `remote_launched` arm is omitted — the port has no remote/CCR path and
/// `call` cannot produce that status, so declaring it would only widen what a
/// hook may substitute. The two arms here are exactly the two statuses `call`
/// emits.
///
/// `harnessNoteCount` / `harnessTailCount` / `harnessSectionHash` are declared
/// because `D2n` declares them (all optional); the port does not populate them
/// yet — see the harness-note residual in the audit report.
static AGENT_OUTPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    let usage = json!({
        "type": "object",
        "properties": {
            "input_tokens": { "type": "number" },
            "output_tokens": { "type": "number" },
            "cache_creation_input_tokens": { "type": ["number", "null"] },
            "cache_read_input_tokens": { "type": ["number", "null"] },
            "server_tool_use": { "type": ["object", "null"] },
            "service_tier": { "type": ["string", "null"] },
            "cache_creation": { "type": ["object", "null"] }
        },
        "required": ["input_tokens", "output_tokens"]
    });
    json!({
        "anyOf": [
            {
                "type": "object",
                "properties": {
                    "status": { "const": "completed" },
                    "agentId": { "type": "string" },
                    "agentType": { "type": "string" },
                    "prompt": { "type": "string" },
                    "content": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "type": { "const": "text" },
                                "text": { "type": "string" }
                            },
                            "required": ["type", "text"]
                        }
                    },
                    "resolvedModel": { "type": "string" },
                    "modelsUsed": { "type": "array", "items": { "type": "string" } },
                    "totalToolUseCount": { "type": "number" },
                    "totalDurationMs": { "type": "number" },
                    "totalTokens": { "type": "number" },
                    "usage": usage,
                    "worktreePath": { "type": "string" },
                    "worktreeBranch": { "type": "string" },
                    "harnessNoteCount": { "type": "number" },
                    "harnessTailCount": { "type": "number" },
                    "harnessSectionHash": { "type": "string" }
                },
                "required": [
                    "status", "agentId", "prompt", "content",
                    "totalToolUseCount", "totalDurationMs", "totalTokens", "usage"
                ]
            },
            {
                "type": "object",
                "properties": {
                    "status": { "const": "async_launched" },
                    "isAsync": { "type": "boolean" },
                    "agentId": { "type": "string", "description": "The ID of the async agent" },
                    "description": { "type": "string", "description": "The description of the task" },
                    "resolvedModel": { "type": "string", "description": "Model in use at the backgrounding transition (a pre-background swap is reflected here)" },
                    "modelsUsed": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Ordered distinct models used before backgrounding (length > 1 means a mid-run swap)"
                    },
                    "prompt": { "type": "string", "description": "The prompt for the agent" },
                    "outputFile": { "type": "string", "description": "Path to the output file for checking agent progress" },
                    "canReadOutputFile": { "type": "boolean", "description": "Whether the calling agent has Read/Bash tools to check progress" }
                },
                "required": ["status", "agentId", "description", "prompt", "outputFile"]
            }
        ]
    })
});

/// The MODEL-FACING input schema = [`AGENT_INPUT_SCHEMA`] with `cwd` removed.
///
/// claude resolves the AgentTool's advertised schema as `yJp().omit({cwd:!0})`
/// (`eEo`): `cwd` exists on the call signature — set internally by
/// `isolation:"worktree"` or an explicit override — but is NEVER advertised to
/// the model, so the model cannot pass it. `AGENT_INPUT_SCHEMA` (with `cwd`)
/// stays the canonical/full schema for deserialization; this projection is what
/// [`Tool::input_schema`] exposes.
///
static AGENT_INPUT_SCHEMA_MODEL: Lazy<Value> = Lazy::new(|| {
    let mut schema = AGENT_INPUT_SCHEMA.clone();
    if let Some(props) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        props.remove("cwd");
    }
    schema
});

static AGENT_INPUT_SCHEMA_MODEL_NO_BACKGROUND: Lazy<Value> = Lazy::new(|| {
    let mut schema = AGENT_INPUT_SCHEMA_MODEL.clone();
    if let Some(props) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        props.remove("run_in_background");
    }
    schema
});

/// Finish `gSn()`'s two remaining projections over the base model-facing schema:
/// the coordinator-mode `model` description suffix and the
/// `CLAUDE_CODE_SUBAGENT_MODEL_FORCE` omission of `model` altogether.
///
/// ```js
/// gSn=m(()=>{let e=z4o().omit({cwd:!0}),n=Bl()||L5()?e.omit({run_in_background:!0}):e;
///            return a.CLAUDE_CODE_SUBAGENT_MODEL_FORCE?n.omit({model:!0}):n})
/// ```
///
/// `run_in_background` is already projected by the caller's choice of base
/// (`advertise_run_in_background`); this applies the rest. Called once per tool
/// instance and memoized, matching the binary's `m(...)` memoization of the
/// whole builder.
fn project_agent_input_schema(base: &Value, is_coordinator: bool) -> Value {
    project_agent_input_schema_with_policy(
        base,
        is_coordinator,
        subagent_model_forced(),
        coordinator_forces_worker_inherit_model(),
    )
}

fn project_agent_input_schema_with_policy(
    base: &Value,
    is_coordinator: bool,
    model_forced: bool,
    coordinator_forces_inherit: bool,
) -> Value {
    let mut schema = base.clone();
    let Some(props) = schema.get_mut("properties").and_then(Value::as_object_mut) else {
        return schema;
    };
    if model_forced {
        props.remove("model");
        props.remove("model_profile");
        schema
            .as_object_mut()
            .expect("object schema")
            .remove("allOf");
        return schema;
    }
    if is_coordinator {
        let suffix = if coordinator_forces_inherit {
            AGENT_MODEL_PARAM_COORDINATOR_FORCED_SUFFIX
        } else {
            AGENT_MODEL_PARAM_COORDINATOR_SUFFIX
        };
        if let Some(model) = props.get_mut("model").and_then(Value::as_object_mut) {
            model.insert(
                "description".to_string(),
                Value::String(format!("{AGENT_MODEL_PARAM_DESCRIPTION}{suffix}")),
            );
        }
        if let Some(profile) = props
            .get_mut("model_profile")
            .and_then(Value::as_object_mut)
        {
            profile.insert(
                "description".to_string(),
                Value::String(format!("{AGENT_MODEL_PROFILE_PARAM_DESCRIPTION}{suffix}")),
            );
        }
    }
    if is_coordinator && coordinator_forces_inherit {
        schema
            .as_object_mut()
            .expect("object schema")
            .remove("allOf");
    }
    schema
}

fn clear_model_overrides_when_forced(parsed: &mut AgentToolInput, forced: bool) {
    if forced {
        parsed.model = None;
        parsed.model_profile = None;
    }
}

fn spawn_model_profile(parsed: &AgentToolInput, is_fork: bool) -> Option<String> {
    if is_fork {
        None
    } else {
        parsed.model_profile.clone()
    }
}

fn model_profile_after_hook(
    input: &Value,
    previous: Option<String>,
) -> Result<Option<String>, ToolError> {
    match input.get("model_profile") {
        None => Ok(previous),
        Some(Value::Null) => Ok(None),
        Some(Value::String(profile)) => Ok(Some(profile.clone())),
        Some(_) => Err(ToolError::InvalidInput(
            "agent.spawn: model_profile must be a string or null".into(),
        )),
    }
}

fn model_after_hook(input: &Value, previous: Option<String>) -> Result<Option<String>, ToolError> {
    match input.get("model") {
        None => Ok(previous),
        Some(Value::Null) => Ok(None),
        Some(Value::String(model)) => Ok(Some(model.clone())),
        Some(_) => Err(ToolError::InvalidInput(
            "agent.spawn: model must be a string or null".into(),
        )),
    }
}

/// Format the M3-05 byte-locked budget-exceeded denial string.
///
/// Caller passes `current_nano_usd`; output is the literal
/// `"Budget exceeded (${dollars:.2}); stopped."` (see `cost/src/budget.rs`).
#[must_use]
pub fn format_budget_denied(current_nano_usd: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let dollars = current_nano_usd as f64 / 1_000_000_000.0;
    format!("Budget exceeded (${dollars:.2}); stopped.")
}

fn format_nano_usd_compact(nano_usd: u64) -> String {
    let whole = nano_usd / 1_000_000_000;
    let fractional = nano_usd % 1_000_000_000;
    if fractional == 0 {
        whole.to_string()
    } else {
        let fraction = format!("{fractional:09}");
        format!("{whole}.{}", fraction.trim_end_matches('0'))
    }
}

fn budget_limit_reached_error(current_nano_usd: u64, limit_nano_usd: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let spent = current_nano_usd as f64 / 1_000_000_000.0;
    let maximum = format_nano_usd_compact(limit_nano_usd);
    format!(
        "Budget limit reached (${spent:.2} spent of the ${maximum} maximum). New agents cannot be started. Complete the remaining work directly with your tools, or wrap up with the results you already have."
    )
}

/// Render a handoff review into the block `EZe` prepends, in its own copy.
fn render_handoff_review(review: &lingxi_core::host::permission_gate::HandoffReview) -> String {
    use lingxi_core::host::permission_gate::HandoffReview;
    match review {
        HandoffReview::Flagged { reason } => {
            crate::classifier_handoff::format_security_warning(reason)
        }
        HandoffReview::Refused => crate::classifier_handoff::SAFEGUARD_REFUSED_WARNING.to_string(),
        HandoffReview::Unavailable { model, detail } => {
            crate::classifier_handoff::classifier_unavailable_warning(model, detail)
        }
    }
}

/// Extract claude's `content: [{type:'text', text}]` array from a subagent's
/// terminal result JSON.
///
/// The runner builds `result.content` as the agent's final text blocks (claude
/// `finalizeAgentTool`, agentToolUtils.ts) on the clean-stop path. Other terminal
/// results (max-turns / stub `{reason}`) carry no `content` key → empty array,
/// which the caller renders as the no-output marker. Returns each block's `text`.
fn extract_content_texts(result: &Value) -> Vec<String> {
    result
        .get("content")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|b| {
                    if b.get("type").and_then(Value::as_str) == Some("text") {
                        b.get("text").and_then(Value::as_str).map(str::to_string)
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Inputs to [`should_run_in_background`] — `vBo`'s argument object minus the
/// isolation fields, which the port resolves separately.
struct BackgroundDecision {
    /// `wantsBackground` — the call's `run_in_background`, tri-state.
    wants_background: Option<bool>,
    /// `n.background===!0` — the selected definition's frontmatter flag.
    definition_background: bool,
    /// `Pw(n)` — the BUILT-IN `web-fetch` agent, which never auto-backgrounds.
    is_builtin_web_fetch: bool,
    /// `e.isCoordinator`.
    is_coordinator: bool,
    /// `callerIsInProcessTeammate`.
    caller_is_in_process_teammate: bool,
    /// `backgroundTasksDisabled` — the only negative gate on the whole group.
    background_tasks_disabled: bool,
}

/// `vBo`'s `shouldRunAsync` for a LOCAL spawn (2.1.266 @2957xxx):
///
/// ```js
/// let d=e.isCoordinator&&!o||e.forceAsync||!o&&r!==!1;
/// let y=r===!0||n.background===!0||!Pw(n)&&d;
/// return {…, shouldRunAsync: E||y&&!e.backgroundTasksDisabled}
/// ```
///
/// (`E` is the remote-launch arm, which the port does not have.)
///
/// The two EXPLICIT arms — an explicit `run_in_background: true` and a
/// definition's `background: true` — sit outside the `!Pw(n)` factor, so they
/// still background the built-in web-fetch agent; only the implicit default
/// does not.
///
/// ⛔ `e.forceAsync` (`L5()&&!Le`, the raw fork FEATURE flag) is NOT modeled.
/// The binary backgrounds every spawn once fork is enabled, but the port's fork
/// path is synchronous end to end — the parent's rendered system prompt and the
/// fork context messages are threaded onto the request the SYNC dispatch builds
/// (`fork_threads_parent_system_prompt_onto_request`,
/// `fork_gate_on_explicit_fork_takes_fork_path`), and `dispatch_async` has no
/// equivalent. Adding the disjunct before the async fork path exists would send
/// forks down a route that drops their inherited context. It only changes the
/// answer for an explicit `run_in_background: false`, which the schema does not
/// advertise while fork is on.
fn should_run_in_background(d: BackgroundDecision) -> bool {
    let auto_background =
        !d.caller_is_in_process_teammate && (d.is_coordinator || d.wants_background != Some(false));
    (d.wants_background == Some(true)
        || d.definition_background
        || (!d.is_builtin_web_fetch && auto_background))
        && !d.background_tasks_disabled
        && !d.caller_is_in_process_teammate
}

fn async_launch_result(
    launch: lingxi_core::host::subagent_spawn::AsyncLaunch,
    task_id: Option<String>,
    prompt: &str,
    description: &str,
    resolved_model: &str,
    can_read_output_file: bool,
) -> ToolCallResult {
    let agent_id_str = launch.agent_id.as_uuid().to_string();
    let prefix = format!(
        "Async agent launched successfully. (This tool result is internal metadata — never quote or paste any part of it, including the agentId below, into a user-facing reply.)\nagentId: {agent_id_str} (internal ID - do not mention to user. Use SendMessage with to: '{agent_id_str}', summary: '<5-10 word recap>' to continue this agent.)\nThe agent is working in the background. You will be notified automatically when it completes. You know nothing about its results until that notification arrives — do not report, assume, or predict them; continue other work or respond to the user in the meantime."
    );
    let tail = if can_read_output_file {
        format!(
            "Do not duplicate this agent's work — avoid working with the same files or topics it is using.\noutput_file: {}\nDo NOT Read or tail this file via the shell tool — it is the full subagent JSONL transcript and reading it will overflow your context. If the user asks for progress, say the agent is still running; you'll get a completion notification.",
            launch.output_file
        )
    } else {
        "In your own words, briefly tell the user what you launched — do not echo this tool result. Agent results will arrive in a subsequent message. If the user asks for progress, say the agent is still running.".to_string()
    };
    let mut data = json!({"isAsync": true, "status": "async_launched", "agentId": agent_id_str, "prompt": prompt, "description": description, "resolvedModel": resolved_model,
        "outputFile": launch.output_file, "canReadOutputFile": can_read_output_file, "model_content": format!("{prefix}\n{tail}")});
    if let Some(task_id) = task_id {
        data["task_id"] = json!(task_id);
    }
    ToolCallResult {
        data,
        model_content: None,
        new_messages: vec![],
        context_modifier: None,
        is_error: false,
        mcp_meta: None,
    }
}

#[path = "foreground_task.rs"]
mod foreground_task;

use lingxi_core::host::subagent_output::max_turns_harness_note;

/// `N2n(e)` (@3530xxx) reduced to the shape the port's runner publishes: the
/// max-turns fall-through is the only completion that stamps
/// `reason: "max_turns_exhausted"` onto its result, and it carries the budget
/// under `max_turns`.
fn max_turns_reached_from_result(result: &Value) -> Option<u64> {
    if result.get("reason").and_then(Value::as_str) != Some("max_turns_exhausted") {
        return None;
    }
    result.get("max_turns").and_then(Value::as_u64)
}

/// Render the model-facing `tool_result` text for a COMPLETED subagent, byte-for-byte
/// per claude-code `mapToolResultToToolResultBlockParam` (AgentTool.tsx:1340-1373).
///
/// - Empty content → the single no-output marker
///   `(Subagent completed but returned no output.)` (AgentTool.tsx:1347-1350).
/// - One-shot built-ins (`Explore` / `Plan`) with NO worktree → content texts ONLY,
///   no trailer (AgentTool.tsx:1356-1362).
/// - Otherwise → content texts + the `agentId`/SendMessage hint (+ worktree fields)
///   + the `<usage>` block (AgentTool.tsx:1363-1373).
///
/// claude joins the `tool_result` content blocks with `\n` when collapsing to a
/// single string; LingXi's `model_content` IS that single string, so the text
/// blocks (and the trailer block) are `\n`-joined here.
fn render_completed_model_content(
    content_texts: &[String],
    agent_id: &str,
    agent_type: &str,
    total_tokens: u64,
    total_tool_use_count: u64,
    total_duration_ms: u64,
    worktree_info: Option<(&str, &str)>,
) -> String {
    // contentOrMarker (AgentTool.tsx:1347-1350).
    let content_or_marker: Vec<String> = if content_texts.is_empty() {
        vec!["(Subagent completed but returned no output.)".to_string()]
    } else {
        content_texts.to_vec()
    };

    // One-shot built-ins skip the trailer when there is no worktree info
    // (AgentTool.tsx:1356). A KEPT worktree (the agent made changes) appends its
    // path + branch (byte-exact `\nworktreePath: …\nworktreeBranch: …`,
    // AgentTool.tsx:1368-1370) right before the `<usage>` block; `None` (no
    // worktree, or a clean one that was removed) keeps the trailer empty.
    let worktree_info_text = match worktree_info {
        Some((path, branch)) => {
            format!("\nworktreePath: {path}\nworktreeBranch: {branch}")
        }
        None => String::new(),
    };
    let is_one_shot = ONE_SHOT_BUILTIN_AGENT_TYPES.contains(&agent_type);
    if is_one_shot && worktree_info_text.is_empty() {
        return content_or_marker.join("\n");
    }

    // Trailer (AgentTool.tsx:1366-1372): agentId/SendMessage hint + worktree +
    // <usage> block, appended after the content blocks. v2.1.185 renamed the
    // token key `total_tokens` → `subagent_tokens` (binary off 202120031;
    // confirmed live in subagent result trailers); the leaked TS still shows the
    // old `total_tokens`, so the binary is canonical here.
    let trailer = format!(
        "agentId: {agent_id} (use SendMessage with to: '{agent_id}', summary: '<5-10 word recap>' to continue this agent){worktree_info_text}\n<usage>subagent_tokens: {total_tokens}\ntool_uses: {total_tool_use_count}\nduration_ms: {total_duration_ms}</usage>"
    );
    let mut blocks = content_or_marker;
    blocks.push(trailer);
    blocks.join("\n")
}

/// `AgentTool` — spawn a subagent (surface only; recursive dispatch lands
/// in `lingxi-coordinator` post-M5).
pub struct AgentTool {
    ctx: BuiltinToolContext,
    /// Whether the ADVERTISED input schema keeps `run_in_background` (binary
    /// `nul` @292883815 — see [`AgentTool::new`]). Schema-only: the async
    /// DISPATCH gate is `!WA()` alone and is evaluated inline in `call`.
    advertise_run_in_background: bool,
    /// `gSn()`'s memoized projection of the advertised schema — the coordinator
    /// `model` description suffix and the `SUBAGENT_MODEL_FORCE` omission,
    /// resolved on first use because both read live session state.
    projected_input_schema: std::sync::OnceLock<Value>,
    /// Injected Fusion orchestrator. `None` on mobile and in `register_all`.
    fusion: Option<Arc<dyn FusionExecutor>>,
    /// Host-owned common terminal recorder for Agent-origin Fusion runs.
    terminal_recorder: Option<Arc<dyn FusionRunRecorder>>,
    /// Pure per-session recorder factory for long-lived hot-switch hosts.
    terminal_recorder_factory: Option<Arc<dyn FusionRunRecorderFactory>>,
}

/// Normalize a subagent `description` the way the binary does — `replace(/\s+/g,
/// " ").trim()`: collapse every run of whitespace to a single space and trim the
/// ends. (`split_whitespace` does both; it tracks Unicode White_Space, matching
/// JS `\s` for the typical ASCII description.)
fn normalize_description_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn fusion_panel_count(parsed: &AgentToolInput, surface: FusionAgentSurface) -> u8 {
    let cap = parsed
        .max_panel
        .unwrap_or(surface.max_panel)
        .clamp(FUSION_MIN_PANEL, surface.max_panel.min(FUSION_MAX_PANEL));
    if let Some(models) = parsed.models.as_ref() {
        return u8::try_from(models.len())
            .unwrap_or(cap)
            .clamp(FUSION_MIN_PANEL, cap);
    }
    let preset = parse_fusion_preset(parsed.preset.as_deref()).unwrap_or(surface.default_preset);
    let n = match preset {
        FusionPreset::Quality => surface.quality_panel_count,
        FusionPreset::Fast => surface.fast_panel_count,
    };
    n.clamp(FUSION_MIN_PANEL, cap)
}

fn parse_fusion_preset(raw: Option<&str>) -> Result<FusionPreset, ToolError> {
    match raw {
        None => Err(ToolError::InvalidInput("missing fusion preset".into())),
        // The wire spelling and rejection message come from the single
        // `FromStr` impl in core::host so the Agent tool and `/fusion`
        // reject an unknown preset identically.
        Some(other) => other
            .parse::<FusionPreset>()
            .map_err(|error| ToolError::InvalidInput(error.to_string())),
    }
}

/// Delegates to the single `lingxi_core::host::parse_fusion_models` every caller
/// (Agent tool, `/fusion`) parses caller-supplied model strings through, so a
/// malformed entry like `"openai:"` is rejected identically from either
/// entrypoint instead of one silently treating the whole literal as a bare
/// model id.
fn parse_fusion_models(raw: &[String]) -> Result<Vec<FusionModelRef>, ToolError> {
    lingxi_core::host::parse_fusion_models(raw)
        .map_err(|error| ToolError::InvalidInput(error.to_string()))
}

fn fusion_request_from_agent(
    fallback_profile: Option<&tool_api::MainLoopModelProfileProvider>,
    parsed: &AgentToolInput,
    ctx: &ToolUseContext,
    surface: FusionAgentSurface,
    executor: &dyn FusionExecutor,
) -> Result<FusionRequest, ToolError> {
    if parsed.cross_provider == Some(true) && !surface.allow_cross_provider {
        return Err(ToolError::InvalidInput(
            lingxi_core::host::FusionError::CrossProviderDenied.to_string(),
        ));
    }
    let preset = match parsed.preset.as_deref() {
        None => surface.default_preset,
        Some(raw) => parse_fusion_preset(Some(raw))?,
    };
    let models = parsed
        .models
        .as_ref()
        .map(|m| parse_fusion_models(m))
        .transpose()?;
    let dimensions = parsed.dimensions.clone().unwrap_or_default();
    let mode = parse_fusion_mode(parsed.fusion_mode.as_deref())?;
    if mode == FusionPanelMode::Implement && !surface.implement_available {
        return Err(ToolError::InvalidInput(
            "fusion_mode \"implement\" is not available on this host; use \"analysis\"".into(),
        ));
    }
    let parent_model =
        main_loop_model_parent(ctx).unwrap_or_else(|| ctx.options.main_loop_model.clone());
    let routed_profile = ctx
        .options
        .model_profile
        .clone()
        .or_else(|| fallback_profile.and_then(|resolve| resolve(&ctx.options.main_loop_model)));
    let parent_profile = executor
        .resolve_parent_profile(&parent_model, routed_profile.as_deref())
        .ok_or_else(|| {
            ToolError::InvalidInput(format!(
                "fusion parent model profile is unavailable for `{parent_model}`"
            ))
        })?;
    Ok(FusionRequest {
        verify_claims: false,
        schema_version: lingxi_core::host::FUSION_SCHEMA_VERSION,
        origin: FusionOrigin::Agent,
        prompt: parsed.prompt.clone(),
        preset,
        models,
        dimensions,
        partial_ok: parsed.partial_ok.unwrap_or(surface.default_partial_ok),
        max_panel: parsed.max_panel,
        cross_provider: parsed.cross_provider.unwrap_or(false),
        parent_profile,
        parent_model,
        mode,
        // Verification commands come from the user's settings or their own
        // `/fusion --verify`; a model can never choose what the host runs.
        verify_commands: Vec::new(),
    })
}

/// The confirmation the user sees before a model-initiated implement run.
fn fusion_implement_prompt(
    task: &str,
    confirmation: &lingxi_core::host::fusion::ImplementConfirmation,
) -> String {
    let panels = confirmation
        .panels
        .map_or_else(|| "Several panels".to_string(), |n| format!("{n} panels"));
    let cost = match confirmation.quote_nano_usd {
        Some(nano) => format!(
            "Reserved cost: up to ${:.2} (an upper bound, several times an analysis run).",
            nano as f64 / 1e9
        ),
        None => "The cost could not be estimated.".to_string(),
    };
    let task: String = task
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(200)
        .collect();
    format!(
        "{panels} will each change the code in their own git worktree, made from a snapshot of \
your workspace, and the host will run your verification commands on every result. Your \
workspace itself is not modified; the model then writes the final change.\n{cost}\n\
Set fusion.implement.autoApproveMaxUsd in your user settings to skip this question for runs \
up to a limit.\n\nTask: {task}"
    )
}

fn parse_fusion_mode(raw: Option<&str>) -> Result<FusionPanelMode, ToolError> {
    match raw {
        None | Some("analysis") => Ok(FusionPanelMode::Analysis),
        Some("implement") => Ok(FusionPanelMode::Implement),
        Some(other) => Err(ToolError::InvalidInput(format!(
            "unknown fusion_mode `{other}`; expected \"analysis\" or \"implement\""
        ))),
    }
}

fn fusion_stage_name(stage: &FusionStage) -> &'static str {
    match stage {
        FusionStage::ResolvingModels => "resolving_models",
        FusionStage::ReservingBudget => "reserving_budget",
        FusionStage::RunningPanels { .. } => "running_panels",
        FusionStage::PanelsDispatched { .. } => "panels_dispatched",
        FusionStage::PreparingWorktrees => "preparing_worktrees",
        FusionStage::CollectingPatches => "collecting_patches",
        FusionStage::Verifying => "verifying",
        FusionStage::CheckingEvidence => "checking_evidence",
        FusionStage::Analyzing => "analyzing",
        FusionStage::Completed => "completed",
        FusionStage::Failed => "failed",
        FusionStage::Cancelled => "cancelled",
    }
}

/// [round-3 review, findings 11/19] Whether a [`FusionStage`] progress event
/// is proof that at least one panel task has genuinely reached the spawner —
/// i.e. real provider calls are (or were) in flight — as opposed to merely
/// "the panel STAGE has started". `RunningPanels { completed: 0, .. }` alone
/// is NOT proof: `FusionOrchestrator::run_panel_stage` emits that BEFORE
/// `panel::run_panels` ever spawns a task. `PanelsDispatched` IS proof —
/// `run_panels` emits it itself, immediately after every panel task has been
/// handed to the spawner, before any of them can have reached a terminal
/// outcome. `RunningPanels { completed, .. }` with `completed > 0` is proof
/// too (a panel can only complete after spawning) — kept as a second arm
/// rather than relying on `PanelsDispatched` alone so this stays correct
/// even against an executor that (legitimately, per the trait's own
/// contract) never emits `PanelsDispatched` at all.
///
/// [round-5 review, finding 10] `Some` also carries the RESOLVED panel count
/// the proving event published — the predicate used to be a bare `bool`
/// (`stage_proves_panel_spawned`) that threw that number away.
///
/// `call_fusion` reserves the WANTED panel count (`fusion_panel_count`)
/// before model resolution has run; the resolver may legitimately settle on
/// FEWER panels (a provider credential expired, `fusion.allowedProfiles`
/// narrowed the catalog, ...). Both shapes that prove a panel genuinely
/// reached the spawner carry that resolved `total`, so the surplus the `Ok`
/// arm already refunds is knowable on the `Err` and drop paths too — the
/// old `AtomicBool` threw the number away and every runtime failure kept the
/// phantom slots charged against
/// `LINGXI_MAX_SUBAGENTS_PER_SESSION` forever.
///
/// Returns at least 1 for a proving event, so "nonzero" keeps meaning
/// exactly what the old boolean meant even against an executor that reports
/// a nonsensical `total: 0` alongside a completion.
fn panels_proven_spawned(stage: &FusionStage) -> Option<u64> {
    match stage {
        FusionStage::PanelsDispatched { total } => Some(u64::from(*total).max(1)),
        FusionStage::RunningPanels { completed, total } if *completed > 0 => {
            Some(u64::from(*total).max(u64::from(*completed)))
        }
        _ => None,
    }
}

fn fusion_tool_result(result: lingxi_core::host::FusionResult) -> ToolCallResult {
    let status = match result.status {
        FusionStatus::Analyzed => "analyzed",
        FusionStatus::Unanalyzed => "unanalyzed",
    };
    let material = lingxi_core::host::render_fusion_material(&result);
    let panels: Vec<Value> = result
        .panels
        .iter()
        .map(|p| {
            json!({
                "panel_id": p.panel_id,
                "status": p.status,
                "duration_ms": p.duration_ms,
                "error_category": p.error_category,
            })
        })
        .collect();
    ToolCallResult {
        data: json!({
            "runId": result.run_id,
            "mode": result.mode,
            "status": status,
            "analysisFailure": result.analysis_failure,
            "panels": panels,
            "usage": result.usage,
            "timing": result.timing,
            "egress": result.egress_profiles,
            // G008: tells the orchestrator's subagent-hook chokepoint
            // (turn_loop.rs) this result carries NO single child `agentId` to
            // attribute a hook pair to — a Fusion run is N panels, each
            // hook-silent by runner-side design (`agent_type ==
            // FUSION_PANEL_TYPE` skips the in-child SubagentStart; a panel
            // definition has no frontmatter Stop). Reusing the chokepoint's
            // existing `subagentHooksFired` field name (rather than a Fusion-
            // specific one) keeps the ordinary Agent-tool `runner_fired_start`
            // reader unchanged; the chokepoint tells the two cases apart by
            // this data's absent `agentId`.
            "subagentHooksFired": true,
        }),
        model_content: Some(material),
        new_messages: vec![],
        context_modifier: None,
        mcp_meta: None,
        is_error: false,
    }
}

/// Preserve computation independently from reliable attempt settlement status.
fn fusion_tool_result_with_settlement(
    result: lingxi_core::host::FusionResult,
    settlement: Option<&lingxi_core::host::FusionAttemptSettlementStatus>,
) -> ToolCallResult {
    let mut output = fusion_tool_result(result);
    if let Some(settlement) = settlement {
        output.data["attemptSettlement"] = json!(settlement);
        if matches!(
            settlement,
            lingxi_core::host::FusionAttemptSettlementStatus::Failed { .. }
        ) {
            output.data["computationStatus"] = output.data["status"].clone();
            output.data["status"] = json!("failed");
            output.is_error = true;
            // Keep the computed answer visible, but never present it as an
            // ordinary successful tool call. Detailed safe diagnostics remain
            // structured data, not instructions appended to the model prompt.
            if let Some(answer) = &mut output.model_content {
                answer.push_str("\n\nFusion accounting failed; the computed answer is retained, but accounting remains unavailable.");
            }
        }
    }
    output
}

/// Map a terminal [`lingxi_core::host::FusionError`] to the [`ToolError`] surfaced
/// to the model (F008). Explicit arms for the load-bearing distinctions the
/// generic `InvalidInput(other.to_string())` fallback erased:
///
/// - `Cancelled` (a user Ctrl-C / cancellation) → [`ToolError::Aborted`], not
///   `InvalidInput` — the old mapping read as "invalid input: fusion
///   cancelled", inviting the model to "fix" its arguments and retry a
///   cancelled 4-5x-cost run.
/// - The runtime-exhaustion family (`Internal`, `AllPanelsFailed`,
///   `AllPanelsFailedPreflight`, `MinPanelsNotMet`, `PanelSetIncomplete`,
///   `TimedOutEmpty`) →
///   [`ToolError::Internal`] — these are host/provider-side outcomes, not a
///   malformed tool call the model could correct by rewriting its input.
///
/// Everything else — including `InvalidRequest`/`InvalidConfiguration`/
/// `InvalidCustomModels` — falls through to the generic
/// `InvalidInput(other.to_string())` arm, which is [`lingxi_core::host::FusionError`]'s
/// own `Display` (e.g. "invalid fusion configuration: fusion.maxPanel must be
/// between 1 and 12") — NOT the bare inner message, so the model sees which
/// category of misconfiguration it hit, matching §3's promise that an invalid
/// config surfaces as `InvalidConfiguration`.
///
/// `BudgetExceeded` is intentionally NOT special-cased here: the caller
/// (`call_fusion`) maps it separately with the live budget numbers (this free
/// function has no budget handle), reusing the same
/// [`budget_limit_reached_error`] format as the pre-spawn budget check a few
/// lines above it.
fn fusion_tool_error(err: lingxi_core::host::FusionError) -> ToolError {
    use lingxi_core::host::FusionError;
    match err {
        FusionError::Cancelled => ToolError::Aborted,
        FusionError::Internal
        | FusionError::AllPanelsFailed
        | FusionError::AllPanelsFailedPreflight
        | FusionError::PanelAdmissionRejected(_)
        | FusionError::MinPanelsNotMet
        | FusionError::PanelSetIncomplete
        | FusionError::TimedOutEmpty => ToolError::Internal(err.to_string()),
        other => ToolError::InvalidInput(other.to_string()),
    }
}

/// The `error_category` values that PROVE the panel's task never reached the
/// subagent spawner, so no subagent was ever allocated and no provider call
/// was ever made for that slot:
///
/// * `"spawn"` — the spawner rejected the panel pre-allocation (e.g. the
///   pool was full).
/// * `"not_dispatched"` — the slot was aborted by the panel bar, or
///   cancelled, before its task ever called the spawner, OR (round-6
///   blocking B1) aborted while parked INSIDE a spawner call that had not
///   yet been allocated a child. Fusion distinguishes the two with
///   `PanelDispatch`'s separate `reached`/`allocated` flags, and only the
///   latter proves a subagent exists.
///
/// SOURCE OF TRUTH: [`lingxi_core::host::fusion::panel_never_dispatched`], which
/// both crates now call — `fusion::panel::is_never_dispatched_category` is a
/// crate-local alias for the very same function. This is deliberately NOT a
/// second `matches!` list: round-6 blocking B2 was exactly two independent
/// lists drifting apart (`"not_dispatched"` was added on the fusion side
/// while this side still compared against `"spawn"` alone, so every such
/// panel silently burned a lifetime spawn slot for a subagent that never
/// existed). `fusion_panels_that_reached_the_spawner` is the only caller.
fn fusion_category_proves_never_dispatched(category: Option<&str>) -> bool {
    lingxi_core::host::fusion::panel_never_dispatched(category)
}

/// How many of a successful [`lingxi_core::host::FusionResult`]'s panels actually
/// reached the subagent spawner — i.e. how many subagents genuinely exist and
/// must stay charged against the session's lifetime spawn quota. `call_fusion`
/// releases `panel_n - this` as surplus on the `Ok` path.
///
/// `result.panels` carries EVERY slot the orchestrator collected, including
/// ones that provably never became a subagent
/// (see [`fusion_category_proves_never_dispatched`]); counting those as
/// spawned under-releases the reservation and permanently over-charges
/// `LINGXI_MAX_SUBAGENTS_PER_SESSION`.
fn fusion_panels_that_reached_the_spawner(panels: &[lingxi_core::host::PanelOutcome]) -> usize {
    panels
        .iter()
        .filter(|panel| !fusion_category_proves_never_dispatched(panel.error_category.as_deref()))
        .count()
}

/// Whether a [`lingxi_core::host::FusionError`] is guaranteed to have made ZERO
/// provider calls (F008 spawn accounting) — the doc comment on
/// [`lingxi_core::host::FusionError`] itself: "Preflight variants guarantee zero
/// provider calls." `call_fusion` releases the FULL `panel_n` reservation
/// only for these; every other variant means panels genuinely spawned (or the
/// error carries no data to say how many), so the reservation stays charged —
/// mirroring how `Ok` releases only the trimmed surplus
/// (`panel_n - result.panels.len()`), never the whole amount.
fn fusion_error_is_preflight(err: &lingxi_core::host::FusionError) -> bool {
    use lingxi_core::host::FusionError;
    matches!(
        err,
        FusionError::Disabled
            | FusionError::UnavailableOnPlatform
            | FusionError::InvalidConfiguration(_)
            | FusionError::InvalidRequest(_)
            | FusionError::TooFewModels { .. }
            | FusionError::InvalidCustomModels(_)
            | FusionError::CrossProviderDenied
            | FusionError::NoJudgeModel { .. }
            | FusionError::StructuredOutputUnsupported
            | FusionError::BudgetReservationUnavailable
            | FusionError::BudgetExceeded
            | FusionError::SpawnLimitExceeded
            // [round-3 review, finding 12] Every panel failed via a
            // pre-allocation spawner rejection — the orchestrator only
            // returns this variant when EVERY collected panel's
            // `error_category` is `"spawn"` (`check_panel_bar`), which
            // guarantees zero provider calls, same as every other variant
            // above. The general `AllPanelsFailed` (NOT listed here) can
            // also cover panels that DID call a provider and lost, so it
            // stays charged.
            | FusionError::AllPanelsFailedPreflight
            | FusionError::PanelAdmissionRejected(_)
            // Implement mode refuses before any panel is dispatched.
            | FusionError::ImplementUnavailable(_)
    )
}

/// [round-2 review, findings 4 & 13] RAII guard over `call_fusion`'s
/// up-front `panel_n` spawn-slot reservation.
///
/// The dispatcher (`orchestrator::turn_loop`'s biased `tokio::select!` around
/// a `Cancel`-behavior tool's call future) races `call_fusion`'s own future
/// against the SAME `CancellationToken` the tool was handed as
/// `ctx.cancel`/`inherit.cancel`; when the token wins, the future is DROPPED
/// mid-flight and neither the `Ok` nor the `Err` match arm below ever runs.
/// Any release logic that lived only in those arms was therefore dead code
/// on an interactive user interrupt, permanently leaking `panel_n` spawn
/// slots for panels that may never have existed.
///
/// The normal arms release exactly what they decide to release and then
/// call [`Self::disarm`]; if the future is instead dropped before either
/// arm runs, `Drop` transfers whatever is still outstanding to an owned
/// terminal finalizer. That finalizer waits for stable prepared facts before
/// applying the same allocation truth the normal `Err(Cancelled)` arm uses.
struct FusionSpawnReservationGuard {
    registry: Arc<dyn lingxi_core::host::task_registry::TaskRegistryHandle>,
    outstanding: u64,
    /// The prepared run's lifecycle authority. A dropped Agent waiter must
    /// defer settlement until this control publishes stable terminal facts;
    /// allocation callbacks can still race an immediate snapshot.
    control: FusionRunControl,
    /// [round-5 review, finding 10] 0 = the forwarder never saw a progress
    /// event that [`panels_proven_spawned`] accepts as proof a panel task
    /// reached the spawner; otherwise the RESOLVED panel count that event
    /// carried (see [`panels_proven_spawned`]). Nonzero-ness is exactly the
    /// old `AtomicBool`; the value is what lets the drop path refund the
    /// reserved-but-never-resolved surplus instead of keeping it charged.
    resolved_panels: Arc<std::sync::atomic::AtomicU64>,
    /// [round-12 review, finding 3] The largest
    /// `FusionProgress::panels_allocated` any event carried, or 0 when the
    /// executor published `Some(0)`. `allocated_panels_observed` preserves
    /// the separate `None` (no allocation figure) state.
    ///
    /// `resolved_panels` above is the count the model resolver settled on —
    /// it INCLUDES panels the spawner then rejected pre-allocation
    /// (`error_category: "spawn"`), which the `Ok` arm filters out of the
    /// charge via `fusion_panels_that_reached_the_spawner`. Charging the
    /// resolved count here made an identical 3-panel dispatch bill 3 slots on
    /// a failure and 2 on a success. See [`allocation_capped_charge`].
    allocated_panels: Arc<std::sync::atomic::AtomicU64>,
    /// Whether an event explicitly published `panels_allocated: Some(_)`,
    /// including `Some(0)`. Zero is a valid allocation count and cannot carry
    /// the missing-value state by itself.
    allocated_panels_observed: Arc<std::sync::atomic::AtomicBool>,
}

/// [round-12 review, finding 3] The number of `panel_n` reservation slots a
/// non-preflight termination may keep charged, given the resolved panel count
/// the progress events carried and the largest allocation figure they
/// carried.
///
/// A missing allocation figure is deliberately different from an explicit
/// `Some(0)`: older/custom executors that publish no figure retain the
/// conservative resolved-count charge, while a real producer that publishes
/// a final zero has proved that no child was allocated. Any published figure
/// can only lower the charge, matching the successful arm's filtering basis.
fn allocation_capped_charge(resolved: u64, allocated: u64, allocated_observed: bool) -> u64 {
    if !allocated_observed {
        resolved
    } else {
        resolved.min(allocated)
    }
}

/// Settle one dropped reservation from the terminal facts, falling back to
/// legacy progress latches only when the prepared envelope did not provide a
/// count. The facts argument is already terminal-stable when present.
fn settle_dropped_fusion_reservation(
    registry: &Arc<dyn lingxi_core::host::task_registry::TaskRegistryHandle>,
    outstanding: u64,
    facts: Option<lingxi_core::host::FusionRunFacts>,
    resolved_panels: &Arc<std::sync::atomic::AtomicU64>,
    allocated_panels: &Arc<std::sync::atomic::AtomicU64>,
    allocated_panels_observed: &Arc<std::sync::atomic::AtomicBool>,
) {
    let resolved = facts
        .as_ref()
        .and_then(|facts| facts.resolved_panels)
        .map(u64::from)
        .unwrap_or_else(|| resolved_panels.load(std::sync::atomic::Ordering::Relaxed));
    let allocated = facts
        .as_ref()
        .and_then(|facts| facts.allocated_panels)
        .map(u64::from)
        .unwrap_or_else(|| allocated_panels.load(std::sync::atomic::Ordering::Relaxed));
    let allocated_observed = facts
        .as_ref()
        .is_some_and(|facts| facts.allocated_panels.is_some())
        || allocated_panels_observed.load(std::sync::atomic::Ordering::Relaxed);
    let charge_basis = resolved.max(allocated);
    let charge = allocation_capped_charge(charge_basis, allocated, allocated_observed);
    let release = if charge_basis == 0 {
        outstanding
    } else {
        outstanding.saturating_sub(charge)
    };
    if release > 0 {
        registry.release_total_agent_spawn_reservations(release);
    }
}

impl FusionSpawnReservationGuard {
    fn new(
        registry: Arc<dyn lingxi_core::host::task_registry::TaskRegistryHandle>,
        panel_n: u64,
        control: FusionRunControl,
        resolved_panels: Arc<std::sync::atomic::AtomicU64>,
        allocated_panels: Arc<std::sync::atomic::AtomicU64>,
        allocated_panels_observed: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self {
            registry,
            outstanding: panel_n,
            control,
            resolved_panels,
            allocated_panels,
            allocated_panels_observed,
        }
    }

    /// Release `amount` (capped at what is still outstanding) right now.
    fn release(&mut self, amount: u64) {
        let amount = amount.min(self.outstanding);
        if amount == 0 {
            return;
        }
        self.registry.release_total_agent_spawn_reservations(amount);
        self.outstanding -= amount;
    }

    /// The caller has finished deciding what to release for this run;
    /// whatever remains outstanding is deliberately kept charged (panels
    /// genuinely ran). Stops `Drop` from acting a second time.
    fn disarm(&mut self) {
        self.outstanding = 0;
    }
}

impl Drop for FusionSpawnReservationGuard {
    fn drop(&mut self) {
        if self.outstanding == 0 {
            return;
        }
        // The caller's future can be dropped while a synchronous allocation
        // receipt is still racing the progress forwarder. Move the remaining
        // reservation into an owned finalizer and wait for the prepared run's
        // terminal notification before projecting its final facts. If no
        // runtime is available (for example during process teardown), retain
        // the charge conservatively rather than refunding from a stale view.
        let outstanding = self.outstanding;
        let registry = self.registry.clone();
        let control = self.control.clone();
        let resolved_panels = self.resolved_panels.clone();
        let allocated_panels = self.allocated_panels.clone();
        let allocated_panels_observed = self.allocated_panels_observed.clone();
        if let Some(outcome) = self.control.terminal_outcome() {
            // A terminal envelope is already stable, so this path is safe even
            // when Drop runs during runtime teardown with no current handle.
            settle_dropped_fusion_reservation(
                &registry,
                outstanding,
                Some(outcome.facts),
                &resolved_panels,
                &allocated_panels,
                &allocated_panels_observed,
            );
        } else if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let outcome = control.wait_terminal().await;
                settle_dropped_fusion_reservation(
                    &registry,
                    outstanding,
                    Some(outcome.facts),
                    &resolved_panels,
                    &allocated_panels,
                    &allocated_panels_observed,
                );
            });
        }
        self.outstanding = 0;
    }
}

/// Default per-session subagent spawn cap (claude 2.1.212 `ofg = 200`).
const MAX_SUBAGENTS_PER_SESSION_DEFAULT: u64 = 200;

/// Resolve the per-session subagent spawn cap from a raw
/// `LINGXI_MAX_SUBAGENTS_PER_SESSION` value (claude 2.1.212 `xtu()` =
/// `LINGXI_MAX_SUBAGENTS_PER_SESSION ?? 200`). Split from the env read for
/// testability. An unset OR unparseable value falls back to the default 200 (the
/// binary keeps a non-numeric env string, whose numeric comparison never trips
/// the cap; treating garbage as "default 200" is the faithful common-case
/// behavior and avoids a silently-disabled cap).
fn max_subagents_per_session_from(raw: Option<&str>) -> u64 {
    raw.and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(MAX_SUBAGENTS_PER_SESSION_DEFAULT)
}

/// The live per-session subagent spawn cap (claude 2.1.212 `xtu()`).
fn max_subagents_per_session() -> u64 {
    max_subagents_per_session_from(
        std::env::var(branding::MAX_SUBAGENTS_PER_SESSION_ENV)
            .ok()
            .as_deref(),
    )
}

fn subagent_depth_limit_error(depth: u32, limit: u32) -> String {
    format!(
        "Subagent nesting limit reached (depth {depth} of {limit}). Complete this task directly using your tools instead of spawning another agent. If the user explicitly requested deeper nesting, ask them to raise {MAX_SUBAGENT_SPAWN_DEPTH_ENV}."
    )
}

fn concurrent_subagent_limit_error(limit: usize) -> String {
    format!(
        "Concurrent subagent limit reached. You can run {limit} subagents at once. Do not retry. If the user wants more concurrent subagents, ask them to increase {MAX_CONCURRENT_SUBAGENTS_ENV}."
    )
}

/// The parent / main-loop model a spawn's `AgentModel::Inherit` + bare family
/// aliases resolve against — claude-code `AgentTool.tsx:418`
/// `getAgentModel(selectedAgent.model, toolUseContext.options.mainLoopModel, …)`.
///
/// At the top level `ToolUseContext.options.main_loop_model` is the LIVE session
/// model (the orchestrator sets it from `session.model`, updated by `/model`
/// switches / resume), so a mid-session model change is reflected in subsequently
/// spawned subagents. On a NESTED spawn the `RegistryToolInvoker` seeds it from
/// the dispatching subagent's own resolved model (claude `runAgent.ts:678`
/// `mainLoopModel: resolvedAgentModel`), so a child inherits its IMMEDIATE
/// parent's model. Threaded onto `SubagentSpawnRequest.parent_model_override`,
/// where it takes precedence over the spawner's boot/live `default_model`. An
/// empty model (the invoker's legacy placeholder / an unset context) yields
/// `None`, leaving the spawner to fall back to its own default.
fn main_loop_model_parent(ctx: &ToolUseContext) -> Option<String> {
    let model = ctx.options.main_loop_model.trim();
    if model.is_empty() || model == "subagent" {
        None
    } else {
        Some(model.to_string())
    }
}

fn parent_permission_mode(
    ctx: &ToolUseContext,
    gate: Option<&Arc<dyn lingxi_core::host::permission_gate::PermissionGate>>,
) -> Option<String> {
    ctx.trusted_effective_permission_mode
        .clone()
        .or_else(|| gate.and_then(|gate| gate.permission_mode()))
}

fn handback_completion_texts(
    handback: Option<&lingxi_core::host::handback::HandbackState>,
    content: &Value,
    sender_name: &str,
    waiting_on_owned_work: bool,
    resumable: bool,
) -> Vec<String> {
    use lingxi_core::host::handback::{
        handback_pointer, handback_withheld, HandbackDisposition, HANDBACK_INTERIM,
    };
    match handback.and_then(|state| state.disposition) {
        Some(HandbackDisposition::Send) => vec![handback_pointer(false, sender_name)],
        Some(HandbackDisposition::Flagged) => vec![handback_pointer(true, sender_name)],
        Some(HandbackDisposition::Withheld) => vec![if waiting_on_owned_work {
            HANDBACK_INTERIM.into()
        } else {
            handback_withheld(resumable)
        }],
        None => extract_content_texts(content),
    }
}

/// The child's agent id, whatever way its run ended.
fn subagent_result_agent_id(
    result: &lingxi_core::host::SubagentResult,
) -> lingxi_core::types::AgentId {
    match result {
        lingxi_core::host::SubagentResult::Completed { agent_id, .. }
        | lingxi_core::host::SubagentResult::Failed { agent_id, .. }
        | lingxi_core::host::SubagentResult::Killed { agent_id, .. } => *agent_id,
    }
}

impl AgentTool {
    fn release_spawn_reservation(&self) {
        if let Some(registry) = &self.ctx.task_registry {
            registry.release_total_agent_spawn_reservation();
        }
    }

    /// Construct.
    ///
    /// Resolves the advertised-schema gate, binary `nul` @292883815:
    ///
    /// ```js
    /// nul=we(()=>{let e=e9v().omit({cwd:!0});return WA()||z1e()?e.omit({run_in_background:!0}):e})
    /// ```
    ///
    /// * `WA()` @286250643 = `wZe().backgroundTasksDisabled||CLAUDE_CODE_DISABLE_BACKGROUND_TASKS`
    ///   — the host-scoped half is settable only by an in-process SDK host, which
    ///   LingXi does not have, so the port reads the env kill-switch
    ///   (`LINGXI_DISABLE_BACKGROUND_TASKS`) alone, exactly as every other
    ///   `WA()`/`DT()` site here does.
    /// * `z1e()` @286314232 = `Obp()!=="disabled"` — the RAW fork-subagent
    ///   FEATURE flag. It is NOT `is_fork_subagent_enabled`: the coordinator /
    ///   non-interactive conditioning lives in `CGf`'s `forkAvailable` argument
    ///   (`i = o && r`), not in `z1e()` itself, and this schema builder has no
    ///   session context at all. The port's stand-in for the missing GrowthBook
    ///   flag is `LINGXI_FORK_SUBAGENT` (see
    ///   [`lingxi_core::host::fork_subagent::is_fork_subagent_enabled`]), read raw here.
    ///
    /// The pre-2.1.238 port gated on `lingxi_core::host::subscription::is_pro_plan()`
    /// instead of the fork flag. That was wrong in both directions: on a Pro plan
    /// it hid `run_in_background` and forced synchronous dispatch, and with fork
    /// enabled it kept advertising a field the binary omits. All four
    /// `Cc()==="pro"` sites in 2.1.238 (284189548, 285093692, 292442005,
    /// 302317223) are the plan predicate, the model-picker suffix, the
    /// Agent-prompt discouragement block, and a statusline hint — none gates
    /// background agents.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        let background_tasks_disabled = lingxi_core::host::env::is_env_truthy(
            std::env::var("LINGXI_DISABLE_BACKGROUND_TASKS")
                .ok()
                .as_deref(),
        );
        let fork_feature_enabled = lingxi_core::host::env::is_env_truthy(
            std::env::var("LINGXI_FORK_SUBAGENT").ok().as_deref(),
        );
        let advertise_run_in_background = !background_tasks_disabled && !fork_feature_enabled;
        Self {
            ctx,
            advertise_run_in_background,
            projected_input_schema: std::sync::OnceLock::new(),
            fusion: None,
            terminal_recorder: None,
            terminal_recorder_factory: None,
        }
    }

    /// Attach a Fusion executor. Listing and `subagent_type: "fusion"` stay
    /// inert until [`FusionExecutor::agent_surface`] reports enabled.
    #[must_use]
    pub fn with_fusion(mut self, executor: Arc<dyn FusionExecutor>) -> Self {
        self.fusion = Some(executor);
        self
    }

    /// Attach the host-owned common terminal recorder. Agent receives no
    /// Slash publication target, even when its trusted session is known.
    #[must_use]
    pub fn with_terminal_recorder(mut self, recorder: Arc<dyn FusionRunRecorder>) -> Self {
        self.terminal_recorder = Some(recorder);
        self
    }

    /// Attach a pure per-session recorder factory. Agent receives no Slash
    /// publication target, but its terminal receipt remains pinned to the
    /// trusted session authority selected before preparation.
    #[must_use]
    pub fn with_terminal_recorder_factory(
        mut self,
        factory: Arc<dyn FusionRunRecorderFactory>,
    ) -> Self {
        self.terminal_recorder_factory = Some(factory);
        self
    }

    fn terminal_recorder_for(
        &self,
        session_id: Option<lingxi_core::types::SessionId>,
    ) -> Option<Arc<dyn FusionRunRecorder>> {
        if let Some(factory) = self.terminal_recorder_factory.as_ref() {
            // A production task normally carries the trusted origin session.
            // If a legacy standalone host has no session binding, retain its
            // explicitly supplied compatibility recorder (the desktop no-
            // persistence path supplies an explicit StorageFailure recorder)
            // instead of inventing an id for a pure factory lookup.
            return session_id.map_or_else(
                || self.terminal_recorder.clone(),
                |session_id| factory.recorder_for(session_id),
            );
        }
        self.terminal_recorder.clone()
    }

    fn fresh_invocation_id() -> String {
        tool_api::util::ids::ulid_or_uuid()
    }

    /// Resolve the trusted session that owns this tool call. Top-level calls
    /// read the live session cell; nested calls use the explicit origin hop
    /// stamped by the parent runner. Never infer this from hook/session
    /// metadata because those may describe a boot session after resume.
    async fn origin_session_id(ctx: &ToolUseContext) -> Option<lingxi_core::types::SessionId> {
        if let Some(session) = ctx.session.as_ref() {
            return Some(session.lock().await.session_id);
        }
        ctx.origin_session_id
    }

    fn budget_for_origin(
        budget: Arc<dyn lingxi_core::host::budget::BudgetEnforcerHandle>,
        origin_session_id: Option<lingxi_core::types::SessionId>,
    ) -> Arc<dyn lingxi_core::host::budget::BudgetEnforcerHandle> {
        origin_session_id
            .and_then(|session_id| budget.scoped_for_session(session_id))
            .unwrap_or(budget)
    }

    /// A model-initiated implement-mode Fusion run changes files in several
    /// worktrees, runs the user's verification commands and costs several
    /// times an analysis run. Ask the user first, unless the host says the run
    /// is within their automatic-approval limit. A call `call_fusion` will
    /// reject anyway is not asked about.
    fn fusion_implement_ask(
        &self,
        input: &Value,
        ctx: &ToolUseContext,
    ) -> Option<PermissionResult> {
        let parsed: AgentToolInput = serde_json::from_value(input.clone()).ok()?;
        let is_fusion = parsed
            .subagent_type
            .as_deref()
            .is_some_and(|agent_type| normalize_agent_type(agent_type) == FUSION_AGENT_TYPE);
        if !is_fusion
            || parse_fusion_mode(parsed.fusion_mode.as_deref()).ok()? != FusionPanelMode::Implement
        {
            return None;
        }
        let executor = self.fusion.as_ref()?;
        let surface = executor.agent_surface();
        if !surface.enabled || !surface.implement_available {
            return None;
        }
        let request = fusion_request_from_agent(
            self.ctx.main_loop_model_profile_provider.as_ref(),
            &parsed,
            ctx,
            surface,
            executor.as_ref(),
        )
        .ok()?;
        let confirmation = executor.implement_confirmation(&request);
        if !confirmation.required {
            return None;
        }
        let message = fusion_implement_prompt(&parsed.prompt, &confirmation);
        Some(PermissionResult::Ask {
            reason: PermissionDecisionReason::Other {
                reason: "Fusion implement mode changes files and runs commands".into(),
            },
            prompt: permission::result::PermissionPrompt {
                title: "Fusion implement mode".into(),
                message,
                options: vec!["Allow".into(), "Deny".into()],
            },
            pending_classifier_check: None,
            metadata: PermissionMetadata::default(),
        })
    }

    fn fusion_surface(&self) -> FusionAgentSurface {
        self.fusion
            .as_ref()
            .map_or_else(FusionAgentSurface::default, |ex| ex.agent_surface())
    }

    fn append_fusion_listing(&self, agents: &mut Vec<SubagentListingEntry>) {
        if !self.fusion_surface().enabled {
            return;
        }
        if agents.iter().any(|a| a.agent_type == FUSION_AGENT_TYPE) {
            return;
        }
        let surface = self.fusion_surface();
        let (when_to_use, tools_description) = if surface.implement_available {
            (
                format!("{FUSION_WHEN_TO_USE} {FUSION_IMPLEMENT_WHEN_TO_USE}"),
                "Fusion deliberation (read-only panels; implement mode edits isolated worktrees)",
            )
        } else {
            (
                FUSION_WHEN_TO_USE.to_string(),
                "Fusion deliberation (read-only panel)",
            )
        };
        let when_to_use = if surface.proactive {
            format!("{FUSION_PROACTIVE_WHEN_TO_USE} {when_to_use}")
        } else {
            when_to_use
        };
        agents.push(SubagentListingEntry {
            agent_type: FUSION_AGENT_TYPE.to_string(),
            when_to_use,
            when_to_use_lean: None,
            tools_description: tools_description.to_string(),
        });
    }

    // [round-2 review, finding 20] Both `call_fusion` not-found returns
    // (executor absent / `!surface.enabled`) render this tail. Every OTHER
    // `Available agents:` tail in this file (the omitted-type and
    // explicit-type not-found arms below) filters out `Agent(<type>)` deny
    // rules and tools-denied types first, "so a denied type never appears as
    // a suggestion" — this one used to skip that filter entirely, so the
    // model could be steered into dispatching a type that is immediately
    // hard-rejected. Apply the SAME deny set here.
    async fn fusion_available_agents_display(&self, is_coordinator: bool) -> String {
        let mut listing = match &self.ctx.subagent_spawner {
            Some(s) => s.agent_listing_for_model().await,
            None => Vec::new(),
        };
        drop_coordinator_hidden_builtins(&mut listing, is_coordinator);
        self.append_fusion_listing(&mut listing);
        let mut denied: Vec<String> = match &self.ctx.permission_gate {
            Some(gate) => gate.agent_deny_content_types().await,
            None => Vec::new(),
        };
        if let Some(spawner) = &self.ctx.subagent_spawner {
            denied.extend(spawner.tools_denied_agent_types().await);
        }
        render_available_agents(
            &listing
                .iter()
                .filter(|a| !denied.iter().any(|d| d == &a.agent_type))
                .map(|a| a.agent_type.clone())
                .collect::<Vec<_>>(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    async fn call_fusion(
        &self,
        parsed: AgentToolInput,
        ctx: ToolUseContext,
        progress: ToolProgressSender,
        bus: &Arc<AnalyticsBus>,
        invocation_id: &str,
        started: Instant,
        is_coordinator: bool,
    ) -> Result<ToolCallResult, ToolError> {
        // F008: `call`'s catalog-lookup dispatch (a few lines above the
        // `call_fusion` call site) intercepts `subagent_type: "fusion"`
        // BEFORE the `Agent(<type>)` deny-rule and tools-denied checks that
        // every OTHER subagent type passes through — so a user's
        // `Agent(fusion)` permission rule silently had no effect. Duplicated
        // here (byte-identical denial text to the catalog path) rather than
        // moving the intercept, since the catalog path's own `explicit_type`
        // resolution (ambiguity matching, `general-purpose` fallback,
        // fork routing) has no notion of the synthetic `fusion` entry to
        // begin with.
        if let Some(gate) = &self.ctx.permission_gate {
            if let Some(source) = gate.agent_type_deny(FUSION_AGENT_TYPE).await {
                Self::emit_failed(
                    bus,
                    invocation_id,
                    "agent_type_denied",
                    started.elapsed().as_millis() as u64,
                )
                .await;
                return Err(ToolError::InvalidInput(format!(
                    "Agent type '{FUSION_AGENT_TYPE}' has been denied by permission rule 'Agent({FUSION_AGENT_TYPE})' from {source}."
                )));
            }
        }
        if let Some(spawner) = &self.ctx.subagent_spawner {
            let tools_denied = spawner.tools_denied_agent_types().await;
            if tools_denied.iter().any(|t| t == FUSION_AGENT_TYPE) {
                Self::emit_failed(
                    bus,
                    invocation_id,
                    "agent_type_tools_denied",
                    started.elapsed().as_millis() as u64,
                )
                .await;
                return Err(ToolError::InvalidInput(agent_type_tools_denied_error(
                    FUSION_AGENT_TYPE,
                )));
            }
        }
        // F008: an explicit `run_in_background: true` is silently ignored by
        // a synchronous, in-tool Fusion run (§1 fixes Agent-origin Fusion to
        // "只写 tool_result" — no background LocalFusion route exists here);
        // reject it explicitly instead of pretending to honor it.
        //
        // This early return in `call_fusion` used to skip `Self::emit_failed`
        // while most sibling rejections (`agent_type_denied`,
        // `agent_type_tools_denied`, `agent_type_not_found`,
        // `fusion_invalid_configuration`, `budget_exceeded`,
        // `subagent_count_cap`) emit one — this fix closes THIS gap so the
        // rejection is visible to the dashboards that see every other
        // `call_fusion` failure. [round-5 review, findings 17 & 20] The two
        // remaining silent gaps it named as out of scope — the
        // `budget_enforcer` mis-wiring and `fusion_request_from_agent`'s
        // rejections, both a few lines below — now emit too, so EVERY
        // reachable return in this function logs exactly one terminal event.
        if parsed.run_in_background == Some(true) {
            Self::emit_failed(
                bus,
                invocation_id,
                "fusion_run_in_background",
                started.elapsed().as_millis() as u64,
            )
            .await;
            return Err(ToolError::InvalidInput(
                "fusion runs inline; use /fusion for a background run".into(),
            ));
        }

        let executor = match self.fusion.as_ref() {
            Some(ex) => Arc::clone(ex),
            None => {
                let available = self.fusion_available_agents_display(is_coordinator).await;
                Self::emit_failed(
                    bus,
                    invocation_id,
                    "agent_type_not_found",
                    started.elapsed().as_millis() as u64,
                )
                .await;
                return Err(ToolError::InvalidInput(format!(
                    "Agent type '{FUSION_AGENT_TYPE}' not found. Available agents: {available}"
                )));
            }
        };
        // F008: a boot-pinned rejection (an invalid `fusion.*` setting at
        // composition-root time — see `RejectedFusionExecutor`) is surfaced
        // AS ITSELF, before the `enabled` gate below — otherwise it always
        // masqueraded as the generic "not found"/`Disabled` message and §3's
        // "invalid config → InvalidConfiguration" promise held only for
        // `/fusion`.
        if let Some(error) = executor.preflight_error() {
            Self::emit_failed(
                bus,
                invocation_id,
                "fusion_invalid_configuration",
                started.elapsed().as_millis() as u64,
            )
            .await;
            return Err(fusion_tool_error(error));
        }
        let surface = executor.agent_surface();
        if !surface.enabled {
            let available = self.fusion_available_agents_display(is_coordinator).await;
            Self::emit_failed(
                bus,
                invocation_id,
                "agent_type_not_found",
                started.elapsed().as_millis() as u64,
            )
            .await;
            return Err(ToolError::InvalidInput(format!(
                "Agent type '{FUSION_AGENT_TYPE}' not found. Available agents: {available}"
            )));
        }

        // [round-5 review, finding 20] This `?` used to propagate with NO
        // terminal analytics event of any kind, unlike every sibling
        // rejection in this function — and, after round 4 put `emit_started`
        // ABOVE it, with a dangling `AGENT_STARTED` that no `AGENT_FAILED`
        // ever closed. `emit_started` now fires below, after every fallible
        // pre-dispatch gate; this gate emits only the failure, exactly like
        // `agent_type_denied` / `budget_exceeded` / `subagent_count_cap`.
        let Some(budget) = self.ctx.budget_enforcer.clone() else {
            Self::emit_failed(
                bus,
                invocation_id,
                "fusion_budget_enforcer_unwired",
                started.elapsed().as_millis() as u64,
            )
            .await;
            return Err(ToolError::Internal(
                "AgentTool: BudgetEnforcerHandle not wired into BuiltinToolContext".into(),
            ));
        };
        let origin_session_id = Self::origin_session_id(&ctx).await;
        let budget = Self::budget_for_origin(budget, origin_session_id);
        if let Err(BudgetError::Exceeded { current_nano_usd }) = budget.check_and_charge(0).await {
            Self::emit_failed(
                bus,
                invocation_id,
                "budget_exceeded",
                started.elapsed().as_millis() as u64,
            )
            .await;
            return Err(match budget.max_session_nano_usd() {
                Some(limit_nano_usd) => ToolError::InvalidInput(budget_limit_reached_error(
                    current_nano_usd,
                    limit_nano_usd,
                )),
                None => ToolError::Internal(format_budget_denied(current_nano_usd)),
            });
        }

        // [round-5 review, findings 17 & 20] Four reachable inputs reject
        // here — `cross_provider: true` against a surface that forbids it,
        // an unknown `preset`, a malformed `models` entry (`"openai:"`), and
        // an unresolvable parent profile. This used to be a bare `?`: no
        // `AGENT_FAILED` at all, and (after round 4's `emit_started` landed
        // above it) a start event that no terminal event ever closed, so a
        // dashboard joining started -> terminal on `invocation_id` showed
        // the invocation running forever and per-type failure counts
        // under-reported exactly the invalid-request class.
        let request = match fusion_request_from_agent(
            self.ctx.main_loop_model_profile_provider.as_ref(),
            &parsed,
            &ctx,
            surface,
            executor.as_ref(),
        ) {
            Ok(request) => request,
            Err(err) => {
                Self::emit_failed(
                    bus,
                    invocation_id,
                    "fusion_invalid_request",
                    started.elapsed().as_millis() as u64,
                )
                .await;
                return Err(err);
            }
        };
        // The run's session is `identity.session_id` below, read live
        // immediately before dispatch so a session switch earlier in this turn
        // cannot leave the run scoped to the wrong ledger.
        let parent_registry = ctx
            .subagent_registry
            .clone()
            .unwrap_or_else(|| Arc::new(tool_api::ToolRegistry::new()));
        let mut invoker_impl =
            tool_api::tool_invoker_impl::RegistryToolInvoker::new(parent_registry);
        if let Some(gate) = self.ctx.permission_gate.clone() {
            invoker_impl = invoker_impl.with_gate(gate);
        }
        let fusion_cancel = ctx.cancel.clone().unwrap_or_default();
        let inherit = FusionInheritance::new(
            SubagentInheritance {
                tool_invoker: Arc::new(invoker_impl),
                budget: budget.clone(),
            },
            fusion_cancel.clone(),
        );
        let identity = FusionRunIdentity::new(
            FusionRunId::generated(),
            origin_session_id,
            FusionOrigin::Agent,
            Some(invocation_id.to_string()),
        );
        let terminal_recorder = self.terminal_recorder_for(origin_session_id);
        let prepared = match executor.clone().prepare(FusionSubmission {
            request,
            inherit,
            identity: identity.clone(),
        }) {
            Ok(prepared) => prepared,
            Err(error) => {
                let control = FusionRunControl::new(
                    identity.clone(),
                    0,
                    fusion_cancel,
                    FusionRunFactsRecorder::default(),
                );
                let summary = FusionPreparedSummary {
                    identity,
                    duration_ms: 0,
                    planned_panels: None,
                };
                let prepared = PreparedFusionRun::failed(summary, control, error.clone());
                let prepared = if let Some(recorder) = terminal_recorder.as_ref() {
                    prepared
                        .with_terminal_capability(FusionTerminalCapability::new(recorder.clone()))
                } else {
                    prepared
                };
                let _ = prepared.activate(FusionActivation::now(), None).await;
                Self::emit_failed(
                    bus,
                    invocation_id,
                    "fusion_invalid_request",
                    started.elapsed().as_millis() as u64,
                )
                .await;
                return Err(fusion_tool_error(error));
            }
        };
        let prepared = if let Some(recorder) = terminal_recorder.as_ref() {
            prepared.with_terminal_capability(FusionTerminalCapability::new(recorder.clone()))
        } else {
            prepared
        };
        let prepared_summary = prepared.summary().clone();
        let prepared_control = prepared.control();
        let panel_n = prepared_summary
            .planned_panels
            .map(u64::from)
            .unwrap_or_else(|| u64::from(fusion_panel_count(&parsed, surface)));
        let cap = max_subagents_per_session();
        // [round-2 review, findings 4 & 13] `panel_stage_observed` must exist
        // BEFORE the reservation so the drop-safety guard below can be
        // constructed with it — see `FusionSpawnReservationGuard`'s doc
        // comment for why the release has to survive `call_fusion`'s own
        // future being dropped, not just an `Ok`/`Err` return.
        let panel_stage_observed = Arc::new(std::sync::atomic::AtomicU64::new(0));
        // [round-12 review, finding 3] Same lifetime requirement as
        // `panel_stage_observed`, for the allocation figure that tells the
        // resolved panel count from the panels a subagent provably exists for.
        let panels_allocated_observed = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let panels_allocated_figure_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut reservation_guard: Option<FusionSpawnReservationGuard> = None;
        if let Some(registry) = &self.ctx.task_registry {
            if let Err(spawned) = registry.try_reserve_total_agent_spawns(panel_n, cap) {
                Self::emit_failed(
                    bus,
                    invocation_id,
                    "subagent_count_cap",
                    started.elapsed().as_millis() as u64,
                )
                .await;
                return Err(ToolError::InvalidInput(format!(
                    "Subagent spawn limit reached ({spawned} of {cap} agents spawned). \
Complete the remaining work directly with your tools instead of spawning more agents. \
If more agents are genuinely needed, ask the user to raise {MAX_SUBAGENTS_PER_SESSION_ENV}."
                )));
            }
            reservation_guard = Some(FusionSpawnReservationGuard::new(
                Arc::clone(registry),
                panel_n,
                prepared_control.clone(),
                Arc::clone(&panel_stage_observed),
                Arc::clone(&panels_allocated_observed),
                Arc::clone(&panels_allocated_figure_seen),
            ));
        }

        // [round-4 review, finding 17; round-5 review, findings 17 & 20]
        // Every `call_fusion` return below this line pairs with exactly one
        // terminal event (`emit_completed` on `Ok`, `emit_failed` on `Err`),
        // and every return ABOVE it emits only `emit_failed` — so a
        // dashboard that joins started -> terminal on `invocation_id`, or
        // counts launches-by-type from `AGENT_STARTED`, never sees a Fusion
        // completion without a start, nor a start left open forever. This is
        // the same position the ordinary `call` dispatch uses: after every
        // pre-dispatch validation gate has cleared (round 4 put it ABOVE the
        // budget-enforcer lookup and `fusion_request_from_agent`, both of
        // which can still reject), immediately before the dispatch itself,
        // which from here on is infallible up to `executor.run`.
        Self::emit_started(
            bus,
            invocation_id,
            FUSION_AGENT_TYPE,
            parsed.prompt.chars().count(),
        )
        .await;

        let (prog_tx, mut prog_rx) = tokio::sync::mpsc::channel::<FusionProgress>(32);
        let forward_progress = progress.clone();
        // F008: `Cancelled` spans both sides of the panel-spawn boundary —
        // `FusionOrchestrator::run` returns it both from the pre-panel
        // `resolve_and_reserve`/biased-select window (zero provider calls)
        // and from a cancel observed mid-run (panels genuinely spawned).
        // The forwarder already sees every progress event on this channel,
        // so it is the cheapest place to learn which side of that boundary
        // a `Cancelled` fell on.
        //
        // [round-2 review, finding 13; round-3 review, findings 11/19]
        // `RunningPanels{completed:0,total:N}` is emitted by
        // `FusionOrchestrator::run_panel_stage` BEFORE it logs N telemetry
        // events and THEN calls `panel::run_panels` — so that event alone
        // does NOT mean a panel spawned. `panels_proven_spawned`
        // recognizes the two shapes that DO: `PanelsDispatched` (emitted by
        // `run_panels` itself, right after every panel task has been handed
        // to the spawner — covers the whole window up to the first
        // completion) and `RunningPanels` with `completed > 0` (a panel
        // could only complete after spawning).
        let panel_stage_observed_writer = panel_stage_observed.clone();
        let panels_allocated_writer = panels_allocated_observed.clone();
        let panels_allocated_figure_seen_writer = panels_allocated_figure_seen.clone();
        let forwarder = tokio::spawn(async move {
            while let Some(event) = prog_rx.recv().await {
                // [round-5 review, finding 10] Keep the LARGEST resolved
                // panel count any proving event carried — not just the fact
                // that one arrived. `PanelsDispatched { total }` and
                // `RunningPanels { completed > 0, total }` both publish the
                // count the model resolver actually settled on, which is
                // what tells the reservation's surplus (reserved at the
                // WANTED count, before resolution ran) from the panels that
                // genuinely exist.
                if let Some(resolved) = panels_proven_spawned(&event.stage) {
                    panel_stage_observed_writer
                        .fetch_max(resolved, std::sync::atomic::Ordering::Relaxed);
                }
                // [round-12 review, finding 3] Same max-latch, for the
                // strictly different question of how many panels the SPAWNER
                // allocated a child for. `panels_allocated` is monotonically
                // non-decreasing within a run, and the last
                // `RunningPanels{completed == total}` event is emitted after
                // every panel finished — hence after every allocation — so
                // the max equals the run's final allocated count.
                if let Some(allocated) = event.panels_allocated {
                    panels_allocated_figure_seen_writer
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    panels_allocated_writer
                        .fetch_max(u64::from(allocated), std::sync::atomic::Ordering::Relaxed);
                }
                let _ = forward_progress
                    .send(tool_api::progress::ToolProgress {
                        tool_use_id: lingxi_core::types::ToolUseId::new(),
                        data: serde_json::json!({
                            // F005: `subagent_activity` is the key
                            // `turn_loop.rs::forward_tool_progress` actually
                            // recognizes and renders — `fusion_stage` (kept
                            // below as an additive field for any consumer
                            // that wants the raw enum tag) was never read by
                            // that chokepoint, so every Fusion progress event
                            // was silently dropped end to end.
                            "subagent_activity": event.stage.label(),
                            "fusion_stage": fusion_stage_name(&event.stage),
                            "message": event.message,
                            "panel_id": event.panel_id,
                        }),
                    })
                    .await;
            }
        });

        let outcome = prepared
            .activate(FusionActivation::now(), Some(prog_tx))
            .await;
        let _ = forwarder.await;
        let facts = outcome.facts;
        match outcome.result {
            Ok(result) => {
                // F008: `panel_n` reserved the WANTED panel count before
                // model resolution ran; a successful run may have actually
                // spawned fewer (over-reservation is not trimmed at
                // resolution time — see F011). Release only the surplus that
                // never spawned, not the whole reservation.
                //
                // [round-4 review, finding 9 / round-6 review B2]
                // `result.panels` includes every slot the orchestrator
                // collected, INCLUDING panels that never reached the subagent
                // spawner at all — the ones `panel.rs` finished as
                // `error_category: "spawn"` (the spawner rejected them
                // pre-allocation) or `"not_dispatched"` (the bar/cancel
                // killed the slot before its task ever called the spawner).
                // Counting those as "spawned" under-releases the reservation
                // and permanently over-charges the session's lifetime spawn
                // quota for subagents that never existed. A prepared
                // executor's sealed allocation fact is stronger than these
                // compact categories: a panel can time out or panic before
                // its allocation receipt reaches the result. Keep the
                // category predicate only for legacy adapters with no exact
                // allocation fact.
                if let Some(guard) = reservation_guard.as_mut() {
                    let spawned = facts
                        .allocated_panels
                        .map(u64::from)
                        .unwrap_or_else(|| {
                            u64::try_from(fusion_panels_that_reached_the_spawner(&result.panels))
                                .unwrap_or(panel_n)
                        })
                        .min(panel_n);
                    let surplus = panel_n.saturating_sub(spawned);
                    guard.release(surplus);
                    // The remaining (spawned) count is deliberately kept
                    // charged — disarm so a later drop of THIS guard (at
                    // function return) does not act on it again.
                    guard.disarm();
                }
                if matches!(
                    &facts.attempt_settlement,
                    Some(lingxi_core::host::FusionAttemptSettlementStatus::Failed { .. })
                ) {
                    Self::emit_failed(
                        bus,
                        invocation_id,
                        "fusion_accounting_failed",
                        started.elapsed().as_millis() as u64,
                    )
                    .await;
                } else {
                    Self::emit_completed(
                        bus,
                        invocation_id,
                        started.elapsed().as_millis() as u64,
                        FUSION_AGENT_TYPE,
                    )
                    .await;
                }
                Ok(fusion_tool_result_with_settlement(
                    result,
                    facts.attempt_settlement.as_ref(),
                ))
            }
            Err(err) => {
                // F008: only a PREFLIGHT error guarantees zero provider
                // calls — release the full `panel_n` reservation for those.
                // Every other variant means panels genuinely ran (the error
                // itself carries no panel count to trim by), so the
                // reservation stays charged; a repeated failing Fusion run
                // still advances `LINGXI_MAX_SUBAGENTS_PER_SESSION`
                // instead of being free to retry forever.
                //
                // `Cancelled` is the one variant that spans BOTH sides of
                // that boundary: `FusionOrchestrator::run` returns it from
                // `resolve_and_reserve` (after model resolution, before
                // `run_panel_stage`) and from the biased `cancel.cancelled()`
                // select arm when the token was already cancelled at entry —
                // both strictly pre-panel — as well as from a cancel that
                // lands after panels have genuinely spawned. Treating it as
                // always-preflight would refund panels that really ran (a
                // free retry); treating it as never-preflight (the prior
                // behaviour) permanently leaks `panel_n` reservations for
                // agents that never existed on every pre-panel cancel. Use
                // whether the forwarder ever observed a progress event that
                // `panels_proven_spawned` recognizes as proof a panel
                // task genuinely reached the spawner to tell the two cases
                // apart.
                let resolved_panels = facts.resolved_panels.map(u64::from).unwrap_or_else(|| {
                    panel_stage_observed.load(std::sync::atomic::Ordering::Relaxed)
                });
                let allocated_panels = facts.allocated_panels.map(u64::from).unwrap_or_else(|| {
                    panels_allocated_observed.load(std::sync::atomic::Ordering::Relaxed)
                });
                let allocated_panels_observed = facts.allocated_panels.is_some()
                    || panels_allocated_figure_seen.load(std::sync::atomic::Ordering::Relaxed);
                let has_allocated_fact = allocated_panels_observed && allocated_panels > 0;
                let releases_full_reservation = fusion_error_is_preflight(&err)
                    || (matches!(err, lingxi_core::host::FusionError::Cancelled)
                        && resolved_panels == 0
                        && !has_allocated_fact);
                if let Some(guard) = reservation_guard.as_mut() {
                    if releases_full_reservation {
                        guard.release(panel_n);
                    } else if resolved_panels > 0 || has_allocated_fact {
                        // [round-5 review, finding 10] The panels genuinely
                        // ran, so the reservation stays charged — but only
                        // for the panels that genuinely EXIST. `panel_n` was
                        // reserved at the WANTED count before model
                        // resolution ran; the progress events the forwarder
                        // already inspects carry the count the resolver
                        // settled on, so refund the same
                        // reserved-but-never-dispatched surplus the `Ok` arm
                        // above refunds. Without this, two outcomes of an
                        // identical 2-of-3 resolution accounted differently
                        // and every runtime failure permanently narrowed
                        // `LINGXI_MAX_SUBAGENTS_PER_SESSION` by the
                        // surplus.
                        //
                        // [round-12 review, finding 3] …and only for the
                        // panels a subagent PROVABLY exists for. The
                        // resolved count includes panels the spawner
                        // rejected pre-allocation, which the `Ok` arm above
                        // filters out with
                        // `fusion_panels_that_reached_the_spawner`; charging
                        // them here made an identical 3-panel dispatch bill
                        // 3 slots on a failure and 2 on a success. See
                        // [`allocation_capped_charge`] for why a missing
                        // figure falls back to the resolved count while an
                        // explicit Some(0) refunds every unresolved slot.
                        // A synchronous allocation receipt can arrive before
                        // the orchestrator publishes a resolved-panel stage;
                        // use the observed allocation as the minimum basis in
                        // that narrow outer-cancel window.
                        let charge_basis = resolved_panels.max(allocated_panels);
                        guard.release(panel_n.saturating_sub(allocation_capped_charge(
                            charge_basis,
                            allocated_panels,
                            allocated_panels_observed,
                        )));
                    }
                    // Either a release above just settled it, or the
                    // reservation stays deliberately charged — disarm so a
                    // later drop of THIS guard does not act on it again.
                    guard.disarm();
                }
                Self::emit_failed(
                    bus,
                    invocation_id,
                    "fusion_failed",
                    started.elapsed().as_millis() as u64,
                )
                .await;
                // BudgetExceeded carries no data of its own — format it with
                // the SAME live numbers (current/limit nano-USD) the
                // pre-spawn budget check above already uses, rather than
                // `fusion_tool_error`'s generic `other.to_string()` fallback.
                if matches!(err, lingxi_core::host::FusionError::BudgetExceeded) {
                    let current_nano_usd = budget.snapshot_total_nano_usd().await;
                    return Err(match budget.max_session_nano_usd() {
                        Some(limit_nano_usd) => ToolError::InvalidInput(
                            budget_limit_reached_error(current_nano_usd, limit_nano_usd),
                        ),
                        None => ToolError::InvalidInput(err.to_string()),
                    });
                }
                Err(fusion_tool_error(err))
            }
        }
    }

    /// Format one agent catalog line for the tool prompt, matching claude-code's
    /// Build the dynamic Agent tool prompt, porting claude-code v2.1.193's
    /// `getPrompt` (binary `bin/claude.exe` offset ~208233723). 2.1.193 ALWAYS
    /// externalizes the agent catalog to a per-turn `agent_listing_delta`
    /// `<system-reminder>` attachment (built by the orchestrator,
    /// [`crate::PoolSubagentSpawner`]-fed); the tool DESCRIPTION carries only the
    /// static pointer line "Available agent types are listed in <system-reminder>
    /// messages in the conversation." — there is NO inline-catalog variant in the
    /// description. The catalog always travels in the per-turn reminder.
    ///
    /// The body is MODEL-GATED, exactly as the binary's `if(m){…}` split
    /// (`m = qk(model)` = `tool_api::dh_simple_system_prompt`): a lean-prompt
    /// model gets the SHORT form (intro + `## When to use` + four terse
    /// bullets); every classic model (sonnet / haiku / `claude-3-*` /
    /// `opus-4-0..4-7`) — and an unknown/absent model, since `Dh(undefined)` is
    /// `false` — gets the LONG form: `## When not to use`, `## Usage notes`,
    /// `## Writing the prompt`, and the `Example usage:` `<example>` blocks.
    ///
    /// `is_coordinator` selects the slim coordinator prompt (intro only; the
    /// coordinator system prompt already covers usage / examples).
    ///
    /// The `d` pro-plan gate (`vi()==="pro"`) IS modeled: a `pro` subscription
    /// (read from the process-global [`lingxi_core::host::subscription::is_pro_plan`]) injects
    /// the "Do not spawn agents unless the user asks" block after the catalog
    /// pointer line and suppresses `## When to use`. It is inert until a
    /// composition root resolves the plan via
    /// [`lingxi_core::host::subscription::set_current_subscription`] (subscription resolution
    /// may be unwired ⇒ `None` ⇒ no block, matching the binary's unknown-plan
    /// default).
    ///
    /// The `o` fork-subagent gate (`isForkSubagentEnabled` / 2.1.232 `SPe`) IS
    /// modeled: when fork is enabled (`is_fork_subagent_enabled(is_coordinator,
    /// is_non_interactive)`, reading the process-global
    /// [`lingxi_core::host::session_flags::is_non_interactive_session`]), the subagent_type
    /// sentence explains `"fork"`, a fork addendum follows `## When to use`, and
    /// the SendMessage bullet gains the `(except subagent_type: "fork", …)`
    /// qualifier. Default ON for interactive non-coordinator sessions; set
    /// `LINGXI_FORK_SUBAGENT=0` to disable.
    ///
    /// Deferred vs binary (no behavioral surface here): the embedded-grep hint
    /// swap (`S = VH()&&Sh()?…`, which only fires in sessions where Glob/Grep
    /// are REMOVED — LingXi always registers them), teammate (`yB`/`_m`) notes,
    /// and the remote-isolation (`V8t`) note. The
    /// fabricated "# MCP Servers" note (NOT present in the 2.1.193 binary) is
    /// removed; `_mcp_server_names` is retained for signature stability.
    ///
    /// `model` is the binary's `CGf({model:e,…})` argument, consumed by the
    /// LEAN/LONG gate `m = qk(e)` (`tool_api::dh_simple_system_prompt`);
    /// `general_purpose_available` is 2.1.238's `generalPurposeAvailable`
    /// (`p7f(agents, allowedAgentTypes)`), computed by the caller from the
    /// PRE-deny-filter listing exactly as the binary's `prompt({agents,…})`
    /// wrapper does (@292885292).
    fn build_prompt(
        agents: &[lingxi_core::host::subagent_spawn::SubagentListingEntry],
        mcp_server_names: &[String],
        is_coordinator: bool,
        model: Option<&str>,
        general_purpose_available: bool,
    ) -> String {
        // Binary `l=!DT()&&!aZ()` @234652470.
        //   `DT()` @230660586 = `return Z.CLAUDE_CODE_DISABLE_BACKGROUND_TASKS`
        //     → the port's `LINGXI_DISABLE_BACKGROUND_TASKS`, read the same way
        //     as the dispatch-site gate at the bottom of `call`.
        //   `aZ()` @227950071 = `a6i.getStore()!==void 0`, an in-process-SDK
        //     `AsyncLocalStorage` probe. LingXi has no in-process SDK host
        //     (CLI/TUI/desktop are always out-of-process), so it is modeled as
        //     a constant `false` — matching the binary's CLI/TUI default.
        // ⇒ `l` is TRUE unless the kill-switch env is set.
        let async_agents_available = !lingxi_core::host::env::is_env_truthy(
            std::env::var("LINGXI_DISABLE_BACKGROUND_TASKS")
                .ok()
                .as_deref(),
        );
        Self::build_prompt_with_async_agents(
            agents,
            mcp_server_names,
            is_coordinator,
            async_agents_available,
            model,
            general_purpose_available,
        )
    }

    /// `build_prompt` with the binary's `l` gate supplied explicitly, so tests
    /// can exercise both arms without touching the process-global env.
    fn build_prompt_with_async_agents(
        agents: &[lingxi_core::host::subagent_spawn::SubagentListingEntry],
        _mcp_server_names: &[String],
        is_coordinator: bool,
        async_agents_available: bool,
        model: Option<&str>,
        general_purpose_available: bool,
    ) -> String {
        // `re = PC({model, leanPrompt})` — the LEAN-prompt flag, computed ONCE
        // and used for the SHORT/LONG arm split below, exactly as `H2n` does.
        let lean = tool_api::dh_simple_system_prompt(model);
        // Catalog placement (binary intro `p`): the 2.1.193 default externalizes
        // the catalog to the orchestrator's `<system-reminder>` attachment, so the
        // description carries only the static pointer line.
        //
        // [round-3 review, finding 6] The externalized catalog is built by
        // `agent_listing_reminder_message` from `builtin_agent_definitions()` +
        // the disk catalog — NEITHER of which knows about the synthetic
        // `fusion` entry `append_fusion_listing` adds to `agents` (that
        // reminder path lives in a different crate and has no seam onto the
        // Agent tool's Fusion state). So under the DEFAULT gate (this arm),
        // the pointer-only text above used to be the model's ENTIRE Fusion
        // signal — which is to say none at all: the reminder catalog never
        // carries `fusion`, so a stock session never sees its `when_to_use`
        // guidance or the "about 4-5x the cost" warning, no matter how many
        // turns pass. `agents` (built by the caller, `Self::prompt`) already
        // carries the synthetic entry whenever the Fusion surface is enabled
        // and an executor is wired — the exact same gate `append_fusion_listing`
        // itself enforces — and already reflects any `Agent(fusion)` deny rule
        // (the caller's deny-filter `retain` runs after the append). Fold that
        // one entry's guidance directly into the pointer line here instead of
        // relying on the reminder catalog to carry it, so the entrypoint stays
        // discoverable under stock settings without depending on a cross-crate
        // wire this file cannot reach.
        let fusion_notice = agents
            .iter()
            .find(|a| a.agent_type == FUSION_AGENT_TYPE)
            .map(|entry| {
                format!(
                    " `{}` is also available: {}",
                    entry.agent_type, entry.when_to_use
                )
            });
        let agent_list_section = format!(
            "Available agent types are listed in <system-reminder> messages in the conversation.{}",
            fusion_notice.unwrap_or_default()
        );

        // Pro-plan gate `d` (binary `d=vi()==="pro"?<block>:""`): on the `pro`
        // plan, discourage spawning. Read from the process-global subscription
        // (the port's `vi()` analog); `None`/non-pro ⇒ empty (the common case,
        // since subscription resolution may be unwired ⇒ gate inert, matching the
        // binary's unknown-plan default). The block is injected right after the
        // catalog pointer line, and (below) it SUPPRESSES the `## When to use`
        // section — both per the binary's `${d}` / `${d?"":…}` placements.
        let pro_block = if lingxi_core::host::subscription::is_pro_plan() {
            // Binary `d=Pi()==="pro"?`\n\n**Do not spawn…`:""` — DOUBLE leading `\n`
            // (injected as `…conversation.${d}\n\n${subagent}`; od -c verified on
            // 2.1.195 @211615145).
            "\n\n**Do not spawn agents unless the user asks.** Each spawn starts cold and re-derives context you already have — it's the expensive path on this plan. A task with \"multiple angles,\" \"thorough,\" or several parts is not a request to spawn; handle it inline with your own tools. Only use this tool when the user explicitly says to use a subagent, or names one of the available agent types."
        } else {
            ""
        };

        // Fork-subagent gate `o` (binary `o=isForkSubagentEnabled()`): when fork
        // is enabled, the subagent_type sentence explains the `"fork"` type, the
        // body gains a fork addendum, and the SendMessage bullet notes the fork
        // exception. The gate is `is_fork_subagent_enabled(is_coordinator,
        // is_non_interactive)` — env `LINGXI_FORK_SUBAGENT` / alias falsy forces
        // OFF, env truthy forces ON, and an unset env defaults ON for interactive
        // non-coordinator sessions. The non-interactive flag is read from the
        // process-global session flag (the port's `getIsNonInteractiveSession()`
        // analog, set by `ConversationOrchestrator::new`).
        let is_fork = lingxi_core::host::fork_subagent::is_fork_subagent_enabled(
            is_coordinator,
            lingxi_core::host::session_flags::effective_non_interactive_session(),
        );

        // Subagent_type sentence — fork variant (binary `${o?…:…}`).
        //
        // 2.1.238 threads a NEW `generalPurposeAvailable` (`n`) argument into
        // BOTH arms (@292433588): when the general-purpose agent is not in the
        // catalog, the "omit it" fallback is replaced by `Gri`
        // ([`SUBAGENT_TYPE_REQUIRED_MESSAGE`]) + ", so choose … one of the
        // listed agent types." 2.1.220's non-fork arm was an unconditional
        // literal ending "If omitted, the general-purpose agent is used.".
        //   `s = `${Gri}, so choose ${i?'`"fork"` or ':""}one of the listed agent types.``
        let agent_type_required_hint = format!(
            "{SUBAGENT_TYPE_REQUIRED_MESSAGE}, so choose {}one of the listed agent types.",
            if is_fork { "`\"fork\"` or " } else { "" }
        );
        let subagent_sentence = if is_fork {
            let tail = if general_purpose_available {
                "any other type — or omitting it — starts a fresh agent (general-purpose by default)."
                    .to_string()
            } else {
                format!("any other type starts a fresh agent. {agent_type_required_hint}")
            };
            format!(
                "When using the {AGENT_TOOL_NAME} tool, specify a subagent_type to select an agent: `\"fork\"` forks yourself (the fork inherits your full conversation context and always runs on your model — a `model` override is ignored); {tail}"
            )
        } else {
            let tail = if general_purpose_available {
                "If omitted, the general-purpose agent is used.".to_string()
            } else {
                agent_type_required_hint.clone()
            };
            format!(
                "When using the {AGENT_TOOL_NAME} tool, specify a subagent_type parameter to select which agent type to use. {tail}"
            )
        };

        // Intro `p` (binary JS source @211615462): the catalog line sits between
        // the two intro sentences (with the `${d}` pro-block appended to it); the
        // subagent_type sentence closes it.
        // Binary `p`: `…available to it.\n\nAvailable agent types…conversation.${d}\n\n${subagent}`
        // — DOUBLE `\n` separators around the catalog line + `${d}` pro-block
        // (od -c verified on 2.1.195; an earlier pass misread the `strings` dump
        // as single `\n`).
        let intro = format!(
            "Launch a new agent to handle complex, multi-step tasks. Each agent type has specific capabilities and tools available to it.\n\n\
{agent_list_section}{pro_block}\n\n\
{subagent_sentence}"
        );

        // Coordinator mode gets the slim intro only (binary `if(t)return p`).
        if is_coordinator {
            return intro;
        }

        // Steer gate `g = DZ()==="default"` (binary @292441984 / the `${g?…:…}`
        // arms). Already ported as
        // `lingxi_core::host::live_sessions::subagent_steer_is_default` and consumed by the
        // system-prompt bullet at `orchestrator/src/prompt/body_sections.rs`;
        // the Agent tool prompt consults the SAME gate.
        let steer_is_default = lingxi_core::host::live_sessions::subagent_steer_is_default();

        // LEAN/LONG split (binary `m = qk(e)`, the shared `Dh(model)` gate the
        // port already implements as `tool_api::dh_simple_system_prompt`):
        // `if(m){ …SHORT… } return …LONG…`. Every other ported tool consults it
        // through `PromptOptions.model` (Read/Bash/Glob/Grep/Write/TodoWrite);
        // the Agent tool used to discard `PromptOptions` entirely and always
        // render the SHORT arm, so a sonnet / haiku / `claude-3-*` /
        // `opus-4-0..4-7` session never saw `## When not to use`,
        // `## Usage notes`, `## Writing the prompt`, or the `<example>` blocks.
        if lean {
            // ---- SHORT (lean) arm — binary `if(m){…}` ----
            //
            // `## When to use` + four terse bullets. NO `## When not to use`, NO
            // `## Usage notes`, NO `<example>`s (those are the LONG form).
            // Em-dashes are U+2014. The teammate suppressions (binary `C =
            // kae()?…:sS()?…:""`) and the remote isolation bullet (`k =
            // qJn()?…:""`, gate `tengu_neapolitan`) are gated OFF by default and
            // remain unmodeled.
            //
            // `## When to use` is SUPPRESSED on the pro plan (binary
            // `${y?"":g?…:…}`): when the pro-block is present, the
            // discouragement replaces the when-to-use guidance. When the steer
            // is NOT `"default"` the heading survives but the "Reach for this
            // when…" lead sentence is dropped and `R` stands alone. The four
            // bullets are NOT gated and always render.
            const WHEN_TO_USE_SEARCH_DIRECTLY: &str = "For a single-fact lookup where you already know the file, symbol, or value, search directly. Once you've delegated a search, don't also run it yourself — wait for the result.";
            let when_to_use = if !pro_block.is_empty() {
                String::new()
            } else if steer_is_default {
                // Binary `${g?`\n\n## When to use\n\nReach…`}` — DOUBLE `\n`
                // before the heading AND before the paragraph (od -c verified on
                // 2.1.195).
                format!(
                    "\n\n## When to use\n\nReach for this when the task matches an available agent type, when you have independent work to run in parallel, or when answering would mean reading across several files — delegate it and you keep the conclusion, not the file dumps. {WHEN_TO_USE_SEARCH_DIRECTLY}"
                )
            } else {
                format!("\n\n## When to use\n\n{WHEN_TO_USE_SEARCH_DIRECTLY}")
            };
            // Fork addendum (binary `${A}`), after `## When to use`, before the
            // bullets.
            let fork_addendum = if is_fork {
                // Binary `A=i?`\n\nA fork runs…`:""` — DOUBLE leading `\n` (od -c
                // verified on 2.1.195). 2.1.238 (@292444519) appends the two
                // "Subagents run in the background" / "Never fabricate…"
                // sentences that 2.1.220 (@234658240) did not carry.
                "\n\nA fork runs in the background and keeps its tool output out of your context. If you are the fork, execute directly — don't re-delegate. Subagents run in the background; you'll be notified when one completes. Never fabricate or predict a pending agent's results — the notification is never something you write yourself; if the user asks before it arrives, say it's still running."
            } else {
                ""
            };
            // SendMessage bullet — fork qualifier (binary `${i?' (except …)':""}`).
            let send_message_bullet = if is_fork {
                format!(
                    "- Use SendMessage with the agent's ID or name to continue a previously spawned agent with its context intact; a new {AGENT_TOOL_NAME} call starts fresh (except subagent_type: \"fork\", which inherits your context)."
                )
            } else {
                format!(
                    "- Use SendMessage with the agent's ID or name to continue a previously spawned agent with its context intact; a new {AGENT_TOOL_NAME} call starts fresh."
                )
            };
            // Binary: `…re-delegate.`:""}\n\n- The agent's final message…\n- Use…`
            // — DOUBLE `\n` before the FIRST bullet, SINGLE `\n` between subsequent
            // bullets (od -c verified on 2.1.195).
            //
            // The agent-definition bullet is an unconditional fixed literal between
            // the SendMessage bullet and the isolation bullet. Ported verbatim
            // except the path is rebranded `.claude/agents/*.md` →
            // `.lingxi/agents/*.md` per the accepted .lingxi naming divergence.
            //
            // FIRST tail bullet — binary @234659244 is a TERNARY on `u`:
            //   `- ${u?"The agent's final report is not shown to the user —
            //         relay what matters.":"The agent's final message is returned
            //         to you as the tool result; it is not shown to the user —
            //         relay what matters."}`
            let final_report_bullet = if async_agents_available {
                "- The agent's final report is not shown to the user — relay what matters."
            } else {
                "- The agent's final message is returned to you as the tool result; it is not shown to the user — relay what matters."
            };
            // BACKGROUND bullet — binary `w = u&&!i ? (o ? <SHORT> : <LONG>) : ""`
            // where `o` is the fork FEATURE flag (`z1e()`) and `i = o && forkAvailable`
            // is the port's `is_fork`. The port has no separate `o`, so the
            // `o=true && i=false` SHORT arm ("Subagents run in the background; …",
            // no `run_in_background: false` sentence) is UNMODELED — unreachable
            // while `is_fork` implies the feature is on. `u=false` OR `is_fork`
            // ⇒ the bullet is dropped entirely, not shortened.
            let background_bullet = if async_agents_available && !is_fork {
                "\n- Subagents run in the background by default; you'll be notified when one completes. Pass `run_in_background: false` only when your very next action depends on the result and nothing else could usefully happen while it runs — otherwise background it so the user can interject. Never fabricate or predict a pending agent's results — the notification is never something you write yourself; if the user asks before it arrives, say it's still running."
            } else {
                ""
            };
            return format!(
                "{intro}{when_to_use}{fork_addendum}\n\n\
{final_report_bullet}\n\
{send_message_bullet}\n\
- Each agent type's model, reasoning effort, and tools come from its definition (`.lingxi/agents/*.md` frontmatter or SDK `agents`).\n\
- `isolation: \"worktree\"` gives the agent its own git worktree (auto-cleaned if unchanged).{background_bullet}"
            );
        }

        // ---- LONG (standard) arm — binary `CGf`'s fall-through `return` ----

        // `v` — `## When not to use`, emitted ONLY on this arm and only when
        // fork is OFF (`v = i ? "" : …`).
        //
        // The binary's search-tool hint is
        // `S = VH()&&Sh() ? "`grep` via the Bash tool" : `the ${Am} tool``
        // (`Am = "Grep"`, @286224107). `VH()&&Sh()` is the EMBEDDED-GREP swap:
        // when it holds, `FNt()` (@287931338) returns `new Set([Glob, Grep])` —
        // i.e. those two tools are REMOVED from the session and `grep` is run
        // through Bash instead. LingXi registers Grep (and Glob) and has no
        // embedded-grep path, so the port takes the `${Am}` arm — "the Grep
        // tool" — which is the truthful hint for the port's own tool set.
        let when_not_to_use = if is_fork {
            ""
        } else {
            "\n## When not to use\n\nIf the target is already known, use the direct tool: Read for a known path, the Grep tool for a specific symbol or string. Reserve this tool for open-ended questions that span the codebase, or tasks that match an available agent type.\n"
        };

        // `- ${u?…:…}` — the async-agents arm of the final-report note.
        let final_report_note = if async_agents_available {
            "When the agent is done, its final report is not visible to the user. To show the user the result, you should send a text message back to the user with a concise summary of the result."
        } else {
            "When the agent is done, it will return a single message back to you. The result returned by the agent is not visible to the user. To show the user the result, you should send a text message back to the user with a concise summary of the result."
        };

        // `${u&&!o?…:""}` — the background / foreground-vs-background pair.
        // `o` is the fork FEATURE flag; the port has no separate `o` (see the
        // SHORT-arm note above), so `is_fork` stands in for both `o` and `i`.
        let background_notes = if async_agents_available && !is_fork {
            "\n- Agents run in the background by default. When an agent runs in the background, you will be automatically notified when it completes — do NOT sleep, poll, or proactively check on its progress. Continue with other work or respond to the user instead.\n- **Foreground vs background**: Pass `run_in_background: false` only when your very next action depends on the agent's result and nothing else could usefully happen while it runs — e.g., a research agent whose finding gates the edit you're about to make. Otherwise let it run in the background (the default) — this includes fire-and-forget work, independent investigations, and anything where the user might hand you something else in the meantime. Wanting the result \"next\" is not enough on its own."
        } else {
            ""
        };
        // `${u&&!i?…:""}` — the "Don't race" bullet.
        let dont_race_note = if async_agents_available && !is_fork {
            "\n- **Don't race**: after launching a background agent, you know nothing about its results. Never fabricate or predict them in any format — not as prose, summary, or structured output. The completion notification arrives in a later turn; it is never something you write yourself. If the user asks before it lands, say the agent is still running — give status, not a guess."
        } else {
            ""
        };
        // `${i?' (except subagent_type: "fork")':""}`.
        let fork_memory_qualifier = if is_fork {
            " (except subagent_type: \"fork\")"
        } else {
            ""
        };
        // `${g?…:""}` — the proactive-use / run-in-parallel pair, gated on the
        // subagent steer being `"default"`.
        let proactive_notes = if steer_is_default {
            format!(
                "\n- If the agent description mentions that it should be used proactively, then you should try your best to use it without the user having to ask for it first.\n- If the user specifies that they want you to run agents \"in parallel\", you MUST send a single message with multiple {AGENT_TOOL_NAME} tool use content blocks. For example, if you need to launch both a build-validator agent and a test-runner agent in parallel, send a single message with both tool calls."
            )
        } else {
            String::new()
        };
        // `a` — the fork-only `## When to fork` section (binary `a=i?…:""`).
        let when_to_fork = if is_fork {
            "\n\n## When to fork\n\nFork yourself (pass `subagent_type: \"fork\"`) when the intermediate tool output isn't worth keeping in your context. The criterion is qualitative — \"will I need this output again\" — not task size. Fork open-ended questions. If research can be broken into independent questions, launch parallel forks in one message. A fork beats a fresh subagent for this — it inherits context and shares your cache.\n\nForks are cheap because they share your prompt cache.\n\n**Don't peek.** The tool result includes an `output_file` path — do not Read or tail it. You get a completion notification; trust it. Reading the transcript mid-flight pulls the fork's tool noise into your context, which defeats the point of forking.\n\n**Don't race.** After launching, you know nothing about what the fork found. Never fabricate or predict fork results in any format — not as prose, summary, or structured output. The notification arrives as a user-role message in a later turn; it is never something you write yourself. If the user asks a follow-up before the notification lands, tell them the fork is still running — give status, not a guess.\n\n**Writing a fork prompt.** Since the fork inherits your context, the prompt is a *directive* — what to do, not what the situation is. Be specific about scope: what's in, what's out, what another agent is handling. Don't re-explain background.\n"
        } else {
            ""
        };
        // `l` — the UNCONDITIONAL `## Writing the prompt` section.
        let writing_the_prompt = format!(
            "\n\n## Writing the prompt\n\n{}Brief the agent like a smart colleague who just walked into the room — it hasn't seen this conversation, doesn't know what you've tried, doesn't understand why this task matters.\n\
- Explain what you're trying to accomplish and why.\n\
- Describe what you've already learned or ruled out.\n\
- Give enough context about the surrounding problem that the agent can make judgment calls rather than just following a narrow instruction.\n\
- If you need a short response, say so (\"report in under 200 words\").\n\
- Lookups: hand over the exact command. Investigations: hand over the question — prescribed steps become dead weight when the premise is wrong.\n\n\
{} command-style prompts produce shallow, generic work.\n\n\
**Never delegate understanding.** Don't write \"based on your findings, fix the bug\" or \"based on the research, implement it.\" Those phrases push synthesis onto the agent instead of doing it yourself. Write prompts that prove you understood: include file paths, line numbers, what specifically to change.",
            if is_fork {
                "Any agent other than a fork starts with zero context. "
            } else {
                ""
            },
            if is_fork {
                "For fresh agents, terse"
            } else {
                "Terse"
            }
        );

        // `${a.CLAUDE_CODE_SUBAGENT_MODEL_FORCE?"":"; the `model` parameter here
        // overrides the definition for this one call"}` (@3569298) — when the
        // deployment pins the subagent model, the `model` argument is gone from
        // the advertised schema, so the bullet must not promise it.
        let model_override_clause = if subagent_model_forced() {
            ""
        } else {
            "; the `model` parameter here overrides the definition for this one call"
        };

        // `d` — the code-reviewer example, shared by both `Example usage:` blocks.
        let code_reviewer_example = format!(
            "<example>\n\
user: \"Can you get a second opinion on whether this migration is safe?\"\n\
assistant: <thinking>I'll ask the code-reviewer agent — it won't see my analysis, so it can give an independent read.</thinking>\n\
{AGENT_TOOL_NAME}({{\n  \
description: \"Independent migration review\",\n  \
subagent_type: \"code-reviewer\",\n  \
prompt: \"{EXAMPLE_MIGRATION_REVIEW_PROMPT}\"\n\
}})\n\
<commentary>\n\
The agent starts with no context from this conversation, so the prompt briefs it: what to assess, the relevant background, and what form the answer should take.\n\
</commentary>\n\
</example>\n"
        );
        // `f`'s leading example — dropped entirely when general-purpose is
        // unavailable (`${!n?"":u?…:…}`), because it omits `subagent_type`.
        let general_purpose_example = if !general_purpose_available {
            String::new()
        } else if async_agents_available {
            format!(
                "<example>\n\
user: \"What's left on this branch before we can ship?\"\n\
assistant: <thinking>A survey question across git state, tests, and config. I'll delegate it and ask for a short report so the raw command output stays out of my context.</thinking>\n\
{AGENT_TOOL_NAME}({{\n  \
description: \"Branch ship-readiness audit\",\n  \
prompt: \"{EXAMPLE_SHIP_AUDIT_PROMPT}\"\n\
}})\n\
assistant: Ship-readiness audit running in the background.\n\
<commentary>\n\
The prompt is self-contained: it states the goal, lists what to check, and caps the response length. The agent runs in the background (the default), so the turn ends here — nothing about its findings is known yet. The report arrives in a SEPARATE turn, as a completion notification from outside; it is never something you write yourself.\n\
</commentary>\n\
[later turn — notification arrives as user message]\n\
assistant: Audit's back. Three blockers: no tests for the new prompt path, GrowthBook gate wired but not in build_flags.yaml, and one uncommitted file.\n\
</example>\n\n\
<example>\n\
user: \"so is the gate wired up or not\"\n\
<commentary>\n\
User asks mid-wait. The audit was launched to answer exactly this, and it hasn't returned. Give status, not a fabricated result.\n\
</commentary>\n\
assistant: Still waiting on the audit — that's one of the things it's checking. Should land shortly.\n\
</example>\n\n"
            )
        } else {
            format!(
                "<example>\n\
user: \"What's left on this branch before we can ship?\"\n\
assistant: <thinking>A survey question across git state, tests, and config. I'll delegate it and ask for a short report so the raw command output stays out of my context.</thinking>\n\
{AGENT_TOOL_NAME}({{\n  \
description: \"Branch ship-readiness audit\",\n  \
prompt: \"{EXAMPLE_SHIP_AUDIT_PROMPT}\"\n\
}})\n\
<commentary>\n\
The prompt is self-contained: it states the goal, lists what to check, and caps the response length. The agent's report comes back as the tool result; relay the findings to the user.\n\
</commentary>\n\
</example>\n\n"
            )
        };
        // `${i?c:f}` — the fork example block replaces the whole non-fork one.
        let examples = if is_fork {
            format!(
                "Example usage:\n\n\
<example>\n\
user: \"What's left on this branch before we can ship?\"\n\
assistant: <thinking>Forking this — it's a survey question. I want the punch list, not the git output in my context.</thinking>\n\
{AGENT_TOOL_NAME}({{\n  \
subagent_type: \"fork\",\n  \
name: \"ship-audit\",\n  \
description: \"Branch ship-readiness audit\",\n  \
prompt: \"{EXAMPLE_SHIP_AUDIT_PROMPT}\"\n\
}})\n\
assistant: Ship-readiness audit running.\n\
<commentary>\n\
Turn ends here. The coordinator knows nothing about the findings yet. What follows is a SEPARATE turn — the notification arrives from outside, as a user-role message. It is not something the coordinator writes.\n\
</commentary>\n\
[later turn — notification arrives as user message]\n\
assistant: Audit's back. Three blockers: no tests for the new prompt path, GrowthBook gate wired but not in build_flags.yaml, and one uncommitted file.\n\
</example>\n\n\
<example>\n\
user: \"so is the gate wired up or not\"\n\
<commentary>\n\
User asks mid-wait. The audit fork was launched to answer exactly this, and it hasn't returned. The coordinator does not have this answer. Give status, not a fabricated result.\n\
</commentary>\n\
assistant: Still waiting on the audit — that's one of the things it's checking. Should land shortly.\n\
</example>\n\n\
<example>\n\
user: \"Can you get a second opinion on whether this migration is safe?\"\n\
assistant: <thinking>I'll ask the code-reviewer agent — it won't see my analysis, so it can give an independent read.</thinking>\n\
<commentary>\n\
A non-fork subagent_type is specified, so the agent starts fresh. It needs full context in the prompt. The briefing explains what to assess and why.\n\
</commentary>\n\
{AGENT_TOOL_NAME}({{\n  \
name: \"migration-review\",\n  \
description: \"Independent migration review\",\n  \
subagent_type: \"code-reviewer\",\n  \
prompt: \"{EXAMPLE_MIGRATION_REVIEW_PROMPT}\"\n\
}})\n\
</example>\n"
            )
        } else {
            format!("Example usage:\n\n{general_purpose_example}{code_reviewer_example}")
        };

        // Binary tail: `${_}\n${v}\n## Usage notes\n\n- …${k}${C}${a}${l}\n\n${i?c:f}`.
        // `k` (remote CCR isolation, gate `qJn()`/`tengu_neapolitan`) is EXCLUDED
        // (remote/cloud surface); `C` (the in-process-SDK `kae()` / teammate
        // `sS()` parameter-suppression note) is inert in the port — `kae()` is a
        // constant `false` and the teammate path is frozen.
        format!(
            "{intro}\n{when_not_to_use}\n## Usage notes\n\n\
- Always include a short description summarizing what the agent will do\n\
- {final_report_note}\n\
- Trust but verify: an agent's summary describes what it intended to do, not necessarily what it did. When an agent writes or edits code, check the actual changes before reporting the work as done.{background_notes}{dont_race_note}\n\
- To continue a previously spawned agent, use SendMessage with the agent's ID or name as the `to` field — that resumes it with full context. A new {AGENT_TOOL_NAME} call starts a fresh agent with no memory of prior runs{fork_memory_qualifier}, so the prompt must be self-contained.\n\
- Each agent type's model, reasoning effort, and tool access are set in its definition (`.lingxi/agents/*.md` frontmatter, or the SDK `agents` option){model_override_clause}.\n\
- Clearly tell the agent whether you expect it to write code or just to do research (search, file reads, web fetches, etc.), since a fresh agent is not aware of the user's intent{proactive_notes}\n\
- With `isolation: \"worktree\"`, the worktree is automatically cleaned up if the agent makes no changes; otherwise the path and branch are returned in the result.{when_to_fork}{writing_the_prompt}\n\n\
{examples}"
        )
    }

    async fn emit_started(
        bus: &Arc<AnalyticsBus>,
        invocation_id: &str,
        subagent_type: &str,
        prompt_chars: usize,
    ) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert(
            "invocation_id".into(),
            AnalyticsValue::String(Verified::assert_safe(invocation_id.to_string()).into_inner()),
        );
        md.insert(
            "subagent_type".into(),
            AnalyticsValue::String(
                PiiTagged::assert_pii_tagged_column(subagent_type.to_string()).into_inner(),
            ),
        );
        md.insert(
            "prompt_chars".into(),
            AnalyticsValue::Int(prompt_chars as i64),
        );
        bus.log_event(AGENT_STARTED, md).await;
    }

    async fn emit_completed(
        bus: &Arc<AnalyticsBus>,
        invocation_id: &str,
        duration_ms: u64,
        subagent_type: &str,
    ) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert(
            "invocation_id".into(),
            AnalyticsValue::String(Verified::assert_safe(invocation_id.to_string()).into_inner()),
        );
        md.insert(
            "subagent_type".into(),
            AnalyticsValue::String(
                PiiTagged::assert_pii_tagged_column(subagent_type.to_string()).into_inner(),
            ),
        );
        md.insert(
            "duration_ms".into(),
            AnalyticsValue::Int(duration_ms as i64),
        );
        bus.log_event(AGENT_COMPLETED_M4_05, md).await;
    }

    async fn emit_failed(
        bus: &Arc<AnalyticsBus>,
        invocation_id: &str,
        error_kind: &str,
        duration_ms: u64,
    ) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert(
            "invocation_id".into(),
            AnalyticsValue::String(Verified::assert_safe(invocation_id.to_string()).into_inner()),
        );
        md.insert(
            "error_kind".into(),
            AnalyticsValue::String(Verified::assert_safe(error_kind.to_string()).into_inner()),
        );
        md.insert(
            "duration_ms".into(),
            AnalyticsValue::Int(duration_ms as i64),
        );
        bus.log_event(AGENT_FAILED, md).await;
    }

    /// (G11) Emit claude `tengu_agent_tool_selected` (AgentTool.tsx:419-428):
    /// the resolved agent + model + source + color + the resume/async/fork flags,
    /// fired AFTER selection resolution and BEFORE dispatch.
    #[allow(clippy::too_many_arguments)]
    async fn emit_agent_tool_selected(
        bus: &Arc<AnalyticsBus>,
        agent_type: &str,
        model: &str,
        source: &str,
        color: Option<&str>,
        is_built_in_agent: bool,
        is_resume: bool,
        is_async: bool,
        is_fork: bool,
    ) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert(
            "agent_type".into(),
            AnalyticsValue::String(
                PiiTagged::assert_pii_tagged_column(agent_type.to_string()).into_inner(),
            ),
        );
        md.insert(
            "model".into(),
            AnalyticsValue::String(
                PiiTagged::assert_pii_tagged_column(model.to_string()).into_inner(),
            ),
        );
        md.insert(
            "source".into(),
            AnalyticsValue::String(Verified::assert_safe(source.to_string()).into_inner()),
        );
        // claude `color: selectedAgent.color` — omitted (not zero-faked) when
        // the agent declares no color.
        if let Some(c) = color {
            md.insert(
                "color".into(),
                AnalyticsValue::String(Verified::assert_safe(c.to_string()).into_inner()),
            );
        }
        md.insert(
            "is_built_in_agent".into(),
            AnalyticsValue::Bool(is_built_in_agent),
        );
        md.insert("is_resume".into(), AnalyticsValue::Bool(is_resume));
        md.insert("is_async".into(), AnalyticsValue::Bool(is_async));
        md.insert("is_fork".into(), AnalyticsValue::Bool(is_fork));
        bus.log_event(TOOL_SELECTED, md).await;
    }

    /// (G11) Emit claude `tengu_agent_tool_completed` (agentToolUtils.ts:322-335).
    #[allow(clippy::too_many_arguments)]
    async fn emit_agent_tool_completed(
        bus: &Arc<AnalyticsBus>,
        agent_type: &str,
        model: &str,
        prompt_char_count: u64,
        response_char_count: u64,
        assistant_message_count: u64,
        total_tool_uses: u64,
        duration_ms: u64,
        total_tokens: u64,
        is_built_in_agent: bool,
        is_async: bool,
    ) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert(
            "agent_type".into(),
            AnalyticsValue::String(
                PiiTagged::assert_pii_tagged_column(agent_type.to_string()).into_inner(),
            ),
        );
        md.insert(
            "model".into(),
            AnalyticsValue::String(
                PiiTagged::assert_pii_tagged_column(model.to_string()).into_inner(),
            ),
        );
        md.insert(
            "prompt_char_count".into(),
            AnalyticsValue::Int(prompt_char_count as i64),
        );
        md.insert(
            "response_char_count".into(),
            AnalyticsValue::Int(response_char_count as i64),
        );
        md.insert(
            "assistant_message_count".into(),
            AnalyticsValue::Int(assistant_message_count as i64),
        );
        md.insert(
            "total_tool_uses".into(),
            AnalyticsValue::Int(total_tool_uses as i64),
        );
        md.insert(
            "duration_ms".into(),
            AnalyticsValue::Int(duration_ms as i64),
        );
        md.insert(
            "total_tokens".into(),
            AnalyticsValue::Int(total_tokens as i64),
        );
        md.insert(
            "is_built_in_agent".into(),
            AnalyticsValue::Bool(is_built_in_agent),
        );
        md.insert("is_async".into(), AnalyticsValue::Bool(is_async));
        bus.log_event(TOOL_COMPLETED, md).await;
    }

    /// (G11) Emit claude `tengu_cache_eviction_hint` (agentToolUtils.ts:340-345):
    /// `scope:'subagent_end'` + the final turn's `last_request_id`.
    async fn emit_cache_eviction_hint(bus: &Arc<AnalyticsBus>, last_request_id: &str) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert(
            "scope".into(),
            AnalyticsValue::String(Verified::assert_safe("subagent_end".to_string()).into_inner()),
        );
        md.insert(
            "last_request_id".into(),
            AnalyticsValue::String(
                PiiTagged::assert_pii_tagged_column(last_request_id.to_string()).into_inner(),
            ),
        );
        bus.log_event(CACHE_EVICTION_HINT, md).await;
    }

    /// (2.1.212) Emit claude `tengu_subagent_output_flagged` (`tHu`): the output
    /// guard neutralized control tags and/or flagged escalation patterns in a
    /// completed subagent's returned text (`surface:'finalize'`). Only called
    /// when at least one *reportable* pattern matched.
    async fn emit_subagent_output_flagged(
        bus: &Arc<AnalyticsBus>,
        agent_id: &str,
        result: &lingxi_core::host::subagent_output_guard::SanitizeResult,
    ) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert(
            "agent_id".into(),
            AnalyticsValue::String(
                PiiTagged::assert_pii_tagged_column(agent_id.to_string()).into_inner(),
            ),
        );
        md.insert(
            "surface".into(),
            AnalyticsValue::String(Verified::assert_safe("finalize".to_string()).into_inner()),
        );
        // `D5(Oo(…))`: sorted-unique reportable pattern / category names, joined
        // with `,`. The names are a fixed vocabulary (not user data).
        md.insert(
            "patterns".into(),
            AnalyticsValue::String(
                Verified::assert_safe(result.reportable_patterns_sorted().join(",")).into_inner(),
            ),
        );
        md.insert(
            "categories".into(),
            AnalyticsValue::String(
                Verified::assert_safe(result.reportable_categories_sorted().join(",")).into_inner(),
            ),
        );
        md.insert(
            "match_count".into(),
            AnalyticsValue::Int(result.reportable_match_count() as i64),
        );
        bus.log_event(SUBAGENT_OUTPUT_FLAGGED, md).await;
    }

    /// Emit claude `tengu_agent_tool_terminated` — a subagent run that ENDED by
    /// being stopped rather than finishing.
    ///
    /// Upstream has three sites: the async kill (`reason` from `killedBy`:
    /// `parent_kill_async` / `system_kill_async` / `user_kill_async`) and two
    /// sync cancels, both `user_cancel_sync`.
    ///
    /// `final_model` / `model_swapped` are NOT emitted. Upstream reads them off
    /// the run's models-used list, and `SubagentResult::Killed` carries only an
    /// agent id here — so the honest options were to omit them or to invent
    /// `model_swapped: false` for a run that may well have hopped. A subagent CAN
    /// swap models now (the refusal cascade), which is exactly why a hardcoded
    /// `false` would be a lie rather than a harmless default.
    async fn emit_agent_tool_terminated(
        bus: &Arc<AnalyticsBus>,
        agent_type: &str,
        model: &str,
        duration_ms: u64,
        is_built_in_agent: bool,
        is_async: bool,
        agent_depth: u32,
        reason: &str,
    ) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert(
            "agent_type".into(),
            AnalyticsValue::String(
                PiiTagged::assert_pii_tagged_column(agent_type.to_string()).into_inner(),
            ),
        );
        md.insert(
            "model".into(),
            AnalyticsValue::String(
                PiiTagged::assert_pii_tagged_column(model.to_string()).into_inner(),
            ),
        );
        md.insert(
            "duration_ms".into(),
            AnalyticsValue::Int(duration_ms as i64),
        );
        md.insert("is_async".into(), AnalyticsValue::Bool(is_async));
        md.insert(
            "is_built_in_agent".into(),
            AnalyticsValue::Bool(is_built_in_agent),
        );
        md.insert(
            "agent_depth".into(),
            AnalyticsValue::Int(i64::from(agent_depth)),
        );
        md.insert(
            "reason".into(),
            AnalyticsValue::String(Verified::assert_safe(reason.to_string()).into_inner()),
        );
        bus.log_event(TOOL_TERMINATED, md).await;
    }

    /// PLANNED #2/G13 — async (`run_in_background`) dispatch.
    ///
    /// claude returns the `async_launched` payload immediately and drives the
    /// lifecycle detached (AgentTool.tsx:686-764). This calls the
    /// [`lingxi_core::host::subagent_spawn::SubagentSpawner::spawn_async`] seam, which
    /// DEFAULTS to a clear error when unwired — so an unwired async branch
    /// surfaces an explicit message rather than silently running synchronously
    /// (the task forbids a silent wrong path). When the production spawner
    /// overrides `spawn_async`, this returns claude's `async_launched` JSON and
    /// registers `name → agentId` (G14, async-only, AgentTool.tsx:703-712).
    ///
    /// `resolved_cwd` / `agent_worktree` are the isolation products the caller
    /// resolved BEFORE branching (claude 2.1.207 creates the worktree before
    /// the sync/async split): the effective cwd (`cwd ?? worktreePath`) rides
    /// `request.cwd` so the background agent's tools operate in the worktree,
    /// and the handle rides `request.worktree` so the detached lifecycle owner
    /// runs the terminal keep/cleanup judgment (claude's `getWorktreeResult`
    /// closure) — NOT here at launch time.
    #[allow(clippy::too_many_arguments)]
    async fn dispatch_async(
        &self,
        bus: &Arc<AnalyticsBus>,
        invocation_id: &str,
        started: Instant,
        spawner: &dyn lingxi_core::host::subagent_spawn::SubagentSpawner,
        parsed: &AgentToolInput,
        effective_type: &str,
        selected: &lingxi_core::host::subagent_spawn::SelectedAgentMeta,
        is_fork: bool,
        ctx: &ToolUseContext,
        budget: Arc<dyn lingxi_core::host::budget::BudgetEnforcerHandle>,
        origin_session_id: Option<lingxi_core::types::SessionId>,
        parent_registry: Arc<tool_api::ToolRegistry>,
        effective_isolation: Option<String>,
        resolved_cwd: Option<String>,
        agent_worktree: Option<lingxi_core::host::worktree::WorktreeHandle>,
        mod_start: Option<Box<dyn AgentSpawnStart>>,
    ) -> Result<ToolCallResult, ToolError> {
        // [round-4 review, finding 15] `dispatch_async` is reached ONLY when
        // `run_in_background` is true (its sole caller gates on that flag
        // before calling), so every dispatch through this invoker is a
        // background task no interactive turn owns — the same invariant
        // `local_workflow_invoker` / `fusion_invoker` mark at the
        // composition root. Without `with_background_owned(true)`, a
        // permission ask this background child raises is wiped by Ctrl-C on
        // an unrelated foreground turn (`pending_prompts.retain` /
        // `dismiss_turn_prompts` in the TUI key on this exact flag), and this
        // invoker — not `local_agent_invoker` — is the one the dominant
        // background `Agent(...)` spawn path actually dispatches through
        // (`local_agent.rs`'s `inheritance.unwrap_or_else(|| ... )` only
        // falls back to `local_agent_invoker` for legacy direct tasks with
        // no inheritance at all).
        let mut invoker_impl =
            tool_api::tool_invoker_impl::RegistryToolInvoker::new(parent_registry)
                .with_background_owned(true);
        if let Some(gate) = self.ctx.permission_gate.clone() {
            invoker_impl = invoker_impl.with_gate(gate);
        }
        let invoker: Arc<dyn lingxi_core::host::tool_invoker::ToolInvoker> =
            spawner.decorate_tool_invoker(Arc::new(invoker_impl));
        let inherit = SubagentInheritance {
            tool_invoker: invoker,
            budget,
        };
        let request = SubagentSpawnRequest {
            stop_hook_scope: lingxi_core::host::subagent_spawn::SubagentStopScope::Session,
            agent_spawn_provenance: ctx.agent_spawn_provenance.clone(),
            teammate_color: None,
            subagent_type: effective_type.to_string(),
            prompt: parsed.prompt.clone(),
            observer: selected.observer.clone().or_else(|| {
                ctx.observer
                    .as_ref()
                    .filter(|observer| observer.agent != effective_type)
                    .cloned()
            }),
            context_paths: parsed.context_paths.clone(),
            description: Some(parsed.description.clone()),
            model: if is_fork { None } else { parsed.model.clone() },
            model_profile: spawn_model_profile(parsed, is_fork),
            run_in_background: true,
            name: if is_fork { None } else { parsed.name.clone() },
            team_name: None,
            // Permission mode belongs to the parent session and agent definition.
            mode: None,
            isolation: if is_fork { None } else { effective_isolation },
            // The RESOLVED cwd (explicit `cwd` override, else the isolation
            // worktree's path — claude `cwd ?? worktreePath`); `None` on fork.
            cwd: resolved_cwd,
            // Ownership transfer of the isolation worktree (claude
            // `getWorktreeResult` handed to the detached task): the local_agent
            // handler runs the terminal keep/cleanup judgment.
            worktree: agent_worktree,
            fork_context_messages: None,
            instruction_context: ctx.instruction_context.clone(),
            fork_parent_system_prompt: None,
            // The Agent tool has no structured-output schema param.
            schema: None,
            structured_output_mode: StructuredOutputMode::default(),
            structured_output_parse_retries: 0,
            effort: None,
            // Thread the originating tool_use_id so the backgrounded agent's
            // `<task-notification>` carries `<tool-use-id>` (claude-code parity).
            tool_use_id: ctx
                .tool_use_id
                .as_ref()
                .map(std::string::ToString::to_string),
            creator_teammate_name: ctx.agent_name.clone(),
            creator_team_name: ctx.team_name.clone(),
            creator_agent_id: ctx.agent_id,
            origin_session_id,
            // Workflow-only spawn seam (defaults; the Agent tool doesn't use the
            // workflow-subagent prompt override/addendum or disallow-union).
            system_prompt_override: None,
            system_prompt_addendum: None,
            additional_disallowed_tools: Vec::new(),
            // Child depth = this agent's depth + 1 (claude `spawnDepth =
            // z6(parentContext) + 1`). The spawner stamps it onto the child's
            // SubagentContext; the resolver applies the configured depth cap.
            depth: ctx.depth + 1,
            // Parent / main-loop model for this child's `Inherit` + family aliases
            // (claude `getAgentModel(…, toolUseContext.options.mainLoopModel, …)`,
            // AgentTool.tsx:418) — see the sync spawn path for the full note.
            parent_model_override: main_loop_model_parent(ctx),
            parent_model_profile_override: ctx.options.model_profile.clone(),
            forked_skill_name: None,
            forked_skill_attribution: None,
            forked_skill_effort: None,
            frozen_command_denies: Vec::new(),
            resumed_history: None,
            max_turns_override: None,
            max_output_tokens_per_turn: None,
            max_input_bytes_per_turn: None,
            query_source_label: None,
            correlation_id: None,
            model_attempt: None,
            handback_opt_in: !is_fork,
            parent_permission_mode: parent_permission_mode(ctx, self.ctx.permission_gate.as_ref()),
            handback_enabled: None,
            restored_handback_state: None,
            restored_handback_history: Vec::new(),
            handback_ends_turn_enabled: None,
            restore_handback_start: None,
        };

        match spawner.spawn_async(request, inherit).await {
            Ok(launch) => {
                if let Some(start) = mod_start {
                    start.started(launch.agent_id, selected.resolved_model.clone());
                }
                // (G14) Register name → agentId for SendMessage routing — ASYNC
                // ONLY, post-launch so a failed spawn leaves no stale entry
                // (claude AgentTool.tsx:700-712). Prefer the ctx-level registry
                // when wired; else the spawner's internal map.
                if let Some(name) = parsed.name.as_deref() {
                    if let Some(reg) = &self.ctx.agent_name_registry {
                        reg.register(name, launch.agent_id).await;
                    } else {
                        spawner.register_name(name, launch.agent_id).await;
                    }
                }
                // claude `canReadOutputFile = tools.some(Read || Bash)`
                // (AgentTool.tsx:753): whether the parent pool can read the
                // output file. Approximated via the wired parent registry.
                let can_read_output_file = ctx.subagent_registry.as_ref().is_some_and(|reg| {
                    reg.find_by_name("Read").is_some() || reg.find_by_name("Bash").is_some()
                });
                Ok(async_launch_result(
                    launch,
                    None,
                    &parsed.prompt,
                    &parsed.description,
                    &selected.resolved_model,
                    can_read_output_file,
                ))
            }
            Err(lingxi_core::host::subagent_spawn::SubagentSpawnError::PoolFull) => {
                self.release_spawn_reservation();
                Self::emit_failed(
                    bus,
                    invocation_id,
                    "subagent_concurrency_cap",
                    started.elapsed().as_millis() as u64,
                )
                .await;
                Err(ToolError::InvalidInput(concurrent_subagent_limit_error(
                    lingxi_core::host::subagent_spawn::max_concurrent_subagents(),
                )))
            }
            Err(e) => {
                Self::emit_failed(
                    bus,
                    invocation_id,
                    "async_spawn_error",
                    started.elapsed().as_millis() as u64,
                )
                .await;
                if let lingxi_core::host::subagent_spawn::SubagentSpawnError::DeniedByHook(reason) =
                    &e
                {
                    return Err(ToolError::InvalidInput(format!("Agent: {reason}")));
                }
                Err(ToolError::Internal(format!(
                    "Agent: async spawn failed: {e}"
                )))
            }
        }
    }
}

#[async_trait]
impl Tool for AgentTool {
    fn name(&self) -> &str {
        AGENT_TOOL_NAME
    }
    fn search_hint(&self) -> Option<&str> {
        // claude `searchHint: 'delegate work to a subagent'` (AgentTool.tsx:227).
        Some("delegate work to a subagent")
    }
    fn get_activity_description(&self, input: &Value) -> Option<String> {
        // Binary `getActivityDescription(e){return e?.description?.replace(/\s+/g,
        // " ").trim()||"Running task"}` — collapse internal whitespace runs to a
        // single space and trim; fall back to "Running task" when absent OR when
        // the normalized value is empty (JS `||`, empty string is falsy). (The
        // leaked TS `?? 'Running task'` was stale — the 2.1.195 binary normalizes.)
        let desc = input
            .get("description")
            .and_then(Value::as_str)
            .map(normalize_description_ws)
            .filter(|s| !s.is_empty());
        Some(desc.unwrap_or_else(|| "Running task".to_string()))
    }
    fn user_facing_name_for_input(&self, input: &Value) -> Option<String> {
        // claude `userFacingName(input)` (UI.tsx:760-775): show the subagent type
        // (except `general-purpose`/`worker`, which display as "Agent").
        let subagent_type = input.get("subagent_type").and_then(Value::as_str);
        Some(match subagent_type {
            Some(t) if t != GENERAL_PURPOSE_AGENT_TYPE => {
                if t == "worker" {
                    // claude: display "worker" agents as "Agent" for cleaner UI.
                    "Agent".to_string()
                } else {
                    t.to_string()
                }
            }
            _ => "Agent".to_string(),
        })
    }
    fn user_facing_name_background_color(&self, input: &Value) -> Option<String> {
        // claude `userFacingNameBackgroundColor(input)` (UI.tsx:776-787): no
        // subagent_type ⇒ None; else the agent's color via `getAgentColor`.
        // claude's `getAgentColor` is process-global; the Rust color manager
        // lives in the `agent` crate and is NOT threaded onto
        // `BuiltinToolContext`, so we return None here (a faithful degraded
        // behavior — no wrong color; documented residual).
        input.get("subagent_type").and_then(Value::as_str)?;
        None
    }
    fn input_schema(&self) -> &Value {
        // claude advertises `yJp().omit({cwd:!0})` — the model-facing schema
        // never exposes `cwd` (set internally by isolation / explicit override).
        // `gSn()`'s remaining two projections (coordinator `model` suffix,
        // `SUBAGENT_MODEL_FORCE` omission) depend on live session state, so the
        // projected schema is memoized per instance the way the binary memoizes
        // the whole builder.
        self.projected_input_schema.get_or_init(|| {
            let base = if self.advertise_run_in_background {
                &*AGENT_INPUT_SCHEMA_MODEL
            } else {
                &*AGENT_INPUT_SCHEMA_MODEL_NO_BACKGROUND
            };
            let is_coordinator = self
                .ctx
                .coordinator_mode
                .as_ref()
                .is_some_and(|m| m.is_enabled());
            project_agent_input_schema(base, is_coordinator)
        })
    }
    fn input_validation_issues(&self, input: &Value) -> Vec<Value> {
        let Some(name) = input.get("name").and_then(Value::as_str) else {
            return Vec::new();
        };
        let mut issues = Vec::new();
        if !matches_agent_name_pattern(name) {
            issues.push(serde_json::json!({"origin":"string","code":"invalid_format","format":"regex","pattern":format!("/{AGENT_NAME_PATTERN}/"),"path":["name"],"message":AGENT_NAME_REGEX_MESSAGE}));
        }
        if name == RESERVED_AGENT_NAME {
            issues.push(serde_json::json!({"code":"custom","path":["name"],"message":reserved_agent_name_message()}));
        }
        // TGo has a second .refine(!l0); Zod reports both for "main".
        let normalized = self.ctx.subagent_spawner.as_ref().map_or_else(
            || lingxi_core::host::live_sessions::normalize_name(name),
            |spawner| spawner.normalize_teammate_recipient(name),
        );
        if normalized == "main" || normalized == "team-lead" || reserved_agent_id_shape(&normalized)
        {
            issues.push(serde_json::json!({"code":"custom","path":["name"],"message":"name must not be a reserved recipient (\"main\" or \"team-lead\", in any spelling) or have the shape of an agent id — those already address an agent directly"}));
        }
        issues
    }
    fn output_schema(&self) -> Option<&Value> {
        Some(&AGENT_OUTPUT_SCHEMA)
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        // claude `maxResultSizeChars: 100_000` (AgentTool.tsx:229) — a flat cap,
        // NOT the shared `MAX_TOOL_OUTPUT_LENGTH`.
        AGENT_MAX_RESULT_SIZE_CHARS
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        // claude `isConcurrencySafe() { return true }` (AgentTool.tsx:1273-1275):
        // the Agent tool delegates to its underlying tools' own permission /
        // concurrency checks, so it is itself concurrency-safe — it is not an
        // exclusive drain barrier in the streaming executor.
        true
    }
    fn is_read_only(&self, _: &Value) -> bool {
        // claude `isReadOnly() { return true }` (AgentTool.tsx:1264-1266):
        // "delegates permission checks to its underlying tools".
        true
    }
    fn is_destructive(&self, _: &Value) -> bool {
        false
    }
    fn is_open_world(&self, _: &Value) -> bool {
        true
    }
    fn interrupt_behavior(&self, input: &Value) -> InterruptBehavior {
        // Fusion owns a multi-stage settlement state (including already
        // realized panel spend and spawn-quota refunds).  The outer turn-loop
        // dispatcher drops `Cancel` tool futures as soon as the shared token
        // fires, which bypasses Fusion's internal settlement arms.  Keep the
        // Fusion input cooperative at this boundary so its own cancellation
        // path observes the token and returns only after accounting is sealed;
        // ordinary Agent spawns retain the existing drop-on-cancel behavior.
        let is_fusion = input
            .get("subagent_type")
            .and_then(Value::as_str)
            .is_some_and(|agent_type| normalize_agent_type(agent_type) == FUSION_AGENT_TYPE);
        if is_fusion {
            InterruptBehavior::Block
        } else {
            InterruptBehavior::Cancel
        }
    }

    async fn check_permissions(&self, input: &Value, ctx: &ToolUseContext) -> PermissionResult {
        if let Some(ask) = self.fusion_implement_ask(input, ctx) {
            return ask;
        }
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "Agent spawn delegates to host runtime; no direct side-effect".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        // claude `async description() { return 'Launch a new agent' }`
        // (AgentTool.tsx:230-232). The dynamic catalog text lives in `prompt()`.
        "Launch a new agent".into()
    }

    async fn prompt(&self, opts: &PromptOptions) -> String {
        // claude-code builds the Agent tool prompt dynamically (getPrompt,
        // AgentTool/prompt.ts:66-287): it injects the live agent catalog (one
        // `formatAgentLine` per resolved AgentDefinition) and the available MCP
        // server names. Pull the catalog from the spawner (defaulted-empty when
        // unwired) and the MCP server names from the registry.
        let mut agents = match &self.ctx.subagent_spawner {
            Some(s) => s.agent_listing_for_model().await,
            None => Vec::new(),
        };
        // 2.1.238 `generalPurposeAvailable` (`p7f(agents, allowedAgentTypes)`,
        // @292885292): computed from the offered catalog, before the
        // `Agent(<x>)` permission-deny filter below — the binary's
        // `prompt({agents,…})` wrapper passes the unfiltered-by-permission
        // catalog to `p7f` and handles deny separately.
        let general_purpose_available = general_purpose_is_available(&agents);
        // F008 fix: append the synthetic `fusion` catalog entry BEFORE the
        // two deny-filter `retain` calls below, not after — otherwise
        // `Agent(fusion)` (or a `fusion` tools-denied rule) has no effect on
        // this listing: the model keeps seeing `fusion` advertised and
        // dispatches it, only to be hard-rejected by `call_fusion`'s own
        // deny gate on every attempt. `append_fusion_listing` is a no-op
        // when the Fusion surface is absent/disabled, so this is otherwise
        // byte-identical to appending after (the entry it may add cannot be
        // seen by `general_purpose_is_available` above it either way, since
        // that check only ever matches the built-in `general-purpose` type).
        self.append_fusion_listing(&mut agents);
        // Filter out agent types denied by a content-ful `Agent(<x>)` rule, so the
        // advertised catalog the model sees excludes them (claude-code `Pxe` —
        // the 2.1.186 Agent(type)-restriction prompt filter). No gate / no rules
        // ⇒ nothing removed (byte-identical to before).
        if let Some(gate) = &self.ctx.permission_gate {
            let denied = gate.agent_deny_content_types().await;
            if !denied.is_empty() {
                agents.retain(|a| !denied.iter().any(|d| d == &a.agent_type));
            }
        }
        // 2.1.238 `NJa` (@290291941) via `$Gr`/`vki`: a BUILT-IN agent whose every
        // permitted tool is denied by the current permission settings is not an
        // available agent, so it must not be advertised either. Empty (the
        // default seam / no deny rules) ⇒ nothing removed.
        if let Some(s) = &self.ctx.subagent_spawner {
            let tools_denied = s.tools_denied_agent_types().await;
            if !tools_denied.is_empty() {
                agents.retain(|a| !tools_denied.iter().any(|d| d == &a.agent_type));
            }
        }
        let mcp_server_names: Vec<String> = match &self.ctx.mcp_registry {
            Some(reg) => reg
                .snapshot()
                .await
                .into_iter()
                .map(|info| info.name)
                .collect(),
            None => Vec::new(),
        };
        // Coordinator-mode signal (TS `isCoordinatorMode()`), read LIVE from the
        // `coordinator_mode` seam on `BuiltinToolContext` (`None` ⇒ not
        // coordinator). Selects the slim coordinator prompt in `build_prompt`.
        let is_coordinator = self
            .ctx
            .coordinator_mode
            .as_ref()
            .is_some_and(|m| m.is_enabled());
        // claude `Agi()==="coordinator"` — `vyt()` never registers the built-in
        // web-fetch agent for a coordinator session.
        drop_coordinator_hidden_builtins(&mut agents, is_coordinator);
        Self::build_prompt(
            &agents,
            &mcp_server_names,
            is_coordinator,
            opts.model.as_deref(),
            general_purpose_available,
        )
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolUseContext,
        progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let started = Instant::now();
        let invocation_id = Self::fresh_invocation_id();
        let bus = self.ctx.bus.clone();

        // 1. Parse input.
        let mut parsed: AgentToolInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => {
                Self::emit_failed(
                    &bus,
                    &invocation_id,
                    "invalid_input",
                    started.elapsed().as_millis() as u64,
                )
                .await;
                return Err(ToolError::InvalidInput(format!(
                    "Agent: invalid input shape: {e}"
                )));
            }
        };

        // Model and profile are one selection: forced inheritance clears both.
        // Keep this policy for Mod rewrites as well as the original call.
        let model_overrides_forced = subagent_model_forced()
            || (coordinator_forces_worker_inherit_model()
                && self
                    .ctx
                    .coordinator_mode
                    .as_ref()
                    .is_some_and(|m| m.is_enabled()));
        clear_model_overrides_when_forced(&mut parsed, model_overrides_forced);
        if parsed.model_profile.is_some() && parsed.model.is_none() {
            return Err(ToolError::InvalidInput(
                "Agent: model_profile requires an explicit model".into(),
            ));
        }

        // `name` zod chain `z.string().regex(uZc).refine(t=>t!==K9)` (binary
        // `exy`). The wire `pattern` (on the advertised `name` property) covers
        // the regex at the turn-loop input gate, but `.refine()` (reserved
        // "main") has NO JSON Schema form, so enforce the FULL chain here at the
        // tool boundary with the byte-exact zod messages. A spawn naming its agent
        // "main" would collide with SendMessage's main-conversation routing, so
        // this is behavioral as well as byte parity.
        if let Some(name) = parsed.name.as_deref() {
            if let Err(msg) = validate_agent_name(name) {
                Self::emit_failed(
                    &bus,
                    &invocation_id,
                    "invalid_input",
                    started.elapsed().as_millis() as u64,
                )
                .await;
                return Err(ToolError::InvalidInput(msg));
            }
        }

        // Claude Code 2.1.217 defaults the nesting cap to 1: the main thread
        // (depth 0) may spawn a child, while that child may not spawn another
        // unless LINGXI_MAX_SUBAGENT_SPAWN_DEPTH raises the limit.
        let depth_limit = lingxi_core::host::subagent_spawn::max_subagent_spawn_depth();
        if ctx.depth >= depth_limit {
            Self::emit_failed(
                &bus,
                &invocation_id,
                "subagent_depth_cap",
                started.elapsed().as_millis() as u64,
            )
            .await;
            return Err(ToolError::InvalidInput(subagent_depth_limit_error(
                ctx.depth,
                depth_limit,
            )));
        }

        // 2.1.266 `a0`: refuse while THIS agent's own stop is still completing.
        // A kill is cooperative before it is forced — the runner keeps
        // dispatching the rest of the current turn's `tool_use` blocks until it
        // next races the stop at a model round-trip — so without this a dying
        // agent can still launch a child that outlives it.
        if let Some(agent_id) = ctx.agent_id {
            if lingxi_core::host::agent_processes::is_stop_pending(&agent_id.to_string()) {
                Self::emit_failed(
                    &bus,
                    &invocation_id,
                    "subagent_spawner_stop_pending",
                    started.elapsed().as_millis() as u64,
                )
                .await;
                return Err(ToolError::InvalidInput(
                    lingxi_core::host::agent_processes::stop_pending_refusal("launch new agents."),
                ));
            }
        }

        // 2.1.263: nesting denial precedes the explicit background denial and
        // both run before fork/type resolution. A named ordinary child has no
        // team_name, so it must not be mistaken for a teammate.
        let caller_is_teammate = ctx.agent_name.is_some() && ctx.team_name.is_some();
        let caller_is_in_process_teammate = caller_is_teammate
            && std::env::var("LINGXI_CODE_TEAMMATE_BACKEND").as_deref() != Ok("split-pane");
        if caller_is_teammate && parsed.name.is_some() {
            return Err(ToolError::InvalidInput(
                "Teammates cannot spawn other teammates — the team roster is flat. To spawn a subagent instead, omit the `name` parameter.".into(),
            ));
        }
        if caller_is_in_process_teammate && parsed.run_in_background == Some(true) {
            return Err(ToolError::InvalidInput(
                "In-process teammates cannot spawn background agents. Use run_in_background=false for synchronous subagents.".into(),
            ));
        }

        // Binary `AgentTool.call`: `n=n.replace(/\s+/g," ").trim()` — normalize the
        // `description` ONCE at entry so every downstream use (the spawn request,
        // the async-launch payload, the completed `data.description`) carries the
        // collapsed/trimmed value.
        parsed.description = normalize_description_ws(&parsed.description);

        // (G7) NO empty-prompt validation: claude-code has no such guard — a
        // `prompt: ""` spawn must succeed (AgentTool.call accepts any prompt).
        // The former hard-reject + `empty_prompt` emit are removed for parity.

        // 2. Wiring guard: spawner must be present (needed for the type
        // validation + MCP gate below, and the dispatch).
        let spawner = self.ctx.subagent_spawner.clone().ok_or_else(|| {
            ToolError::Internal(
                "AgentTool: SubagentSpawner not wired into BuiltinToolContext".into(),
            )
        })?;

        // 3. Fork-subagent routing (claude-code 2.1.232 `AgentTool.call`):
        //   A = subagent_type is present AND normalizes to `"fork"`
        //   I = `fUf` / `SPe()` (fork feature available)
        //   L = A && I
        // An OMITTED type is general-purpose (`t ?? GENERAL_PURPOSE`). Only an
        // explicit `subagent_type: "fork"` inherits conversation + prompt cache.
        //
        // The gate (`is_fork_subagent_enabled`) is off in coordinator mode.
        // `is_coordinator` is read LIVE from the `coordinator_mode` seam on
        // `BuiltinToolContext` (`None` ⇒ not coordinator). Headless is honored
        // via `ctx.options.is_non_interactive_session` unless the env override
        // is explicitly true.
        let is_coordinator = self
            .ctx
            .coordinator_mode
            .as_ref()
            .is_some_and(|m| m.is_enabled());
        let wants_fork = parsed
            .subagent_type
            .as_deref()
            .is_some_and(|t| normalize_agent_type(t) == "fork");
        let is_fork = wants_fork
            && lingxi_core::host::fork_subagent::is_fork_subagent_enabled(
                is_coordinator,
                ctx.options.is_non_interactive_session,
            );

        // Recursion guard (claude `AgentTool.tsx:332-334`): fork children keep the
        // Agent tool in their pool for cache-identical tool defs, so a fork inside
        // a forked worker must be rejected. claude's primary check is `querySource
        // === agent:builtin:fork`, which has NO carrier on `ToolUseContext` in
        // this arch — so the message-scan fallback (`is_in_fork_child`) is the
        // sole guard. It relies on the `<fork-boilerplate>` tag being present in
        // the child's inherited transcript, which `build_child_message`
        // guarantees. (FLAG: faithful but narrower than claude's dual check.)
        if parsed
            .subagent_type
            .as_deref()
            .is_some_and(|t| normalize_agent_type(t) == FUSION_AGENT_TYPE)
        {
            return self
                .call_fusion(
                    parsed,
                    ctx,
                    progress,
                    &bus,
                    &invocation_id,
                    started,
                    is_coordinator,
                )
                .await;
        }

        if is_fork && lingxi_core::host::fork_subagent::is_in_fork_child(&ctx.messages) {
            Self::emit_failed(
                &bus,
                &invocation_id,
                "fork_in_fork_child",
                started.elapsed().as_millis() as u64,
            )
            .await;
            return Err(ToolError::InvalidInput(
                "Fork is not available inside a forked worker. Complete your task directly using your tools.".into(),
            ));
        }

        // 2.1.263 src_160988549.js:3517864. Named teammates branch before
        // ordinary type resolution, concurrency accounting, and MCP checks.
        // The session owns the implicit team; input cannot select a team.
        let teammate_candidate = spawner.teammate_enabled()
            && parsed.name.is_some()
            && !is_fork
            && parsed.isolation.is_none()
            && parsed.cwd.is_none();
        let listing = if teammate_candidate {
            spawner.agent_listing().await
        } else {
            Vec::new()
        };
        let special_type = parsed.subagent_type.as_deref().is_some_and(|kind| {
            let normalized = normalize_agent_type(WEB_FETCH_AGENT_TYPE);
            listing
                .iter()
                .any(|agent| agent.agent_type == WEB_FETCH_AGENT_TYPE)
                && (kind == WEB_FETCH_AGENT_TYPE
                    || (normalize_agent_type(kind) == normalized
                        && !listing.iter().any(|agent| {
                            agent.agent_type != WEB_FETCH_AGENT_TYPE
                                && normalize_agent_type(&agent.agent_type) == normalized
                        })))
        });
        if teammate_candidate && !special_type {
            let requested_type = parsed.subagent_type.clone();
            let requested = requested_type.as_deref();
            if let (Some(kind), Some(gate)) = (requested, &self.ctx.permission_gate) {
                if let Some(source) = gate.agent_type_deny(kind).await {
                    return Err(ToolError::InvalidInput(format!(
                        "Agent type '{kind}' has been denied by permission rule 'Agent({kind})' from {source}."
                    )));
                }
            }
            let resolved = requested.and_then(|kind| {
                listing
                    .iter()
                    .find(|agent| agent.agent_type == kind)
                    .or_else(|| {
                        let mut matches = listing.iter().filter(|agent| {
                            normalize_agent_type(&agent.agent_type) == normalize_agent_type(kind)
                        });
                        let first = matches.next();
                        if matches.next().is_none() {
                            first
                        } else {
                            None
                        }
                    })
            });
            let mut kind = resolved
                .map(|agent| agent.agent_type.as_str())
                .or(requested)
                .unwrap_or(GENERAL_PURPOSE_AGENT_TYPE);
            if let Some(gate) = &self.ctx.permission_gate {
                if let Some(source) = gate.agent_type_deny(kind).await {
                    return Err(ToolError::InvalidInput(format!(
                        "Agent type '{kind}' has been denied by permission rule 'Agent({kind})' from {source}."
                    )));
                }
            }
            let budget = self.ctx.budget_enforcer.clone().ok_or_else(|| {
                ToolError::Internal(
                    "AgentTool: BudgetEnforcerHandle not wired into BuiltinToolContext".into(),
                )
            })?;
            let origin_session_id = Self::origin_session_id(&ctx).await;
            let budget = Self::budget_for_origin(budget, origin_session_id);
            if let Err(BudgetError::Exceeded { current_nano_usd }) =
                budget.check_and_charge(0).await
            {
                return Err(match budget.max_session_nano_usd() {
                    Some(limit) => {
                        ToolError::InvalidInput(budget_limit_reached_error(current_nano_usd, limit))
                    }
                    None => ToolError::Internal(format_budget_denied(current_nano_usd)),
                });
            }
            let registry = ctx.subagent_registry.clone().ok_or_else(|| {
                ToolError::Internal("AgentTool: parent ToolRegistry not threaded via ToolUseContext.subagent_registry".into())
            })?;
            let mut invoker = tool_api::tool_invoker_impl::RegistryToolInvoker::new(registry)
                .with_background_owned(true);
            if let Some(gate) = self.ctx.permission_gate.clone() {
                invoker = invoker.with_gate(gate);
            }
            let selected = spawner
                .resolve_selection(kind, parsed.model.as_deref())
                .await;
            let inherited_mode = self
                .ctx
                .permission_gate
                .as_ref()
                .and_then(|gate| gate.permission_mode())
                .unwrap_or_else(|| self.ctx.permission_mode.wire_str().to_owned());
            // Native named teammates enter the same `agent.spawn` hook
            // dispatcher as ordinary Agent calls, with `isTeammate:true`.
            // Keep provenance in the host sidecar below; never reconstruct it
            // from hook-rewritten JSON.
            let original_kind = kind.to_owned();
            let original_parent = ctx.agent_id.map(|id| id.as_uuid().to_string());
            let mut named_spawn_input = json!({
                "tool_use_id": ctx.tool_use_id.as_ref().map(ToString::to_string).unwrap_or_default(),
                "prompt": parsed.prompt,
                "description": parsed.description,
                "subagentType": kind,
                "provider": if selected.is_built_in {
                    json!({"plugin":"engine","tier":"core"})
                } else if selected.source == "plugin" {
                    json!({"plugin":kind.split(':').next().unwrap_or("engine"),"tier":"user"})
                } else {
                    json!({"plugin":"engine","tier":"core"})
                },
                "parentModel": main_loop_model_parent(&ctx).unwrap_or_else(|| self.ctx.default_model.clone()),
                "permissionMode": self.ctx.permission_gate.as_ref()
                    .and_then(|gate| gate.permission_mode())
                    .unwrap_or_else(|| self.ctx.permission_mode.wire_str().to_owned()),
                "background": false,
                "fork": false,
                "isTeammate": true,
            });
            if let Some(parent_agent_id) = original_parent.as_ref() {
                named_spawn_input["parentAgentId"] = json!(parent_agent_id);
            }
            if let Some(name) = parsed.name.as_deref() {
                named_spawn_input["name"] = json!(name);
            }
            if let Some(model) = parsed.model.as_deref() {
                named_spawn_input["model"] = json!(model);
            }
            if let Some(profile) = parsed.model_profile.as_deref() {
                named_spawn_input["model_profile"] = json!(profile);
            }

            let mut named_spawn_start: Option<Box<dyn AgentSpawnStart>> = None;
            match spawner
                .begin_agent_spawn(
                    named_spawn_input.clone(),
                    ctx.agent_spawn_provenance.clone(),
                )
                .await
            {
                Ok(AgentSpawnAdmission::Bypass(_)) => {}
                Ok(AgentSpawnAdmission::Answered(answer)) => {
                    let reason = answer
                        .get("deny")
                        .and_then(Value::as_str)
                        .unwrap_or("agent.spawn: a hook answered without passing the spawn on");
                    return Err(ToolError::InvalidInput(format!(
                        "Subagent spawn denied by a plugin: {reason}"
                    )));
                }
                Ok(AgentSpawnAdmission::Forwarded { input, start }) => {
                    let parent_matches = match (&original_parent, input.get("parentAgentId")) {
                        (None, None) => true,
                        (Some(expected), Some(actual)) => actual.as_str() == Some(expected),
                        _ => false,
                    };
                    if input.get("isTeammate").and_then(Value::as_bool) != Some(true)
                        || !parent_matches
                        || input.get("background").and_then(Value::as_bool) != Some(false)
                    {
                        start.failed(
                            "agent.spawn: named teammate identity or execution mode was changed"
                                .into(),
                        );
                        return Err(ToolError::InvalidInput(
                            "A plugin's agent.spawn hook changed the named teammate identity or execution mode.".into(),
                        ));
                    }
                    let Some(next_type) = input.get("subagentType").and_then(Value::as_str) else {
                        start
                            .failed("agent.spawn: named teammate input has no subagentType".into());
                        return Err(ToolError::InvalidInput(
                            "agent.spawn has no subagentType for the named teammate".into(),
                        ));
                    };
                    if next_type != original_kind {
                        let next_agent = listing
                            .iter()
                            .find(|agent| agent.agent_type == next_type)
                            .or_else(|| {
                                let mut matches = listing.iter().filter(|agent| {
                                    normalize_agent_type(&agent.agent_type)
                                        == normalize_agent_type(next_type)
                                });
                                let first = matches.next();
                                if matches.next().is_none() {
                                    first
                                } else {
                                    None
                                }
                            });
                        let Some(next_agent) = next_agent else {
                            start.failed(format!(
                                "agent.spawn: named teammate type '{next_type}' is unavailable"
                            ));
                            return Err(ToolError::InvalidInput(format!(
                                "A plugin's agent.spawn hook rewrote the named teammate to unavailable type '{next_type}'."
                            )));
                        };
                        kind = next_agent.agent_type.as_str();
                        if let Some(gate) = &self.ctx.permission_gate {
                            if let Some(source) = gate.agent_type_deny(kind).await {
                                start.failed(format!(
                                    "agent.spawn: rewritten agent type denied by {source}"
                                ));
                                return Err(ToolError::InvalidInput(format!(
                                    "A plugin's agent.spawn hook rewrote the named teammate to agent type '{kind}', which a permission rule denies ({source})."
                                )));
                            }
                        }
                    }
                    parsed.prompt = input
                        .get("prompt")
                        .and_then(Value::as_str)
                        .unwrap_or(&parsed.prompt)
                        .to_owned();
                    if let Some(description) = input.get("description").and_then(Value::as_str) {
                        parsed.description = normalize_description_ws(description);
                    }
                    parsed.name = input
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or(parsed.name);
                    parsed.model = match model_after_hook(&input, parsed.model.take()) {
                        Ok(model) => model,
                        Err(error) => {
                            start.failed(error.to_string());
                            return Err(error);
                        }
                    };
                    parsed.model_profile =
                        match model_profile_after_hook(&input, parsed.model_profile.take()) {
                            Ok(profile) => profile,
                            Err(error) => {
                                start.failed(error.to_string());
                                return Err(error);
                            }
                        };
                    clear_model_overrides_when_forced(&mut parsed, model_overrides_forced);
                    if parsed.model_profile.is_some() && parsed.model.is_none() {
                        start.failed("agent.spawn: model_profile requires a model".into());
                        return Err(ToolError::InvalidInput(
                            "agent.spawn: model_profile requires an explicit model".into(),
                        ));
                    }
                    named_spawn_start = Some(start);
                }
                Err(error) => {
                    return Err(ToolError::Internal(format!(
                        "agent.spawn Mod dispatch failed: {error}"
                    )));
                }
            }

            let request = SubagentSpawnRequest {
                instruction_context: ctx.instruction_context.clone(),
                subagent_type: kind.into(),
                prompt: parsed.prompt.clone(),
                description: Some(parsed.description.clone()),
                name: parsed.name.clone(),
                // The spawner resolves model and profile together. Selection
                // metadata is for display, and must not replace the call's
                // model before the route has been resolved.
                model: parsed.model.clone(),
                model_profile: spawn_model_profile(&parsed, false),
                agent_spawn_provenance: ctx.agent_spawn_provenance.clone(),
                // The obsolete input mode never overrides the live parent mode.
                mode: Some(inherited_mode),
                // Snapshot the caller's current working directory only after
                // teammate eligibility has ruled out an explicit cwd override.
                cwd: Some(
                    ctx.cwd
                        .clone()
                        .unwrap_or_else(|| self.ctx.cwd())
                        .to_string_lossy()
                        .into_owned(),
                ),
                creator_agent_id: ctx.agent_id,
                origin_session_id,
                parent_model_override: main_loop_model_parent(&ctx),
                parent_model_profile_override: ctx.options.model_profile.clone(),
                parent_permission_mode: parent_permission_mode(
                    &ctx,
                    self.ctx.permission_gate.as_ref(),
                ),
                depth: ctx.depth + 1,
                ..Default::default()
            };
            let launch = spawner
                .spawn_teammate(
                    request,
                    SubagentInheritance {
                        tool_invoker: Arc::new(invoker),
                        budget,
                    },
                )
                .await;
            let launch = match launch {
                Ok(launch) => {
                    if let Some(start) = named_spawn_start.take() {
                        if let Some(agent_id) =
                            lingxi_core::types::AgentId::parse_prefixed(&launch.agent_id)
                        {
                            start.started(agent_id, launch.model.clone());
                        } else {
                            start.failed(
                                "agent.spawn: teammate returned an invalid agent id".into(),
                            );
                            return Err(ToolError::Internal(
                                "named teammate launch returned an invalid agent id".into(),
                            ));
                        }
                    }
                    launch
                }
                Err(error) => {
                    if let Some(start) = named_spawn_start.take() {
                        start.failed(format!("agent.spawn: teammate startup failed: {error}"));
                    }
                    return Err(ToolError::Internal(error.to_string()));
                }
            };
            let mut data = serde_json::to_value(&launch)
                .map_err(|error| ToolError::Internal(error.to_string()))?;
            data["status"] = json!("teammate_spawned");
            data["prompt"] = json!(parsed.prompt);
            return Ok(ToolCallResult {
                data,
                model_content: Some(format!(
                    "Spawned successfully. (This tool result is internal metadata — never quote or paste any part of it, including the ID below, into a user-facing reply.)\nagent_id: {}\nname: {}\nThe agent is now running and will receive instructions via mailbox.",
                    launch.teammate_id, launch.name
                )),
                new_messages: vec![],
                context_modifier: None,
                is_error: false,
                mcp_meta: None,
            });
        }

        // Resolve the EFFECTIVE subagent type (2.1.232 `L ? Yne : t ?? general`):
        //   - FORK PATH (explicit `"fork"` + gate) ⇒ the synthetic `fork` type.
        //   - OMITTED (`None`) ⇒ `general-purpose`.
        //   - EXPLICIT (`Some(x)` not fork, or fork with gate off) ⇒ catalog
        //     lookup; unknown types are REJECTED with claude's
        //     "Agent type 'x' not found. Available agents: …" error.
        // Denied-by-permission-rule (claude-code `getDenyRuleForAgent` →
        // `AgentTypeError`, AgentTool.tsx:349-351): a content-ful `Agent(<type>)`
        // deny rule rejects the resolved subagent type — checked BEFORE the
        // not-found lookup (binary `o5e` precedes the catalog match) and applied
        // to the `general-purpose` default too (deny `Agent(general-purpose)`
        // blocks an omitted type). Byte-exact message + raw `SettingSource`.
        //
        // 2.1.238 `NJa`/`mdr` (@290291941): a BUILT-IN agent whose every permitted
        // tool is denied by the current permission settings is filtered out of
        // `$Gr`'s available set, and asking for it by name raises `hdr`'s
        // "every tool it may use is denied" error. Resolved ONCE here and used
        // as a second unavailability reason everywhere `agent_deny_content_types`
        // is (the `Available agents:` tails, the ambiguity `(unavailable)`
        // markers, and the pre-resolution rejection below). Empty by default
        // (the trait's default seam, or no deny rules wired) ⇒ no behaviour
        // change.
        let tools_denied: Vec<String> = spawner.tools_denied_agent_types().await;
        let mut effective_type: String = if is_fork {
            lingxi_core::host::fork_subagent::FORK_SUBAGENT_TYPE.to_string()
        } else {
            let candidate = parsed
                .subagent_type
                .as_deref()
                .unwrap_or(GENERAL_PURPOSE_AGENT_TYPE);
            if let Some(gate) = &self.ctx.permission_gate {
                if let Some(source) = gate.agent_type_deny(candidate).await {
                    Self::emit_failed(
                        &bus,
                        &invocation_id,
                        "agent_type_denied",
                        started.elapsed().as_millis() as u64,
                    )
                    .await;
                    return Err(ToolError::InvalidInput(format!(
                        "Agent type '{candidate}' has been denied by permission rule 'Agent({candidate})' from {source}."
                    )));
                }
            }
            // claude @292890415, in the `Gt.length===1 && !kt.has(...)` arm:
            // `if(Br) throw …denied by permission rule…;
            //  if(mdr(yr,y)) throw new ISt(hdr(wr))` — the tools-denied error is
            // raised only AFTER the `Agent(<type>)` deny rule was ruled out, so
            // it sits directly below that check. Hoisted ahead of the catalog
            // lookup for the same reason the deny check is: the requested type
            // is not in the available set, so the lookup could only mislabel it
            // "not found".
            if tools_denied.iter().any(|t| t == candidate) {
                Self::emit_failed(
                    &bus,
                    &invocation_id,
                    "agent_type_tools_denied",
                    started.elapsed().as_millis() as u64,
                )
                .await;
                return Err(ToolError::InvalidInput(agent_type_tools_denied_error(
                    candidate,
                )));
            }
            match parsed.subagent_type.as_deref() {
                None => {
                    // 2.1.238 @292889305 (NEW — 2.1.220 @234673007 went straight
                    // to `let lt = t ?? $Fe.agentType`): an OMITTED
                    // `subagent_type` only defaults to `general-purpose` when
                    // that agent is actually reachable
                    // (`if(t===void 0&&!p7f(k,A)){…throw new ISt(
                    //   `${Gri}. Available agents: ${YLi(Ot)}`)}`).
                    //
                    // The available-agents tail is the DENY-FILTERED catalog
                    // (`Xt = $Gr(k,A,y).map(agentType)`) routed through `YLi`
                    // (the `"none"` fallback). The binary's
                    // `Ot = x && f7f(B)===null ? [GAe, ...Xt] : Xt` fork prepend
                    // is unreachable here: the port only reaches this arm when
                    // `is_fork` is false, and `is_fork` is exactly
                    // "fork enabled AND subagent_type omitted".
                    let mut listing = spawner.agent_listing_for_model().await;
                    drop_coordinator_hidden_builtins(&mut listing, is_coordinator);
                    if !general_purpose_is_available(&listing) {
                        let mut denied = match &self.ctx.permission_gate {
                            Some(gate) => gate.agent_deny_content_types().await,
                            None => Vec::new(),
                        };
                        // `Xt = $Gr(k,A,y)` is deny-filtered AND `NJa`-filtered.
                        denied.extend(tools_denied.iter().cloned());
                        let available = listing
                            .iter()
                            .filter(|a| !denied.iter().any(|d| d == &a.agent_type))
                            .map(|a| a.agent_type.clone())
                            .collect::<Vec<_>>();
                        Self::emit_failed(
                            &bus,
                            &invocation_id,
                            "agent_type_missing",
                            started.elapsed().as_millis() as u64,
                        )
                        .await;
                        return Err(ToolError::InvalidInput(format!(
                            "{SUBAGENT_TYPE_REQUIRED_MESSAGE}. Available agents: {}",
                            render_available_agents(&available)
                        )));
                    }
                    GENERAL_PURPOSE_AGENT_TYPE.to_string()
                }
                Some(explicit) => {
                    // Keep the raw catalog as the execution resolver: a
                    // model-selected type remains dispatchable even if a Mod
                    // hid it from future offers. Only after an exact miss do
                    // aliases and error suggestions use the model-facing set.
                    let mut listing = spawner.agent_listing().await;
                    drop_coordinator_hidden_builtins(&mut listing, is_coordinator);
                    if listing.iter().any(|a| a.agent_type == explicit) {
                        explicit.to_string()
                    } else {
                        let mut offered_listing = spawner.agent_listing_for_model().await;
                        drop_coordinator_hidden_builtins(&mut offered_listing, is_coordinator);
                        // The `Available agents:` set is the deny-filtered listing
                        // (claude-code `Pxe`), so a denied type never appears as a
                        // suggestion.
                        let type_denied: Vec<String> = match &self.ctx.permission_gate {
                            Some(gate) => gate.agent_deny_content_types().await,
                            None => Vec::new(),
                        };
                        // claude `kt` (the available-type set) is `$Gr`'s output:
                        // deny-filtered AND `NJa`-filtered, so a tools-denied type
                        // is "(unavailable)" in an ambiguity message and absent
                        // from every `Available agents:` tail. `type_denied` stays
                        // separate because claude checks the `Agent(<x>)` deny rule
                        // BEFORE `mdr` — a type that is both must report the rule.
                        let mut denied = type_denied.clone();
                        denied.extend(tools_denied.iter().cloned());
                        let is_denied = |t: &str| denied.iter().any(|d| d.as_str() == t);
                        let available = || {
                            offered_listing
                                .iter()
                                .filter(|a| !denied.iter().any(|d| d == &a.agent_type))
                                .map(|a| a.agent_type.clone())
                                .collect::<Vec<_>>()
                        };
                        // 2.1.238 routes every `Available agents:` list through
                        // `YLi` (@292880950): `e.join(", ")||"none"`, so an
                        // empty catalog renders the word `none` rather than an
                        // empty tail. 2.1.220 interpolated a bare `join(", ")`.
                        let available_display = || render_available_agents(&available());
                        // claude `Agt()` normalized fallback (AgentTool.tsx): when the
                        // exact name misses, match candidates whose normalized form
                        // (NFKC/lowercase/strip ws+dash+underscore) equals the
                        // candidate's. A single available match resolves; multiple is
                        // an ambiguity error; zero (or a single denied match) falls
                        // through to not-found.
                        let norm = normalize_agent_type(explicit);
                        let matches: Vec<String> = offered_listing
                            .iter()
                            .filter(|a| normalize_agent_type(&a.agent_type) == norm)
                            .map(|a| a.agent_type.clone())
                            .collect();
                        if matches.len() > 1 {
                            let avail_matches: Vec<&String> =
                                matches.iter().filter(|&m| !is_denied(m)).collect();
                            let matches_disp = matches
                                .iter()
                                .map(|m| {
                                    if is_denied(m) {
                                        format!("{m} (unavailable)")
                                    } else {
                                        m.clone()
                                    }
                                })
                                .collect::<Vec<_>>()
                                .join(", ");
                            let tail = if avail_matches.is_empty() {
                                format!(
                                    "None of these are available. Available agents: {}",
                                    available_display()
                                )
                            } else {
                                format!(
                                    "Use the exact name: {}",
                                    avail_matches
                                        .iter()
                                        .map(|s| s.as_str())
                                        .collect::<Vec<_>>()
                                        .join(" or ")
                                )
                            };
                            Self::emit_failed(
                                &bus,
                                &invocation_id,
                                "agent_type_ambiguous",
                                started.elapsed().as_millis() as u64,
                            )
                            .await;
                            return Err(ToolError::InvalidInput(format!(
                                "Agent type '{explicit}' is ambiguous — matches {matches_disp}. {tail}"
                            )));
                        }
                        if matches.len() == 1 && !is_denied(&matches[0]) {
                            matches.into_iter().next().unwrap()
                        } else if matches.len() == 1
                            && !type_denied.iter().any(|d| d == &matches[0])
                            && tools_denied.iter().any(|d| d == &matches[0])
                        {
                            // claude @292890415, `Gt.length===1 && !kt.has(...)`:
                            // `if(Br) throw …denied by permission rule…;
                            //  if(mdr(yr,y)) throw new ISt(hdr(wr))` — the
                            // normalized match resolved to a real agent that is
                            // unavailable ONLY because every tool it may use is
                            // denied, so it reports that, not "not found".
                            let matched = matches[0].clone();
                            Self::emit_failed(
                                &bus,
                                &invocation_id,
                                "agent_type_tools_denied",
                                started.elapsed().as_millis() as u64,
                            )
                            .await;
                            return Err(ToolError::InvalidInput(agent_type_tools_denied_error(
                                &matched,
                            )));
                        } else {
                            Self::emit_failed(
                                &bus,
                                &invocation_id,
                                "agent_type_not_found",
                                started.elapsed().as_millis() as u64,
                            )
                            .await;
                            return Err(ToolError::InvalidInput(format!(
                                "Agent type '{explicit}' not found. Available agents: {}",
                                available_display()
                            )));
                        }
                    }
                }
            }
        };

        let mut selected = spawner
            .resolve_selection(&effective_type, parsed.model.as_deref())
            .await;
        if caller_is_in_process_teammate && selected.background {
            return Err(ToolError::InvalidInput(format!(
                "In-process teammates cannot spawn background agents. Agent '{}' has background: true in its definition.",
                selected.agent_type
            )));
        }

        // Claude Code 2.1.217 rejects rather than queues a launch once the
        // runtime already has the configured number of active subagents. Keep
        // this before the session-total counter so a rejected concurrent spawn
        // does not consume one of the 200 lifetime slots.
        let concurrent_cap = lingxi_core::host::subagent_spawn::max_concurrent_subagents();
        if spawner.concurrent_subagent_count().await >= concurrent_cap {
            Self::emit_failed(
                &bus,
                &invocation_id,
                "subagent_concurrency_cap",
                started.elapsed().as_millis() as u64,
            )
            .await;
            return Err(ToolError::InvalidInput(concurrent_subagent_limit_error(
                concurrent_cap,
            )));
        }

        // Share the main session's max-budget gate with every foreground and
        // background child. Keep this before the lifetime-spawn counter and MCP
        // wait, matching Claude's pre-spawn budget boundary.
        let budget = self.ctx.budget_enforcer.clone().ok_or_else(|| {
            ToolError::Internal(
                "AgentTool: BudgetEnforcerHandle not wired into BuiltinToolContext".into(),
            )
        })?;
        let origin_session_id = Self::origin_session_id(&ctx).await;
        let budget = Self::budget_for_origin(budget, origin_session_id);
        if let Err(BudgetError::Exceeded { current_nano_usd }) = budget.check_and_charge(0).await {
            Self::emit_failed(
                &bus,
                &invocation_id,
                "budget_exceeded",
                started.elapsed().as_millis() as u64,
            )
            .await;
            return Err(match budget.max_session_nano_usd() {
                Some(limit_nano_usd) => ToolError::InvalidInput(budget_limit_reached_error(
                    current_nano_usd,
                    limit_nano_usd,
                )),
                None => ToolError::Internal(format_budget_denied(current_nano_usd)),
            });
        }

        // Per-session subagent spawn cap (claude 2.1.212 `AgentTool.call` `N()`):
        // before every spawn, reject the launch once the session has already
        // spawned `LINGXI_MAX_SUBAGENTS_PER_SESSION` agents (default 200),
        // otherwise bump the counter. The counter lives on the session
        // `taskRegistry` (`getTotalAgentSpawns` / `incrementTotalAgentSpawns`) so
        // it is shared across every `AgentTool::call` in the session and across
        // the fork / regular / teammate spawn paths. Matches the binary's
        // placement: after type resolution, before the required-MCP gate and the
        // sync/async dispatch. The Rust seam reserves atomically because tool
        // calls can execute on parallel tasks; a pool-full allocation race rolls
        // that reservation back. When no registry is wired the cap is inert.
        if let Some(registry) = &self.ctx.task_registry {
            let cap = max_subagents_per_session();
            if let Err(spawned) = registry.try_reserve_total_agent_spawn(cap) {
                Self::emit_failed(
                    &bus,
                    &invocation_id,
                    "subagent_count_cap",
                    started.elapsed().as_millis() as u64,
                )
                .await;
                return Err(ToolError::InvalidInput(format!(
                    "Subagent spawn limit reached ({spawned} of {cap} agents spawned). \
Complete the remaining work directly with your tools instead of spawning more agents. \
If more agents are genuinely needed, ask the user to raise {MAX_SUBAGENTS_PER_SESSION_ENV}."
                )));
            }
        }

        // 4. (G3) Required-MCP-servers gate (claude AgentTool.tsx:367-409): if the
        // resolved agent declares `required_mcp_servers`, every required pattern
        // must match an MCP server that currently exposes tools (connected AND
        // authenticated). A missing requirement is a hard error listing the
        // unmatched patterns + the servers that DO have tools.
        //
        // Pending-wait (claude AgentTool.tsx): if any REQUIRED server is currently
        // PENDING (connecting / awaiting-OAuth / reconnecting), wait up to 30s
        // (500ms poll) for it to either expose tools or fail BEFORE checking
        // availability — so an agent that needs an OAuth/slow-login MCP server is
        // not spuriously failed mid-connect. The loop stops early when a required
        // server FAILS (no point waiting) or when none remain pending. Reads the
        // registry's INTERNAL pending/failed state (the public `McpStatus` UI
        // projection collapses Connecting/AwaitingOAuth/Reconnecting →
        // `Disconnected`; `servers_pending`/`servers_failed` read the real state).
        let required_mcp_servers = spawner.resolve_required_mcp_servers(&effective_type).await;
        if !required_mcp_servers.is_empty() {
            if let Some(reg) = &self.ctx.mcp_registry {
                // claude `he.name.toLowerCase().includes(pattern.toLowerCase())`:
                // a server name matches a required pattern by case-insensitive
                // substring. Reused for the pending + failed lists.
                let any_required = |names: &[String]| {
                    names.iter().any(|name| {
                        let n = name.to_lowercase();
                        required_mcp_servers
                            .iter()
                            .any(|pat| n.contains(&pat.to_lowercase()))
                    })
                };
                if any_required(&reg.servers_pending().await) {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                    while std::time::Instant::now() < deadline {
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        // A required server FAILED → stop waiting.
                        if any_required(&reg.servers_failed().await) {
                            break;
                        }
                        // No required server still pending → stop waiting.
                        if !any_required(&reg.servers_pending().await) {
                            break;
                        }
                    }
                }
            }
            let servers_with_tools: Vec<String> = match &self.ctx.mcp_registry {
                Some(reg) => reg.servers_with_tools().await,
                None => Vec::new(),
            };
            // claude `hasRequiredMcpServers` (loadAgentsDir.ts:229-242): every
            // required pattern must match (case-insensitive substring) at least
            // one server-with-tools.
            let missing: Vec<String> = required_mcp_servers
                .iter()
                .filter(|pattern| {
                    let p = pattern.to_lowercase();
                    !servers_with_tools
                        .iter()
                        .any(|server| server.to_lowercase().contains(&p))
                })
                .cloned()
                .collect();
            if !missing.is_empty() {
                let servers_list = if servers_with_tools.is_empty() {
                    "none".to_string()
                } else {
                    servers_with_tools.join(", ")
                };
                Self::emit_failed(
                    &bus,
                    &invocation_id,
                    "missing_required_mcp_servers",
                    started.elapsed().as_millis() as u64,
                )
                .await;
                return Err(ToolError::Internal(format!(
                    "Agent '{effective_type}' requires MCP servers matching: {}. \
MCP servers with tools: {servers_list}. \
Use /mcp to configure and authenticate the required MCP servers.",
                    missing.join(", ")
                )));
            }
        }

        // 5. The parent registry must be present for recursive dispatch.
        let parent_registry = ctx.subagent_registry.clone().ok_or_else(|| {
            ToolError::Internal(
                "AgentTool: parent ToolRegistry not threaded via ToolUseContext.subagent_registry"
                    .into(),
            )
        })?;

        // 6. Emit started (telemetry keys on the EFFECTIVE type, not the raw
        // optional input).
        Self::emit_started(
            &bus,
            &invocation_id,
            &effective_type,
            parsed.prompt.chars().count(),
        )
        .await;

        // (G11) Resolve the pre-spawn selection metadata (source / color /
        // resolved model / is_built_in) so we can emit claude's
        // `tengu_agent_tool_selected` (AgentTool.tsx:419-428). One cheap
        // catalog lookup via the spawner seam.
        // Claude Code 2.1.206 defaults a local subagent to background execution:
        // `run_in_background !== false`, with an agent definition's
        // `background: true` also forcing the async path. The binary then gates
        // the local group on background tasks being enabled. The
        // agent definition's `background` frontmatter flag is surfaced on
        // `SelectedAgentMeta.background`, so a `background: true` agent dispatches
        // async even when the caller omits `run_in_background`. The env kill-switch
        // forces the whole local-async group off — an explicit `run_in_background:
        // true` then runs SYNCHRONOUSLY. (claude gates only the LOCAL group with
        // `!dqt`; the remote path is separate and out of scope here.) The same
        // `run_in_background` value drives BOTH the telemetry `is_async` flag and
        // the async-dispatch branch below.
        let background_tasks_disabled = lingxi_core::host::env::is_env_truthy(
            std::env::var("LINGXI_DISABLE_BACKGROUND_TASKS")
                .ok()
                .as_deref(),
        );
        // Binary @292889340: `let Q=sv(),Y=z1e()&&!w,J=WA(); … let U=q||(o===!0||
        // G.background===!0||Q&&!w||Y||!w&&o!==!1)&&!J`. The ONLY negative gate on
        // the local-async group is `!J` = `!WA()` — the background-tasks
        // kill-switch. The fork flag `z1e()` appears as a POSITIVE term (`Y`,
        // fork forces background) and the plan predicate `Cc()` does not appear at
        // all, so neither may suppress async dispatch here. (Before 2.1.238 the
        // port ANDed in `is_pro_plan()`, which forced every Pro-plan subagent to
        // run synchronously; the binary never does that.)
        // 2.1.266 moved this decision into `vBo` (@2957xxx) and, in doing so,
        // factored the AUTO-background arm behind `!Pw(n)`:
        //
        // ```js
        // let d=e.isCoordinator&&!o||e.forceAsync||!o&&r!==!1;
        // let y=r===!0||n.background===!0||!Pw(n)&&d;
        // ```
        //
        // `Pw(e)` (@1514599) = `e.source==="built-in"&&e.agentType===Wy` — the
        // built-in web-fetch agent. It answers a question the caller is usually
        // waiting on (its own `whenToUse` tells the model to run it in the
        // foreground), so omitting `run_in_background` must NOT background it;
        // only an explicit `run_in_background: true`, or `background: true` in a
        // definition, still can. The two explicit arms stay outside the factor,
        // exactly as the binary has them.
        let mut run_in_background = should_run_in_background(BackgroundDecision {
            wants_background: parsed.run_in_background,
            definition_background: selected.background,
            is_builtin_web_fetch: selected.is_built_in && effective_type == WEB_FETCH_AGENT_TYPE,
            is_coordinator: self
                .ctx
                .coordinator_mode
                .as_ref()
                .is_some_and(|m| m.is_enabled()),
            caller_is_in_process_teammate,
            background_tasks_disabled,
        });
        // Claude's `agent.spawn` Mod chain is two-phase: `next(e)` first hands
        // the rewritten request back to this tool, then waits for the real
        // child to start. Keep the completion handle alive through all
        // revalidation and worktree setup; dropping it rejects `next(e)`.
        let original_type = effective_type.clone();
        let original_cwd = parsed.cwd.clone();
        let original_background = run_in_background;
        let mut spawn_input = json!({
            "tool_use_id": ctx.tool_use_id.as_ref().map(ToString::to_string).unwrap_or_default(),
            "prompt": parsed.prompt,
            "description": parsed.description,
            "subagentType": effective_type,
            "provider": if selected.is_built_in {
                json!({"plugin":"engine","tier":"core"})
            } else if selected.source == "plugin" {
                json!({"plugin":effective_type.split(':').next().unwrap_or("engine"),"tier":"user"})
            } else {
                json!({"plugin":"engine","tier":"core"})
            },
            "parentModel": main_loop_model_parent(&ctx).unwrap_or_else(|| self.ctx.default_model.clone()),
            "permissionMode": self.ctx.permission_gate.as_ref()
                .and_then(|gate| gate.permission_mode())
                .unwrap_or_else(|| self.ctx.permission_mode.wire_str().to_owned()),
            "background": run_in_background,
            "fork": is_fork,
        });
        if let Some(agent_id) = ctx.agent_id {
            spawn_input["parentAgentId"] = json!(agent_id.as_uuid().to_string());
        }
        if let Some(name) = parsed.name.as_deref() {
            spawn_input["name"] = json!(name);
        }
        if !is_fork {
            if let Some(model) = parsed.model.as_deref() {
                spawn_input["model"] = json!(model);
            }
            if let Some(profile) = parsed.model_profile.as_deref() {
                spawn_input["model_profile"] = json!(profile);
            }
        }
        if let Some(cwd) = parsed.cwd.as_deref() {
            spawn_input["cwd"] = json!(cwd);
        }
        let original_spawn_input = spawn_input.clone();
        let mut mod_start: Option<Box<dyn AgentSpawnStart>> = None;
        match spawner
            .begin_agent_spawn(spawn_input, ctx.agent_spawn_provenance.clone())
            .await
        {
            Ok(AgentSpawnAdmission::Bypass(_)) => {}
            Ok(AgentSpawnAdmission::Answered(answer)) => {
                self.release_spawn_reservation();
                let reason = answer
                    .get("deny")
                    .and_then(Value::as_str)
                    .unwrap_or("agent.spawn: a hook answered without passing the spawn on");
                return Err(ToolError::InvalidInput(format!(
                    "Subagent spawn denied by a plugin: {reason}"
                )));
            }
            Ok(AgentSpawnAdmission::Forwarded { input, start }) => {
                let rewritten = input != original_spawn_input;
                let next_type = input
                    .get("subagentType")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        ToolError::InvalidInput("agent.spawn has no subagentType".into())
                    })?;
                if next_type != effective_type {
                    let exists = !is_fork
                        && spawner
                            .agent_listing()
                            .await
                            .iter()
                            .any(|entry| entry.agent_type == next_type);
                    if !exists {
                        self.release_spawn_reservation();
                        return Err(ToolError::InvalidInput(format!(
                            "a hook's subagentType '{next_type}' names no agent this call can dispatch"
                        )));
                    }
                    if let Some(gate) = &self.ctx.permission_gate {
                        if let Some(source) = gate.agent_type_deny(next_type).await {
                            self.release_spawn_reservation();
                            return Err(ToolError::InvalidInput(format!(
                                "A plugin's agent.spawn hook rewrote this spawn to agent type '{next_type}', which a permission rule denies ({source}). Dispatch it directly."
                            )));
                        }
                    }
                    if spawner
                        .tools_denied_agent_types()
                        .await
                        .iter()
                        .any(|kind| kind == next_type)
                    {
                        self.release_spawn_reservation();
                        return Err(ToolError::InvalidInput(agent_type_tools_denied_error(
                            next_type,
                        )));
                    }
                    selected = spawner
                        .resolve_selection(next_type, input.get("model").and_then(Value::as_str))
                        .await;
                    effective_type = next_type.to_owned();
                }
                parsed.prompt = input["prompt"].as_str().unwrap_or_default().to_owned();
                parsed.description =
                    normalize_description_ws(input["description"].as_str().unwrap_or_default());
                parsed.model = input
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                parsed.model_profile =
                    match model_profile_after_hook(&input, parsed.model_profile.take()) {
                        Ok(profile) => profile,
                        Err(error) => {
                            self.release_spawn_reservation();
                            start.failed(error.to_string());
                            return Err(error);
                        }
                    };
                clear_model_overrides_when_forced(&mut parsed, model_overrides_forced || is_fork);
                if parsed.model_profile.is_some() && parsed.model.is_none() {
                    self.release_spawn_reservation();
                    start.failed("agent.spawn: model_profile requires a model".into());
                    return Err(ToolError::InvalidInput(
                        "agent.spawn: model_profile requires an explicit model".into(),
                    ));
                }
                parsed.cwd = input.get("cwd").and_then(Value::as_str).map(str::to_owned);
                if parsed.model
                    != original_spawn_input
                        .get("model")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                {
                    selected = spawner
                        .resolve_selection(&effective_type, parsed.model.as_deref())
                        .await;
                }
                if input["background"].as_bool() != Some(original_background)
                    || effective_type != original_type
                {
                    if caller_is_in_process_teammate
                        && input["background"] == true
                        && !original_background
                    {
                        self.release_spawn_reservation();
                        return Err(ToolError::InvalidInput(
                            "In-process teammates cannot spawn background agents; a plugin's agent.spawn hook backgrounded this one.".into()
                        ));
                    }
                    parsed.run_in_background = input["background"].as_bool();
                    run_in_background = should_run_in_background(BackgroundDecision {
                        wants_background: parsed.run_in_background,
                        definition_background: selected.background,
                        is_builtin_web_fetch: selected.is_built_in
                            && effective_type == WEB_FETCH_AGENT_TYPE,
                        is_coordinator: self
                            .ctx
                            .coordinator_mode
                            .as_ref()
                            .is_some_and(|m| m.is_enabled()),
                        caller_is_in_process_teammate,
                        background_tasks_disabled,
                    });
                }
                let effective_isolation_after_rewrite = parsed
                    .isolation
                    .as_deref()
                    .or(selected.isolation.as_deref());
                if parsed.cwd != original_cwd
                    && effective_isolation_after_rewrite == Some("worktree")
                {
                    self.release_spawn_reservation();
                    return Err(ToolError::InvalidInput(
                        "A plugin's agent.spawn hook set cwd on a spawn isolated in a worktree; cwd and isolation: \"worktree\" are mutually exclusive.".into()
                    ));
                }
                if effective_type != original_type {
                    let required = spawner.resolve_required_mcp_servers(&effective_type).await;
                    if !required.is_empty() {
                        let available = match &self.ctx.mcp_registry {
                            Some(registry) => registry.servers_with_tools().await,
                            None => Vec::new(),
                        };
                        let missing: Vec<_> = required
                            .iter()
                            .filter(|pattern| {
                                !available.iter().any(|server| {
                                    server.to_lowercase().contains(&pattern.to_lowercase())
                                })
                            })
                            .collect();
                        if !missing.is_empty() {
                            self.release_spawn_reservation();
                            return Err(ToolError::InvalidInput(format!(
                                "Agent '{}', named by a plugin's agent.spawn hook, requires MCP servers matching: {}; none has tools yet. Use /mcp to configure and authenticate them.",
                                effective_type,
                                missing
                                    .iter()
                                    .map(|value| value.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            )));
                        }
                    }
                }
                if rewritten {
                    if let Some(gate) = &self.ctx.permission_gate {
                        let rewritten_input = json!({
                            "subagent_type": effective_type,
                            "prompt": parsed.prompt,
                            "description": parsed.description,
                            "model": parsed.model,
                            "model_profile": parsed.model_profile,
                            "run_in_background": run_in_background,
                            "cwd": parsed.cwd,
                        });
                        if let Some(verdict) = gate.check_mod_query(
                            "Agent",
                            &rewritten_input,
                            &lingxi_core::host::permission_gate::PermissionCheckContext::default(),
                        ).await {
                            if verdict.verdict.rule.is_some() && verdict.verdict.decision != lingxi_core::host::permission_gate::ModToolCheckDecision::Allow {
                                self.release_spawn_reservation();
                                return Err(ToolError::InvalidInput(format!(
                                    "A plugin's agent.spawn hook rewrote this spawn into one a permission rule refuses: {} Dispatch it directly.",
                                    verdict.verdict.reason.unwrap_or_else(|| "permission denied".into())
                                )));
                            }
                        }
                    }
                }
                mod_start = Some(start);
            }
            Err(error) => {
                self.release_spawn_reservation();
                return Err(ToolError::Internal(format!(
                    "agent.spawn Mod dispatch failed: {error}"
                )));
            }
        }
        let is_async = run_in_background;
        Self::emit_agent_tool_selected(
            &bus,
            &selected.agent_type,
            &selected.resolved_model,
            &selected.source,
            selected.color.as_deref(),
            selected.is_built_in,
            false, // is_resume — resume path not modeled
            is_async,
            is_fork,
        )
        .await;

        // Worktree / cwd isolation — resolved BEFORE the sync/async branch,
        // matching claude 2.1.207 (`ye=null; if(Y==="worktree") ye=await
        // createAgentWorktree(agentWorktreeSlug(id))` precedes the
        // `run_in_background` branch, and the effective cwd `ge = l ??
        // ye?.worktreePath` is threaded into BOTH). When the caller requests
        // `isolation:"worktree"` or the selected agent definition declares
        // `isolation: worktree` (non-fork), create a git worktree (slug
        // `agent-<id>` → branch `worktree-agent-<id>` under `.lingxi/worktrees/`,
        // matching claude's scheme) and run the agent in it; an explicit `cwd`
        // takes precedence as the run dir (claude `cwd ?? worktreePath`).
        // `remote` is deferred (run local). The handle is held for the
        // post-completion keep/cleanup judgment: run HERE on the sync path, and
        // by the detached background lifecycle on the async path (claude hands
        // the `getWorktreeResult` closure to the task — see `dispatch_async`).
        // The model-facing `isolation` argument wins over the definition's
        // frontmatter; omitted args inherit `SelectedAgentMeta.isolation`.
        let effective_isolation = if is_fork {
            None
        } else {
            parsed
                .isolation
                .clone()
                .or_else(|| selected.isolation.clone())
        };
        // claude 2.1.238 @292890415, immediately after `z = s ?? G.isolation`:
        //
        // ```js
        // if(z&&J9(G)){ T(`[web-fetch agent] isolation:'${z}' ignored; the built-in
        //   web-fetch agent always runs as a local agent`); z=void 0 }
        // ```
        //
        // `J9(e)` (@287972xxx) = `e.source==="built-in" && e.agentType===KH`, so
        // BOTH halves are required — a user agent that happens to be named
        // `web-fetch` keeps its isolation. `selected.is_built_in` is claude's
        // `source === 'built-in'`. New in 2.1.238 (`oracle.sh count
        // 'web-fetch agent'` = 0 in 2.1.220, 5 in 2.1.238).
        let effective_isolation = match effective_isolation {
            Some(iso) if selected.is_built_in && effective_type == WEB_FETCH_AGENT_TYPE => {
                tracing::info!(
                    "[web-fetch agent] isolation:'{iso}' ignored; the built-in web-fetch agent always runs as a local agent"
                );
                None
            }
            other => other,
        };
        let mut agent_worktree: Option<lingxi_core::host::worktree::WorktreeHandle> = None;
        let mut resolved_cwd: Option<String> = if is_fork { None } else { parsed.cwd.clone() };
        if effective_isolation.as_deref() == Some("worktree") {
            let slug = format!("agent-{invocation_id}");
            match self.ctx.worktree.create_worktree(&slug, None, &[]).await {
                Ok(handle) => {
                    if resolved_cwd.is_none() {
                        resolved_cwd = Some(handle.path.to_string_lossy().into_owned());
                    }
                    agent_worktree = Some(handle);
                }
                Err(e) => {
                    Self::emit_failed(
                        &bus,
                        &invocation_id,
                        "worktree_create_failed",
                        started.elapsed().as_millis() as u64,
                    )
                    .await;
                    return Err(ToolError::Internal(format!(
                        "Cannot create agent worktree: {e}"
                    )));
                }
            }
        }

        // PLANNED #2/G13: async (`run_in_background`) spawn. claude returns an
        // `async_launched` payload immediately and drives the lifecycle detached
        // (AgentTool.tsx:686-764). The async seam (`spawn_async`) defaults to a
        // CLEAR error when unwired — surface it rather than silently falling back
        // to a sync spawn (the task forbids a silent wrong path). When the
        // production spawner overrides `spawn_async`, this branch returns the
        // `async_launched` JSON + registers `name → agentId` (G14, async-only).
        if run_in_background {
            return self
                .dispatch_async(
                    &bus,
                    &invocation_id,
                    started,
                    spawner.as_ref(),
                    &parsed,
                    &effective_type,
                    &selected,
                    is_fork,
                    &ctx,
                    budget.clone(),
                    origin_session_id,
                    parent_registry.clone(),
                    effective_isolation.clone(),
                    resolved_cwd,
                    agent_worktree,
                    mod_start,
                )
                .await;
        }

        // 8. Build the inheritance bundle and dispatch into the spawner.
        // The recursion-lock invariant: `parent_registry` is passed verbatim
        // into the bundle via `RegistryToolInvoker::new(parent_registry)`.
        // The budget Arc is cloned (no deep clone — `Arc::clone` only bumps
        // the refcount), so `Arc::ptr_eq` between parent + child holds.
        // (3b) Gate the subagent's tool dispatch with the same permission gate
        // the main loop uses, when one is wired. `None` → unconditional dispatch
        // (legacy). The gate rides into the spawner via the inheritance bundle.
        let mut invoker_impl =
            tool_api::tool_invoker_impl::RegistryToolInvoker::new(parent_registry.clone());
        if let Some(gate) = self.ctx.permission_gate.clone() {
            invoker_impl = invoker_impl.with_gate(gate);
        }
        let invoker: Arc<dyn lingxi_core::host::tool_invoker::ToolInvoker> =
            spawner.decorate_tool_invoker(Arc::new(invoker_impl));
        let inherit = SubagentInheritance {
            tool_invoker: invoker,
            budget: budget.clone(),
        };
        // Fork path: preserve the parent's history and replace its in-flight
        // assistant with the cloned tool-use/placeholder directive tail. The
        // directive text is already the trailing block, so on the fork path
        // `request.prompt` is unused as a seed (the spawner seeds an empty
        // prompt_messages — see handle.rs spawn note (A)).
        let fork_context_messages: Option<Vec<lingxi_core::types::ConversationMessage>> = if is_fork
        {
            let fork_msgs = lingxi_core::host::fork_subagent::build_forked_context(
                &parsed.prompt,
                &ctx.messages,
            );
            // Worktree notice (claude AgentTool.tsx:598-602): when the fork child
            // runs in an isolated worktree, claude appends
            // `build_worktree_notice(getCwd(), worktreeInfo.worktreePath)` to the
            // fork prefix. LingXi's fork path deliberately carries no
            // teammate/isolation/cwd overrides (see the request fields below), so
            // there is no worktree to notice here — do NOT fabricate one.
            Some(fork_msgs)
        } else {
            None
        };

        let request = SubagentSpawnRequest {
            stop_hook_scope: lingxi_core::host::subagent_spawn::SubagentStopScope::Session,
            agent_spawn_provenance: ctx.agent_spawn_provenance.clone(),
            teammate_color: None,
            // Propagate the RESOLVED effective type (fork → `fork`; omitted →
            // general-purpose; explicit-validated otherwise), not the raw input.
            subagent_type: effective_type.clone(),
            prompt: parsed.prompt.clone(),
            observer: selected.observer.clone().or_else(|| {
                ctx.observer
                    .as_ref()
                    .filter(|observer| observer.agent != effective_type)
                    .cloned()
            }),
            context_paths: parsed.context_paths.clone(),
            // AgentTool spawn-surface parity: thread the new params through.
            // `model` is mapped to the agent model override by the spawner; the
            // resolved `cwd`/`worktree` are computed above before the sync/async
            // branch. Remote/team-specific execution remains a higher-level
            // router concern.
            description: Some(parsed.description.clone()),
            // Fork path sends `model: None` (claude `model: undefined`) so the
            // FORK_AGENT's `Inherit` resolves to the parent model unchanged; the
            // explicit-model override is honored only on the non-fork path.
            model: if is_fork { None } else { parsed.model.clone() },
            model_profile: spawn_model_profile(&parsed, is_fork),
            // This is the synchronous arm after the resolved availability and
            // subscription gates above. Do not leak the caller's now-disabled
            // request bit into the child runtime snapshot.
            run_in_background,
            // Fork path carries no teammate/isolation/cwd overrides.
            name: if is_fork { None } else { parsed.name.clone() },
            team_name: None,
            // Permission mode belongs to the parent session and agent definition.
            mode: None,
            isolation: if is_fork {
                None
            } else {
                effective_isolation.clone()
            },
            // The RESOLVED cwd (the explicit `cwd` override, else the worktree
            // path for `isolation:"worktree"`) — the spawner sets
            // `SubagentContext.cwd` from this so the agent's tools operate there.
            cwd: resolved_cwd.clone(),
            // The resolved isolation worktree rides the request for shape
            // consistency with the async path; the SYNC judgment below stays
            // authoritative here (the spawner ignores the field).
            worktree: agent_worktree.clone(),
            // Fork-subagent carriers (codex #5): the byte-exact forked prefix the
            // spawner replays as the cache prefix, and the parent's already-
            // rendered system prompt bytes (TS `forkContextMessages` /
            // `override.systemPrompt = forkParentSystemPrompt`). Both `None` on
            // the non-fork path. A `None` `fork_parent_system_prompt` (the
            // orchestrator has not yet threaded `renderedSystemPrompt` onto
            // `ToolUseContext`) means the child runs with FORK_AGENT's empty
            // system prompt — functional, not byte-identical — see follow-ups.
            fork_context_messages,
            instruction_context: ctx.instruction_context.clone(),
            fork_parent_system_prompt: if is_fork {
                ctx.fork_parent_system_prompt.clone()
            } else {
                None
            },
            schema: None,
            structured_output_mode: StructuredOutputMode::default(),
            structured_output_parse_retries: 0,
            effort: None,
            // Keep the model's tool-use identity across both launch paths so
            // foreground registration and Mod `agent.spawn` name the same call.
            tool_use_id: ctx.tool_use_id.as_ref().map(ToString::to_string),
            creator_teammate_name: ctx.agent_name.clone(),
            creator_team_name: ctx.team_name.clone(),
            creator_agent_id: ctx.agent_id,
            origin_session_id,
            // Workflow-only spawn seam (defaults; unused by the Agent tool).
            system_prompt_override: None,
            system_prompt_addendum: None,
            additional_disallowed_tools: Vec::new(),
            // Child depth = this agent's depth + 1 (claude `spawnDepth =
            // z6(parentContext) + 1`). The spawner stamps it onto the child's
            // SubagentContext; the resolver applies the configured depth cap.
            depth: ctx.depth + 1,
            // The parent / main-loop model this child's `Inherit` + family aliases
            // resolve against (claude `getAgentModel(selectedAgent.model,
            // toolUseContext.options.mainLoopModel, …)`, AgentTool.tsx:418): the
            // LIVE session model at the top level (a mid-session `/model` switch is
            // reflected), or the IMMEDIATE parent subagent's resolved model on a
            // nested spawn (the RegistryToolInvoker seeds `main_loop_model` from the
            // dispatching runner's own model, mirroring runAgent.ts:678). When set
            // it takes precedence over the spawner's boot/live `default_model`.
            parent_model_override: main_loop_model_parent(&ctx),
            parent_model_profile_override: ctx.options.model_profile.clone(),
            forked_skill_name: None,
            forked_skill_attribution: None,
            forked_skill_effort: None,
            frozen_command_denies: Vec::new(),
            resumed_history: None,
            max_turns_override: None,
            max_output_tokens_per_turn: None,
            max_input_bytes_per_turn: None,
            query_source_label: None,
            correlation_id: None,
            model_attempt: None,
            handback_opt_in: !is_fork,
            parent_permission_mode: parent_permission_mode(&ctx, self.ctx.permission_gate.as_ref()),
            handback_enabled: None,
            restored_handback_state: None,
            restored_handback_history: Vec::new(),
            handback_ends_turn_enabled: None,
            restore_handback_start: None,
        };

        // Nested-progress bridge: `spawn_with_progress` feeds one String line per
        // subagent tool call; forward each as a `ToolProgress` the turn loop
        // re-emits as `SubagentActivity`, so the subagent's work renders under
        // this Agent cell. The forwarder ends when `spawn_with_progress` returns
        // (its `prog_tx` drops → `prog_rx` closes).
        let (prog_tx, mut prog_rx) = tokio::sync::mpsc::channel::<String>(64);
        let forward_progress = progress.clone();
        let forwarder = tokio::spawn(async move {
            while let Some(line) = prog_rx.recv().await {
                // (2.1.212 `--forward-subagent-text`) A forwarded subagent
                // assistant message arrives sentinel-wrapped as a JSON line; hand
                // its inner message through as a structured `forward_subagent_message`
                // payload so the turn loop's stream-json sink can re-emit it with
                // `parent_tool_use_id` set. Everything else is a plain nested
                // activity line.
                let data = if let Some((message, display_only)) =
                    decode_forward_subagent_tombstone(&line)
                {
                    serde_json::json!({
                        "forward_server_fallback_tombstone": {
                            "message": message,
                            "display_only": display_only,
                        }
                    })
                } else {
                    decode_forward_subagent_message(&line).map_or_else(
                        || serde_json::json!({ "subagent_activity": line }),
                        |message| serde_json::json!({ "forward_subagent_message": message }),
                    )
                };
                let _ = forward_progress
                    .send(tool_api::progress::ToolProgress {
                        tool_use_id: lingxi_core::types::ToolUseId::new(),
                        data,
                    })
                    .await;
            }
        });

        let spawner_owns_stop =
            spawner.owns_subagent_stop_hooks(false, origin_session_id, request.stop_hook_scope);
        let (outcome, foreground_worktree_result, terminal_hooks_owned) = if let Some(registry) =
            self.ctx.task_registry.clone()
        {
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let has_mod = mod_start.is_some();
            let run = foreground_task::run(
                spawner.clone(),
                request,
                inherit,
                prog_tx,
                progress.clone(),
                ctx.tool_use_id.clone(),
                registry,
                self.ctx.clone(),
                has_mod.then_some(started_tx),
            );
            let result = if let Some(start) = mod_start.take() {
                let mut run = Box::pin(run);
                tokio::select! {
                    biased;
                    receipt = started_rx => {
                        match receipt {
                            Ok((agent_id, model)) => start.started(agent_id, model),
                            Err(_) => start.failed("agent.spawn: the foreground child did not start".into()),
                        }
                        run.await
                    }
                    result = &mut run => {
                        start.failed("agent.spawn: the foreground child did not start".into());
                        result
                    }
                }
            } else {
                run.await
            };
            match result {
                foreground_task::ForegroundResult::Finished(result, worktree, hooks_owned) => {
                    (result, Some(worktree), hooks_owned)
                }
                foreground_task::ForegroundResult::Backgrounded(launch, task_id) => {
                    if let Some(name) = parsed.name.as_deref() {
                        if let Some(names) = self.ctx.agent_name_registry.as_ref() {
                            names.register(name, launch.agent_id).await;
                        } else {
                            spawner.register_name(name, launch.agent_id).await;
                        }
                    }
                    let can_read = ctx.subagent_registry.as_ref().is_some_and(|reg| {
                        reg.find_by_name("Read").is_some() || reg.find_by_name("Bash").is_some()
                    });
                    return Ok(async_launch_result(
                        launch,
                        Some(task_id),
                        &parsed.prompt,
                        &parsed.description,
                        &selected.resolved_model,
                        can_read,
                    ));
                }
            }
        } else {
            let result = if let Some(start) = mod_start.take() {
                let (started_tx, started_rx) = tokio::sync::oneshot::channel();
                let observer = Arc::new(ModStartReceiptObserver(std::sync::Mutex::new(Some(
                    started_tx,
                ))));
                let mut spawn = Box::pin(spawner.spawn_with_observer(
                    request,
                    inherit,
                    Some(prog_tx),
                    Some(observer),
                ));
                tokio::select! {
                    biased;
                    receipt = started_rx => {
                        match receipt {
                            Ok((agent_id, model)) => start.started(agent_id, model),
                            Err(_) => start.failed("agent.spawn: the foreground child did not start".into()),
                        }
                        spawn.await
                    }
                    result = &mut spawn => {
                        start.failed("agent.spawn: the foreground child did not start".into());
                        result
                    }
                }
            } else {
                spawner
                    .spawn_with_progress(request, inherit, Some(prog_tx))
                    .await
            };
            (result, None, spawner_owns_stop)
        };
        // `prog_tx` is now dropped → the forwarder drains and exits.
        let _ = forwarder.await;
        let duration_ms = started.elapsed().as_millis() as u64;

        // The Bash tool tells a synchronous subagent that a command it
        // backgrounds "is terminated when you give your final response". This
        // is where that becomes true: claude-code sweeps the finishing agent's
        // own shells in its agent-run cleanup (`nHn`), for any outcome. Only
        // shells this agent started are killed, so the main session's and other
        // agents' background work is untouched.
        if let Some(agent_id) = outcome.as_ref().ok().map(subagent_result_agent_id) {
            if let Some(registry) = self.ctx.task_registry.as_ref() {
                let retains_owned_work = matches!(
                    &outcome,
                    Ok(SubagentResult::Completed { handback: Some(state), .. }) if state.active
                ) && registry.agent_waiting_on_owned_work(agent_id).await;
                let killed = if retains_owned_work {
                    0
                } else {
                    registry.kill_background_shells_for_agent(agent_id).await
                };
                if killed > 0 {
                    tracing::debug!(
                        target: "tool_agent",
                        %agent_id,
                        killed,
                        "stopped the finishing subagent's background shells"
                    );
                }
            }
        }

        // Worktree lifecycle (claude `fe()` / `getWorktreeResult`): once the
        // agent finished, KEEP the worktree (return its path + branch) if it
        // left changes, else REMOVE it (auto-clean). The judgment itself lives
        // in `lingxi_core::host::worktree::agent_worktree_result` (shared with the ASYNC
        // lifecycle owner in the local_agent task handler); it runs for ANY
        // outcome so a worktree never leaks on a failed/killed agent.
        let worktree_result: Option<(String, String)> = match foreground_worktree_result {
            Some(result) => result,
            None => match &agent_worktree {
                Some(handle) => {
                    lingxi_core::host::worktree::agent_worktree_result(
                        self.ctx.worktree.as_ref(),
                        handle,
                    )
                    .await
                }
                None => None,
            },
        };

        match outcome {
            Ok(SubagentResult::Completed {
                handback,
                agent_id,
                content,
                usage,
                total_tool_use_count,
                total_duration_ms,
                total_tokens,
                assistant_message_count,
                response_char_count,
                last_request_id,
                ..
            }) => {
                // Internal back-compat event (kept).
                Self::emit_completed(&bus, &invocation_id, duration_ms, &effective_type).await;
                // (G11) claude `tengu_agent_tool_completed` (agentToolUtils.ts:322).
                Self::emit_agent_tool_completed(
                    &bus,
                    &effective_type,
                    &selected.resolved_model,
                    parsed.prompt.chars().count() as u64,
                    response_char_count,
                    assistant_message_count,
                    total_tool_use_count,
                    duration_ms,
                    total_tokens,
                    selected.is_built_in,
                    is_async,
                )
                .await;
                // (G11) claude `tengu_cache_eviction_hint` — only when a
                // last_request_id is present (agentToolUtils.ts:339).
                if let Some(req_id) = last_request_id.as_deref() {
                    Self::emit_cache_eviction_hint(&bus, req_id).await;
                }

                // claude `finalizeAgentTool` content[] (agentToolUtils.ts:348-356):
                // the agent's final text blocks (backward-scan applied in the
                // runner). Re-materialize the `{type:'text', text}` blocks for the
                // result's `content` array.
                let agent_id_str = agent_id.as_uuid().to_string();
                let waiting_on_owned_work = if let Some(registry) = self.ctx.task_registry.as_ref()
                {
                    registry.agent_waiting_on_owned_work(agent_id).await
                } else {
                    false
                };
                let raw_content_texts = handback_completion_texts(
                    handback.as_ref(),
                    &content,
                    parsed.name.as_deref().unwrap_or_else(|| {
                        if selected.agent_type.is_empty() {
                            &effective_type
                        } else {
                            &selected.agent_type
                        }
                    }),
                    waiting_on_owned_work,
                    self.ctx.task_registry.is_some(),
                );

                // (2.1.212) Indirect-prompt-injection hardening (claude
                // `ZDu`/`tHu`): run the output guard over the subagent's returned
                // text before it reaches the parent model — neutralize control /
                // model-layer tags, flag escalation patterns, and (when anything
                // reportable matched) prepend a warning block. The sanitized
                // blocks feed BOTH the result `content` array and the model-facing
                // string, and a `tengu_subagent_output_flagged` event is emitted.
                let sanitized =
                    lingxi_core::host::subagent_output_guard::sanitize_blocks(&raw_content_texts);
                if sanitized.any_reportable() {
                    Self::emit_subagent_output_flagged(&bus, &agent_id.to_string(), &sanitized)
                        .await;
                }
                let mut content_texts = sanitized.content;
                // `bft`'s harness notes (`Le`) are unshifted in front of the
                // report before anything else reads the content, so they reach
                // BOTH the structured `content[]` and the model-facing string.
                // Without this a turn-limited subagent rendered as
                // `(Subagent completed but returned no output.)` — the model was
                // told the run produced nothing, with no hint that it had merely
                // run out of turns and could be continued.
                if let Some(limit) = max_turns_reached_from_result(&content) {
                    let note = max_turns_harness_note(
                        limit,
                        &effective_type,
                        !raw_content_texts.is_empty(),
                    );
                    content_texts.insert(0, note);
                }
                // `EZe` — the auto-mode classifier's second consumer. A
                // subagent's hand-back is agent-authored untrusted output that
                // the parent model is about to act on, so auto mode reviews it
                // and PREPENDS its verdict:
                // `cu.content=[{type:"text",text:xg.warning},...cu.content]`.
                // In front of the harness notes too, matching upstream, which
                // unshifts onto the already-noted content. The work is always
                // delivered — this only ever adds a warning.
                let completed_work_review =
                    match handback.as_ref().and_then(|state| state.disposition) {
                        Some(
                            lingxi_core::host::handback::HandbackDisposition::Send
                            | lingxi_core::host::handback::HandbackDisposition::Flagged,
                        ) => None,
                        Some(lingxi_core::host::handback::HandbackDisposition::Withheld) => {
                            Some(String::new())
                        }
                        None => Some(content_texts.join("\n")),
                    };
                if let (Some(gate), Some(handed_back)) =
                    (self.ctx.permission_gate.as_ref(), completed_work_review)
                {
                    let transcript = self
                        .ctx
                        .subagent_spawner
                        .as_ref()
                        .and_then(|spawner| spawner.transcript_path(agent_id));
                    if let Some(review) = gate
                        .review_subagent_handoff(transcript.as_deref(), &handed_back)
                        .await
                    {
                        content_texts.insert(0, render_handoff_review(&review));
                    }
                }
                let content_blocks: Vec<Value> = content_texts
                    .iter()
                    .map(|t| json!({ "type": "text", "text": t }))
                    .collect();

                // The model-facing string (claude `mapToolResultToToolResultBlockParam`,
                // AgentTool.tsx:1340-1373) — consumed by orchestrator
                // `tool_result_to_model_text` (reads `data.model_content`). This is
                // what keeps the bytes the MODEL sees byte-identical to claude.
                let model_content = render_completed_model_content(
                    &content_texts,
                    &agent_id_str,
                    &effective_type,
                    total_tokens,
                    total_tool_use_count,
                    total_duration_ms,
                    worktree_result
                        .as_ref()
                        .map(|(p, b)| (p.as_str(), b.as_str())),
                );

                // claude completed return shape (AgentTool.tsx:1253-1260 +
                // finalizeAgentTool return). DROP the `subagent_type`/`result`
                // keys claude does not emit; surface claude's structured fields.
                // `usage` mirrors claude's Anthropic usage object
                // (`agentToolUtils.ts:238-256`). The nullable sub-objects
                // server_tool_use/service_tier/cache_creation are ALWAYS present
                // in claude's shape (emitted as `null` when absent), so we emit
                // them as `null` here too — null is the correct shape, NOT
                // zero-faked numerics. Faithfully populating them (web-search /
                // web-fetch request counts, service tier, 1h/5m cache split)
                // is DEFERRED pending the deeper `llm_runtime::ExecutionUsage` extension
                // (see SubagentUsage doc); the key SHAPE matches now.
                let mut data = json!({
                    "status": "completed",
                    "prompt": parsed.prompt,
                    "agentId": agent_id_str,
                    "agentType": effective_type,
                    "content": content_blocks,
                    "totalToolUseCount": total_tool_use_count,
                    "totalDurationMs": total_duration_ms,
                    "totalTokens": total_tokens,
                    "usage": {
                        "input_tokens": usage.input_tokens,
                        "output_tokens": usage.output_tokens,
                        "cache_creation_input_tokens": usage.cache_creation_input_tokens,
                        "cache_read_input_tokens": usage.cache_read_input_tokens,
                        "server_tool_use": serde_json::Value::Null,
                        "service_tier": serde_json::Value::Null,
                        "cache_creation": serde_json::Value::Null,
                    },
                    // model-facing string consumed by orchestrator
                    // `tool_result_to_model_text` (turn_loop.rs).
                    "model_content": model_content,
                    // R7 (single-fire): the child runner (`run_subagent`)
                    // already fired the canonical `SubagentStart` (collecting
                    // additionalContext for the child) and the child's own
                    // frontmatter `Stop`→`SubagentStop`, exactly as claude
                    // does inside `runAgent`. This engine-internal flag tells
                    // the orchestrator chokepoint to SKIP its own
                    // SubagentStart fire (no double-fire) and to fire only the
                    // session/plugin `SubagentStop` complement. Invisible to
                    // the model (it reads `model_content`) and never written
                    // to JSONL — same category as `model_content` itself.
                    "subagentHooksFired": true,
                    "subagentStopHooksOwned": terminal_hooks_owned,
                });
                // claude spreads `worktreePath`/`worktreeBranch` into `data` ONLY
                // when the worktree was KEPT (the agent left changes).
                if let Some((path, branch)) = &worktree_result {
                    if let Some(obj) = data.as_object_mut() {
                        obj.insert("worktreePath".into(), json!(path));
                        obj.insert("worktreeBranch".into(), json!(branch));
                    }
                }
                Ok(ToolCallResult {
                    data,
                    model_content: None,
                    new_messages: vec![],
                    context_modifier: None,
                    is_error: false,
                    mcp_meta: None,
                })
            }
            Ok(SubagentResult::Failed {
                agent_id, reason, ..
            }) => {
                Self::emit_failed(&bus, &invocation_id, "subagent_failed", duration_ms).await;
                Err(ToolError::SubagentFailed {
                    agent_id,
                    reason,
                    terminal_hooks_owned,
                })
            }
            Ok(SubagentResult::Killed { .. }) => {
                Self::emit_failed(&bus, &invocation_id, "killed", duration_ms).await;
                // The sync twin of upstream's kill sites. Before this the event
                // had an emitter and no caller — the run ended and the metric
                // that exists to count stopped runs never fired.
                Self::emit_agent_tool_terminated(
                    &bus,
                    &effective_type,
                    &selected.resolved_model,
                    duration_ms,
                    selected.is_built_in,
                    false,
                    ctx.depth,
                    "user_cancel_sync",
                )
                .await;
                Err(ToolError::Internal("Agent: subagent was killed".into()))
            }
            Err(lingxi_core::host::subagent_spawn::SubagentSpawnError::PoolFull) => {
                self.release_spawn_reservation();
                Self::emit_failed(
                    &bus,
                    &invocation_id,
                    "subagent_concurrency_cap",
                    duration_ms,
                )
                .await;
                Err(ToolError::InvalidInput(concurrent_subagent_limit_error(
                    lingxi_core::host::subagent_spawn::max_concurrent_subagents(),
                )))
            }
            // A plugin hook saying "no" is a POLICY outcome, not a crash.
            // Folding it into `Internal` told the user the runtime broke and
            // filed it under `spawn_error` — the variant existed for exactly
            // this distinction and nothing was matching on it.
            Err(lingxi_core::host::subagent_spawn::SubagentSpawnError::DeniedByHook(reason)) => {
                // 🚨 Give the lifetime slot back. The 200-per-session cap is
                // reserved BEFORE the spawn, and only the pool-full arms
                // released it — so a plugin denying a common spawn would walk
                // the session to "Subagent spawn limit reached" without a
                // single agent ever running. Pre-existing for Runtime/Internal,
                // but this is the first spawn error a plugin raises on purpose
                // and repeatedly.
                self.release_spawn_reservation();
                Self::emit_failed(&bus, &invocation_id, "spawn_denied_by_hook", duration_ms).await;
                Err(ToolError::InvalidInput(format!("Agent: {reason}")))
            }
            Err(e) => {
                Self::emit_failed(&bus, &invocation_id, "spawn_error", duration_ms).await;
                Err(ToolError::Internal(format!("Agent: spawner failed: {e}")))
            }
        }
    }
}

/// Test-only: serializes every test whose prompt output depends on a
/// process-global env gate such as `LINGXI_FORK_SUBAGENT`, which
/// `build_prompt_with_async_agents` consults.
///
/// Lives HERE, not inside `agent_test`, so the description tests below can
/// reach it. They previously could not, and their module doc claimed they
/// "never" touch the process env — true of the background-tasks kill switch it
/// was written about, but NOT of the fork gate, which is read inside the
/// builder. That is how they raced.
#[cfg(test)]
pub(crate) static AGENT_PROMPT_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod forwarded_server_fallback_progress_tests {
    use super::decode_forward_subagent_tombstone;

    #[test]
    fn tombstone_progress_preserves_the_complete_row_and_display_flag() {
        let row = serde_json::json!({
            "type": "assistant",
            "uuid": "outer-row-id",
            "timestamp": "2026-10-04T00:00:00.000Z",
            "message": {"id": "inner-message-id", "content": []},
        });
        let line = serde_json::json!({
            lingxi_core::host::subagent_spawn::FORWARD_SUBAGENT_SERVER_FALLBACK_TOMBSTONE_SENTINEL: {
                "message": row,
                "display_only": true,
            }
        })
        .to_string();
        let (decoded, display_only) =
            decode_forward_subagent_tombstone(&line).expect("typed progress envelope");
        assert_eq!(decoded["uuid"], "outer-row-id");
        assert_eq!(decoded["message"]["id"], "inner-message-id");
        assert!(display_only);
    }
}

#[cfg(test)]
#[path = "agent_test.rs"]
mod agent_test;

/// F-cluster: the two `l`-gated bullets at the tail of the Agent tool
/// description (binary `mvd` @234647480).
///
/// These tests pass `async_agents_available` EXPLICITLY, so they cannot race
/// the `LINGXI_DISABLE_BACKGROUND_TASKS` manipulation in `agent_test.rs`.
///
/// ⚠️ That is NOT sufficient on its own: the builder also consults
/// `LINGXI_FORK_SUBAGENT` internally (`is_fork_subagent_enabled`), which the
/// fork tests in `agent_test.rs` flip. Every test here therefore runs under
/// [`prompt_env`], which takes the shared [`AGENT_PROMPT_ENV_LOCK`] and clears
/// that variable. An earlier version of this doc claimed these tests never
/// touch the process env; the fork gate made that false and the suite failed
/// intermittently on whichever test happened to run alongside a fork test.
#[cfg(test)]
mod f_description_l_gate_tests {
    use super::{AgentTool, AGENT_PROMPT_ENV_LOCK};

    /// Pin the non-fork description (`LINGXI_FORK_SUBAGENT=0`). 2.1.232 defaults
    /// the feature ON for interactive sessions; these tests lock the `l`-gate
    /// tail bullets of the non-fork form.
    fn prompt_env<T>(body: impl FnOnce() -> T) -> T {
        let _guard = AGENT_PROMPT_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let saved = std::env::var("LINGXI_FORK_SUBAGENT").ok();
        std::env::set_var("LINGXI_FORK_SUBAGENT", "0");
        let out = body();
        match saved {
            Some(value) => std::env::set_var("LINGXI_FORK_SUBAGENT", value),
            None => std::env::remove_var("LINGXI_FORK_SUBAGENT"),
        }
        out
    }

    fn agents() -> Vec<lingxi_core::host::subagent_spawn::SubagentListingEntry> {
        vec![lingxi_core::host::subagent_spawn::SubagentListingEntry {
            agent_type: "general-purpose".into(),
            when_to_use: "anything".into(),
            when_to_use_lean: None,
            tools_description: "All tools".into(),
        }]
    }

    /// Every assertion in this module targets the SHORT (lean) arm of the
    /// binary's `m = qk(model)` split, so the model is pinned to a lean-prompt
    /// one. `general_purpose_available` is `true` — `agents()` lists it.
    const LEAN_MODEL: Option<&str> = Some("claude-opus-5");

    /// Binary @234659244: the first tail bullet is a TERNARY on
    /// `l=!DT()&&!aZ()`. `l` is TRUE by default (`DT()` @230660586 reads an
    /// unset env; `aZ()` @227950071 is the in-process-SDK
    /// `AsyncLocalStorage.getStore()!==void 0`, false outside the SDK), so the
    /// DEFAULT description carries the "final report" arm, NOT the
    /// "final message is returned to you as the tool result" arm.
    #[test]
    fn first_tail_bullet_uses_the_l_true_arm_by_default() {
        let p = prompt_env(|| {
            AgentTool::build_prompt_with_async_agents(&agents(), &[], false, true, LEAN_MODEL, true)
        });
        assert!(
            p.contains(
                "\n\n- The agent's final report is not shown to the user \u{2014} relay what matters.\n- Use "
            ),
            "l=true must emit the final-REPORT bullet as the first tail bullet"
        );
        assert!(
            !p.contains("The agent's final message is returned to you as the tool result"),
            "the l=false arm must not appear when l is true"
        );
        // Exactly one "relay what matters" bullet — a ternary, not two bullets.
        assert_eq!(p.matches("relay what matters").count(), 1);
    }

    /// The `l=false` arm (binary @234659430) — reachable when background tasks
    /// are disabled.
    #[test]
    fn first_tail_bullet_uses_the_l_false_arm_when_async_agents_unavailable() {
        let p = prompt_env(|| {
            AgentTool::build_prompt_with_async_agents(
                &agents(),
                &[],
                false,
                false,
                LEAN_MODEL,
                true,
            )
        });
        assert!(p.contains(
            "\n\n- The agent's final message is returned to you as the tool result; it is not shown to the user \u{2014} relay what matters.\n- Use "
        ));
        assert!(!p.contains("The agent's final report is not shown to the user"));
    }

    /// Binary 2.1.238 @292443853: `w = u && !i ? (o ? SHORT : LONG) : ""`. With
    /// the fork feature off (`o=false`) the LONG arm runs, and it does NOT stop
    /// at "so the user can interject." — two more sentences follow. Anchored
    /// with `ends_with` because the truncated text is a strict PREFIX of the
    /// correct text, so `contains` on the old string does not discriminate.
    #[test]
    fn background_bullet_carries_the_full_long_arm() {
        let p = prompt_env(|| {
            AgentTool::build_prompt_with_async_agents(&agents(), &[], false, true, LEAN_MODEL, true)
        });
        assert!(
            p.ends_with(
                "\n- Subagents run in the background by default; you'll be notified when one completes. Pass `run_in_background: false` only when your very next action depends on the result and nothing else could usefully happen while it runs — otherwise background it so the user can interject. Never fabricate or predict a pending agent's results \u{2014} the notification is never something you write yourself; if the user asks before it arrives, say it's still running."
            ),
            "background bullet must be the FULL long arm and must be last; got tail: {:?}",
            &p[p.len().saturating_sub(400)..]
        );
    }

    /// `T = l && !o ? … : ""` — when `l` is false there is NO background bullet
    /// at all, not a shortened one.
    #[test]
    fn background_bullet_absent_when_async_agents_unavailable() {
        let p = prompt_env(|| {
            AgentTool::build_prompt_with_async_agents(
                &agents(),
                &[],
                false,
                false,
                LEAN_MODEL,
                true,
            )
        });
        assert!(
            !p.contains("Subagents run in the background"),
            "l=false must drop the background bullet entirely"
        );
        assert!(p.ends_with(
            "- `isolation: \"worktree\"` gives the agent its own git worktree (auto-cleaned if unchanged)."
        ));
    }

    /// Binary tail order: `${C}\n\n- <final-report>\n- Use ${mf} …\n- Each agent
    /// type's …\n- \`isolation: "worktree"\` …${b}${T}${v}` with `b`/`v` empty
    /// by default ⇒ the background bullet is LAST.
    #[test]
    fn tail_bullet_order_matches_the_binary() {
        let p = prompt_env(|| {
            AgentTool::build_prompt_with_async_agents(&agents(), &[], false, true, LEAN_MODEL, true)
        });
        let i1 = p.find("- The agent's final report is not shown").unwrap();
        let i2 = p.find("- Use SendMessage").unwrap();
        let i3 = p.find("- Each agent type's model").unwrap();
        let i4 = p.find("- `isolation: \"worktree\"`").unwrap();
        let i5 = p.find("- Subagents run in the background").unwrap();
        assert!(
            i1 < i2 && i2 < i3 && i3 < i4 && i4 < i5,
            "tail bullet order: {i1} {i2} {i3} {i4} {i5}"
        );
    }

    /// The coordinator branch (`if(t) return g`) returns before any tail bullet,
    /// so the `l` gate cannot leak into it.
    #[test]
    fn coordinator_branch_has_no_tail_bullets_either_way() {
        for l in [true, false] {
            let p = prompt_env(|| {
                AgentTool::build_prompt_with_async_agents(&agents(), &[], true, l, LEAN_MODEL, true)
            });
            assert!(!p.contains("relay what matters"));
            assert!(!p.contains("Subagents run in the background"));
        }
    }

    /// [round-3 review, finding 6] The current system-reminder catalog
    /// (`agent_listing_reminder_message`, a different crate) is built from
    /// `builtin_agent_definitions()` + the disk catalog — neither of which
    /// carries the synthetic `fusion` entry `append_fusion_listing` adds to
    /// `agents`. Before this fix the default-gate pointer line ignored
    /// `agents` entirely, so a stock session (Fusion enabled, executor wired)
    /// never saw the `fusion` entry's `when_to_use` guidance or its
    /// "about 4-5x the cost" warning through ANY surface — the entry was
    /// computed and then silently discarded. `caller`-supplied `agents`
    /// already carries the entry only when the Fusion surface is enabled and
    /// an executor is wired (`append_fusion_listing`'s own gate) and already
    /// reflects an `Agent(fusion)` deny rule (the caller's deny-filter runs
    /// after the append) — so folding it into the pointer line here is exact,
    /// not a new gate.
    #[test]
    fn default_gate_still_advertises_fusion_when_the_entry_is_present() {
        let mut fixture = agents();
        fixture.push(lingxi_core::host::subagent_spawn::SubagentListingEntry {
            agent_type: "fusion".into(),
            when_to_use: crate::agent::FUSION_WHEN_TO_USE.into(),
            when_to_use_lean: None,
            tools_description: "Fusion deliberation (read-only panel)".into(),
        });
        let p = prompt_env(|| {
            AgentTool::build_prompt_with_async_agents(&fixture, &[], false, true, LEAN_MODEL, true)
        });
        assert!(
            p.contains(
                "Available agent types are listed in <system-reminder> messages in the conversation."
            ),
            "the default-gate pointer sentence must still be present verbatim; got: {p:?}"
        );
        assert!(
            p.contains("`fusion` is also available")
                && p.contains("Parallel multi-model deliberation"),
            "the fusion entry's when_to_use guidance must reach the model under the default gate; got: {p:?}"
        );
    }

    /// Sibling: when `agents` carries no `fusion` entry (surface disabled or
    /// no executor wired — `append_fusion_listing`'s own no-op case), the
    /// default-gate pointer line must stay byte-identical to the pre-fix
    /// text — no stray advertisement for a feature that is not actually
    /// available.
    #[test]
    fn default_gate_omits_fusion_notice_when_the_entry_is_absent() {
        let p = prompt_env(|| {
            AgentTool::build_prompt_with_async_agents(&agents(), &[], false, true, LEAN_MODEL, true)
        });
        assert!(
            !p.contains("also available"),
            "no fusion entry ⇒ no notice; got: {p:?}"
        );
    }
}

/// Round-3 review findings 11/19 (rework): narrow, direct unit tests for the
/// two pure decision points a Fusion cancel's spawn-reservation refund
/// depends on — `panels_proven_spawned` (does a progress event prove a
/// panel task genuinely reached the spawner?) and `fusion_error_is_preflight`
/// (does a terminal `FusionError` guarantee zero provider calls?). Kept
/// inline rather than in `agent_test.rs` so this fixer's changes stay
/// isolated to files it owns — a full end-to-end run through
/// `AgentTool::execute` with a fake executor belongs there, alongside the
/// existing `fusion_cancelled_after_panels_spawned_keeps_reservation_charged`
/// / `fusion_cancelled_with_only_prespawn_progress_releases_the_full_reservation`
/// pair. [round-5 review, finding 10] The third case that pair asked for —
/// an executor that emits `PanelsDispatched` and then `Cancelled` with NO
/// completion event at all — now exists there as
/// `fusion_runtime_errors_refund_the_panels_that_never_resolved`, which
/// asserts the resulting `get_total_agent_spawns()` total rather than merely
/// that the call returned `Err`.
#[cfg(test)]
mod round3_finding_11_19_spawn_signal_tests {
    use super::{fusion_error_is_preflight, panels_proven_spawned};

    /// The round-3 pins below are stated through the same predicate the
    /// production forwarder calls; `Some`/`None` is byte-for-byte the
    /// `true`/`false` the old `stage_proves_panel_spawned` returned.
    fn stage_proves_panel_spawned(stage: &FusionStage) -> bool {
        panels_proven_spawned(stage).is_some()
    }
    use lingxi_core::host::{FusionError, FusionStage};

    /// [round-5 review, finding 10] The proving events carry the RESOLVED
    /// panel count, which is what lets `call_fusion`'s `Err` and drop paths
    /// refund the reserved-but-never-dispatched surplus. The old
    /// `AtomicBool` threw this number away.
    #[test]
    fn a_proving_stage_reports_the_resolved_panel_count() {
        assert_eq!(
            panels_proven_spawned(&FusionStage::PanelsDispatched { total: 2 }),
            Some(2),
            "PanelsDispatched must report the resolved total, not just `true`"
        );
        assert_eq!(
            panels_proven_spawned(&FusionStage::RunningPanels {
                completed: 1,
                total: 2
            }),
            Some(2),
            "RunningPanels with a completion must report the resolved total"
        );
    }

    /// Nonzero-ness must keep meaning exactly what the old boolean meant,
    /// even against an executor reporting a nonsensical `total: 0`: a run
    /// that proved a spawn must never be refunded as if no panel existed.
    #[test]
    fn a_proving_stage_never_reports_zero_panels() {
        assert_eq!(
            panels_proven_spawned(&FusionStage::PanelsDispatched { total: 0 }),
            Some(1)
        );
        assert_eq!(
            panels_proven_spawned(&FusionStage::RunningPanels {
                completed: 2,
                total: 0
            }),
            Some(2)
        );
    }

    #[test]
    fn a_non_proving_stage_reports_no_panel_count() {
        assert_eq!(
            panels_proven_spawned(&FusionStage::RunningPanels {
                completed: 0,
                total: 3
            }),
            None,
            "the pre-spawn RunningPanels event must not license keeping ANY slot charged"
        );
        assert_eq!(panels_proven_spawned(&FusionStage::ResolvingModels), None);
    }

    /// The defect: before this fix, the ENTIRE window between "N panel
    /// tasks spawned and calling the provider" and "the first panel
    /// finishes" produced no signal a caller could use to tell it apart
    /// from "no panel task exists yet" — a cancel landing there refunded
    /// the whole reservation for panels that really ran.
    #[test]
    fn panels_dispatched_proves_a_panel_spawned_even_with_zero_completions() {
        assert!(stage_proves_panel_spawned(&FusionStage::PanelsDispatched {
            total: 3
        }));
    }

    #[test]
    fn pre_spawn_running_panels_with_zero_completed_does_not_prove_a_spawn() {
        assert!(!stage_proves_panel_spawned(&FusionStage::RunningPanels {
            completed: 0,
            total: 3
        }));
    }

    #[test]
    fn running_panels_with_at_least_one_completed_proves_a_spawn() {
        assert!(stage_proves_panel_spawned(&FusionStage::RunningPanels {
            completed: 1,
            total: 3
        }));
    }

    #[test]
    fn every_other_stage_does_not_prove_a_spawn() {
        for stage in [
            FusionStage::ResolvingModels,
            FusionStage::ReservingBudget,
            FusionStage::Analyzing,
            FusionStage::Completed,
            FusionStage::Failed,
            FusionStage::Cancelled,
        ] {
            assert!(
                !stage_proves_panel_spawned(&stage),
                "{stage:?} must not be mistaken for proof a panel spawned"
            );
        }
    }

    /// [round-3 review, finding 12] `AllPanelsFailedPreflight` guarantees
    /// zero provider calls (the orchestrator only returns it when EVERY
    /// panel's `error_category` is `"spawn"`) and must release the full
    /// reservation, same as every other preflight variant.
    #[test]
    fn all_panels_failed_preflight_is_treated_as_preflight() {
        assert!(fusion_error_is_preflight(
            &FusionError::AllPanelsFailedPreflight
        ));
    }

    /// Negative guard: the general `AllPanelsFailed` can cover panels that
    /// DID call a provider and lost — it must stay charged.
    #[test]
    fn general_all_panels_failed_stays_charged_not_preflight() {
        assert!(!fusion_error_is_preflight(&FusionError::AllPanelsFailed));
    }
}
