// lingxi-code/crates/core/src/settings/schema.rs
//! `SettingsJson` — strongly-typed mirror of claude-code's `settings.json`.
//!
//! Field naming policy: keep camelCase on the wire (claude-code emits it),
//! `snake_case` in Rust via `#[serde(rename_all = "camelCase")]`. New fields
//! land as `Option<T>` so deserializing an older file never fails on a
//! missing key — this matches v3 Event/Effect stability policy.
//!
//! Unknown keys are tolerated-and-ignored across the board: claude-code's
//! zod `SettingsSchema` is explicitly `.passthrough()` (`types.ts:1072`,
//! consumed via `safeParse` at `settings.ts:219`), so unknown keys never fail
//! a TS load either. Typed fields exist so the engine can ACCESS known
//! values — they are not a load-time acceptance gate.
//!
//! KNOWN DIVERGENCE (safe while nothing round-trips this struct to disk):
//! TS `.passthrough()` RETAINS unknown keys in the parsed/merged in-memory
//! settings; this typed struct DROPS them, so a merged `SettingsJson`
//! re-serialized to disk would lose them. No Rust path does that today —
//! `/effort`, `ConfigTool`, and the migrations crate all write settings via
//! raw `serde_json::Map` read-modify-write, which preserves unknown keys.
//!
//! `$schema` is NOT emitted (claude-code @ commit 6a25909 doesn't emit one)
//! but is kept as a typed field so a file that carries one round-trips it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

// This key is an upstream passthrough setting read with `=== true`, rather
// than a schema-validated boolean. Preserve the presence of null/nonboolean
// values as explicit false so they override a lower tier without rejecting it.
fn deserialize_code_review_suggestion<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Value::deserialize(deserializer).map(|value| Some(value == Value::Bool(true)))
}

/// Per-field merge strategy.
///
/// The merger ([`crate::settings::merger`]) consults [`strategy_for`] to pick
/// the right combinator for a given JSON key. Defaults to [`MergeStrategy::Override`]
/// for any key not explicitly registered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeStrategy {
    /// Concatenate the two arrays and drop duplicates while preserving order.
    ConcatDedup,
    /// Recurse into the two objects, merging key-by-key under their own strategies.
    DeepMerge,
    /// Later source wins. Used for scalars and any field not in the table.
    Override,
}

/// Per-field strategy table. Order does not matter; the lookup is linear.
///
/// Entries here MUST match spec §7 wire identifiers byte-for-byte.
pub const MERGE_STRATEGIES: &[(&str, MergeStrategy)] = &[
    // Array-merge fields (spec §7).
    ("trustedDirectories", MergeStrategy::ConcatDedup),
    ("additionalDirectories", MergeStrategy::ConcatDedup),
    ("enabledTools", MergeStrategy::ConcatDedup),
    ("additionalIncludes", MergeStrategy::ConcatDedup),
    ("lingxiMdExcludes", MergeStrategy::ConcatDedup),
    // `companyAnnouncements` is an array field; CC's `settingsMergeCustomizer`
    // concat-dedups all arrays across settings tiers, so it merges the same way
    // as the other array keys (later-tier entries appended, dupes dropped).
    ("companyAnnouncements", MergeStrategy::ConcatDedup),
    // Object-merge fields (spec §7).
    ("sandbox", MergeStrategy::DeepMerge),
    ("hooks", MergeStrategy::DeepMerge),
    ("permissions", MergeStrategy::DeepMerge),
    ("policyHelpers", MergeStrategy::DeepMerge),
    ("spellcheck", MergeStrategy::DeepMerge),
    ("additionalMarketplaces", MergeStrategy::DeepMerge),
    ("enabledPlugins", MergeStrategy::DeepMerge),
    ("pluginConfigs", MergeStrategy::DeepMerge),
    ("extraKnownMarketplaces", MergeStrategy::DeepMerge),
    // Per-model effort / cap overrides. Nested `maxEffortLevel` is folded at
    // the consumer (lowest wins); DeepMerge keeps sibling model keys.
    ("modelSettings", MergeStrategy::DeepMerge),
    // `attribution` is a plain object of optional strings, so the customizer's
    // array special-case never applies and it deep-merges per field: a layer
    // naming only `pr` leaves an inherited `commit` in place.
    ("attribution", MergeStrategy::DeepMerge),
    // NB: `outputStyle` is intentionally NOT here — TS types it as a string and
    // merges it scalar-override (settingsMergeCustomizer special-cases only
    // arrays), so it falls through to the default Override strategy.
    // LingXi extension — deep-merge so multiple settings layers can each
    // declare a subset of provider profiles.
    ("providers", MergeStrategy::DeepMerge),
    // LingXi extension — deep-merge so multiple settings layers can each
    // contribute routing aliases, fallback chains, and retry policy.
    ("routing", MergeStrategy::DeepMerge),
    // LingXi extension — Fusion multi-model deliberation. Deep-merge objects;
    // arrays (panel lists, dimensions, allowedProfiles) replace as a whole.
    ("fusion", MergeStrategy::DeepMerge),
    // Managed model-restriction map — deep-merge (CC `settingsMergeCustomizer`
    // leaves objects to lodash's recursive merge; only specific arrays concat).
    // NB: `availableModels` (array) and `enforceAvailableModels` (scalar) are
    // deliberately NOT here — they fall through to the default Override, matching
    // CC returning the source array for non-concat arrays.
    ("modelOverrides", MergeStrategy::DeepMerge),
    // `vimInsertModeRemaps` — deep-merge, matching `merger::merge`'s
    // `merge_string_map` for this field (a one-level record has no nested
    // structure, so a per-key union with `next` winning IS the deep merge).
    // It was MISSING here while `merge` deep-merged it: the table and the
    // merger disagreed, so `merger::merge_raw_layer` — and through it the
    // desktop settings snapshot — took Override for a field the engine
    // actually unions. (`tracer` was unaffected: it treats `DeepMerge` and
    // `Override` identically, so only a `ConcatDedup` gap would reach it.)
    // Pinned in both directions by
    // `merger::tests::raw_layer_merge_agrees_with_the_typed_merge` and
    // `merger::tests::every_field_merge_combines_is_registered_and_fixtured`.
    ("vimInsertModeRemaps", MergeStrategy::DeepMerge),
    // HTTP-hook security allowlists (H-BIN-12). Both are arrays, and CC's
    // `settingsMergeCustomizer` (`ipe`) concat-dedups EVERY array except
    // `fallbackModel` (`WSm(e,t)=Mo([...e,...t])`); both describe strings say
    // verbatim "Arrays merge across settings sources (same semantics as
    // allowedMcpServers)." So both are ConcatDedup.
    ("allowedHttpHookUrls", MergeStrategy::ConcatDedup),
    ("httpHookAllowedEnvVars", MergeStrategy::ConcatDedup),
];

/// Look up the merge strategy for a field name.
///
/// Returns `None` for unknown fields; callers default unknown fields to
/// [`MergeStrategy::Override`] (later source wins).
#[must_use]
pub fn strategy_for(field: &str) -> Option<MergeStrategy> {
    MERGE_STRATEGIES
        .iter()
        .find_map(|(k, v)| (*k == field).then_some(*v))
}

/// Execution backend requested for teammates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum TeammateMode {
    /// Choose the current terminal backend automatically.
    #[serde(rename = "auto")]
    Auto,
    /// Run inside the existing process.
    #[serde(rename = "in-process")]
    InProcess,
    /// Require terminal panes.
    #[serde(rename = "tmux")]
    Tmux,
    /// Require the native iTerm2 backend.
    #[serde(rename = "iterm2")]
    ITerm2,
}

/// Prompt-cache lifetime accepted by Claude Code 2.1.290 settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum PromptCacheTtl {
    /// Keep the cache for five minutes.
    #[serde(rename = "5m")]
    FiveMinutes,
    /// Keep the cache for one hour.
    #[serde(rename = "1h")]
    OneHour,
}

/// Main and non-main cache TTL settings separated from the full settings tree
/// for request-time LLM policy sampling.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PromptCacheTtlSettings {
    /// `promptCacheTtl` applied to main-query sources.
    pub main: Option<PromptCacheTtl>,
    /// `subagentPromptCacheTtl` applied to every non-main query source.
    pub subagent: Option<PromptCacheTtl>,
}

fn deserialize_prompt_cache_ttl<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<PromptCacheTtl>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(match value.as_str() {
        Some("5m") => Some(PromptCacheTtl::FiveMinutes),
        Some("1h") => Some(PromptCacheTtl::OneHour),
        _ => None,
    })
}

/// Mirror of claude-code's `settings.json` shape.
///
/// All fields `Option<T>` so a partial file (one layer of the 4-layer stack)
/// can omit a field without it showing up as `Some(Default::default())` —
/// "absent" must round-trip distinctly from "explicitly set to default".
///
/// Unknown keys are tolerated-and-ignored, matching claude-code's zod
/// `SettingsSchema` `.passthrough()` (`types.ts:1072`; `safeParse` at
/// `settings.ts:219`). Known fields keep their typed parses. Tolerance also
/// un-breaks settings files carrying keys written by `ConfigTool`, `/effort`
/// (`effortLevel`), or the migrations subsystem (`env`,
/// `skipDangerousModePermissionPrompt`, `enableAllProjectMcpServers`, …) —
/// under the previous `deny_unknown_fields` strictness any such key made the
/// whole-file load fail, and production callers' `.ok()` then silently
/// dropped the entire settings layer.
/// `settings.attribution` — per-trailer overrides for the git attribution lines.
///
/// Both fields are optional and independent: naming only `commit` leaves the PR
/// trailer at its default. An empty string DISABLES that trailer, which is why
/// these are `Option<String>` rather than `String` — "unset" and "set to empty"
/// must stay distinguishable.
fn deserialize_effort<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
    levels: &[&str],
) -> Result<Option<String>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value
        .as_str()
        .filter(|level| levels.contains(level))
        .map(str::to_string))
}

fn deserialize_persisted_effort<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    deserialize_effort(deserializer, &["low", "medium", "high", "xhigh"])
}

fn deserialize_max_effort<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    deserialize_effort(deserializer, &crate::host::effort::LEVELS)
}

fn deserialize_model_settings<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<indexmap::IndexMap<String, ModelSettings>>, D::Error> {
    let mut value = serde_json::Value::deserialize(deserializer)?;
    if let Some(entries) = value.as_object_mut() {
        entries.retain(|key, _| !crate::host::effort_table::object_prototype_key(key));
    }
    serde_json::from_value(value).map_err(serde::de::Error::custom)
}

/// Per-model effort overrides (`modelSettings.<model>`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelSettings {
    /// Persisted effort for this model.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_persisted_effort"
    )]
    pub effort_level: Option<String>,
    /// Cap for this model; replaces the top-level `maxEffortLevel`. `"max"`
    /// exempts the model from the top-level cap.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_max_effort"
    )]
    pub max_effort_level: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Attribution {
    /// Replaces the `Co-Authored-By:` commit trailer. Empty string = no trailer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Replaces the PR-body attribution line. Empty string = no line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<String>,
}

/// Product usage region, independent of a cloud deployment/signing region.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderRegion {
    /// Connections intended for mainland China.
    ChinaMainland,
    /// Connections intended for international use.
    #[default]
    International,
}

impl ProviderRegion {
    /// Stable settings representation, shared by every client surface.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ChinaMainland => "china_mainland",
            Self::International => "international",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SettingsJson {
    /// Native Fast mode default and session-only opt-in policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast_mode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast_mode_per_session_opt_in: Option<bool>,
    /// Region used to list and resolve model connections. Absence means international.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_region: Option<ProviderRegion>,
    /// Terminal backend used for experimental agent-team members.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub teammate_mode: Option<TeammateMode>,

    /// Main-query cache lifetime (`promptCacheTtl`).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_prompt_cache_ttl"
    )]
    pub prompt_cache_ttl: Option<PromptCacheTtl>,

    /// Subagent and background-query cache lifetime (`subagentPromptCacheTtl`).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_prompt_cache_ttl"
    )]
    pub subagent_prompt_cache_ttl: Option<PromptCacheTtl>,

    /// Forward-compat: claude-code @ 6a25909 does NOT emit this; we keep it
    /// typed so a `$schema` reference injected by IDE tooling round-trips
    /// instead of being stripped on re-serialize.
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub dollar_schema: Option<String>,

    /// Array-merge field (concat-dedup).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trusted_directories: Option<Vec<String>>,

    /// Array-merge field (concat-dedup).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_directories: Option<Vec<String>>,

    /// Array-merge field (concat-dedup).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_tools: Option<Vec<String>>,

    /// Array-merge field (concat-dedup).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_includes: Option<Vec<String>>,

    /// `lingxiMdExcludes` (array-merge, concat-dedup): glob patterns or absolute
    /// paths of `LINGXI.md` files to exclude from loading (claude-code
    /// `settings/types.ts:1053`, gate `isLingxiMdExcluded`, `claudemd.ts:547`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lingxi_md_excludes: Option<Vec<String>>,

    /// Object-merge field (deep-merge).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<BTreeMap<String, Value>>,

    /// Object-merge field (deep-merge).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<BTreeMap<String, Value>>,

    /// Object-merge field (deep-merge). claude-code `permissions` block:
    /// `{ "allow": [...], "deny": [...], "ask": [...], "defaultMode": "...",
    ///    "additionalDirectories": [...] }` (rule strings like `"Bash(npm run *)"`).
    /// Opaque here — projected into typed rules by
    /// `permission::permission_rules_from_settings_json`. The typed field
    /// exists for ACCESS (rule projection + the `DeepMerge` strategy), not as a
    /// load gate — unknown keys are tolerated-and-ignored anyway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<BTreeMap<String, Value>>,

    /// Scalar field (later source wins). TS types this `outputStyle:
    /// z.string().optional()` (settings/types.ts:639; `type OutputStyle =
    /// string`, config.ts:181), and `settingsMergeCustomizer` special-cases
    /// only arrays, so a string `outputStyle` takes scalar-override
    /// (settings.ts:538-547) — NOT deep-merge. Typing it as a map made a real
    /// `"outputStyle": "Explanatory"` settings.json fail the whole load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_style: Option<String>,

    /// Scalar field (later source wins). `askUserQuestionTimeout`: the idle
    /// window before an `AskUserQuestion` prompt auto-continues with the
    /// answers selected so far. Oracle 2.1.201 zod:
    /// `askUserQuestionTimeout:E.enum(["60s","5m","10m","never"]).catch(void 0)`
    /// with default `never` ("auto-continue only runs when explicitly set to
    /// 60s/5m/10m"). Typed `Option<String>` (like `outputStyle`), tolerant of
    /// unknown values via the zod `.catch` (parsed into the typed
    /// `tool_ui::ask_user_question::AskUserQuestionTimeout` at the tool layer).
    /// Scalar-override merge (not in `MERGE_STRATEGIES`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask_user_question_timeout: Option<String>,

    /// Object-merge field (deep-merge). Managed helper commands keyed by OS /
    /// fallback scope. Kept opaque until the helper-resolution consumer lands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_helpers: Option<BTreeMap<String, Value>>,

    /// Scalar field (later source wins). Whether Claude.ai skill sync is
    /// enabled. Schema-only until the marketplace sync consumer lands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_claude_ai_skills: Option<bool>,

    /// Object-merge field (deep-merge). Alias-shaped marketplace declarations
    /// keyed by marketplace name, parallel to `extraKnownMarketplaces`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_marketplaces: Option<BTreeMap<String, Value>>,

    /// Array field (scalar-override merge). Alias-shaped marketplace allowlist,
    /// parallel to `strictKnownMarketplaces`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_marketplaces: Option<Vec<Value>>,

    /// Scalar field (later source wins). Managed gate for command-sourced
    /// plugins. Unset consumer semantics still follow `allowManagedHooksOnly`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable_command_plugin_sources: Option<bool>,

    /// Object-merge field (deep-merge). Spellcheck configuration block. Kept
    /// opaque until the spellcheck consumer lands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spellcheck: Option<BTreeMap<String, Value>>,

    /// Scalar field (later source wins). `dialogExpiry`: how long permission /
    /// ask-user dialogs stay armed. 2.1.232 `_Vp`:
    /// `["default","60s","5m","10m","never"]`. UI `"default"` is stored as
    /// omitted (`void 0`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dialog_expiry: Option<String>,

    /// Scalar field (later source wins). `modelProposedGoals` enum:
    /// `auto|alwaysAsk|disabled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_proposed_goals: Option<String>,

    /// Scalar field (later source wins). `keybindingFlavor` enum:
    /// `classic|readline`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keybinding_flavor: Option<String>,

    /// Scalar field (later source wins). Whether the UI may auto-continue at a
    /// usage-limit stop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_continue_at_usage_limit: Option<bool>,

    /// Scalar field (later source wins). `crossSessionInbound`: accept / hold /
    /// refuse messages from other live sessions on this machine. 2.1.232 `bVp`:
    /// `["default","accept","hold","refuse"]`. `"default"` is stored omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cross_session_inbound: Option<String>,

    /// Command prepended to CLI self-spawns. Only user, `--settings`, managed,
    /// and the dedicated environment variables may contribute at the consumer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_wrapper: Option<String>,

    /// Main interactive status-line command configuration. Kept as raw JSON so
    /// forward-compatible Claude fields survive settings composition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_line: Option<Value>,

    /// Per-task status-line command configuration for running subagents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_status_line: Option<Value>,

    /// Scalar field (later source wins). `viewMode`: the transcript view mode
    /// applied on startup. CC 2.1.207 settings zod (verbatim):
    /// `viewMode:E.enum(["default","verbose","focus"]).optional().catch(void 0)
    /// .describe("Default transcript view mode on startup")`. Backs the
    /// H-BIN-11 `/focus` command (which toggles `viewMode==="focus"`) and the
    /// `/tui` renderer split. Typed `Option<String>` (like `outputStyle` /
    /// `askUserQuestionTimeout`), tolerant of unknown values via the zod
    /// `.catch(void 0)` — an out-of-enum string round-trips as the raw value and
    /// is treated as `default` by the reader. Scalar-override merge (not in
    /// `MERGE_STRATEGIES`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view_mode: Option<String>,

    /// Scalar field (later source wins). `emojiCompletionEnabled` controls
    /// `:shortcode` suggestions in the interactive composer. Absent defaults
    /// to enabled, matching Claude Code 2.1.217.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emoji_completion_enabled: Option<bool>,

    /// Scalar field (later source wins). When true, retain API-provided
    /// thinking summaries instead of requesting redacted-thinking blocks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_thinking_summaries: Option<bool>,

    /// LingXi extension. When enabled, a primary model without image input
    /// support may use its provider profile's internal vision delegate.
    /// Absence resolves to enabled at the consumer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision_delegation_enabled: Option<bool>,

    /// Scalar field (later source wins). User opt-in for the feature-gated
    /// proactive agent/mobile push notification surface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_push_notif_enabled: Option<bool>,

    /// Scalar field (later source wins). Soft cap, in characters, on a task's
    /// model-facing output before `TaskOutput` truncates it to the tail with a
    /// `[Truncated. Full output: …]` header (claude-code `Ge().taskOutputMaxChars`).
    ///
    /// Read through `see()`, which CLAMPS it to `4_000..=128_000` — a value
    /// outside that range is pulled to the nearest bound rather than rejected.
    /// When set it wins over `TASK_MAX_OUTPUT_LENGTH`; absent, the env var
    /// applies over a 32_000 default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_output_max_chars: Option<u32>,

    /// Scalar field (later source wins). How much Bash output Claude receives
    /// inline before it is saved to a file (2.1.261, alongside
    /// `taskOutputMaxChars`). Read through the same `see()` CLAMP of
    /// `4_000..=128_000`, and when set it wins over `BASH_MAX_OUTPUT_LENGTH`;
    /// absent, the env var applies over the built-in default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bash_output_max_chars: Option<u32>,

    /// Scalar field (later source wins). Overrides the git commit / PR
    /// attribution trailers Bash's git sections interpolate.
    ///
    /// claude-code `$gs()`: the OBJECT wins over [`Self::include_co_authored_by`],
    /// and it counts only when it names at least one of the two
    /// (`RWn(e) = e !== void 0 && (e.commit !== void 0 || e.pr !== void 0)`);
    /// whichever it omits keeps its default. An explicit EMPTY STRING is a
    /// value, not an absence — it disables that trailer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution: Option<Attribution>,

    /// Scalar field (later source wins). The coarse switch that predates
    /// [`Self::attribution`]: `false` empties BOTH trailers. Ignored whenever
    /// `attribution` names either field, matching `$gs()`'s arm order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_co_authored_by: Option<bool>,

    /// Scalar field (later source wins). Whether the Bash tool's prompt carries
    /// the git / PR instruction sections. Default `true`.
    ///
    /// claude-code `iQ()` consults this ONLY when
    /// `CLAUDE_CODE_DISABLE_GIT_INSTRUCTIONS` is undefined — a defined env var
    /// wins outright in BOTH directions, so an explicit env `false` re-enables
    /// the sections over a settings `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_git_instructions: Option<bool>,

    /// Include `/code-review medium` in the Bash precommit skill suggestion
    /// when that skill is available. Only an explicit `true` enables it.
    #[serde(
        default,
        deserialize_with = "deserialize_code_review_suggestion",
        skip_serializing_if = "Option::is_none"
    )]
    pub include_code_review_suggestion: Option<bool>,

    /// Scalar field (later source wins). When enabled, a Bash command that
    /// changes files reports a diff of them in its tool result (CLI-5).
    ///
    /// 🚨 Read the TIER, not just this merged value. claude-code's `y0r()`
    /// consults `Bk("bashEditDiffEnabled")` — the USER / flag / policy tiers
    /// only — for the arm that turns the feature ON, so a checked-in project
    /// settings file cannot make Claude start hashing and diffing the repo.
    /// A tier or merged `false` turns it off from ANY layer. See
    /// `tool_shell::bash_edit_diff::enabled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bash_edit_diff_enabled: Option<bool>,

    /// Scalar field (later source wins). When enabled, a literal `ultracode`
    /// token in a submitted prompt emits the Workflow authorization reminder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_keyword_trigger_enabled: Option<bool>,

    /// Scalar field (later source wins). Turns the file-based auto-memory
    /// feature (the `# Memory` prompt section + per-turn memdir recall) on or
    /// off. **Unset means ON** — claude-code's gate ends `return!0`
    /// (`dLt()`, oracle `src_166572870.js`); see [`memory::auto_memory_enabled`]
    /// for the full precedence, including the env killswitch that overrides
    /// this key in BOTH directions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_memory_enabled: Option<bool>,

    /// Persisted `/effort` default (`effortLevel`). `max` is session-only and
    /// is not written here.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_persisted_effort"
    )]
    pub effort_level: Option<String>,

    /// Client-side effort cap (`maxEffortLevel`). `/effort`, `--effort`,
    /// `LINGXI_EFFORT_LEVEL`, and model defaults above this value are clamped
    /// to it. `"max"` does not cap. Across settings files the **lowest**
    /// applicable value wins — do not rely on the merged scalar for that fold.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_max_effort"
    )]
    pub max_effort_level: Option<String>,

    /// Per-model `effortLevel` / `maxEffortLevel`. A matching model's
    /// `maxEffortLevel` replaces the top-level cap for that model (`"max"`
    /// exempts it).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_model_settings"
    )]
    #[schemars(with = "Option<BTreeMap<String, ModelSettings>>")]
    pub model_settings: Option<indexmap::IndexMap<String, ModelSettings>>,

    /// Scalar field (later source wins). Session-scoped gate for dynamic
    /// workflows. Absence resolves to enabled at the composition root, matching
    /// Claude Code's default-on path when no plan/experiment gate disables it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_workflows: Option<bool>,

    /// Scalar field (later source wins). `workflowSizeGuideline` controls the
    /// advisory fan-out size appended to the Workflow tool prompt. Valid wire
    /// values are `unrestricted`, `small`, `medium`, and `large`; consumers
    /// normalize unknown values. Absence resolves to `medium` at the use site.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_size_guideline: Option<String>,

    /// Scalar field (later source wins). Telemetry on/off toggle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry_enabled: Option<bool>,

    /// Scalar field (later source wins). `axScreenReader`: accessibility
    /// screen-reader mode (2.1.201 settings schema `screenReader` group:
    /// "Render screen-reader friendly output (flat text, no decorative borders
    /// or animations). Overridden by the CLAUDE_AX_SCREEN_READER env var and
    /// the --ax-screen-reader CLI flag."). Read by the `ax_screen_reader` gate
    /// as the lowest-precedence source (below the env var and CLI flag). Key
    /// stays `axScreenReader` verbatim (config wire key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ax_screen_reader: Option<bool>,

    /// Scalar field (later source wins). `alwaysThinkingEnabled`: when set to
    /// `false`, disables extended thinking for the session UNLESS a fixed budget
    /// is pinned via the `MAX_THINKING_TOKENS` env var or the
    /// `--max-thinking-tokens` flag (both pre-empt this). Consumed by the boot
    /// session `ThinkingConfig` resolver (`llm_runtime::model::thinking::
    /// session_thinking_from_env`, binary `qIe()`:
    /// `if(e.alwaysThinkingEnabled===!1)return!1;return!0`). Key stays
    /// `alwaysThinkingEnabled` verbatim (config wire key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub always_thinking_enabled: Option<bool>,

    /// Scalar field (later source wins). `skipWebFetchPreflight`: skip the
    /// `WebFetch` domain-blocklist preflight for enterprise hosts whose network
    /// policy blocks outbound connections to `claude.ai`/`api.anthropic.com`.
    /// CC 2.1.207 settings zod (verbatim):
    /// `skipWebFetchPreflight:E.boolean().optional().describe("Skip the WebFetch
    /// blocklist check for enterprise environments with restrictive security
    /// policies")`. Consumed by `WebFetchTool` via
    /// `BuiltinToolContext::skip_web_fetch_preflight` — when true the tool skips
    /// the blocklist check entirely (binary
    /// `if(!Mi().skipWebFetchPreflight)switch((await DSd(g)).status){…}`;
    /// leaked `utils.ts:423-424` `if(!settings.skipWebFetchPreflight){…}`).
    /// Scalar-override merge (not in `MERGE_STRATEGIES`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_web_fetch_preflight: Option<bool>,

    /// Scalar field (later source wins). `disableArtifact`: opt out of the
    /// `Artifact` tool (parity 2.1.207 H-BIN-03). CC 2.1.207 settings zod
    /// (verbatim): `disableArtifact:E.boolean().optional().describe("Disable the
    /// Artifact tool (also via CLAUDE_CODE_DISABLE_ARTIFACT).")`. The env half
    /// (`CLAUDE_CODE_DISABLE_ARTIFACT`) is wired in `tool_api::artifact_gate`
    /// (binary `R9i()`); threading this settings value into that gate lands with
    /// the Stage-2 publish pipeline. Scalar-override merge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable_artifact: Option<bool>,

    /// Scalar field (later source wins). `enableArtifact`: explicitly enable /
    /// disable the `Artifact` tool for this user (parity 2.1.207 H-BIN-03). CC
    /// 2.1.207 settings zod (verbatim): `enableArtifact:E.boolean().optional()
    /// .describe("Enable or disable the Artifact tool for this user. Unset
    /// defaults to enabled once the feature is available.")`. Read by the tool
    /// gate's `P7t() ?? L7t()` tail (binary): when set it wins over the default,
    /// when unset the tool is enabled once the `tengu_cobalt_plinth` gate is
    /// available. Schema-only today (the gate's live settings read is Stage-2);
    /// the key round-trips so it is ACCESSIBLE. Scalar-override merge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_artifact: Option<bool>,

    /// Scalar field (later source wins). `disableAgentView`: disable the agent
    /// view surface (`claude agents`, `--bg`, `/background`, the on-demand
    /// daemon) and the redefined background-session `/fork` + `/subtask`. CC
    /// 2.1.215 settings zod (verbatim describe): "Disable agent view (`claude
    /// agents`, `--bg`, /background, the on-demand daemon). Typically set in
    /// managed settings. Equivalent to CLAUDE_CODE_DISABLE_AGENT_VIEW=1." Read by
    /// `I2i()` (the `vO()` gate) alongside the `CLAUDE_CODE_DISABLE_AGENT_VIEW`
    /// env var: `settings.disableAgentView === true` disables agent view exactly
    /// like a truthy env var. Threaded into command registration via
    /// [`crate::host::agent_view::is_enabled_with_setting`] (see
    /// `command_api::builtins::register_core_batch_8`). Scalar-override merge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable_agent_view: Option<bool>,

    /// `disableAllHooks` — when true, ALL hooks are disabled (claude hook-dispatch
    /// gate `Ql()`). One half of the `/goal` hooks-restricted gate (`kEt`): a
    /// restricted-hooks session rejects `/goal`. Scalar-override merge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable_all_hooks: Option<bool>,

    /// `allowManagedHooksOnly` — when true, only managed-policy hooks run. The
    /// other half of the `/goal` hooks-restricted gate (claude
    /// `allowManagedHooksOnly===!0`). Scalar-override merge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_managed_hooks_only: Option<bool>,

    /// Scalar field (later source wins). Default model alias.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    /// Array field (scalar-override merge). `availableModels`: managed allowlist
    /// of models a user may select. CC 2.1.207 settings zod (verbatim describe):
    /// "Allowlist of models that users can select. Accepts family aliases
    /// (\"opus\" allows any opus version), version prefixes (\"opus-4-5\" allows
    /// only that version), and full model IDs. If undefined, all models are
    /// available. If empty array, only the default model is available."
    /// Typically set in managed (`policySettings`) settings by enterprise
    /// administrators. Consumed via [`llm_runtime::model::allowlist`] — matcher +
    /// policy-provenance for the `enforceAvailableModels` gate. Absent ⇒ None
    /// (no restriction), distinct from `Some(vec![])` (only the default model).
    /// Scalar-override merge (CC `settingsMergeCustomizer` returns the source
    /// array for non-concat arrays), so NOT in `MERGE_STRATEGIES`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_models: Option<Vec<String>>,

    /// Scalar field (later source wins). `enforceAvailableModels`: gate on the
    /// managed `availableModels` allowlist. CC 2.1.207 zod (verbatim describe):
    /// "When true and availableModels is a non-empty array, the Default model
    /// selection is also constrained: if the default model for the user tier is
    /// not in availableModels, Default resolves to the first allowed
    /// availableModels entry instead. Has no effect when availableModels is unset
    /// or an empty array. Typically set in managed settings by enterprise
    /// administrators." The flag only binds with a POLICY-owned allowlist
    /// (`llm_runtime::model::allowlist::resolve_enforcement`). Scalar-override
    /// merge (not in `MERGE_STRATEGIES`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enforce_available_models: Option<bool>,

    /// Object-merge field (deep-merge). `modelOverrides`: managed mapping from
    /// Anthropic model ID to provider-specific model ID. CC 2.1.207 zod
    /// (verbatim describe): "Override mapping from Anthropic model ID (e.g.
    /// \"claude-opus-4-6\") to provider-specific model ID (e.g. a Bedrock
    /// inference profile ARN). Typically set in managed settings by enterprise
    /// administrators." Feeds the same `availableModels` enforcement path
    /// (reverse-mapped in the matcher, `overridesMap`). Deep-merged across tiers
    /// (CC's `settingsMergeCustomizer` leaves objects to lodash's recursive
    /// merge).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_overrides: Option<BTreeMap<String, String>>,

    /// Scalar field (later source wins). `awsAuthRefresh`: path to a script
    /// that refreshes AWS authentication (2.1.198 settings schema: "Path to a
    /// script that refreshes AWS authentication"). Consumed by the llm-runtime
    /// AWS auth-refresh flow (`llm_runtime::aws_auth`, binary fn `ZBd`) when a
    /// Bedrock-style request fails with an expired-STS auth error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aws_auth_refresh: Option<String>,

    /// Scalar field (later source wins). `awsCredentialExport`: path to a
    /// script that exports AWS credentials (2.1.198 settings schema: "Path to
    /// a script that exports AWS credentials"; binary fn `t2d` parses its
    /// stdout as STS JSON — `{Credentials:{AccessKeyId,SecretAccessKey,
    /// SessionToken,Expiration}}` or the flat equivalent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aws_credential_export: Option<String>,

    /// Scalar field (later source wins). `gcpAuthRefresh`: command to refresh
    /// GCP authentication (2.1.198 settings schema: "Command to refresh GCP
    /// authentication (e.g., gcloud auth application-default login)").
    /// Schema-only today — the GCP refresh runtime (binary `zzr`/`Yzr`) is not
    /// ported; the key is typed so it round-trips and is ACCESSIBLE when the
    /// Vertex flow lands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gcp_auth_refresh: Option<String>,

    // ── HTTP-hook security allowlists (H-BIN-12) ─────────────────────────────
    // Two CC 2.1.207 settings keys the HTTP hook executor reads live per
    // execution (`PFy()=Wn()`), consumed by `hooks::HttpExecutor` — the URL
    // allowlist gate + the per-hook env-var intersection. Both are array-merge
    // (ConcatDedup, see `MERGE_STRATEGIES`).
    /// Array-merge field (concat-dedup). `allowedHttpHookUrls`: allowlist of URL
    /// patterns HTTP hooks may target. CC 2.1.207 zod (verbatim describe):
    /// "Allowlist of URL patterns that HTTP hooks may target. Supports * as a
    /// wildcard (e.g. \"https://hooks.example.com/*\"). When set, HTTP hooks with
    /// non-matching URLs are blocked. If undefined, all URLs are allowed. If
    /// empty array, no HTTP hooks are allowed. Arrays merge across settings
    /// sources (same semantics as allowedMcpServers)." Consumed by the HTTP hook
    /// executor via [`crate::settings::schema`] → the `hooks` crate's
    /// `HttpExecutor`: `None` ⇒ all URLs allowed; `Some(empty)` ⇒ block ALL HTTP
    /// hooks; `Some(patterns)` ⇒ the hook URL must match ≥1 pattern (CC `NBr`
    /// wildcard matcher) or the request is blocked before dispatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_http_hook_urls: Option<Vec<String>>,

    /// Array-merge field (concat-dedup). `httpHookAllowedEnvVars`: allowlist of
    /// environment variable names HTTP hooks may interpolate into headers. CC
    /// 2.1.207 zod (verbatim describe): "Allowlist of environment variable names
    /// HTTP hooks may interpolate into headers. When set, each hook's effective
    /// allowedEnvVars is the intersection with this list. If undefined, no
    /// restriction is applied. Arrays merge across settings sources (same
    /// semantics as allowedMcpServers)." Consumed by the HTTP hook executor:
    /// `None` ⇒ per-hook `allowedEnvVars` used as-is; `Some(list)` ⇒ each hook's
    /// effective allowlist is its own `allowedEnvVars` ∩ this global list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_hook_allowed_env_vars: Option<Vec<String>>,

    // ── Enterprise login / version managed-policy keys (H-BIN-09) ────────────
    // Six admin-provisioned `managed-settings.json` keys CC 2.1.207 both schemas
    // AND enforces; consumed via [`crate::settings::enterprise`]. All scalar-
    // override merge (none in `MERGE_STRATEGIES`). See that module for wiring
    // status (version gate LIVE; login-flow method-lock/org-pin DORMANT).
    /// Scalar field (later source wins). `forceLoginMethod`: force a specific
    /// OAuth login method. CC 2.1.207 zod (verbatim describe): "Force a specific
    /// login method: \"claudeai\" for Claude Pro/Max, \"console\" for Console
    /// billing, \"gateway\" for the Cloud gateway OIDC device flow". Enum
    /// `claudeai|console|gateway` with `.catch(void 0)` — an out-of-set value
    /// degrades to "no forced method". Typed `Option<String>` (like
    /// `askUserQuestionTimeout`); the enum + `.catch` degrade live in the typed
    /// accessor [`SettingsJson::force_login_method_parsed`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force_login_method: Option<String>,

    /// Scalar field (later source wins). `forceLoginGatewayUrl`: Cloud gateway
    /// URL to pre-fill and auto-connect to during login. CC 2.1.207 zod:
    /// `forceLoginGatewayUrl:E.string().url().optional().catch(void 0)`
    /// ("@internal Cloud gateway URL to pre-fill and auto-connect to during
    /// login. Typically set in local managed settings alongside forceLoginMethod:
    /// \"gateway\" so users never type the URL."). Pre-filled into the OAuth
    /// screen by the login flow (DORMANT wiring — see `enterprise`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force_login_gateway_url: Option<String>,

    /// Scalar field (later source wins). `forceLoginOrgUUID`: pin OAuth login to
    /// an organization (or list). CC 2.1.207 zod:
    /// `forceLoginOrgUUID:E.union([E.string(),E.array(E.string())]).optional()`
    /// ("Organization UUID to require for OAuth login. Accepts a single UUID
    /// string or an array of UUIDs (any one is permitted). When set in managed
    /// settings, login fails if the authenticated account does not belong to a
    /// listed organization."). Kept as opaque `Value` (string | string[]) so a
    /// malformed value is TOLERATED at load and degraded at the consumer
    /// ([`SettingsJson::force_login_org_pin`]) rather than failing the whole
    /// settings file.
    ///
    /// NB explicit `rename`: serde's `camelCase` rule would emit `forceLoginOrgUuid`,
    /// but the CC wire key is the all-caps `forceLoginOrgUUID`.
    #[serde(
        rename = "forceLoginOrgUUID",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub force_login_org_uuid: Option<Value>,

    /// Scalar field (later source wins). `parentSettingsBehavior`: whether the
    /// SDK parent managed-settings tier (`Options.managedSettings` /
    /// `--managed-settings`) layers under this admin tier. CC 2.1.207 zod enum
    /// `first-wins|merge` ("first-wins (default): parent is dropped … merge:
    /// parent's restrictive-only-filtered settings union under the admin
    /// winner."). Consumed by [`crate::settings::enterprise::should_merge_parent_settings`]
    /// (the SDK parent tier itself is not yet modeled in lingxi).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_settings_behavior: Option<String>,

    /// Scalar field (later source wins). `minimumVersion`: USER setting — CC
    /// 2.1.207 zod: "Minimum version to stay on - prevents downgrades when
    /// switching to stable channel". Consumed ONLY by CC's auto-updater
    /// channel-downgrade guard (`Zlo`/`Wn()`); lingxi has no auto-updater, so
    /// this is schema-only (dead-by-missing-subsystem) — do NOT confuse with the
    /// managed startup gate `requiredMinimumVersion`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_version: Option<String>,

    /// Scalar field (later source wins). `requiredMinimumVersion`: managed
    /// startup exit gate — CC 2.1.207 zod: "Minimum Claude Code version required
    /// to start. If the running version is older, Claude Code exits at startup
    /// with instructions to update. Only enforced from managed (policy)
    /// settings." Consumed by [`crate::settings::enterprise::version_gate`]
    /// (LIVE, wired at CLI boot).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_minimum_version: Option<String>,

    /// Scalar field (later source wins). `requiredMaximumVersion`: managed
    /// startup exit gate — CC 2.1.207 zod: "Maximum Claude Code version allowed
    /// to start. If the running version is newer, Claude Code exits at startup
    /// with instructions to install an approved version. Only enforced from
    /// managed (policy) settings." Ships with `requiredMinimumVersion` in the
    /// same `version_gate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_maximum_version: Option<String>,

    /// Scalar field (later source wins). `forceRemoteSettingsRefresh`: CC 2.1.207
    /// zod: "When set in managed settings, the CLI blocks startup until remote
    /// managed settings are freshly fetched, and exits if the fetch fails."
    /// Schema-only — lingxi has no remote managed-settings fetcher; the key is
    /// typed so it round-trips and is ACCESSIBLE when that subsystem lands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force_remote_settings_refresh: Option<bool>,

    /// Array-merge field (concat-dedup). `companyAnnouncements`: strings shown
    /// in the TUI startup header (one selected per process). CC 2.1.207 zod
    /// (verbatim): `companyAnnouncements:E.array(E.string()).optional()
    /// .describe("Company announcements to display at startup (one will be
    /// randomly selected if multiple are provided)")`. Consumed by the selection
    /// helper [`crate::settings::company_announcements::select_company_announcement`]
    /// (port of binary `jxo`/`oip`): non-empty entries only, `[0]` when
    /// `numStartups==1` else uniform-random, memoized once per process. Merged
    /// concat-dedup, matching CC's `settingsMergeCustomizer` array customizer
    /// (`Array.isArray(t)&&Array.isArray(e)?(t.forEach(a=>{e.includes(a)||e.push(a)}))`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub company_announcements: Option<Vec<String>>,

    /// Scalar field (later source wins). `plansDirectory`: custom directory for
    /// plan-mode plan files, relative to the project root. CC 2.1.207 zod
    /// (verbatim): `plansDirectory:E.string().optional().describe("Custom
    /// directory for plan files, relative to project root. If not set, defaults
    /// to ~/.claude/plans/")`. Consumed by the orchestrator plan-file resolver
    /// (`ConversationOrchestrator::plan_file_path`, port of binary `iT`): when
    /// set, resolved against the project root with a within-root containment
    /// check; on failure the byte-exact error `plansDirectory must be within
    /// project root: {r}` is logged and the resolver falls back to the default
    /// `<project-root>/.lingxi/plans` (DIVERGENCE: upstream `KPp()` defaults to
    /// `~/.claude/plans/`, which LingXi's file tools reject as outside their
    /// trusted set). Scalar-override merge (not in `MERGE_STRATEGIES`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plans_directory: Option<String>,

    /// Scalar field (later source wins). `apiKeyHelper`: path to (or command
    /// line of) a script whose stdout is the Anthropic auth value. CC 2.1.207
    /// zod (verbatim): `apiKeyHelper:E.string().optional().describe("Path to a
    /// script that outputs authentication values")`. Consumed by the auth
    /// executor (`llm_runtime::auth::anthropic::run_api_key_helper`, port of
    /// binary `LTh`) with the TTL cache (`api_key_helper_ttl_ms`, port of `obc`).
    /// Scalar-override merge (not in `MERGE_STRATEGIES`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_helper: Option<String>,

    /// Object-merge field (deep-merge, `MERGE_STRATEGIES`). `vimInsertModeRemaps`: two-key insert-mode sequences
    /// to key names. Claude Code 2.1.208 uses this for common Vim insert-exit
    /// mappings, for example `{ "jj": "Escape" }`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vim_insert_mode_remaps: Option<BTreeMap<String, String>>,

    /// Scalar field (later source wins). `otelHeadersHelper`: path to (or shell
    /// command for) a script whose stdout is a JSON object of OTLP export header
    /// `k:v` strings — used to inject short-lived bearer tokens into the
    /// OpenTelemetry monitoring exporters (parity 2.1.207 H-BIN-06). Read by the
    /// binary `otelHeadersHelper` runner (`RRi()`/`wRi()`), which validates the
    /// output ("must return a JSON object with string key-value pairs"), caches
    /// it, and re-invokes at most once per
    /// `CLAUDE_CODE_OTEL_HEADERS_HELPER_DEBOUNCE_MS` window. The validation +
    /// debounce state machine lives in `telemetry::otel::headers_helper`; wiring
    /// this value into the live export path is the H-BIN-06 egress remainder.
    /// The key round-trips so it is ACCESSIBLE. Scalar-override merge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub otel_headers_helper: Option<String>,

    /// Object-merge field. On-disk enabled plugin allowlist keyed by
    /// `plugin@marketplace`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_plugins: Option<BTreeMap<String, Value>>,

    /// Object-merge field. Non-sensitive plugin config records keyed by
    /// `plugin@marketplace`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_configs: Option<BTreeMap<String, Value>>,

    /// Object-merge field. Extra marketplace declarations keyed by marketplace
    /// name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_known_marketplaces: Option<BTreeMap<String, Value>>,

    /// Array field (scalar-override merge). Managed allowlist of known
    /// marketplaces; entries may be strings or structured source objects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict_known_marketplaces: Option<Vec<Value>>,

    /// Array field (scalar-override merge). Managed blocklist of marketplace
    /// names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_marketplaces: Option<Vec<String>>,

    /// Object-merge field (deep-merge). `LingXi` extension (claude-code has no
    /// such key): named LLM provider profiles. Each entry has the shape:
    /// `{ "type": "openai"|"openai-responses"|"anthropic"|"gemini"|"azure-openai"
    ///           |"bedrock-claude"|"vertex-claude"|"vertex-gemini", "baseUrl": "...",
    ///    "apiKeyEnv": "GROQ_API_KEY",
    ///    "models": [{ "id": "model-id", "aliases": ["alias"]?,
    ///                 "capabilities": {...}? }] }`.
    ///
    /// **Wired (3c-T2):** parsed by `platform_common::apply_settings_providers`
    /// and appended to `llm_runtime::ClientConfig` in `build()`.  `models` is
    /// REQUIRED per entry; an absent or empty list is an error at engine startup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub providers: Option<BTreeMap<String, Value>>,

    /// Object-merge field (deep-merge). `LingXi` extension: routing config
    /// for model aliases, fallback chains, and retry policy. Shape:
    /// `{ "aliases": {alias: "profile/model"},
    ///    "fallback": {"<primary-model-or-alias>": ["profile/model", …]},
    ///    "retry": {"maxAttempts": n, "backoffMs": n} }`.
    ///
    /// **Wiring status (batch-1 T1):**
    /// - `aliases`: **WIRED** — each alias is pushed onto the target
    ///   `ModelProfile` (resolved across all configured + builtin profiles).
    /// - `fallback`: **WIRED** — parsed by `platform_common::parse_routing_overrides`
    ///   and threaded into the adapter as per-model fallback overrides.
    ///   Per-model entry wins over the global `DesktopConfig.fallback_model` /
    ///   argv fallback.  Only chain[0] is used; longer chains log a warning.
    /// - `retry.maxAttempts`: **WIRED** — sets `RetryControl.max_retries`.
    ///   Precedence: `LINGXI_MAX_RETRIES` env > `maxAttempts` > default (10).
    /// - `retry.backoffMs`: **WIRED** — sets the exponential-backoff ladder's
    ///   first rung (overrides the default base of 500ms). Ladder is
    ///   `min(backoffMs * 2^attempt, 32000)`; `backoffMs=1000` →
    ///   `[1000, 2000, 4000, 8000, 16000, 32000, …]`. Additive jitter
    ///   `+ rand(0, 0.25) * base` still applies. Server-sent `Retry-After` is
    ///   never scaled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<Value>,

    /// Object-merge field (deep-merge). LingXi extension: Fusion multi-model
    /// deliberation. Absent ⇒ Fusion stays disabled (default `enabled: false`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fusion: Option<FusionSettingsJson>,
}

/// Panel completion policy. Missing settings retain the wait-all behavior.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FusionCompletionPolicy {
    #[default]
    WaitAll,
    /// After the first quorum, allow a fixed ten-second grace period.
    QuorumAfterGrace,
}

/// One explicitly configured Fusion model, named by the provider profile that
/// owns it plus that profile's wire model id — the same `(profile, model)` pair
/// `/model` switches to and `fusion::CatalogModel` is keyed by.
///
/// Fusion has two model ROLES (panel, analyst) and both of
/// them must be named here before a run may start. There is deliberately no
/// automatic ranking any more: a checked-in hint table used to pick panels and
/// the analyst on the operator's behalf, which meant the set of models a run
/// actually spent money on was invisible in the settings and changed whenever
/// the table or the live catalog did. `llm_runtime::fusion_hints` survives only
/// as the SUGGESTION source the `/fusion setup` wizard sorts its candidate list
/// by; nothing reads it at run time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FusionModelSelectionJson {
    /// Provider profile name (`anthropic`, `openai`, `openrouter`, ...).
    pub profile: String,
    /// Wire model id as that profile spells it.
    pub model: String,
}

impl FusionModelSelectionJson {
    /// Reject a half-written entry. `field` names the setting for the message.
    fn validate(&self, field: &str) -> Result<(), crate::settings::SettingsError> {
        use crate::settings::SettingsError::SchemaViolation;
        if self.profile.trim().is_empty() {
            return Err(SchemaViolation(format!(
                "{field}.profile must not be empty"
            )));
        }
        if self.model.trim().is_empty() {
            return Err(SchemaViolation(format!("{field}.model must not be empty")));
        }
        Ok(())
    }
}

/// Typed `settings.fusion` object. Every field is `Option` so a partial layer
/// can set a subset. Runtime defaults are applied by the Fusion orchestrator;
/// [`SettingsJson::validate`] rejects values that would be dangerous if used.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FusionSettingsJson {
    /// Master switch for the Agent listing. Default false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// `quality` or `fast`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// Quality preset panel count (min 2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality_panel_count: Option<u8>,
    /// Fast preset panel count (min 2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast_panel_count: Option<u8>,
    /// Hard cap 2..=8.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_panel: Option<u8>,
    /// Minimum successful panels before analysis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_successful_panels: Option<u8>,
    /// Continue when some panels fail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial_ok: Option<bool>,
    /// Opt-in early quorum completion; omitted means wait_all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_policy: Option<FusionCompletionPolicy>,
    /// Per-panel turn cap (1..=12).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panel_max_turns: Option<u32>,
    /// Per-turn output token cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panel_max_output_tokens_per_turn: Option<u32>,
    /// Per-turn reserved input tokens (not 1 byte = 1 token).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panel_reserved_input_tokens_per_turn: Option<u32>,
    /// Optional hard reservation ceiling in nano-USD. This caps reserved spend
    /// up front; realized cost may end lower.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_reserved_nano_usd: Option<u64>,
    /// Analyst output cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analyst_max_output_tokens: Option<u32>,
    /// Let the analyst check the panels' claims with read-only tools (Read,
    /// Grep, Glob) instead of comparing the reports blind. Analysis mode and
    /// the `quality` preset only. Costs several analyst turns, and the analyst
    /// then runs as a subagent rather than a single structured call. Default
    /// false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analyst_tools: Option<bool>,
    /// Turn cap for the tool-using analyst (1..=12). Default 6.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analyst_max_turns: Option<u32>,
    /// Panel idle timeout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panel_idle_timeout_ms: Option<u64>,
    /// Panel total timeout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panel_total_timeout_ms: Option<u64>,
    /// Analyst timeout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analyst_timeout_ms: Option<u64>,
    /// End-to-end timeout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_timeout_ms: Option<u64>,
    /// Analyst protocol retries (0 or 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analysis_protocol_retries: Option<u8>,
    /// `/fusion` default cross-provider. `true` allows prompt egress beyond the
    /// parent provider/profile unless the caller explicitly requests same-provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slash_cross_provider_default: Option<bool>,
    /// Agent may request cross-provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_cross_provider_for_agent: Option<bool>,
    /// Fusion mode: the main model starts a Fusion run by default for
    /// substantial coding, review and planning work instead of only when it
    /// judges the extra cost worthwhile. Needs `enabled`. Default false. A host
    /// mode switch ("handle the next task in parallel") writes this key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proactive: Option<bool>,
    /// Hard allowlist of profile names. Empty = no extra restriction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_profiles: Option<Vec<String>>,
    /// The panel roster, in the operator's own priority order. A preset takes
    /// the FIRST `qualityPanelCount` / `fastPanelCount` entries, so the order
    /// is meaningful. Required: a run with no roster fails preflight with
    /// `FusionError::NotConfigured` rather than picking models by itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panel_models: Option<Vec<FusionModelSelectionJson>>,
    /// The analyst (judge) that scores the panel reports. Required. It must be
    /// able to emit constrained JSON on its own profile's wire codec; that is a
    /// capability check made against the live catalog at preflight, not here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analyst_model: Option<FusionModelSelectionJson>,
    /// Implement mode (`/fusion --implement`, Agent `fusion_mode: "implement"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub implement: Option<FusionImplementSettingsJson>,
}

/// Typed `settings.fusion.implement` object: each panel changes the code in
/// its own git worktree and the host verifies it. Every field is `Option`;
/// the orchestrator applies the defaults.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FusionImplementSettingsJson {
    /// Per-panel turn cap (1..=200). Default 40.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    /// Per-panel total timeout. Default 1_800_000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panel_timeout_ms: Option<u64>,
    /// End-to-end timeout of an implement run. Default 3_600_000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_timeout_ms: Option<u64>,
    /// Commands the host runs, in order, in every panel's worktree once the
    /// panel is done (e.g. `cargo check --locked`). `/fusion --implement
    /// --verify <cmd>` replaces them for one run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_commands: Option<Vec<String>>,
    /// Per-command verification timeout. Default 600_000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_timeout_ms: Option<u64>,
    /// Worktrees verified at once (1..=8). Default 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_concurrency: Option<u8>,
    /// Hours a panel worktree with changes is kept. Default 24.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retain_hours: Option<u32>,
    /// Implement mode refuses to start with less free disk than this where
    /// the worktrees go. Default 5 GiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_free_disk_bytes: Option<u64>,
    /// Implement runs the model starts whose quote is at most this many USD
    /// run without asking. Read from user and local settings only: a
    /// checked-in project file cannot turn on unattended spending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_approve_max_usd: Option<f64>,
}

/// Most verification commands one run executes per worktree.
pub const FUSION_MAX_VERIFY_COMMANDS: usize = 16;

impl FusionImplementSettingsJson {
    fn validate(&self) -> Result<(), crate::settings::SettingsError> {
        use crate::settings::SettingsError::SchemaViolation;
        if let Some(turns) = self.max_turns {
            if !(1..=200).contains(&turns) {
                return Err(SchemaViolation(
                    "fusion.implement.maxTurns must be in 1..=200".into(),
                ));
            }
        }
        for (name, value) in [
            ("fusion.implement.panelTimeoutMs", self.panel_timeout_ms),
            ("fusion.implement.totalTimeoutMs", self.total_timeout_ms),
            ("fusion.implement.verifyTimeoutMs", self.verify_timeout_ms),
        ] {
            if value == Some(0) {
                return Err(SchemaViolation(format!("{name} must be positive")));
            }
        }
        if let Some(n) = self.verify_concurrency {
            if !(1..=8).contains(&n) {
                return Err(SchemaViolation(
                    "fusion.implement.verifyConcurrency must be in 1..=8".into(),
                ));
            }
        }
        if let Some(commands) = &self.verify_commands {
            if commands.len() > FUSION_MAX_VERIFY_COMMANDS {
                return Err(SchemaViolation(format!(
                    "fusion.implement.verifyCommands may list at most {FUSION_MAX_VERIFY_COMMANDS} commands"
                )));
            }
            if commands.iter().any(|command| command.trim().is_empty()) {
                return Err(SchemaViolation(
                    "fusion.implement.verifyCommands entries must not be empty".into(),
                ));
            }
        }
        if let Some(usd) = self.auto_approve_max_usd {
            if !usd.is_finite() || usd < 0.0 {
                return Err(SchemaViolation(
                    "fusion.implement.autoApproveMaxUsd must be a non-negative number".into(),
                ));
            }
        }
        Ok(())
    }
}

impl FusionSettingsJson {
    /// Reject values that would make Fusion unusable or unbounded.
    ///
    /// Absent fields are not errors — the orchestrator fills runtime defaults.
    /// Invalid *present* values fail the settings load.
    pub fn validate(&self) -> Result<(), crate::settings::SettingsError> {
        use crate::settings::SettingsError::SchemaViolation;
        if self.partial_ok == Some(false)
            && self.completion_policy == Some(FusionCompletionPolicy::QuorumAfterGrace)
        {
            return Err(SchemaViolation(
                "fusion.completionPolicy quorum_after_grace requires fusion.partialOk=true".into(),
            ));
        }
        if let Some(preset) = self.preset.as_deref() {
            if preset != "quality" && preset != "fast" {
                return Err(SchemaViolation(format!(
                    "fusion.preset `{preset}` must be quality or fast"
                )));
            }
        }
        let max_panel = self.max_panel.unwrap_or(8);
        if !(2..=8).contains(&max_panel) {
            return Err(SchemaViolation("fusion.maxPanel must be in 2..=8".into()));
        }
        for (name, value) in [
            ("fusion.qualityPanelCount", self.quality_panel_count),
            ("fusion.fastPanelCount", self.fast_panel_count),
            ("fusion.minSuccessfulPanels", self.min_successful_panels),
        ] {
            if let Some(n) = value {
                if n < 2 {
                    return Err(SchemaViolation(format!("{name} must be at least 2")));
                }
                if n > max_panel {
                    return Err(SchemaViolation(format!(
                        "{name} must not exceed fusion.maxPanel"
                    )));
                }
            }
        }
        if let Some(turns) = self.panel_max_turns {
            // The `fusion-panel` agent definition
            // (`agent::builtins::fusion_panel_definition`) hard-codes
            // `max_turns: 12` and a request override only ever LOWERS it
            // (`.min()`), so the runner can never actually run a panel past
            // 12 turns. Accepting a configured value above that would only
            // inflate `budget::quote`'s per-turn reservation (up to 2.7x)
            // for turns that could never be spent — a spurious
            // `BudgetExceeded` (F002).
            if !(1..=12).contains(&turns) {
                return Err(SchemaViolation(
                    "fusion.panelMaxTurns must be in 1..=12".into(),
                ));
            }
        }
        for (name, value) in [
            (
                "fusion.panelMaxOutputTokensPerTurn",
                self.panel_max_output_tokens_per_turn,
            ),
            (
                "fusion.panelReservedInputTokensPerTurn",
                self.panel_reserved_input_tokens_per_turn,
            ),
            (
                "fusion.analystMaxOutputTokens",
                self.analyst_max_output_tokens,
            ),
        ] {
            if let Some(n) = value {
                if n == 0 {
                    return Err(SchemaViolation(format!("{name} must be positive")));
                }
            }
        }
        if let Some(turns) = self.analyst_max_turns {
            // The `fusion-analyst` agent definition
            // (`agent::builtins::fusion_analyst_definition`) caps at 12.
            if !(1..=12).contains(&turns) {
                return Err(SchemaViolation(
                    "fusion.analystMaxTurns must be in 1..=12".into(),
                ));
            }
        }
        if let Some(retries) = self.analysis_protocol_retries {
            if retries > 1 {
                return Err(SchemaViolation(
                    "fusion.analysisProtocolRetries must be 0 or 1".into(),
                ));
            }
        }
        // Default kept in lockstep with `fusion::FusionRuntimeConfig::defaults`
        // (F004) — both must agree so an absent `fusion.totalTimeoutMs` passes
        // this validator and then the runtime config builds successfully.
        let total = self.total_timeout_ms.unwrap_or(1_200_000);
        if total == 0 {
            return Err(SchemaViolation(
                "fusion.totalTimeoutMs must be positive".into(),
            ));
        }
        for (name, value) in [
            ("fusion.panelIdleTimeoutMs", self.panel_idle_timeout_ms),
            ("fusion.panelTotalTimeoutMs", self.panel_total_timeout_ms),
            ("fusion.analystTimeoutMs", self.analyst_timeout_ms),
        ] {
            // Finding [15]: same per-file hazard the stage-SUM check below
            // is deliberately gated against — a tier that sets a stage
            // field but not `totalTimeoutMs` has no opinion on the total;
            // it may be raised in a DIFFERENT tier. Defaulting `total` to
            // the runtime's restrictive 1_200_000 here (unconditionally,
            // before this gate) rejected a file whose merged view is fine
            // (e.g. this tier setting `panelTotalTimeoutMs: 1500000` while
            // a user tier sets `totalTimeoutMs: 2400000`), and
            // `read_layer_or_skip` silently dropped that tier's unrelated
            // `permissions`/`hooks`/`model` along with it.
            // `FusionRuntimeConfig::from_settings` re-checks the identical
            // per-stage-vs-total invariant on the MERGED view for
            // `panelTotalTimeoutMs` and `analystTimeoutMs` — each is a
            // term of its stage-sum
            // check (fusion/src/config.rs). `panelIdleTimeoutMs` is NOT a
            // term of that sum, so nothing re-checks it once this gate
            // closes; see the same-file `panelIdleTimeoutMs` vs
            // `panelTotalTimeoutMs` check below, which restores the
            // invariant that actually governs it (Finding [24]).
            if let Some(stage) = value {
                if self.total_timeout_ms.is_some() && stage > total {
                    return Err(SchemaViolation(format!(
                        "{name} must not exceed fusion.totalTimeoutMs"
                    )));
                }
            }
            if let Some(stage) = value {
                if stage == 0 {
                    return Err(SchemaViolation(format!("{name} must be positive")));
                }
            }
        }
        // Finding [24]: `panelIdleTimeoutMs` is the one stage field above
        // that is NOT a term of `FusionRuntimeConfig::from_settings`'s
        // stage-sum re-check (fusion/src/config.rs), and the
        // `totalTimeoutMs`-gated loop above says nothing about it either —
        // the invariant that actually governs it is not "<= totalTimeoutMs"
        // anyway: the panel-level stall detector (`stall_timeout_ms`,
        // fusion/src/panel.rs) is only ever reachable while the panel
        // itself is still running, i.e. while
        // `panelIdleTimeoutMs <= panelTotalTimeoutMs`. Check that
        // invariant directly, same-file only (no cross-tier hazard: both
        // sides must be present IN THIS FILE, so a tier that sets only one
        // of them is left to the other tier/default exactly as before —
        // `FusionRuntimeConfig::from_settings` re-checks the identical
        // invariant on the merged view, comparing the merged/defaulted
        // `panel_total_timeout_ms` against the pre-default
        // `panel_idle_timeout_ms` — but ONLY when some tier actually set
        // `panelIdleTimeoutMs` at all, so a merged DEFAULT idle value
        // (180_000) can never trip that check just because a different
        // tier lowered `panelTotalTimeoutMs` below it; a settings file that
        // never mentions `panelIdleTimeoutMs` must not be rejected over a
        // field it never touched. Same gating pattern as the stage-sum and
        // min-successful-panels merged-view checks beside it. Independent
        // of validation, `panel::panel_stall_timeout_ms` also clamps the
        // watchdog deadline at the spawn use site as defense in depth for a
        // config built in-process without going through `from_settings` at
        // all).
        if let (Some(idle), Some(panel_total)) =
            (self.panel_idle_timeout_ms, self.panel_total_timeout_ms)
        {
            if idle > panel_total {
                return Err(SchemaViolation(
                    "fusion.panelIdleTimeoutMs must not exceed fusion.panelTotalTimeoutMs".into(),
                ));
            }
        }
        // F004: a run whose panels all completed must not be able to report
        // "timed out before any panel completed" just because the analyst
        // retry loop, summed with the panel stage, can
        // exceed the end-to-end deadline. Defaults here mirror
        // `fusion::FusionRuntimeConfig::defaults`.
        //
        // Same per-file hazard as the `minSuccessfulPanels` check above, in
        // BOTH directions: only run this comparison when `totalTimeoutMs` is
        // present IN THIS FILE *and* at least one of the stage fields
        // is too. A tier that sets stage fields but not `totalTimeoutMs` has
        // no opinion on the total — it may be raised in a different tier —
        // so defaulting `total` to the runtime's 1_200_000 here would reject
        // a file whose merged view is fine (e.g. a project tier setting only
        // `panelTotalTimeoutMs: 900000` while a user tier sets
        // `totalTimeoutMs: 1800000`), and `read_layer_or_skip` would then
        // silently drop that tier's unrelated `permissions`/`hooks`/`model`
        // along with it. `FusionRuntimeConfig::from_settings` re-checks the
        // identical invariant on the MERGED view and fails only the fusion
        // run, which is the right blast radius for a genuine violation.
        if self.total_timeout_ms.is_some()
            && (self.panel_total_timeout_ms.is_some() || self.analyst_timeout_ms.is_some())
        {
            let panel_total = self.panel_total_timeout_ms.unwrap_or(600_000);
            let analyst = self.analyst_timeout_ms.unwrap_or(120_000);
            let retries = u64::from(self.analysis_protocol_retries.unwrap_or(1));
            let stage_sum = panel_total.saturating_add(analyst.saturating_mul(1 + retries));
            if stage_sum > total {
                return Err(SchemaViolation(format!(
                    "fusion.panelTotalTimeoutMs + fusion.analystTimeoutMs*(1+fusion.analysisProtocolRetries) ({stage_sum}) must not exceed fusion.totalTimeoutMs ({total})"
                )));
            }
        }
        // F011 item 6: `minSuccessfulPanels` above BOTH preset panel counts
        // can never be met by an automatic run — the runtime would silently
        // clamp it at spawn time instead of ever meeting the documented bar.
        //
        // This is a per-FILE validator (`read_settings_file` calls it on
        // each settings tier individually, before merging), so unlike the
        // other checks in this function it must NOT fill in defaults for an
        // absent counterpart field: a project-tier file that sets only
        // `minSuccessfulPanels` has no opinion on `qualityPanelCount` /
        // `fastPanelCount` — those may come from a different tier — and
        // defaulting them here would reject a file whose MERGED view is
        // perfectly valid, silently dropping that file's permissions/hooks/
        // model settings along with it (see `read_layer_or_skip`). Only
        // compare when at least one of the two counts is present IN THIS
        // SAME FILE; take `min` over whichever of them actually appear.
        if let Some(min_successful) = self.min_successful_panels {
            let smallest_present_preset = match (self.quality_panel_count, self.fast_panel_count) {
                (Some(quality), Some(fast)) => Some(quality.min(fast)),
                (Some(quality), None) => Some(quality),
                (None, Some(fast)) => Some(fast),
                (None, None) => None,
            };
            if let Some(smallest_preset) = smallest_present_preset {
                if min_successful > smallest_preset {
                    return Err(SchemaViolation(format!(
                        "fusion.minSuccessfulPanels ({min_successful}) must not exceed min(fusion.qualityPanelCount, fusion.fastPanelCount) ({smallest_preset})"
                    )));
                }
            }
        }
        self.validate_model_roles(max_panel)?;
        if let Some(implement) = &self.implement {
            implement.validate()?;
        }
        Ok(())
    }

    /// Shape checks for the three model-role settings.
    ///
    /// Deliberately NOT a liveness check: whether a named `(profile, model)`
    /// exists, is reachable with the operator's credentials, has usable
    /// capacity, or can emit constrained JSON are all facts about the live
    /// catalog, and a settings file that names a model the current machine
    /// cannot reach must still load (the same file syncs across machines).
    /// Those checks run at Fusion preflight, where they can name the exact
    /// route and cost nothing when Fusion is never used.
    fn validate_model_roles(&self, max_panel: u8) -> Result<(), crate::settings::SettingsError> {
        use crate::settings::SettingsError::SchemaViolation;
        if let Some(analyst) = self.analyst_model.as_ref() {
            analyst.validate("fusion.analystModel")?;
        }
        let Some(panels) = self.panel_models.as_ref() else {
            return Ok(());
        };
        for (index, entry) in panels.iter().enumerate() {
            entry.validate(&format!("fusion.panelModels[{index}]"))?;
        }
        if panels.len() < 2 {
            return Err(SchemaViolation(format!(
                "fusion.panelModels must name at least 2 models (found {})",
                panels.len()
            )));
        }
        // `max_panel` is this file's own value or the 8 hard cap; either way a
        // roster longer than it could never be spawned in full, and silently
        // truncating it would hide models the operator believes are running.
        if panels.len() > usize::from(max_panel) {
            return Err(SchemaViolation(format!(
                "fusion.panelModels has {} entries, exceeding fusion.maxPanel ({max_panel})",
                panels.len()
            )));
        }
        // Same-file only, for the per-file hazard the preset check above
        // documents: a tier that sets only `minSuccessfulPanels` has no
        // opinion on the roster, which may live in another tier.
        // `FusionRuntimeConfig::from_settings` re-checks this on the merged
        // view, where both sides are concrete.
        if let Some(min_successful) = self.min_successful_panels {
            if panels.len() < usize::from(min_successful) {
                return Err(SchemaViolation(format!(
                    "fusion.panelModels has {} entries, fewer than fusion.minSuccessfulPanels ({min_successful})",
                    panels.len()
                )));
            }
        }
        // Exact-pair duplicates only. Two GATEWAY spellings of one underlying
        // model (`openai/gpt-5.6-sol` behind openrouter and `gpt-5.6-sol`
        // behind openai) are also a real defect — a two-model "ensemble" that
        // is one model twice — but detecting that needs the canonical-key
        // normaliser that lives with the resolver, so it is caught at
        // preflight instead, where the error can name both routes.
        for (index, entry) in panels.iter().enumerate() {
            if panels[..index]
                .iter()
                .any(|earlier| earlier.profile == entry.profile && earlier.model == entry.model)
            {
                return Err(SchemaViolation(format!(
                    "fusion.panelModels lists `{}/{}` twice",
                    entry.profile, entry.model
                )));
            }
        }
        Ok(())
    }
}

impl SettingsJson {
    /// Return the two current Native prompt-cache settings as a compact
    /// request-time policy input.
    #[must_use]
    pub fn prompt_cache_ttl_settings(&self) -> PromptCacheTtlSettings {
        PromptCacheTtlSettings {
            main: self.prompt_cache_ttl,
            subagent: self.subagent_prompt_cache_ttl,
        }
    }

    /// Run cross-field semantic checks beyond what serde's typed
    /// deserialization catches (unknown keys are tolerated, not validated).
    ///
    /// # Errors
    ///
    /// Returns [`SettingsError::SchemaViolation`] with a human-readable trail
    /// pointing at the offending field path (e.g. `trustedDirectories[1]`).
    ///
    /// [`SettingsError::SchemaViolation`]: crate::settings::SettingsError::SchemaViolation
    pub fn validate(&self) -> Result<(), crate::settings::SettingsError> {
        for (field_name, array) in [
            ("trustedDirectories", self.trusted_directories.as_deref()),
            (
                "additionalDirectories",
                self.additional_directories.as_deref(),
            ),
            ("enabledTools", self.enabled_tools.as_deref()),
            ("additionalIncludes", self.additional_includes.as_deref()),
            ("lingxiMdExcludes", self.lingxi_md_excludes.as_deref()),
        ] {
            if let Some(arr) = array {
                for (i, s) in arr.iter().enumerate() {
                    if s.is_empty() {
                        return Err(crate::settings::SettingsError::SchemaViolation(format!(
                            "{field_name}[{i}] is empty"
                        )));
                    }
                }
            }
        }
        validate_enum(
            "modelProposedGoals",
            self.model_proposed_goals.as_deref(),
            &["auto", "alwaysAsk", "disabled"],
        )?;
        validate_enum(
            "keybindingFlavor",
            self.keybinding_flavor.as_deref(),
            &["classic", "readline"],
        )?;
        if let Some(fusion) = self.fusion.as_ref() {
            fusion.validate()?;
        }
        Ok(())
    }

    /// Effective `disableCommandPluginSources` value. When unset, this follows
    /// the managed hook restriction flag exactly like Claude Code 2.1.238.
    #[must_use]
    pub fn effective_disable_command_plugin_sources(&self) -> bool {
        self.disable_command_plugin_sources
            .unwrap_or(self.allow_managed_hooks_only == Some(true))
    }

    /// Effective extra marketplace declarations, honoring the 2.1.238 alias
    /// key `additionalMarketplaces`. When both spellings are present, the old
    /// canonical key wins and the alias is ignored with a warning.
    pub fn effective_extra_known_marketplaces<F>(
        &self,
        mut warn: F,
    ) -> Option<BTreeMap<String, Value>>
    where
        F: FnMut(&str),
    {
        match (
            self.extra_known_marketplaces.as_ref(),
            self.additional_marketplaces.as_ref(),
        ) {
            (Some(canonical), Some(_alias)) => {
                warn(
                    "additionalMarketplaces is ignored because extraKnownMarketplaces is also set",
                );
                Some(canonical.clone())
            }
            (Some(canonical), None) => Some(canonical.clone()),
            (None, Some(alias)) => Some(alias.clone()),
            (None, None) => None,
        }
    }

    /// Effective strict marketplace allowlist, honoring the 2.1.238 alias key
    /// `allowedMarketplaces`. When both spellings are present, the old
    /// canonical key wins and the alias is ignored with a warning.
    pub fn effective_strict_known_marketplaces<F>(&self, mut warn: F) -> Option<Vec<Value>>
    where
        F: FnMut(&str),
    {
        match (
            self.strict_known_marketplaces.as_ref(),
            self.allowed_marketplaces.as_ref(),
        ) {
            (Some(canonical), Some(_alias)) => {
                warn("allowedMarketplaces is ignored because strictKnownMarketplaces is also set");
                Some(canonical.clone())
            }
            (Some(canonical), None) => Some(canonical.clone()),
            (None, Some(alias)) => Some(alias.clone()),
            (None, None) => None,
        }
    }
}

fn validate_enum(
    field_name: &str,
    value: Option<&str>,
    allowed: &[&str],
) -> Result<(), crate::settings::SettingsError> {
    let Some(value) = value else {
        return Ok(());
    };
    if allowed.contains(&value) {
        return Ok(());
    }
    Err(crate::settings::SettingsError::SchemaViolation(format!(
        "{field_name} must be one of {} (got {value:?})",
        allowed.join(", ")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fusion_completion_policy_is_typed_optional_and_validated() {
        let absent: FusionSettingsJson = serde_json::from_str("{}").unwrap();
        assert_eq!(absent.completion_policy, None);
        assert!(serde_json::to_value(&absent)
            .unwrap()
            .get("completionPolicy")
            .is_none());
        for (name, policy) in [
            ("wait_all", FusionCompletionPolicy::WaitAll),
            (
                "quorum_after_grace",
                FusionCompletionPolicy::QuorumAfterGrace,
            ),
        ] {
            let settings: FusionSettingsJson =
                serde_json::from_value(serde_json::json!({"completionPolicy": name})).unwrap();
            assert_eq!(settings.completion_policy, Some(policy));
            settings.validate().unwrap();
            assert_eq!(
                serde_json::to_value(settings).unwrap()["completionPolicy"],
                name
            );
        }
        assert!(
            serde_json::from_str::<FusionSettingsJson>(r#"{"completionPolicy":"fast"}"#).is_err()
        );
        assert!(
            serde_json::from_str::<FusionSettingsJson>(r#"{"completionPolicy":true}"#).is_err()
        );
        let invalid: FusionSettingsJson =
            serde_json::from_str(r#"{"completionPolicy":"quorum_after_grace","partialOk":false}"#)
                .unwrap();
        assert!(invalid
            .validate()
            .unwrap_err()
            .to_string()
            .contains("completionPolicy"));
        let wait_all: FusionSettingsJson =
            serde_json::from_str(r#"{"completionPolicy":"wait_all","partialOk":false}"#).unwrap();
        wait_all.validate().unwrap();
    }

    #[test]
    fn fusion_min_successful_above_both_preset_counts_is_rejected() {
        // F011 item 6: minSuccessfulPanels above BOTH quality and fast preset
        // counts can never be met by an automatic run.
        let settings = FusionSettingsJson {
            quality_panel_count: Some(3),
            fast_panel_count: Some(2),
            min_successful_panels: Some(3),
            ..FusionSettingsJson::default()
        };
        let err = settings.validate().unwrap_err();
        assert!(
            matches!(&err, crate::settings::SettingsError::SchemaViolation(msg) if msg.contains("minSuccessfulPanels")),
            "expected a minSuccessfulPanels violation, got {err:?}"
        );
    }

    #[test]
    fn fusion_min_successful_at_the_smaller_preset_count_is_accepted() {
        let settings = FusionSettingsJson {
            quality_panel_count: Some(3),
            fast_panel_count: Some(2),
            min_successful_panels: Some(2),
            ..FusionSettingsJson::default()
        };
        settings.validate().expect("2 <= min(3, 2) must pass");
    }

    #[test]
    fn fusion_min_successful_alone_in_a_file_does_not_trip_the_preset_check() {
        // Regression for a false rejection: `validate()` runs PER SETTINGS
        // FILE (see `read_settings_file` / `read_layer_or_skip`), so a
        // project-tier file that sets only `minSuccessfulPanels` has no
        // opinion on `qualityPanelCount` / `fastPanelCount` — those may live
        // in a different tier. Filling them in with the runtime's
        // RESTRICTIVE defaults (3 and 2) here would reject
        // `{"fusion":{"minSuccessfulPanels":3}}` even though the merged
        // config (e.g. a user tier setting qualityPanelCount=5,
        // fastPanelCount=4) is perfectly valid, and the whole file — not
        // just the fusion block — would then be silently dropped.
        let settings: SettingsJson =
            serde_json::from_str(r#"{"fusion":{"minSuccessfulPanels":3}}"#).unwrap();
        settings
            .validate()
            .expect("no preset count present in this file — must not be rejected");
    }

    #[test]
    fn fusion_total_timeout_alone_in_a_file_does_not_trip_the_stage_sum_check() {
        // Same per-file hazard as above, for the stage-sum check: a tier
        // that only lowers `totalTimeoutMs` has no opinion on
        // `panelTotalTimeoutMs` / `analystTimeoutMs` — defaulting both
        // against this file's total alone would reject a file that sets
        // nothing else (840_000 > 500_000).
        let settings: SettingsJson =
            serde_json::from_str(r#"{"fusion":{"totalTimeoutMs":500000}}"#).unwrap();
        settings
            .validate()
            .expect("no stage field present in this file — must not be rejected");
    }

    #[test]
    fn fusion_stage_field_alone_in_a_file_does_not_trip_the_stage_sum_check() {
        // Finding [15]: the mirror-image per-file hazard from the test
        // above. A project-tier file that sets only `panelTotalTimeoutMs`
        // has no opinion on `totalTimeoutMs` — it may be raised in a
        // DIFFERENT tier (e.g. a user tier setting `totalTimeoutMs:
        // 1800000`, under which 900_000 + 120_000*2 + 180_000 = 1_320_000
        // fits easily). Defaulting `total` to the runtime's restrictive
        // 1_200_000 here must not reject this file on its own
        // (900_000 + 120_000*2 + 180_000 = 1_320_000 > 1_200_000) — that
        // would silently drop the whole tier's unrelated
        // permissions/hooks/model, not just the fusion block.
        let settings: SettingsJson =
            serde_json::from_str(r#"{"fusion":{"panelTotalTimeoutMs":900000}}"#).unwrap();
        settings
            .validate()
            .expect("no totalTimeoutMs present in this file — must not be rejected");
    }

    #[test]
    fn fusion_stage_field_alone_above_the_default_total_does_not_trip_the_per_stage_bound_check() {
        // Finding [15]: the OLDER per-stage `stage > total` check (three
        // lines above the stage-sum check the two tests above pin) still
        // substitutes the runtime's default `total` (1_200_000) for a file
        // that sets no `totalTimeoutMs` of its own, with no presence gate.
        // A project-tier file that raises ONE stage timeout above 20
        // minutes — e.g. `panelTotalTimeoutMs: 1500000`, valid under a user
        // tier's `totalTimeoutMs: 2400000` — has no opinion on the total; it
        // may be raised in a DIFFERENT tier, exactly like the stage-SUM
        // check two tests above already protects against. Filling in the
        // restrictive 1_200_000 default here rejects this file on its own
        // even though the merged config is perfectly valid, and
        // `read_layer_or_skip` then silently drops the whole tier's
        // unrelated permissions/hooks/model along with it.
        let settings: SettingsJson =
            serde_json::from_str(r#"{"fusion":{"panelTotalTimeoutMs":1500000}}"#).unwrap();
        settings
            .validate()
            .expect("no totalTimeoutMs present in this file — must not be rejected");
    }

    #[test]
    fn fusion_panel_idle_timeout_above_same_file_panel_total_is_rejected() {
        // Finding [24]: `panelIdleTimeoutMs` is the one stage field in the
        // `totalTimeoutMs`-gated loop above that `FusionRuntimeConfig::
        // from_settings`'s merged-view stage-sum re-check does NOT cover
        // (it is not a term of that sum), so once `totalTimeoutMs` is
        // absent from a file nothing validates it — and the invariant that
        // actually matters is `panelIdleTimeoutMs <= panelTotalTimeoutMs`
        // (the panel-level stall detector can only ever fire while the
        // panel itself is still running), not a comparison against
        // `totalTimeoutMs`. This must be rejected even with no
        // `totalTimeoutMs` anywhere in the file.
        let settings: SettingsJson = serde_json::from_str(
            r#"{"fusion":{"panelIdleTimeoutMs":5000000,"panelTotalTimeoutMs":600000}}"#,
        )
        .unwrap();
        let err = settings.validate().unwrap_err();
        assert!(
            matches!(&err, crate::settings::SettingsError::SchemaViolation(msg) if msg.contains("panelIdleTimeoutMs") && msg.contains("panelTotalTimeoutMs")),
            "expected a panelIdleTimeoutMs vs panelTotalTimeoutMs violation, got {err:?}"
        );
    }

    #[test]
    fn fusion_panel_idle_timeout_at_or_below_same_file_panel_total_is_accepted() {
        let settings: SettingsJson = serde_json::from_str(
            r#"{"fusion":{"panelIdleTimeoutMs":600000,"panelTotalTimeoutMs":600000}}"#,
        )
        .unwrap();
        settings
            .validate()
            .expect("panelIdleTimeoutMs == panelTotalTimeoutMs must pass");
    }

    #[test]
    fn fusion_panel_idle_timeout_alone_in_a_file_does_not_trip_the_same_file_check() {
        // Same per-file hazard as the other checks in this module: a tier
        // that sets only `panelIdleTimeoutMs` has no opinion on
        // `panelTotalTimeoutMs` — it may be raised in a different tier —
        // so THIS same-file-only check must not fire when the counterpart
        // field is absent from this file. That is NOT the same claim as "a
        // panelIdleTimeoutMs of 5000000 with no panelTotalTimeoutMs
        // anywhere is safe to run with" — it plainly isn't (the default
        // panel_total_timeout_ms is 600_000). Round-3 review finding [24]:
        // that merged-view case is rejected by
        // `FusionRuntimeConfig::from_settings`
        // (fusion/src/config.rs::from_settings_rejects_a_merged_panel_idle_timeout_above_the_default_panel_total),
        // and the watchdog deadline is additionally clamped at the spawn
        // use site (fusion/src/panel.rs::panel_stall_timeout_ms) as defense
        // in depth. This test covers only THIS function's narrow, correct,
        // per-file scope.
        let settings: SettingsJson =
            serde_json::from_str(r#"{"fusion":{"panelIdleTimeoutMs":5000000}}"#).unwrap();
        settings
            .validate()
            .expect("no panelTotalTimeoutMs present in this file — must not be rejected here");
    }

    #[test]
    fn fusion_model_roles_round_trip_and_keep_roster_order() {
        let settings: SettingsJson = serde_json::from_str(
            r#"{"fusion":{
                "panelModels":[
                    {"profile":"anthropic","model":"claude-opus-5"},
                    {"profile":"openai","model":"gpt-5.6-sol"},
                    {"profile":"google","model":"gemini-3-pro"}
                ],
                "analystModel":{"profile":"openai","model":"gpt-5.6-terra"}
            }}"#,
        )
        .unwrap();
        settings.validate().expect("a complete role set is valid");
        let fusion = settings.fusion.as_ref().unwrap();
        let panels = fusion.panel_models.as_ref().unwrap();
        // Order is load-bearing: a preset takes the first N entries, so a
        // round-trip that sorted or set-ified the roster would silently change
        // which models run.
        assert_eq!(
            panels
                .iter()
                .map(|entry| format!("{}/{}", entry.profile, entry.model))
                .collect::<Vec<_>>(),
            vec![
                "anthropic/claude-opus-5".to_string(),
                "openai/gpt-5.6-sol".to_string(),
                "google/gemini-3-pro".to_string(),
            ]
        );
        assert_eq!(
            fusion.analyst_model.as_ref().unwrap().model,
            "gpt-5.6-terra"
        );
        // Absent roles serialize away rather than writing `null`, so a merge
        // of this layer over another cannot shadow the lower layer's roles.
        let written = serde_json::to_value(fusion).unwrap();
        assert!(written.get("panelModels").is_some());
        let empty = serde_json::to_value(FusionSettingsJson::default()).unwrap();
        assert!(empty.get("panelModels").is_none());
        assert!(empty.get("analystModel").is_none());
    }

    #[test]
    fn fusion_panel_roster_shape_violations_each_name_the_offender() {
        let reject = |body: &str| -> String {
            let settings: SettingsJson = serde_json::from_str(body).unwrap();
            settings
                .validate()
                .expect_err("this roster must be rejected")
                .to_string()
        };
        assert!(reject(
            r#"{"fusion":{"panelModels":[{"profile":"anthropic","model":"claude-opus-5"}]}}"#
        )
        .contains("at least 2 models"));
        assert!(reject(
            r#"{"fusion":{"panelModels":[
                {"profile":"anthropic","model":"claude-opus-5"},
                {"profile":"anthropic","model":"claude-opus-5"}
            ]}}"#
        )
        .contains("lists `anthropic/claude-opus-5` twice"));
        assert!(reject(
            r#"{"fusion":{"panelModels":[
                {"profile":"anthropic","model":"claude-opus-5"},
                {"profile":"","model":"gpt-5.6-sol"}
            ]}}"#
        )
        .contains("fusion.panelModels[1].profile must not be empty"));
        assert!(reject(
            r#"{"fusion":{"maxPanel":2,"panelModels":[
                {"profile":"anthropic","model":"claude-opus-5"},
                {"profile":"openai","model":"gpt-5.6-sol"},
                {"profile":"google","model":"gemini-3-pro"}
            ]}}"#
        )
        .contains("exceeding fusion.maxPanel (2)"));
        assert!(reject(
            r#"{"fusion":{"minSuccessfulPanels":3,"qualityPanelCount":3,"fastPanelCount":3,
            "panelModels":[
                {"profile":"anthropic","model":"claude-opus-5"},
                {"profile":"openai","model":"gpt-5.6-sol"}
            ]}}"#
        )
        .contains("fewer than fusion.minSuccessfulPanels (3)"));
        assert!(
            reject(r#"{"fusion":{"analystModel":{"profile":"openai","model":"  "}}}"#)
                .contains("fusion.analystModel.model must not be empty")
        );
    }

    #[test]
    fn a_roster_in_one_tier_and_min_successful_in_another_is_not_rejected_per_file() {
        // The same per-file hazard every other cross-field check here guards:
        // a tier that raises `minSuccessfulPanels` has no opinion on a roster
        // that lives in a different tier, and rejecting the file would make
        // `read_layer_or_skip` drop its unrelated permissions/hooks/model too.
        let roster_only: SettingsJson = serde_json::from_str(
            r#"{"fusion":{"panelModels":[
                {"profile":"anthropic","model":"claude-opus-5"},
                {"profile":"openai","model":"gpt-5.6-sol"}
            ]}}"#,
        )
        .unwrap();
        roster_only
            .validate()
            .expect("roster alone is a valid file");
        let bar_only: SettingsJson =
            serde_json::from_str(r#"{"fusion":{"minSuccessfulPanels":3,"qualityPanelCount":3}}"#)
                .unwrap();
        bar_only
            .validate()
            .expect("a bar with no roster in the same file is a valid file");
    }

    #[test]
    fn agent_push_notification_setting_parses() {
        let parsed: SettingsJson =
            serde_json::from_str(r#"{"agentPushNotifEnabled":true}"#).unwrap();
        assert_eq!(parsed.agent_push_notif_enabled, Some(true));
    }

    #[test]
    fn deserializes_minimal_settings() {
        let json = r#"{"trustedDirectories": ["/foo"]}"#;
        let parsed: SettingsJson = serde_json::from_str(json).unwrap();
        assert_eq!(
            parsed.trusted_directories.as_deref(),
            Some(&["/foo".to_string()][..])
        );
    }

    #[test]
    fn code_review_suggestion_tolerates_passthrough_values_and_keeps_presence() {
        for value in ["true", "false", "null", "1", "\"false\"", "{}", "[]"] {
            let parsed: SettingsJson = serde_json::from_str(&format!(
                "{{\"includeCodeReviewSuggestion\":{value},\"requiredMinimumVersion\":\"999.0.0\"}}"
            ))
            .unwrap();
            assert_eq!(parsed.include_code_review_suggestion, Some(value == "true"));
            assert_eq!(parsed.required_minimum_version.as_deref(), Some("999.0.0"));
            let previous = SettingsJson {
                include_code_review_suggestion: Some(true),
                ..Default::default()
            };
            assert_eq!(
                crate::settings::merger::merge(previous, parsed).include_code_review_suggestion,
                Some(value == "true")
            );
        }
        assert_eq!(SettingsJson::default().include_code_review_suggestion, None);
    }

    #[test]
    fn deserializes_workflow_size_guideline() {
        let parsed: SettingsJson =
            serde_json::from_str(r#"{"workflowSizeGuideline":"medium"}"#).unwrap();
        assert_eq!(parsed.workflow_size_guideline.as_deref(), Some("medium"));
    }

    #[test]
    fn deserializes_enable_workflows() {
        let parsed: SettingsJson = serde_json::from_str(r#"{"enableWorkflows":false}"#).unwrap();
        assert_eq!(parsed.enable_workflows, Some(false));
    }

    #[test]
    fn ignores_unknown_fields_zod_passthrough_parity() {
        // zod `SettingsSchema` is `.passthrough()` (types.ts:1072; safeParse
        // at settings.ts:219) — unknown keys never fail a TS load; known
        // siblings must still parse typed.
        let json = r#"{"trustedDirectories": ["/foo"], "bogusField": 1}"#;
        let parsed: SettingsJson =
            serde_json::from_str(json).expect("unknown keys must be ignored, not rejected");
        assert_eq!(
            parsed.trusted_directories.as_deref(),
            Some(&["/foo".to_string()][..])
        );
    }

    #[test]
    fn accepts_permissions_block() {
        // Regression history: under the (since-removed) `deny_unknown_fields`
        // strictness, a settings.json carrying a claude-code permissions block
        // broke the entire load until the field was declared. Today unknown
        // keys are tolerated anyway; the typed field exists for ACCESS — rule
        // projection via `permission::permission_rules_from_settings_json`
        // and the DeepMerge strategy — not load-gating.
        let json = r#"{
            "permissions": { "allow": ["Bash(npm run *)"], "deny": ["Read(./secrets/**)"], "ask": [], "defaultMode": "default" }
        }"#;
        let parsed: SettingsJson =
            serde_json::from_str(json).expect("permissions block must parse");
        assert!(parsed.permissions.is_some());
        assert!(
            strategy_for("permissions").is_some(),
            "permissions has a merge strategy"
        );
    }

    #[test]
    fn max_effort_level_and_model_settings_parse() {
        let parsed: SettingsJson = serde_json::from_str(
            r#"{
                "maxEffortLevel": "high",
                "effortLevel": "medium",
                "modelSettings": {
                    "claude-opus-4-7": { "maxEffortLevel": "low" }
                }
            }"#,
        )
        .expect("maxEffortLevel schema");
        assert_eq!(parsed.max_effort_level.as_deref(), Some("high"));
        assert_eq!(parsed.effort_level.as_deref(), Some("medium"));
        assert_eq!(
            parsed
                .model_settings
                .as_ref()
                .and_then(|m| m.get("claude-opus-4-7"))
                .and_then(|e| e.max_effort_level.as_deref()),
            Some("low")
        );
        assert!(
            matches!(
                strategy_for("modelSettings"),
                Some(MergeStrategy::DeepMerge)
            ),
            "modelSettings deep-merges; the lowest-wins cap is folded at the consumer"
        );
        assert!(
            strategy_for("maxEffortLevel").is_none(),
            "top-level maxEffortLevel is not last-wins-merged"
        );
    }

    #[test]
    fn accepts_string_output_style() {
        // OUTSTYLE.1 regression history: `output_style` was once typed as a
        // map, so a real claude-code settings.json carrying the documented
        // `"outputStyle": "Explanatory"` (a string) failed deserialization
        // under the then-strict schema and broke the ENTIRE settings load
        // (dropping model / permissions / hooks in that layer). TS types it
        // `z.string()` — it must parse as a string so the value is ACCESSIBLE
        // typed (unknown-key tolerance alone would strip it, not surface it).
        let json = r#"{ "outputStyle": "Explanatory", "model": "claude-sonnet-4-5" }"#;
        let parsed: SettingsJson =
            serde_json::from_str(json).expect("string outputStyle must parse");
        assert_eq!(parsed.output_style.as_deref(), Some("Explanatory"));
        assert!(
            parsed.model.is_some(),
            "sibling fields must survive the load"
        );
        // Scalar-override, not deep-merge.
        assert!(
            strategy_for("outputStyle").is_none(),
            "outputStyle must be scalar-override"
        );
    }

    #[test]
    fn tolerates_dollar_schema_field_for_future_compat() {
        // claude-code @ 6a25909 does NOT emit "$schema". We tolerate it for forward-compat.
        let json = r#"{"$schema": "https://example.com/schema.json", "trustedDirectories": []}"#;
        let parsed = serde_json::from_str::<SettingsJson>(json);
        assert!(parsed.is_ok(), "should tolerate $schema field: {parsed:?}");
    }

    #[test]
    fn merge_strategies_table_covers_array_fields() {
        // The four spec §7 array-merge fields MUST be registered as ConcatDedup.
        for field in [
            "trustedDirectories",
            "additionalDirectories",
            "enabledTools",
            "additionalIncludes",
            "lingxiMdExcludes",
        ] {
            let strat =
                strategy_for(field).unwrap_or_else(|| panic!("missing strategy for {field}"));
            assert!(
                matches!(strat, MergeStrategy::ConcatDedup),
                "{field} should be ConcatDedup, got {strat:?}"
            );
        }
    }

    #[test]
    fn merge_strategies_table_covers_object_fields() {
        // The spec §7 object-merge fields MUST be registered as DeepMerge
        // (plus the `permissions` block — claude-code parity).
        for field in ["sandbox", "hooks", "permissions"] {
            let strat =
                strategy_for(field).unwrap_or_else(|| panic!("missing strategy for {field}"));
            assert!(
                matches!(strat, MergeStrategy::DeepMerge),
                "{field} should be DeepMerge, got {strat:?}"
            );
        }
        // OUTSTYLE.1: `outputStyle` is a scalar string in claude-code, so it
        // must NOT be a registered DeepMerge field — it falls through to the
        // default Override strategy (later layer wins).
        assert!(
            strategy_for("outputStyle").is_none(),
            "outputStyle must default to Override (scalar), not be registered DeepMerge"
        );
    }

    #[test]
    fn skip_web_fetch_preflight_parses_roundtrips_and_is_scalar_override() {
        // CC 2.1.207 zod: `skipWebFetchPreflight:E.boolean().optional()`. A
        // settings.json carrying `"skipWebFetchPreflight": true` must parse into
        // the typed `Some(true)` (unknown-key tolerance alone would strip it, so
        // the value would never be ACCESSIBLE to the WebFetch tool).
        let json = r#"{ "skipWebFetchPreflight": true, "model": "claude-sonnet-4-5" }"#;
        let parsed: SettingsJson =
            serde_json::from_str(json).expect("skipWebFetchPreflight bool must parse");
        assert_eq!(parsed.skip_web_fetch_preflight, Some(true));
        assert!(
            parsed.model.is_some(),
            "sibling fields must survive the load"
        );
        // camelCase on the wire; round-trips.
        let back = serde_json::to_string(&parsed).expect("serialize");
        assert!(back.contains("\"skipWebFetchPreflight\":true"), "{back}");
        // Absent ⇒ None (distinct from `Some(false)`), and NOT emitted on
        // re-serialize (skip_serializing_if).
        let absent: SettingsJson = serde_json::from_str(r#"{"model":"x"}"#).unwrap();
        assert_eq!(absent.skip_web_fetch_preflight, None);
        assert!(!serde_json::to_string(&absent)
            .unwrap()
            .contains("skipWebFetchPreflight"));
        // Scalar-override merge (later layer wins) — must NOT be a registered
        // ConcatDedup/DeepMerge field (falls through to the default Override).
        assert!(
            strategy_for("skipWebFetchPreflight").is_none(),
            "skipWebFetchPreflight must be scalar-override (later source wins)"
        );
    }

    #[test]
    fn view_mode_parses_roundtrips_and_is_scalar_override() {
        // H-BIN-11 (cc2.1.207) zod:
        // `viewMode:E.enum(["default","verbose","focus"]).optional()
        //  .catch(void 0).describe("Default transcript view mode on startup")`.
        // A settings.json carrying `"viewMode": "focus"` must parse into the
        // typed `Some("focus")` so `/focus`/`/tui` can read it.
        for value in ["default", "verbose", "focus"] {
            let json = format!(r#"{{ "viewMode": "{value}", "model": "claude-sonnet-4-5" }}"#);
            let parsed: SettingsJson =
                serde_json::from_str(&json).expect("viewMode enum value must parse");
            assert_eq!(parsed.view_mode.as_deref(), Some(value));
            assert!(parsed.model.is_some(), "sibling fields must survive");
        }
        // camelCase on the wire; round-trips.
        let parsed: SettingsJson = serde_json::from_str(r#"{"viewMode":"focus"}"#).unwrap();
        let back = serde_json::to_string(&parsed).expect("serialize");
        assert!(back.contains("\"viewMode\":\"focus\""), "{back}");
        // zod `.catch(void 0)` tolerance: an out-of-enum value still parses
        // (the string round-trips; the reader treats it as `default`).
        let odd: SettingsJson =
            serde_json::from_str(r#"{"viewMode":"bogus","model":"x"}"#).expect("catch tolerance");
        assert_eq!(odd.view_mode.as_deref(), Some("bogus"));
        assert!(
            odd.model.is_some(),
            "sibling fields survive an odd viewMode"
        );
        // Absent ⇒ None, and NOT emitted on re-serialize (skip_serializing_if).
        let absent: SettingsJson = serde_json::from_str(r#"{"model":"x"}"#).unwrap();
        assert_eq!(absent.view_mode, None);
        assert!(!serde_json::to_string(&absent).unwrap().contains("viewMode"));
        // Scalar-override merge (later layer wins) — NOT a registered
        // ConcatDedup/DeepMerge field.
        assert!(
            strategy_for("viewMode").is_none(),
            "viewMode must be scalar-override (later source wins)"
        );
    }

    #[test]
    fn validate_rejects_empty_string_in_trusted_dirs() {
        let json = r#"{"trustedDirectories": ["/foo", ""]}"#;
        let parsed: SettingsJson = serde_json::from_str(json).unwrap();
        let err = parsed.validate().unwrap_err();
        assert!(
            matches!(err, crate::settings::SettingsError::SchemaViolation(ref s) if s.contains("trustedDirectories[1]")),
            "expected SchemaViolation pointing at index 1, got: {err:?}"
        );
    }

    #[test]
    fn validate_accepts_well_formed_settings() {
        let json = r#"{"trustedDirectories": ["/foo"], "telemetryEnabled": true}"#;
        let parsed: SettingsJson = serde_json::from_str(json).unwrap();
        assert!(parsed.validate().is_ok());
    }

    #[test]
    fn new_238_settings_keys_parse_roundtrip_and_validate() {
        let json = r#"{
            "policyHelpers": {
                "defaultSettings": { "command": "/usr/local/bin/policy-helper" },
                "darwin": { "command": "/opt/policy-helper" }
            },
            "syncClaudeAiSkills": true,
            "additionalMarketplaces": {
                "corp": { "source": { "source": "directory", "path": "/tmp/corp" } }
            },
            "allowedMarketplaces": [
                "corp",
                { "source": "github", "repo": "acme/plugins" }
            ],
            "disableCommandPluginSources": true,
            "spellcheck": { "enabled": true, "checkFilenames": false },
            "dialogExpiry": "5m",
            "modelProposedGoals": "alwaysAsk",
            "keybindingFlavor": "readline",
            "autoContinueAtUsageLimit": false,
            "crossSessionInbound": "hold"
        }"#;
        let parsed: SettingsJson = serde_json::from_str(json).expect("parse");
        assert_eq!(parsed.sync_claude_ai_skills, Some(true));
        assert_eq!(
            parsed
                .additional_marketplaces
                .as_ref()
                .and_then(|m| m.get("corp")),
            Some(&serde_json::json!({
                "source": { "source": "directory", "path": "/tmp/corp" }
            }))
        );
        assert_eq!(
            parsed.allowed_marketplaces.as_deref(),
            Some(
                &[
                    serde_json::json!("corp"),
                    serde_json::json!({ "source": "github", "repo": "acme/plugins" })
                ][..]
            )
        );
        assert_eq!(parsed.disable_command_plugin_sources, Some(true));
        assert_eq!(parsed.model_proposed_goals.as_deref(), Some("alwaysAsk"));
        assert_eq!(parsed.keybinding_flavor.as_deref(), Some("readline"));
        assert_eq!(parsed.auto_continue_at_usage_limit, Some(false));
        assert!(parsed.policy_helpers.is_some());
        assert!(parsed.spellcheck.is_some());
        parsed.validate().expect("enum values should validate");

        let back = serde_json::to_string(&parsed).expect("serialize");
        for key in [
            "policyHelpers",
            "syncClaudeAiSkills",
            "additionalMarketplaces",
            "allowedMarketplaces",
            "disableCommandPluginSources",
            "spellcheck",
            "dialogExpiry",
            "modelProposedGoals",
            "keybindingFlavor",
            "autoContinueAtUsageLimit",
            "crossSessionInbound",
        ] {
            assert!(
                back.contains(&format!("\"{key}\"")),
                "missing {key}: {back}"
            );
        }
        let ordered_keys = [
            "policyHelpers",
            "syncClaudeAiSkills",
            "additionalMarketplaces",
            "allowedMarketplaces",
            "disableCommandPluginSources",
            "spellcheck",
            "dialogExpiry",
            "modelProposedGoals",
            "keybindingFlavor",
            "autoContinueAtUsageLimit",
            "crossSessionInbound",
        ];
        let positions = ordered_keys
            .iter()
            .map(|key| back.find(&format!("\"{key}\"")).expect("serialized key"))
            .collect::<Vec<_>>();
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "2.1.238 settings keys must serialize in oracle schema order: {back}"
        );
        assert_eq!(
            strategy_for("policyHelpers"),
            Some(MergeStrategy::DeepMerge)
        );
        assert_eq!(strategy_for("spellcheck"), Some(MergeStrategy::DeepMerge));
        assert_eq!(
            strategy_for("additionalMarketplaces"),
            Some(MergeStrategy::DeepMerge)
        );
        for key in [
            "syncClaudeAiSkills",
            "allowedMarketplaces",
            "disableCommandPluginSources",
            "modelProposedGoals",
            "keybindingFlavor",
            "autoContinueAtUsageLimit",
        ] {
            assert!(
                strategy_for(key).is_none(),
                "{key} must remain scalar-override"
            );
        }
    }

    #[test]
    fn validate_rejects_invalid_new_238_enums() {
        let bad_keybinding: SettingsJson =
            serde_json::from_str(r#"{"keybindingFlavor":"vim"}"#).expect("parse");
        let err = bad_keybinding
            .validate()
            .expect_err("invalid enum must fail");
        assert!(format!("{err:?}").contains("keybindingFlavor"));

        let bad_goals: SettingsJson =
            serde_json::from_str(r#"{"modelProposedGoals":"sometimes"}"#).expect("parse");
        let err = bad_goals.validate().expect_err("invalid enum must fail");
        assert!(format!("{err:?}").contains("modelProposedGoals"));
    }

    #[test]
    fn new_238_alias_helpers_and_disable_command_default_follow_existing_surface() {
        let parsed: SettingsJson = serde_json::from_str(
            r#"{
                "allowManagedHooksOnly": true,
                "additionalMarketplaces": {
                    "alias": { "source": { "source": "directory", "path": "/tmp/alias" } }
                },
                "allowedMarketplaces": ["alias"]
            }"#,
        )
        .expect("parse");
        assert!(parsed.effective_disable_command_plugin_sources());

        let mut warnings = Vec::new();
        let extra =
            parsed.effective_extra_known_marketplaces(|warning| warnings.push(warning.to_string()));
        assert_eq!(
            extra.as_ref().and_then(|m| m.get("alias")),
            Some(&serde_json::json!({
                "source": { "source": "directory", "path": "/tmp/alias" }
            }))
        );
        let allowed = parsed
            .effective_strict_known_marketplaces(|warning| warnings.push(warning.to_string()));
        assert_eq!(allowed, Some(vec![serde_json::json!("alias")]));
        assert!(warnings.is_empty());

        let parsed: SettingsJson = serde_json::from_str(
            r#"{
                "allowManagedHooksOnly": false,
                "disableCommandPluginSources": false,
                "extraKnownMarketplaces": {
                    "canonical": { "source": { "source": "directory", "path": "/tmp/canonical" } }
                },
                "additionalMarketplaces": {
                    "alias": { "source": { "source": "directory", "path": "/tmp/alias" } }
                },
                "strictKnownMarketplaces": ["canonical"],
                "allowedMarketplaces": ["alias"]
            }"#,
        )
        .expect("parse");
        assert!(!parsed.effective_disable_command_plugin_sources());

        let mut warnings = Vec::new();
        let extra =
            parsed.effective_extra_known_marketplaces(|warning| warnings.push(warning.to_string()));
        let allowed = parsed
            .effective_strict_known_marketplaces(|warning| warnings.push(warning.to_string()));
        assert_eq!(
            extra.as_ref().and_then(|m| m.get("canonical")),
            Some(&serde_json::json!({
                "source": { "source": "directory", "path": "/tmp/canonical" }
            }))
        );
        assert_eq!(allowed, Some(vec![serde_json::json!("canonical")]));
        assert_eq!(
            warnings,
            vec![
                "additionalMarketplaces is ignored because extraKnownMarketplaces is also set",
                "allowedMarketplaces is ignored because strictKnownMarketplaces is also set",
            ]
        );
    }

    #[test]
    fn plugin_and_marketplace_settings_parse_roundtrip_and_merge_correctly() {
        let json = r#"{
            "enabledPlugins": { "demo@example": true },
            "pluginConfigs": {
                "demo@example": { "options": { "REGION": "us-east-1" } }
            },
            "extraKnownMarketplaces": {
                "team": { "source": { "type": "directory", "path": "/tmp/team" } }
            },
            "strictKnownMarketplaces": [
                { "source": "github", "repo": "acme/plugins" }
            ],
            "blockedMarketplaces": ["deprecated"],
            "model": "claude-sonnet-4-5"
        }"#;
        let parsed: SettingsJson = serde_json::from_str(json).expect("parse");
        assert!(parsed.enabled_plugins.is_some());
        assert!(parsed.plugin_configs.is_some());
        assert!(parsed.extra_known_marketplaces.is_some());
        assert!(parsed.strict_known_marketplaces.is_some());
        assert_eq!(
            parsed.blocked_marketplaces.as_deref(),
            Some(&["deprecated".to_string()][..])
        );

        let back = serde_json::to_string(&parsed).expect("serialize");
        for key in [
            "enabledPlugins",
            "pluginConfigs",
            "extraKnownMarketplaces",
            "strictKnownMarketplaces",
            "blockedMarketplaces",
        ] {
            assert!(
                back.contains(&format!("\"{key}\"")),
                "missing {key}: {back}"
            );
        }

        for key in ["enabledPlugins", "pluginConfigs", "extraKnownMarketplaces"] {
            assert_eq!(
                strategy_for(key),
                Some(MergeStrategy::DeepMerge),
                "{key} must deep-merge"
            );
        }
        for key in ["strictKnownMarketplaces", "blockedMarketplaces"] {
            assert!(
                strategy_for(key).is_none(),
                "{key} must remain scalar-override"
            );
        }
    }

    #[test]
    fn aws_gcp_auth_refresh_keys_parse_and_roundtrip() {
        // 2.1.198 settings schema: awsAuthRefresh / awsCredentialExport /
        // gcpAuthRefresh are plain string keys (script paths / commands).
        let json = r#"{
            "awsAuthRefresh": "aws sso login --profile myprofile",
            "awsCredentialExport": "/usr/local/bin/export-aws-creds.sh",
            "gcpAuthRefresh": "gcloud auth application-default login"
        }"#;
        let parsed: SettingsJson = serde_json::from_str(json).expect("parse");
        assert_eq!(
            parsed.aws_auth_refresh.as_deref(),
            Some("aws sso login --profile myprofile")
        );
        assert_eq!(
            parsed.aws_credential_export.as_deref(),
            Some("/usr/local/bin/export-aws-creds.sh")
        );
        assert_eq!(
            parsed.gcp_auth_refresh.as_deref(),
            Some("gcloud auth application-default login")
        );
        // camelCase on the wire (claude-code emits camelCase; scalar-override
        // merge — none of the three is in MERGE_STRATEGIES).
        let back = serde_json::to_string(&parsed).expect("serialize");
        assert!(back.contains("\"awsAuthRefresh\""));
        assert!(back.contains("\"awsCredentialExport\""));
        assert!(back.contains("\"gcpAuthRefresh\""));
        for key in ["awsAuthRefresh", "awsCredentialExport", "gcpAuthRefresh"] {
            assert!(
                strategy_for(key).is_none(),
                "{key} must be scalar-override (later source wins)"
            );
        }
    }

    #[test]
    fn otel_headers_helper_key_parses_and_roundtrips() {
        // 2.1.207 Monitoring schema (H-BIN-06): otelHeadersHelper is a plain
        // string key (script path / shell command). camelCase on the wire.
        let json = r#"{ "otelHeadersHelper": "/opt/otel/get-headers.sh" }"#;
        let parsed: SettingsJson = serde_json::from_str(json).expect("parse");
        assert_eq!(
            parsed.otel_headers_helper.as_deref(),
            Some("/opt/otel/get-headers.sh")
        );
        // Round-trips under the exact camelCase wire key CC emits/reads.
        let back = serde_json::to_string(&parsed).expect("serialize");
        assert!(
            back.contains("\"otelHeadersHelper\""),
            "must serialize back to the byte-exact camelCase key"
        );
        // Scalar-override merge (later source wins; not in MERGE_STRATEGIES).
        assert!(strategy_for("otelHeadersHelper").is_none());
    }

    #[test]
    fn ask_user_question_timeout_parses_and_is_scalar_override() {
        // 2.1.201 settings schema: askUserQuestionTimeout is an enum string
        // (60s|5m|10m|never), scalar-override merge. Default is `never`.
        let json = r#"{ "askUserQuestionTimeout": "5m", "model": "claude-sonnet-4-5" }"#;
        let parsed: SettingsJson =
            serde_json::from_str(json).expect("askUserQuestionTimeout must parse");
        assert_eq!(parsed.ask_user_question_timeout.as_deref(), Some("5m"));
        // Sibling survives the load.
        assert!(parsed.model.is_some());
        // camelCase on the wire + scalar-override (not registered DeepMerge).
        let back = serde_json::to_string(&parsed).expect("serialize");
        assert!(back.contains("\"askUserQuestionTimeout\""));
        assert!(
            strategy_for("askUserQuestionTimeout").is_none(),
            "askUserQuestionTimeout must be scalar-override (later source wins)"
        );
    }

    #[test]
    fn prompt_cache_ttl_settings_use_current_enum_values_and_scalar_override() {
        let parsed: SettingsJson = serde_json::from_str(
            r#"{"promptCacheTtl":"1h","subagentPromptCacheTtl":"5m"}"#,
        )
        .unwrap();
        assert_eq!(parsed.prompt_cache_ttl, Some(PromptCacheTtl::OneHour));
        assert_eq!(
            parsed.subagent_prompt_cache_ttl,
            Some(PromptCacheTtl::FiveMinutes)
        );
        let serialized = serde_json::to_value(&parsed).unwrap();
        assert_eq!(serialized["promptCacheTtl"], "1h");
        assert_eq!(serialized["subagentPromptCacheTtl"], "5m");

        let invalid: SettingsJson = serde_json::from_value(serde_json::json!({
            "promptCacheTtl": "never",
            "subagentPromptCacheTtl": true
        }))
        .unwrap();
        assert_eq!(invalid.prompt_cache_ttl, None);
        assert_eq!(invalid.subagent_prompt_cache_ttl, None);
        assert!(strategy_for("promptCacheTtl").is_none());
        assert!(strategy_for("subagentPromptCacheTtl").is_none());
    }

    #[test]
    fn ask_user_question_timeout_absent_is_none() {
        // Absent key ⇒ None (the tool layer resolves absent ⇒ default `never`).
        let parsed: SettingsJson = serde_json::from_str(r#"{"model":"opus"}"#).expect("parse");
        assert!(parsed.ask_user_question_timeout.is_none());
    }

    #[test]
    fn providers_field_roundtrips() {
        let raw = r#"{"providers":{"groq":{"type":"openai","baseUrl":"https://api.groq.com/openai/v1","apiKeyEnv":"GROQ_API_KEY"}}}"#;
        let parsed: SettingsJson = serde_json::from_str(raw).expect("parse");
        assert!(parsed.providers.is_some());
        let back = serde_json::to_string(&parsed).expect("serialize");
        assert!(back.contains("\"providers\""));
    }

    #[test]
    fn managed_model_allowlist_keys_parse_and_roundtrip() {
        // CC 2.1.207 managed model-restriction keys: availableModels (array),
        // enforceAvailableModels (bool), modelOverrides (record). A managed
        // settings.json carrying them must parse into the typed fields (unknown-
        // key tolerance alone would strip them, so the allowlist would be
        // silently dropped and enforcement never bind).
        let json = r#"{
            "availableModels": ["opus", "claude-sonnet-4-5"],
            "enforceAvailableModels": true,
            "modelOverrides": { "claude-opus-4-6": "arn:aws:bedrock:us-east-1::inference-profile/opus" },
            "model": "claude-sonnet-4-5"
        }"#;
        let parsed: SettingsJson = serde_json::from_str(json).expect("managed keys must parse");
        assert_eq!(
            parsed.available_models.as_deref(),
            Some(&["opus".to_string(), "claude-sonnet-4-5".to_string()][..])
        );
        assert_eq!(parsed.enforce_available_models, Some(true));
        assert_eq!(
            parsed
                .model_overrides
                .as_ref()
                .and_then(|m| m.get("claude-opus-4-6"))
                .map(String::as_str),
            Some("arn:aws:bedrock:us-east-1::inference-profile/opus")
        );
        // Sibling survives the load.
        assert!(parsed.model.is_some());
        // camelCase on the wire; round-trips.
        let back = serde_json::to_string(&parsed).expect("serialize");
        assert!(back.contains("\"availableModels\""), "{back}");
        assert!(back.contains("\"enforceAvailableModels\":true"), "{back}");
        assert!(back.contains("\"modelOverrides\""), "{back}");
    }

    #[test]
    fn available_models_absent_vs_empty_are_distinct() {
        // Absent ⇒ None (no restriction); explicit [] ⇒ Some(vec![]) (only the
        // default model available) — the two must round-trip distinctly.
        let absent: SettingsJson = serde_json::from_str(r#"{"model":"opus"}"#).unwrap();
        assert!(absent.available_models.is_none());
        assert!(!serde_json::to_string(&absent)
            .unwrap()
            .contains("availableModels"));
        let empty: SettingsJson = serde_json::from_str(r#"{"availableModels":[]}"#).unwrap();
        assert_eq!(empty.available_models.as_deref(), Some(&[][..]));
    }

    #[test]
    fn model_allowlist_merge_strategies() {
        // modelOverrides deep-merges (CC leaves objects to lodash recursive
        // merge); availableModels/enforceAvailableModels are scalar-override.
        assert!(
            matches!(
                strategy_for("modelOverrides"),
                Some(MergeStrategy::DeepMerge)
            ),
            "modelOverrides must deep-merge"
        );
        assert!(
            strategy_for("availableModels").is_none(),
            "availableModels must be scalar-override (source array wins)"
        );
        assert!(
            strategy_for("enforceAvailableModels").is_none(),
            "enforceAvailableModels must be scalar-override"
        );
    }

    #[test]
    fn enterprise_login_version_keys_parse_and_roundtrip() {
        // CC 2.1.207 enterprise login/version managed-policy keys (H-BIN-09). A
        // managed settings.json carrying them must parse into the typed fields
        // (unknown-key tolerance alone would strip them, so the version gate /
        // login policy would never see them).
        let json = r#"{
            "forceLoginMethod": "gateway",
            "forceLoginGatewayUrl": "https://gw.example.com/oidc",
            "forceLoginOrgUUID": ["11111111-2222-3333-4444-555555555555"],
            "parentSettingsBehavior": "merge",
            "minimumVersion": "2.1.0",
            "requiredMinimumVersion": "2.1.207",
            "requiredMaximumVersion": "3.0.0",
            "forceRemoteSettingsRefresh": true,
            "model": "claude-sonnet-4-5"
        }"#;
        let parsed: SettingsJson =
            serde_json::from_str(json).expect("enterprise policy keys must parse");
        assert_eq!(parsed.force_login_method.as_deref(), Some("gateway"));
        assert_eq!(
            parsed.force_login_gateway_url.as_deref(),
            Some("https://gw.example.com/oidc")
        );
        assert_eq!(
            parsed.force_login_org_uuid,
            Some(Value::Array(vec![Value::String(
                "11111111-2222-3333-4444-555555555555".to_string()
            )]))
        );
        assert_eq!(parsed.parent_settings_behavior.as_deref(), Some("merge"));
        assert_eq!(parsed.minimum_version.as_deref(), Some("2.1.0"));
        assert_eq!(parsed.required_minimum_version.as_deref(), Some("2.1.207"));
        assert_eq!(parsed.required_maximum_version.as_deref(), Some("3.0.0"));
        assert_eq!(parsed.force_remote_settings_refresh, Some(true));
        // Sibling survives the load.
        assert!(parsed.model.is_some());
        // camelCase on the wire; round-trips.
        let back = serde_json::to_string(&parsed).expect("serialize");
        for key in [
            "forceLoginMethod",
            "forceLoginGatewayUrl",
            "forceLoginOrgUUID",
            "parentSettingsBehavior",
            "minimumVersion",
            "requiredMinimumVersion",
            "requiredMaximumVersion",
            "forceRemoteSettingsRefresh",
        ] {
            assert!(
                back.contains(&format!("\"{key}\"")),
                "{key} missing in {back}"
            );
            // All scalar-override merge (later source wins).
            assert!(
                strategy_for(key).is_none(),
                "{key} must be scalar-override (later source wins)"
            );
        }
    }

    #[test]
    fn force_login_org_uuid_accepts_single_string() {
        // The union (string | string[]) — a single string form must parse and
        // NOT fail the load.
        let json = r#"{"forceLoginOrgUUID": "org-abc"}"#;
        let parsed: SettingsJson = serde_json::from_str(json).expect("single-string form parses");
        assert_eq!(
            parsed.force_login_org_uuid,
            Some(Value::String("org-abc".to_string()))
        );
    }

    #[test]
    fn enterprise_keys_absent_are_none_and_not_emitted() {
        let absent: SettingsJson = serde_json::from_str(r#"{"model":"x"}"#).unwrap();
        assert!(absent.force_login_method.is_none());
        assert!(absent.required_minimum_version.is_none());
        assert!(absent.force_remote_settings_refresh.is_none());
        let back = serde_json::to_string(&absent).unwrap();
        for key in [
            "forceLoginMethod",
            "requiredMinimumVersion",
            "forceRemoteSettingsRefresh",
        ] {
            assert!(!back.contains(key), "{key} must not be emitted when absent");
        }
    }

    #[test]
    fn http_hook_security_keys_parse_roundtrip_and_are_concat_dedup() {
        // H-BIN-12: CC 2.1.207 HTTP-hook security allowlists. A settings.json
        // carrying them must parse into the typed fields (unknown-key tolerance
        // alone would strip them, so the HTTP hook executor would never see the
        // policy). Both are array-merge (ConcatDedup) — CC `settingsMergeCustomizer`
        // concat-dedups every array except `fallbackModel`.
        let json = r#"{
            "allowedHttpHookUrls": ["https://hooks.example.com/*"],
            "httpHookAllowedEnvVars": ["HOOK_TOKEN", "TEAM_ID"],
            "model": "claude-sonnet-4-5"
        }"#;
        let parsed: SettingsJson = serde_json::from_str(json).expect("http-hook keys must parse");
        assert_eq!(
            parsed.allowed_http_hook_urls.as_deref(),
            Some(&["https://hooks.example.com/*".to_string()][..])
        );
        assert_eq!(
            parsed.http_hook_allowed_env_vars.as_deref(),
            Some(&["HOOK_TOKEN".to_string(), "TEAM_ID".to_string()][..])
        );
        // Sibling survives the load.
        assert!(parsed.model.is_some());
        // camelCase on the wire; round-trips.
        let back = serde_json::to_string(&parsed).expect("serialize");
        assert!(back.contains("\"allowedHttpHookUrls\""), "{back}");
        assert!(back.contains("\"httpHookAllowedEnvVars\""), "{back}");
        // Both concat-dedup (arrays merge across settings sources).
        for key in ["allowedHttpHookUrls", "httpHookAllowedEnvVars"] {
            assert!(
                matches!(strategy_for(key), Some(MergeStrategy::ConcatDedup)),
                "{key} must be ConcatDedup"
            );
        }
    }

    #[test]
    fn allowed_http_hook_urls_absent_vs_empty_are_distinct() {
        // Absent ⇒ None (all URLs allowed); explicit [] ⇒ Some(vec![]) (block ALL
        // HTTP hooks) — the two must round-trip distinctly.
        let absent: SettingsJson = serde_json::from_str(r#"{"model":"opus"}"#).unwrap();
        assert!(absent.allowed_http_hook_urls.is_none());
        assert!(!serde_json::to_string(&absent)
            .unwrap()
            .contains("allowedHttpHookUrls"));
        let empty: SettingsJson = serde_json::from_str(r#"{"allowedHttpHookUrls":[]}"#).unwrap();
        assert_eq!(empty.allowed_http_hook_urls.as_deref(), Some(&[][..]));
    }

    #[test]
    fn company_announcements_parses_roundtrips_and_is_concat_dedup() {
        // CC 2.1.207 zod: `companyAnnouncements:E.array(E.string()).optional()`.
        let json = r#"{ "companyAnnouncements": ["hi", "there"], "model": "x" }"#;
        let parsed: SettingsJson =
            serde_json::from_str(json).expect("companyAnnouncements array must parse");
        assert_eq!(
            parsed.company_announcements.as_deref(),
            Some(&["hi".to_string(), "there".to_string()][..])
        );
        // Sibling survives.
        assert!(parsed.model.is_some());
        // camelCase on the wire; round-trips.
        let back = serde_json::to_string(&parsed).expect("serialize");
        assert!(
            back.contains("\"companyAnnouncements\":[\"hi\",\"there\"]"),
            "{back}"
        );
        // Absent ⇒ None, not emitted.
        let absent: SettingsJson = serde_json::from_str(r#"{"model":"x"}"#).unwrap();
        assert_eq!(absent.company_announcements, None);
        assert!(!serde_json::to_string(&absent)
            .unwrap()
            .contains("companyAnnouncements"));
        // Array-merge (concat-dedup), matching CC's array customizer.
        assert!(
            matches!(
                strategy_for("companyAnnouncements"),
                Some(MergeStrategy::ConcatDedup)
            ),
            "companyAnnouncements must be ConcatDedup"
        );
    }

    #[test]
    fn plans_directory_and_api_key_helper_parse_roundtrip_and_are_scalar_override() {
        // CC 2.1.207 zod: both are `E.string().optional()`, scalar-override merge.
        let json = r#"{ "plansDirectory": "docs/plans", "apiKeyHelper": "/usr/local/bin/get-key.sh", "model": "x" }"#;
        let parsed: SettingsJson = serde_json::from_str(json).expect("must parse");
        assert_eq!(parsed.plans_directory.as_deref(), Some("docs/plans"));
        assert_eq!(
            parsed.api_key_helper.as_deref(),
            Some("/usr/local/bin/get-key.sh")
        );
        assert!(parsed.model.is_some());
        // camelCase on the wire.
        let back = serde_json::to_string(&parsed).expect("serialize");
        assert!(back.contains("\"plansDirectory\":\"docs/plans\""), "{back}");
        assert!(
            back.contains("\"apiKeyHelper\":\"/usr/local/bin/get-key.sh\""),
            "{back}"
        );
        // Absent ⇒ None, not emitted.
        let absent: SettingsJson = serde_json::from_str(r#"{"model":"x"}"#).unwrap();
        assert_eq!(absent.plans_directory, None);
        assert_eq!(absent.api_key_helper, None);
        // Scalar-override merge (not registered ConcatDedup/DeepMerge).
        assert!(strategy_for("plansDirectory").is_none());
        assert!(strategy_for("apiKeyHelper").is_none());
    }

    #[test]
    fn routing_field_roundtrips() {
        let raw = r#"{"routing":{"aliases":{"fast":"openai/gpt-4o"},"retry":{"maxAttempts":2,"backoffMs":100}}}"#;
        let parsed: SettingsJson = serde_json::from_str(raw).expect("parse");
        assert!(parsed.routing.is_some());
        let back = serde_json::to_string(&parsed).expect("serialize");
        assert!(back.contains("\"routing\""));
    }

    #[test]
    fn fusion_defaults_disabled_and_deep_merges() {
        assert_eq!(strategy_for("fusion"), Some(MergeStrategy::DeepMerge));
        let absent: SettingsJson = serde_json::from_str(r#"{"model":"x"}"#).unwrap();
        assert!(absent.fusion.is_none());
        absent.validate().expect("absent fusion is valid");

        let parsed: SettingsJson = serde_json::from_str(
            r#"{"fusion":{"enabled":false,"preset":"quality","qualityPanelCount":3}}"#,
        )
        .unwrap();
        assert_eq!(parsed.fusion.as_ref().unwrap().enabled, Some(false));
        assert_eq!(parsed.fusion.as_ref().unwrap().quality_panel_count, Some(3));
        parsed.validate().expect("default-shaped fusion is valid");
        let back = serde_json::to_string(&parsed).unwrap();
        assert!(back.contains("\"fusion\""));
        assert!(back.contains("\"qualityPanelCount\""));
    }

    #[test]
    fn fusion_rejects_out_of_range_counts_and_timeouts() {
        let too_few: SettingsJson =
            serde_json::from_str(r#"{"fusion":{"qualityPanelCount":1}}"#).unwrap();
        assert!(too_few.validate().is_err());

        let max_panel: SettingsJson = serde_json::from_str(r#"{"fusion":{"maxPanel":9}}"#).unwrap();
        assert!(max_panel.validate().is_err());

        let timeout: SettingsJson = serde_json::from_str(
            r#"{"fusion":{"totalTimeoutMs":1000,"panelTotalTimeoutMs":2000}}"#,
        )
        .unwrap();
        assert!(timeout.validate().is_err());

        let zero_total: SettingsJson =
            serde_json::from_str(r#"{"fusion":{"totalTimeoutMs":0}}"#).unwrap();
        assert!(zero_total.validate().is_err());

        // Finding [15]: a stage field set ALONE, above the runtime's
        // default `totalTimeoutMs` (1_200_000), must NOT be rejected by
        // this per-FILE validator — this file has no opinion on the total;
        // it may be raised in a different tier (see
        // `fusion_stage_field_alone_above_the_default_total_does_not_trip_the_per_stage_bound_check`).
        // Only `FusionRuntimeConfig::from_settings`, on the merged view,
        // may reject this.
        let stage_alone_exceeds_default_total: SettingsJson =
            serde_json::from_str(r#"{"fusion":{"panelTotalTimeoutMs":1200001}}"#).unwrap();
        assert!(stage_alone_exceeds_default_total.validate().is_ok());
        // The SAME idea, with `totalTimeoutMs` present in this same file (so
        // this file DOES have an opinion on the total), is still rejected —
        // by the per-stage branch specifically. `panelIdleTimeoutMs` is used
        // here (not `panelTotalTimeoutMs`/`analystTimeoutMs`) because
        // those two also feed the
        // stage-SUM check just below (F004): a fixture built from any of
        // them would be rejected by either branch, so `is_err()` would stop
        // isolating the per-stage branch this test exists to pin.
        // `panelIdleTimeoutMs` is not part of that sum, so only the
        // per-stage branch can produce this Err.
        let exceeds_default_total: SettingsJson = serde_json::from_str(
            r#"{"fusion":{"totalTimeoutMs":1200000,"panelIdleTimeoutMs":1200001}}"#,
        )
        .unwrap();
        let err = exceeds_default_total
            .validate()
            .expect_err("panelIdleTimeoutMs 1200001 > totalTimeoutMs 1200000");
        assert!(
            matches!(&err, crate::settings::SettingsError::SchemaViolation(msg) if msg.contains("panelIdleTimeoutMs")),
            "expected the per-stage branch to name panelIdleTimeoutMs, got {err:?}"
        );

        let retries: SettingsJson =
            serde_json::from_str(r#"{"fusion":{"analysisProtocolRetries":2}}"#).unwrap();
        assert!(retries.validate().is_err());
    }

    #[test]
    fn fusion_rejects_stage_timeout_sum_exceeding_total_even_when_each_stage_fits_alone() {
        // Each individual stage is well under `totalTimeoutMs` on its own, but
        // panelTotal + analyst*(1+retries) sums past it — every
        // per-field "must not exceed total" check above passes, so only the
        // dedicated stage-sum check (F004) can catch this.
        let sum_exceeds_total: SettingsJson = serde_json::from_str(
            r#"{"fusion":{
                "totalTimeoutMs": 100000,
                "panelTotalTimeoutMs": 60000,
                "analystTimeoutMs": 30000,
                "analysisProtocolRetries": 1
            }}"#,
        )
        .unwrap();
        let err = sum_exceeds_total
            .validate()
            .expect_err("60000 + 30000*2 = 120000 > totalTimeoutMs 100000");
        assert!(
            matches!(&err, crate::settings::SettingsError::SchemaViolation(msg) if msg.contains("totalTimeoutMs"))
        );

        // The documented defaults (panelTotal 600_000 + analyst 120_000*2 =
        // 840_000) must fit under the default total
        // (1_200_000) with an absent `totalTimeoutMs`.
        let defaults_fit: SettingsJson =
            serde_json::from_str(r#"{"fusion":{"enabled":true}}"#).unwrap();
        defaults_fit
            .validate()
            .expect("documented stage defaults must fit under the default total");
    }

    /// The `fusion-panel` agent definition (`agent::builtins::fusion_panel_definition`)
    /// hard-codes `max_turns: 12` and only ever LOWERS it via `.min()` — the
    /// runner will never actually run a panel past 12 turns. A configured
    /// `panelMaxTurns` above 12 therefore only inflates
    /// `budget::quote`'s per-turn reservation (up to 2.7x for the schema's
    /// old 32 ceiling) without buying any more real turns — a spurious
    /// `BudgetExceeded` for money that could never be spent (F002).
    #[test]
    fn fusion_panel_max_turns_is_capped_at_the_runner_ceiling_of_12() {
        let at_ceiling: SettingsJson =
            serde_json::from_str(r#"{"fusion":{"panelMaxTurns":12}}"#).unwrap();
        assert!(
            at_ceiling.validate().is_ok(),
            "12 matches the runner's hard-coded fusion-panel max_turns and must stay valid"
        );

        let above_ceiling: SettingsJson =
            serde_json::from_str(r#"{"fusion":{"panelMaxTurns":13}}"#).unwrap();
        assert!(
            above_ceiling.validate().is_err(),
            "13 can never actually run — the runner caps every panel at 12 turns \
             regardless of this setting, so accepting it only over-reserves budget"
        );
    }
}
