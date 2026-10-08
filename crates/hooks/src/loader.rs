//! Hook loader (M6-07) — projects parsed settings JSON into a
//! `Vec<HookDefinition>` ready to feed into [`crate::HookRegistry::register`].
//!
//! Settings shape (claude-code compatible — `settings.json`):
//! ```json
//! {
//!   "hooks": {
//!     "PreToolUse": [
//!       { "matcher": "Write|Edit", "hooks": [
//!         { "type": "command", "command": "./fmt.sh", "timeout": 60 }
//!       ]}
//!     ]
//!   }
//! }
//! ```
//! Missing or empty `hooks` block yields `vec![]`. Unknown event names and
//! entries lacking their type's required field are skipped — the loader is a
//! best-effort projection, not a strict validator. An entry whose `type` is
//! unrecognized (or absent) is skipped WITH a `tracing::warn!`; the oracle's
//! config validator (`n8s`) records every rejected entry as
//! `{path:"hooks.<Event>", reason:…}` rather than dropping it silently.
//!
//! ## Event-name coverage
//!
//! [`parse_event_type`] recognizes all 33 of claude-code's `HOOK_EVENTS`
//! (`entrypoints/sdk/coreTypes.ts:25-53`); each maps to a
//! [`HookEventType`] variant.
//!
//! ## Per-hook fields
//!
//! The oracle's settings hook types plus `"function"` — `"command"`, `"http"`,
//! `"agent"`, `"prompt"`, and `"mcp_tool"` — are parsed (mapping onto
//! [`HookExecutor::Command`] / [`HookExecutor::Http`] / [`HookExecutor::Agent`] /
//! [`HookExecutor::Prompt`] / [`HookExecutor::McpTool`] respectively; the
//! executor already routes each variant to its runner in `executor.rs`). The
//! `"mcp_tool"` arm (`server` + `tool` required, optional `input` map) loads the
//! hook so it registers and matches; its INVOCATION is the documented residual
//! on [`HookExecutor::McpTool`] (no name-addressed MCP invoker seam exists in
//! this crate), and its executor arm reports the same structured "not wired"
//! error the `Command`/`Prompt` arms report without their runner. The `"prompt"`
//! type (`schemas/hooks.ts:67-95`) is the inline single-turn LLM evaluator
//! (`execPromptHook.ts`): its `prompt` (required) and optional `model` are
//! carried onto [`HookExecutor::Prompt`], and `prompt_executor.rs` runs it
//! against the injected `HookPromptRunner` (a structured no-op when no runner is
//! wired). A `prompt` entry missing its `prompt` field is skipped.
//!
//! The additive `once` (`schemas/hooks.ts:51-54`) and `statusMessage`
//! (`schemas/hooks.ts:47-50`) fields are parsed and carried onto
//! [`HookDefinition`]. The executor applies once-after-success and emits live
//! progress that the TUI renders without persisting it to the transcript.
//!
//! ### `http` / `agent` field-level mapping
//!
//! The claude-code `http` schema (`schemas/hooks.ts:97-126`) carries `headers`
//! env-var interpolation gated by an `allowedEnvVars` allowlist; both are now
//! parsed onto [`HookExecutor::Http`] (`headers` + `allowed_env_vars`) and the
//! executor interpolates `$VAR`/`${VAR}` references in header VALUES gated on the
//! allowlist (the `cHm` port — names absent from the list resolve to `""`). The
//! `agent` schema (`schemas/hooks.ts:128-163`) carries NO `agent_type`
//! (claude-code's agent hook runs an inline verifier `query()`, not a named
//! subagent); the Rust [`HookExecutor::Agent`] variant requires a non-optional
//! `agent_type`, so it is filled with the crate's canonical default
//! `"general-purpose"` (matching the agent-hook fixtures in
//! `parity_hooks_runtime.rs` and the default in `agent_executor.rs`). The `model`
//! override both schemas allow is now carried: `prompt` onto
//! [`HookExecutor::Prompt::model`] and `agent` onto [`HookExecutor::Agent::model`]
//! (threaded to the spawner). The remaining `agent`-hook residual is the
//! inline-verifier-vs-named-subagent semantic (claude-code runs an in-process
//! `query()`; LingXi spawns a named subagent), which is an architectural
//! difference, not a dropped field.

use crate::definition::{HookCondition, HookDefinition, HookExecutor, HookSource};
use crate::events::HookEventType;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::utf16_json::Utf16JsonProjectionError;
use lingxi_core::types::HookId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use thiserror::Error;

/// Managed-policy gate over which settings-tier hooks are allowed to load.
///
/// Byte-faithful port of claude-code's hook-config resolver `vBr`
/// (`claude.exe` off 195521070):
///
/// ```js
/// function vBr(){
///   let e=Hn("policySettings");
///   if(e?.disableAllHooks===!0)return{};                       // (A) no hooks at all
///   if(e?.allowManagedHooksOnly===!0||Bl())return e?.hooks??{};// (B) managed-only (safe-mode too)
///   if(iS("hooks"))return e?.hooks??{};                        // (C) strictPluginOnly → managed-only
///   let t=ts();                                                // main effective settings
///   if(t.disableAllHooks===!0)return e?.hooks??{};             // (D) settings-tier disable → managed-only
///   return t.hooks??{};                                        // (E) all merged hooks
/// }
/// ```
///
/// where `Bl()` (off 192139854) = `CLAUDE_CODE_SAFE_MODE || --safe-mode`, and
/// `iS("hooks")` (off 195520574) = the `strictPluginOnlyCustomization` policy
/// flag (`true`, or an array containing `"hooks"`).
///
/// `vBr` returns the merged hook *config map*; this crate instead loads each
/// tier's hooks separately ([`parse_hooks_from_settings_json`] per [`HookSource`]), so
/// the equivalent decision is "is THIS source's tier allowed to load at all?".
/// [`Self::allows_source`] reproduces `vBr`'s branches as a per-source predicate:
/// when only managed hooks survive (branches B/C/D) every non-[`HookSource::Settings(lingxi_core::types::SettingsScope::Managed)`]
/// tier is suppressed; when `disableAllHooks` is set in the policy tier
/// (branch A) even the managed tier is suppressed.
///
/// `policySettings.disableAllHooks` / `allowManagedHooksOnly` are read from the
/// managed/policy settings tier; `safe_mode` is the runtime `Bl()` signal;
/// `settings_disable_all_hooks` is the merged-settings (`ts()`) `disableAllHooks`
/// flag (branch D). Sourcing all three from resolved settings lives at the engine
/// composition root (cross-crate); this struct is the pure decision the loader
/// applies. The default-constructed gate (all `false`) allows every tier — the
/// faithful no-managed-policy path.
// Each bool is a distinct binary signal in `vBr` (disableAllHooks /
// allowManagedHooksOnly / Bl() safe-mode / iS("hooks") / ts().disableAllHooks);
// collapsing them into an enum would lose the 1:1 mapping to the source flags.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HookPolicyGate {
    /// `policySettings.disableAllHooks === true` — suppresses EVERY tier,
    /// including managed (branch A: `vBr` returns `{}`).
    pub policy_disable_all_hooks: bool,
    /// `policySettings.allowManagedHooksOnly === true` — only the managed tier
    /// loads (branch B).
    pub allow_managed_hooks_only: bool,
    /// `Bl()` — `CLAUDE_CODE_SAFE_MODE` env or `--safe-mode` flag. Folds into
    /// branch B (managed-only) exactly like `allowManagedHooksOnly`.
    pub safe_mode: bool,
    /// `strictPluginOnlyCustomization` (`iS("hooks")`, branch C) — also collapses
    /// to managed-only.
    pub strict_plugin_only: bool,
    /// `ts().disableAllHooks === true` (the merged main-settings flag, branch D)
    /// — collapses to managed-only (the managed tier is still honored).
    pub settings_disable_all_hooks: bool,
}

/// Policy-settings shape consumed by [`HookPolicyGate::from_policy_settings_json`].
#[derive(Debug, Default, Deserialize)]
struct PolicySettingsTop {
    #[serde(default, rename = "disableAllHooks")]
    disable_all_hooks: Option<bool>,
    #[serde(default, rename = "allowManagedHooksOnly")]
    allow_managed_hooks_only: Option<bool>,
}

impl HookPolicyGate {
    /// Build a gate from the raw policy/managed `settings.json` text plus the
    /// two runtime signals (`safe_mode` = `Bl()`, `settings_disable_all_hooks` =
    /// `ts().disableAllHooks`).
    ///
    /// Mirrors the binary's `e?.` optional-chaining fail-open: a malformed /
    /// missing policy JSON parses to all-`None` (no gating from the policy tier),
    /// exactly as `Hn("policySettings")` yielding `undefined` leaves
    /// `e?.disableAllHooks` and `e?.allowManagedHooksOnly` `undefined` (falsy).
    ///
    /// `strict_plugin_only` (the `iS("hooks")` branch) is sourced separately at
    /// the composition root (it reads a DIFFERENT policy field,
    /// `strictPluginOnlyCustomization`, which can be a bool or a string array);
    /// callers that resolve it should set it via the returned value's field.
    #[must_use]
    pub fn from_policy_settings_json(
        policy_json: Option<&str>,
        safe_mode: bool,
        settings_disable_all_hooks: bool,
    ) -> Self {
        let policy: PolicySettingsTop = policy_json
            .and_then(|raw| serde_json::from_str(raw).ok())
            .unwrap_or_default();
        Self {
            // `=== !0` ⇒ strict `Some(true)`.
            policy_disable_all_hooks: policy.disable_all_hooks == Some(true),
            allow_managed_hooks_only: policy.allow_managed_hooks_only == Some(true),
            safe_mode,
            strict_plugin_only: false,
            settings_disable_all_hooks,
        }
    }

    /// `true` when only the managed/policy tier's hooks are allowed (branches
    /// B/C/D of `vBr`): `allowManagedHooksOnly`, safe-mode, strict-plugin-only,
    /// or the merged-settings `disableAllHooks`. (Independent of branch A, which
    /// suppresses even the managed tier — see [`Self::allows_source`].)
    #[must_use]
    pub fn managed_only(&self) -> bool {
        self.allow_managed_hooks_only
            || self.safe_mode
            || self.strict_plugin_only
            || self.settings_disable_all_hooks
    }

    /// Whether hooks from `source` are allowed to load under this gate.
    ///
    /// - Branch A (`policy_disable_all_hooks`): no tier loads — returns `false`
    ///   for every source, including [`HookSource::Settings(lingxi_core::types::SettingsScope::Managed)`].
    /// - Branches B/C/D ([`Self::managed_only`]): only [`HookSource::Settings(lingxi_core::types::SettingsScope::Managed)`]
    ///   loads.
    /// - Otherwise (branch E): every tier loads.
    #[must_use]
    pub fn allows_source(&self, source: HookSource) -> bool {
        if self.policy_disable_all_hooks {
            return false;
        }
        if self.managed_only() {
            return source == HookSource::Settings(lingxi_core::types::SettingsScope::Managed);
        }
        true
    }
}

fn deserialize_optional_mcp_input<'de, D>(
    deserializer: D,
) -> Result<Option<Utf16JsonProjection>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let input = lingxi_core::types::utf16_json::raw_json_serde::deserialize_option(deserializer)?;
    match input {
        None => Ok(None),
        Some(input) if input.at_pointer("").is_some_and(|root| root.is_null()) => Ok(None),
        Some(input) if input.object_entries().is_some() => Ok(Some(input)),
        Some(_) => Err(serde::de::Error::custom("expected an object")),
    }
}

/// Top-level settings shape consumed by the hook loader.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct SettingsTop {
    #[serde(default)]
    pub(crate) hooks: HashMap<String, Vec<MatcherGroup>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MatcherGroup {
    #[serde(
        default,
        serialize_with = "crate::definition::hook_if_pattern_source::serialize_option",
        deserialize_with = "crate::definition::hook_if_pattern_source::deserialize_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub matcher: Option<Utf16JsonProjection>,
    #[serde(default)]
    pub hooks: Vec<HookEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookEntry {
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// `command` hook executable (`schemas/hooks.ts:33-34`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// `command` hook exec-form `args` (`schemas/hooks.ts`): when present,
    /// `command` is resolved as an executable and spawned directly with these
    /// arguments — no shell. Carried onto [`HookExecutor::Command`]'s `args`
    /// (the executor already supports it; the loader previously dropped it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    /// `command` hook `shell` selector (oracle 2.1.238 @ 282293705,
    /// `shell:Mr(w$u).optional()` with `w$u=["bash","powershell"]`):
    /// "Shell interpreter. 'bash' uses your $SHELL (bash/zsh/sh); 'powershell'
    /// uses pwsh. Defaults to bash (powershell on Windows without Git Bash)."
    ///
    /// Kept as a raw `String` so an out-of-enum value can be rejected the way
    /// zod's `Mr(w$u)` rejects it — the whole entry is dropped — rather than
    /// silently defaulting. Carried onto [`HookExecutor::Command`]'s `shell`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    /// `http` hook endpoint URL (`schemas/hooks.ts:99`). Always sent via POST.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// `http` hook request headers (`schemas/hooks.ts:106-111`). Values may
    /// reference env vars via `$VAR`/`${VAR}`, interpolated by the executor
    /// gated on [`Self::allowed_env_vars`] (claude-code `cHm`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
    /// `http` hook `allowedEnvVars` (`schemas/hooks.ts`): env-var names header
    /// values may interpolate. Names absent here resolve to `""`. Required for
    /// any interpolation to occur.
    #[serde(
        default,
        rename = "allowedEnvVars",
        skip_serializing_if = "Option::is_none"
    )]
    pub allowed_env_vars: Option<Vec<String>>,
    /// `mcp_tool` hook target server (oracle `McpToolHookSchema.server`,
    /// 2.1.238 @ 282295711: "Name of an already-configured MCP server to
    /// invoke"). Required by the `mcp_tool` arm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// `mcp_tool` hook target tool on [`Self::server`] (oracle
    /// `McpToolHookSchema.tool`: "Name of the tool on that server to call").
    /// Required by the `mcp_tool` arm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// `mcp_tool` arguments (oracle `McpToolHookSchema.input`,
    /// `lo(H(),Fn()).optional()`): "Arguments passed to the MCP tool. String
    /// values support `${path}` interpolation from the hook input JSON (e.g.
    /// `"${tool_input.file_path}"`)." Carried onto
    /// the `input` object of [`HookExecutor::McpTool`]; the interpolation happens at
    /// invocation time (see that variant's residual note).
    #[serde(
        default,
        serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option",
        deserialize_with = "deserialize_optional_mcp_input",
        skip_serializing_if = "Option::is_none"
    )]
    pub input: Option<Utf16JsonProjection>,
    /// `agent` / `prompt` hook prompt text (`schemas/hooks.ts:138-142` /
    /// `67-73`). For an `agent` hook it is the verifier prompt; for a `prompt`
    /// hook it is the inline-LLM evaluation prompt (with `$ARGUMENTS`).
    #[serde(
        default,
        serialize_with = "crate::definition::hook_if_pattern_source::serialize_option",
        deserialize_with = "crate::definition::hook_if_pattern_source::deserialize_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub prompt: Option<Utf16JsonProjection>,
    /// `prompt` / `agent` hook model override (`schemas/hooks.ts:81-86`).
    /// Consumed only by the `prompt` arm; the `agent` arm has no model field on
    /// its [`HookExecutor::Agent`] variant, so it is dropped there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// `prompt` hook `continueOnBlock` (`schemas/hooks.ts`): when the hook blocks
    /// (`ok:false`), sets the `decision:"block"` `continue` value. Default false
    /// (turn ends). Carried onto [`HookExecutor::Prompt`]'s `continue_on_block`;
    /// the prompt executor sets `prevent_continuation = !continue_on_block`.
    #[serde(
        default,
        rename = "continueOnBlock",
        skip_serializing_if = "Option::is_none"
    )]
    pub continue_on_block: Option<bool>,
    /// `function` hook body — plugin-supplied JavaScript, run in the
    /// deny-by-default sandbox (`crate::function_hook`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// `function` hook wall-clock budget in milliseconds (upstream `budgetMs`).
    #[serde(default, rename = "budgetMs", skip_serializing_if = "Option::is_none")]
    pub budget_ms: Option<u64>,
    /// `timeout` in seconds, shared by all hook types
    /// (`schemas/hooks.ts:42-46` / `75-79` / `101-105` / `144-148`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    /// Optional execution priority. Higher values run before lower values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
    /// claude-code `once` (`schemas/hooks.ts:51-54`): run once then remove.
    /// Parsed and carried onto [`HookDefinition::once`]; the executor performs
    /// the self-removal after successful completion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub once: Option<bool>,
    /// claude-code `async` (`schemas/hooks.ts`: `async:boolean().optional()
    /// .describe("If true, hook runs in background without blocking")`). Maps to
    /// [`HookDefinition::blocking`] = `!async` — `blocking == false` routes the
    /// hook to the background async registry.
    #[serde(default, rename = "async", skip_serializing_if = "Option::is_none")]
    pub r#async: Option<bool>,
    /// Reawaken the agent after an asynchronous completion.
    #[serde(
        default,
        rename = "asyncRewake",
        skip_serializing_if = "Option::is_none"
    )]
    pub async_rewake: Option<bool>,
    /// Async timeout in milliseconds. Missing/zero resolves to 15 seconds.
    #[serde(
        default,
        rename = "asyncTimeout",
        skip_serializing_if = "Option::is_none"
    )]
    pub async_timeout: Option<u64>,
    /// Optional meta-context used for an asynchronous rewake.
    #[serde(
        default,
        rename = "rewakeMessage",
        skip_serializing_if = "Option::is_none"
    )]
    pub rewake_message: Option<String>,
    /// claude-code `statusMessage` (`schemas/hooks.ts:47-50`): custom spinner
    /// text. Parsed and carried onto [`HookDefinition::status_message`] for the
    /// executor's live progress observer.
    #[serde(
        default,
        rename = "statusMessage",
        skip_serializing_if = "Option::is_none"
    )]
    pub status_message: Option<String>,
    /// claude-code per-hook `if` (`schemas/hooks.ts:35` `IfConditionSchema`):
    /// a permission-rule-syntax pattern (e.g. `"Bash(git push:*)"`) that gates
    /// the hook on the tool name + tool input matching. Parsed and carried onto
    /// [`HookCondition::if_pattern`], evaluated by
    /// [`crate::matcher::matches_if_condition`] in `match_event`.
    #[serde(
        default,
        rename = "if",
        serialize_with = "crate::definition::hook_if_pattern_source::serialize_option",
        deserialize_with = "crate::definition::hook_if_pattern_source::deserialize_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub if_pattern: Option<Utf16JsonProjection>,
}

/// Default agent type for an `agent` hook.
///
/// claude-code's `agent` schema (`schemas/hooks.ts:128-163`) carries NO agent
/// type — its agent hook runs an inline verifier `query()` rather than a named
/// subagent. The Rust [`HookExecutor::Agent`] variant, by contrast, spawns a
/// subagent and so requires a non-optional `agent_type`. This crate's canonical
/// default (the `agent_executor.rs` spawn target, the `parity_hooks_runtime.rs`
/// fixture, and the `agent_type` doc example in `definition.rs`) is
/// `"general-purpose"`, so that is what an `agent` settings entry resolves to.
const DEFAULT_AGENT_TYPE: &str = "general-purpose";

/// Parse the raw JSON string of a settings file into hook definitions.
///
/// Unknown event names are silently skipped. An entry whose `type` is
/// `"command"`/`"http"`/`"agent"`/`"prompt"`/`"mcp_tool"` but is missing a field
/// that type requires (`command` / `url` / `prompt` / `prompt` /
/// `server`+`tool`) is skipped; an entry whose `type` is unrecognized or absent
/// is skipped with a `tracing::warn!`. Returns `Ok(vec![])` when the input has
/// no `hooks` block at all.
///
/// `source` is propagated onto every returned [`HookDefinition`] so the
/// registry can later display trust info per origin.
pub fn parse_hooks_from_settings_json(
    raw: &str,
    source: HookSource,
) -> Result<Vec<HookDefinition>, serde_json::Error> {
    parse_into(raw, source)
}

/// Errors produced while projecting source JSON directly into hook
/// definitions.
#[derive(Debug, Error)]
pub enum HookSettingsProjectionError {
    /// The exact source JSON projection could not be stringified.
    #[error(transparent)]
    Projection(#[from] Utf16JsonProjectionError),
    /// The settings shape was invalid for the hook loader.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Parse the settings-shaped root of an exact source JSON projection.
///
/// This accepts the same shape as [`parse_hooks_from_settings_json`], but its
/// arbitrary members have already crossed into [`Utf16JsonProjection`] before
/// any ordinary serde JSON values were materialized. Stringifying this flat
/// carrier uses JavaScript JSON semantics and preserves its UTF-16 strings;
/// [`HookEntry::input`] then parses from raw JSON again into the same carrier.
pub fn parse_hooks_from_settings_projection(
    raw: &Utf16JsonProjection,
    source: HookSource,
) -> Result<Vec<HookDefinition>, HookSettingsProjectionError> {
    parse_into(&raw.to_json_string()?, source).map_err(Into::into)
}

/// Parse a source projection whose root is the hook event tree itself, as in
/// an inline plugin manifest `hooks` object. The projection is wrapped in the
/// ordinary `{ "hooks": ... }` settings envelope without converting it to a
/// recursive `serde_json::Value`.
pub fn parse_hooks_from_tree_projection(
    hooks: &Utf16JsonProjection,
    source: HookSource,
) -> Result<Vec<HookDefinition>, HookSettingsProjectionError> {
    let settings = Utf16JsonProjection::object(vec![("hooks".to_owned(), hooks.clone())])?;
    parse_hooks_from_settings_projection(&settings, source)
}

/// [`parse_hooks_from_settings_json`] gated by a [`HookPolicyGate`].
///
/// When the gate suppresses `source` ([`HookPolicyGate::allows_source`] is
/// `false` — e.g. a `disableAllHooks` / `allowManagedHooksOnly` managed policy,
/// or safe-mode), this returns `Ok(vec![])` WITHOUT parsing `raw`, mirroring the
/// binary's `vBr` returning `{}` / only the managed tier's hooks for the
/// corresponding tiers. When the gate allows the source it is byte-identical to
/// [`parse_hooks_from_settings_json`].
///
/// Composition roots that have resolved managed/policy settings + the safe-mode
/// signal should call this per source instead of the ungated variant so the
/// policy gate is honored at load time. The default-constructed gate
/// ([`HookPolicyGate::default`]) allows every source, so wiring it in is
/// behavior-neutral until a real policy is supplied.
pub fn parse_hooks_from_settings_json_gated(
    raw: &str,
    source: HookSource,
    gate: HookPolicyGate,
) -> Result<Vec<HookDefinition>, serde_json::Error> {
    if !gate.allows_source(source) {
        return Ok(Vec::new());
    }
    parse_into(raw, source)
}

/// Gated counterpart to [`parse_hooks_from_settings_projection`]. A denied
/// source is suppressed without stringifying or parsing its payload.
pub fn parse_hooks_from_settings_projection_gated(
    raw: &Utf16JsonProjection,
    source: HookSource,
    gate: HookPolicyGate,
) -> Result<Vec<HookDefinition>, HookSettingsProjectionError> {
    if !gate.allows_source(source) {
        return Ok(Vec::new());
    }
    parse_hooks_from_settings_projection(raw, source)
}

/// Gated counterpart to [`parse_hooks_from_tree_projection`].
pub fn parse_hooks_from_tree_projection_gated(
    hooks: &Utf16JsonProjection,
    source: HookSource,
    gate: HookPolicyGate,
) -> Result<Vec<HookDefinition>, HookSettingsProjectionError> {
    if !gate.allows_source(source) {
        return Ok(Vec::new());
    }
    parse_hooks_from_tree_projection(hooks, source)
}

/// Shared projection used by both the ungated and gated entry points.
fn parse_into(raw: &str, source: HookSource) -> Result<Vec<HookDefinition>, serde_json::Error> {
    let top: SettingsTop = serde_json::from_str(raw)?;
    let mut out = Vec::new();
    for (event_name, groups) in top.hooks {
        let Some(event_type) = parse_event_type(&event_name) else {
            continue;
        };
        for group in groups {
            for entry in group.hooks {
                let Some((name, executor)) = build_executor(&entry) else {
                    continue;
                };
                // The tool-name `matcher` is per-GROUP; the `if`-condition is
                // per-HOOK — combine them into one condition built per entry so a
                // hook can carry both (claude-code applies both filters).
                let condition = build_condition(group.matcher.as_ref(), entry.if_pattern.as_ref());
                out.push(HookDefinition {
                    id: HookId::new(),
                    name,
                    events: vec![event_type.clone()],
                    if_condition: condition,
                    executor,
                    source,
                    // claude routes an `async: true` settings hook to the
                    // background (non-blocking) path; `blocking == false` is that
                    // path here. Defaults to blocking when the field is absent.
                    blocking: !entry.r#async.unwrap_or(false),
                    timeout: entry.timeout.map(Duration::from_secs),
                    priority: entry.priority.unwrap_or(0),
                    once: entry.once.unwrap_or(false),
                    status_message: entry.status_message.clone(),
                    async_rewake: entry.async_rewake.unwrap_or(false),
                    async_timeout: entry
                        .async_timeout
                        .filter(|timeout| *timeout > 0)
                        .map(Duration::from_millis),
                    rewake_message: entry
                        .rewake_message
                        .clone()
                        .filter(|message| !message.trim().is_empty()),
                });
            }
        }
    }
    Ok(out)
}

/// Combine a group's tool-name `matcher` and a hook's `if`-condition into a
/// single [`HookCondition`], or `None` when neither is present.
///
/// `matcher` (per-group) populates the B3 tool-name fields; `if_pattern`
/// (per-hook) populates the `if`-condition field. A hook may carry both — both
/// gate firing in `match_event`, mirroring claude-code's two sequential filters.
pub(crate) fn build_condition(
    matcher: Option<&Utf16JsonProjection>,
    if_pattern: Option<&Utf16JsonProjection>,
) -> Option<HookCondition> {
    if matcher.is_none() && if_pattern.is_none() {
        return None;
    }
    Some(HookCondition {
        pattern: matcher
            .cloned()
            .unwrap_or_else(|| Utf16JsonProjection::from("")),
        match_tool_name: matcher.is_some(),
        match_input: if_pattern.is_some(),
        if_pattern: if_pattern.cloned(),
    })
}

/// Project a single settings [`HookEntry`] onto its `(name, HookExecutor)`.
///
/// Returns `None` (so the caller skips the entry) when the `type` is missing,
/// unrecognized, or is a known type missing its required field. The per-type
/// timeout is NOT consumed here — it is carried
/// onto [`HookDefinition::timeout`] by the caller for every type uniformly, so
/// the executor's per-hook-timeout logic applies identically across arms.
pub(crate) fn build_executor(entry: &HookEntry) -> Option<(String, HookExecutor)> {
    match entry.kind.as_deref() {
        Some("command") => {
            let command = entry.command.clone()?;
            // `shell:Mr(w$u).optional()` — an omitted field is `None` (resolved
            // at spawn time by `default_hook_shell`); a PRESENT value outside
            // `["bash","powershell"]` fails the zod enum, which rejects the
            // whole hook entry, so we return `None` and the caller skips it.
            let shell = match entry.shell.as_deref() {
                None => None,
                Some(raw) => match crate::definition::HookShell::from_wire(raw) {
                    Some(parsed) => Some(parsed),
                    None => {
                        tracing::warn!(
                            "Hook {command:?} has an unrecognized shell: {raw:?}; \
                             valid values are \"bash\" and \"powershell\". Hook ignored."
                        );
                        return None;
                    }
                },
            };
            let executor = HookExecutor::Command {
                command: command.clone(),
                args: entry.args.clone().unwrap_or_default(),
                env: HashMap::new(),
                cwd: None,
                shell,
            };
            Some((command, executor))
        }
        Some("http") => {
            let url = entry.url.clone()?;
            let executor = HookExecutor::Http {
                url: url.clone(),
                // claude-code always POSTs the hook input JSON
                // (`utils/hooks/execHttpHook.ts`; `schemas/hooks.ts:99`).
                method: "POST".to_string(),
                headers: entry.headers.clone().unwrap_or_default(),
                allowed_env_vars: entry.allowed_env_vars.clone().unwrap_or_default(),
                // A `timeout: 0` (or omitted) defers to the executor's HTTP
                // default; the parsed seconds value is the per-hook override.
                timeout: entry.timeout.map_or(Duration::ZERO, Duration::from_secs),
            };
            Some((url, executor))
        }
        Some("agent") => {
            let prompt = entry.prompt.clone()?;
            let executor = HookExecutor::Agent {
                agent_type: DEFAULT_AGENT_TYPE.to_string(),
                prompt,
                // claude-code's agent hook carries an optional `model` override
                // for the inline verifier; thread it to the spawner.
                model: entry.model.clone(),
            };
            // The hook name mirrors the command-hook convention of naming the
            // hook after its primary user-supplied field — here the agent type.
            Some((DEFAULT_AGENT_TYPE.to_string(), executor))
        }
        Some("function") => {
            // `function` hook: plugin-supplied JS evaluated in
            // `crate::function_hook::Sandbox`. An entry with no `source` is
            // skipped exactly like a `command` entry with no `command` —
            // silently dropping is the established shape for a malformed entry.
            let source = entry.source.clone()?;
            let executor = HookExecutor::Function {
                source,
                budget_ms: entry.budget_ms,
            };
            // Named after its type, like `prompt`: the body is the primary
            // field and is far too long to name a hook after.
            Some(("function".to_string(), executor))
        }
        Some("prompt") => {
            // `prompt` hook (`schemas/hooks.ts:67-95`): the inline single-turn
            // LLM evaluator (`execPromptHook.ts`). Routes to
            // [`HookExecutor::Prompt`], executed by `prompt_executor.rs` against
            // the injected `HookPromptRunner`.
            let prompt = entry.prompt.clone()?;
            let executor = HookExecutor::Prompt {
                prompt,
                model: entry.model.clone(),
                continue_on_block: entry.continue_on_block.unwrap_or(false),
            };
            // The hook name mirrors the command/http/agent convention; the
            // prompt's primary user-supplied field is the prompt itself, so
            // name the hook `"prompt"` (the type) to stay short and stable.
            Some(("prompt".to_string(), executor))
        }
        Some("mcp_tool") => {
            // `mcp_tool` hook (oracle `McpToolHookSchema`, 2.1.238 @ 282295711;
            // byte-identical in 2.1.220). `server` + `tool` are REQUIRED by the
            // schema (`H()`, not `.optional()`), so an entry missing either is
            // skipped exactly like a `command` entry with no `command`.
            let server = entry.server.clone()?;
            let tool = entry.tool.clone()?;
            let executor = HookExecutor::McpTool {
                server: server.clone(),
                tool: tool.clone(),
                input: entry
                    .input
                    .clone()
                    .unwrap_or_else(crate::definition::empty_mcp_tool_input),
            };
            // The oracle's own hook-display function names an `mcp_tool` hook
            // `${server}/${tool}` (2.1.238 @ 296901778:
            // `case"mcp_tool":return`${e.server}/${e.tool}``), so use that as the
            // hook name — the same "name it after its primary user-supplied
            // field" convention the other arms follow.
            Some((format!("{server}/{tool}"), executor))
        }
        // Any other unknown type and a missing `type` are skipped — but NOT
        // silently: the oracle's `n8s` config validator records every rejected
        // entry as `{path:"hooks.<Event>", reason:…}` and surfaces it, so a
        // dropped entry must at least leave a trace here.
        other => {
            match other {
                Some(kind) => tracing::warn!(
                    "Ignoring hook entry with unsupported type {kind:?} (supported: \
                     command, http, agent, prompt, function, mcp_tool)"
                ),
                None => tracing::warn!("Ignoring hook entry with no \"type\" field"),
            }
            None
        }
    }
}

/// Map a settings hook event-name string to its [`HookEventType`] variant.
///
/// The recognized names are claude-code's `HOOK_EVENTS`
/// (`entrypoints/sdk/coreTypes.ts:25-53`). Every one of those 33 event names
/// has a corresponding `HookEvent`/`HookEventType` variant in
/// [`crate::events`], so all 33 are recognized here. Unrecognized names
/// (including event names with no variant yet, and arbitrary typos) return
/// `None` and are silently skipped by the caller.
pub(crate) fn parse_event_type(name: &str) -> Option<HookEventType> {
    match name {
        "PreToolUse" => Some(HookEventType::PreToolUse),
        "PostToolUse" => Some(HookEventType::PostToolUse),
        "PostToolUseFailure" => Some(HookEventType::PostToolUseFailure),
        "Notification" => Some(HookEventType::Notification),
        "UserPromptSubmit" => Some(HookEventType::UserPromptSubmit),
        "SessionStart" => Some(HookEventType::SessionStart),
        "SessionEnd" => Some(HookEventType::SessionEnd),
        "Stop" => Some(HookEventType::Stop),
        "StopFailure" => Some(HookEventType::StopFailure),
        "SubagentStart" => Some(HookEventType::SubagentStart),
        "SubagentStop" => Some(HookEventType::SubagentStop),
        "PreCompact" => Some(HookEventType::PreCompact),
        "PostCompact" => Some(HookEventType::PostCompact),
        "PreModelSwitch" => Some(HookEventType::PreModelSwitch),
        "PostModelSwitch" => Some(HookEventType::PostModelSwitch),
        "PermissionRequest" => Some(HookEventType::PermissionRequest),
        "PermissionDenied" => Some(HookEventType::PermissionDenied),
        "Setup" => Some(HookEventType::Setup),
        "TeammateIdle" => Some(HookEventType::TeammateIdle),
        "TaskCreated" => Some(HookEventType::TaskCreated),
        "TaskCompleted" => Some(HookEventType::TaskCompleted),
        "Elicitation" => Some(HookEventType::Elicitation),
        "ElicitationResult" => Some(HookEventType::ElicitationResult),
        "ConfigChange" => Some(HookEventType::ConfigChange),
        "WorktreeCreate" => Some(HookEventType::WorktreeCreate),
        "WorktreeRemove" => Some(HookEventType::WorktreeRemove),
        "InstructionsLoaded" => Some(HookEventType::InstructionsLoaded),
        "CwdChanged" => Some(HookEventType::CwdChanged),
        "FileChanged" => Some(HookEventType::FileChanged),
        // #39: three more user-configurable events (claude-code registry `nym`,
        // BIN off 205713189). Without these arms a settings.json hook keyed on
        // them is silently dropped at load.
        "PostToolBatch" => Some(HookEventType::PostToolBatch),
        "UserPromptExpansion" => Some(HookEventType::UserPromptExpansion),
        "MessageDisplay" => Some(HookEventType::MessageDisplay),
        "DirectoryAdded" => Some(HookEventType::DirectoryAdded),
        _ => None,
    }
}

pub(crate) fn event_type_name(event_type: &HookEventType) -> &'static str {
    match event_type {
        HookEventType::AgentSpawn => "AgentSpawn",
        HookEventType::PreToolUse => "PreToolUse",
        HookEventType::PostToolUse => "PostToolUse",
        HookEventType::PostToolUseFailure => "PostToolUseFailure",
        HookEventType::Notification => "Notification",
        HookEventType::UserPromptSubmit => "UserPromptSubmit",
        HookEventType::SessionStart => "SessionStart",
        HookEventType::SessionEnd => "SessionEnd",
        HookEventType::Stop => "Stop",
        HookEventType::StopFailure => "StopFailure",
        HookEventType::SubagentStart => "SubagentStart",
        HookEventType::SubagentStop => "SubagentStop",
        HookEventType::PreCompact => "PreCompact",
        HookEventType::PostCompact => "PostCompact",
        HookEventType::PreModelSwitch => "PreModelSwitch",
        HookEventType::PostModelSwitch => "PostModelSwitch",
        HookEventType::PermissionRequest => "PermissionRequest",
        HookEventType::PermissionDenied => "PermissionDenied",
        HookEventType::Setup => "Setup",
        HookEventType::TeammateIdle => "TeammateIdle",
        HookEventType::TaskCreated => "TaskCreated",
        HookEventType::TaskCompleted => "TaskCompleted",
        HookEventType::Elicitation => "Elicitation",
        HookEventType::ElicitationResult => "ElicitationResult",
        HookEventType::ConfigChange => "ConfigChange",
        HookEventType::WorktreeCreate => "WorktreeCreate",
        HookEventType::WorktreeRemove => "WorktreeRemove",
        HookEventType::InstructionsLoaded => "InstructionsLoaded",
        HookEventType::CwdChanged => "CwdChanged",
        HookEventType::FileChanged => "FileChanged",
        HookEventType::PostToolBatch => "PostToolBatch",
        HookEventType::UserPromptExpansion => "UserPromptExpansion",
        HookEventType::MessageDisplay => "MessageDisplay",
        HookEventType::DirectoryAdded => "DirectoryAdded",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_settings_yields_no_hooks() {
        let hooks = parse_hooks_from_settings_json(
            "{}",
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert!(hooks.is_empty());
    }

    #[test]
    fn one_pretooluse_command_hook() {
        let raw = r#"{
          "hooks": {
            "PreToolUse": [
              { "matcher": "Write|Edit", "hooks": [
                { "type": "command", "command": "./fmt.sh", "timeout": 30 }
              ]}
            ]
          }
        }"#;
        let hooks = parse_hooks_from_settings_json(
            raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].name, "./fmt.sh");
        assert_eq!(hooks[0].events, vec![HookEventType::PreToolUse]);
        assert_eq!(hooks[0].timeout, Some(Duration::from_secs(30)));
        assert_eq!(
            hooks[0].source,
            HookSource::Settings(lingxi_core::types::SettingsScope::Project)
        );
        let cond = hooks[0].if_condition.as_ref().expect("matcher present");
        assert_eq!(cond.pattern.string_at(""), Some("Write|Edit"));
        // No `if` → if_pattern stays None, match_input false.
        assert_eq!(hooks[0].if_pattern(), None);
        assert!(!cond.match_input);
    }

    #[test]
    fn per_hook_if_condition_is_parsed_and_combines_with_matcher() {
        let raw = r#"{
          "hooks": {
            "PreToolUse": [
              { "matcher": "Bash", "hooks": [
                { "type": "command", "command": "./guard.sh", "if": "Bash(git push:*)" }
              ]}
            ]
          }
        }"#;
        let hooks = parse_hooks_from_settings_json(
            raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        assert_eq!(hooks.len(), 1);
        // Tool-name matcher AND the `if`-condition are both carried on one condition.
        assert_eq!(
            hooks[0].matcher().and_then(|matcher| matcher.string_at("")),
            Some("Bash")
        );
        assert_eq!(
            hooks[0]
                .if_pattern()
                .and_then(|pattern| pattern.string_at("")),
            Some("Bash(git push:*)")
        );
        let cond = hooks[0].if_condition.as_ref().expect("condition present");
        assert!(cond.match_tool_name);
        assert!(cond.match_input);
    }

    #[test]
    fn if_condition_without_matcher_builds_condition() {
        let raw = r#"{
          "hooks": {
            "PreToolUse": [
              { "hooks": [
                { "type": "command", "command": "./guard.sh", "if": "Bash(rm:*)" }
              ]}
            ]
          }
        }"#;
        let hooks = parse_hooks_from_settings_json(
            raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert_eq!(hooks.len(), 1);
        // No group matcher → no tool-name matcher, but the `if` is carried.
        assert_eq!(hooks[0].matcher(), None);
        assert_eq!(
            hooks[0]
                .if_pattern()
                .and_then(|pattern| pattern.string_at("")),
            Some("Bash(rm:*)")
        );
    }

    #[test]
    fn if_condition_projection_keeps_unpaired_source_units() {
        let raw = r#"{
          "hooks": {
            "PreToolUse": [{
              "hooks": [{
                "type": "command",
                "command": "./guard.sh",
                "if": "Edit(*\ud800.rs)"
              }]
            }]
          }
        }"#;
        let hooks = parse_hooks_from_settings_json(
            raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        let actual = hooks[0].if_pattern().expect("if pattern is present");
        let expected =
            Utf16JsonProjection::parse(r#""Edit(*\ud800.rs)""#).expect("expected source string");
        assert_eq!(actual, &expected);

        let condition = hooks[0]
            .if_condition
            .as_ref()
            .expect("condition is present");
        let serialized = serde_json::to_string(condition).unwrap();
        let round_trip: HookCondition = serde_json::from_str(&serialized).unwrap();
        assert_eq!(round_trip.if_pattern, Some(expected));
    }

    #[test]
    fn group_matcher_projection_keeps_unpaired_regexp_units() {
        let hooks = parse_hooks_from_settings_json(
            r#"{"hooks":{"UserPromptExpansion":[{"matcher":"^\ud800$","hooks":[{"type":"command","command":"./guard.sh"}]}]}}"#,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        let actual = hooks[0].matcher().expect("matcher is present");
        let expected = Utf16JsonProjection::parse(r#""^\ud800$""#).unwrap();
        assert_eq!(actual, &expected);

        let serialized = serde_json::to_string(&hooks[0].if_condition).unwrap();
        let round_trip: Option<HookCondition> = serde_json::from_str(&serialized).unwrap();
        assert_eq!(round_trip.unwrap().pattern, expected);
    }

    #[test]
    fn unknown_event_is_skipped() {
        let raw =
            r#"{ "hooks": { "Bogus": [{ "hooks": [{ "type": "command", "command": "x" }]}]}}"#;
        let hooks = parse_hooks_from_settings_json(
            raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert!(hooks.is_empty());
    }

    #[test]
    fn missing_command_field_is_skipped() {
        let raw = r#"{ "hooks": { "Stop": [{ "hooks": [{ "type": "command" }]}]}}"#;
        let hooks = parse_hooks_from_settings_json(
            raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert!(hooks.is_empty());
    }

    /// A `command` entry under `event_name` (no matcher) so the parsed
    /// definition reflects exactly one event.
    fn one_command(event_name: &str) -> String {
        format!(
            r#"{{ "hooks": {{ "{event_name}": [{{ "hooks": [
                {{ "type": "command", "command": "x" }}
            ]}}]}}}}"#
        )
    }

    #[test]
    fn all_33_event_names_parse_to_their_variant() {
        // Mirrors claude-code HOOK_EVENTS (entrypoints/sdk/coreTypes.ts:25-53).
        // Each (settings name -> expected HookEventType) must round-trip.
        let cases: &[(&str, HookEventType)] = &[
            ("PreToolUse", HookEventType::PreToolUse),
            ("PostToolUse", HookEventType::PostToolUse),
            ("PostToolUseFailure", HookEventType::PostToolUseFailure),
            ("Notification", HookEventType::Notification),
            ("UserPromptSubmit", HookEventType::UserPromptSubmit),
            ("SessionStart", HookEventType::SessionStart),
            ("SessionEnd", HookEventType::SessionEnd),
            ("Stop", HookEventType::Stop),
            ("StopFailure", HookEventType::StopFailure),
            ("SubagentStart", HookEventType::SubagentStart),
            ("SubagentStop", HookEventType::SubagentStop),
            ("PreCompact", HookEventType::PreCompact),
            ("PostCompact", HookEventType::PostCompact),
            ("PreModelSwitch", HookEventType::PreModelSwitch),
            ("PostModelSwitch", HookEventType::PostModelSwitch),
            ("PermissionRequest", HookEventType::PermissionRequest),
            ("PermissionDenied", HookEventType::PermissionDenied),
            ("Setup", HookEventType::Setup),
            ("TeammateIdle", HookEventType::TeammateIdle),
            ("TaskCreated", HookEventType::TaskCreated),
            ("TaskCompleted", HookEventType::TaskCompleted),
            ("Elicitation", HookEventType::Elicitation),
            ("ElicitationResult", HookEventType::ElicitationResult),
            ("ConfigChange", HookEventType::ConfigChange),
            ("WorktreeCreate", HookEventType::WorktreeCreate),
            ("WorktreeRemove", HookEventType::WorktreeRemove),
            ("InstructionsLoaded", HookEventType::InstructionsLoaded),
            ("CwdChanged", HookEventType::CwdChanged),
            ("FileChanged", HookEventType::FileChanged),
            ("PostToolBatch", HookEventType::PostToolBatch),
            ("UserPromptExpansion", HookEventType::UserPromptExpansion),
            ("MessageDisplay", HookEventType::MessageDisplay),
            ("DirectoryAdded", HookEventType::DirectoryAdded),
        ];
        assert_eq!(
            cases.len(),
            33,
            "claude-code 2.1.251 HOOK_EVENTS has 33 names"
        );
        for (name, expected) in cases {
            let raw = one_command(name);
            let hooks = parse_hooks_from_settings_json(
                &raw,
                HookSource::Settings(lingxi_core::types::SettingsScope::User),
            )
            .unwrap();
            assert_eq!(hooks.len(), 1, "event {name} should produce one hook");
            assert_eq!(
                hooks[0].events,
                vec![expected.clone()],
                "event name {name} must map to {expected:?}",
            );
        }
    }

    #[test]
    fn newly_recognized_event_parses_with_no_matcher() {
        // FileChanged was NOT recognized before this change; assert it now is
        // and carries no if-condition when no matcher is supplied.
        let raw = one_command("FileChanged");
        let hooks = parse_hooks_from_settings_json(
            &raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].events, vec![HookEventType::FileChanged]);
        assert!(hooks[0].if_condition.is_none());
    }

    #[test]
    fn once_true_parses_onto_definition() {
        let raw = r#"{
          "hooks": {
            "Stop": [{ "hooks": [
              { "type": "command", "command": "cleanup.sh", "once": true }
            ]}]
          }
        }"#;
        let hooks = parse_hooks_from_settings_json(
            raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert_eq!(hooks.len(), 1);
        assert!(
            hooks[0].once,
            "once:true must be carried onto the definition"
        );
        assert_eq!(hooks[0].status_message, None);
    }

    #[test]
    fn once_defaults_to_false_when_absent() {
        let raw = one_command("Stop");
        let hooks = parse_hooks_from_settings_json(
            &raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert_eq!(hooks.len(), 1);
        assert!(!hooks[0].once, "absent once must default to false");
    }

    #[test]
    fn status_message_parses_onto_definition() {
        let raw = r#"{
          "hooks": {
            "PostToolUse": [{ "hooks": [
              { "type": "command", "command": "fmt.sh", "statusMessage": "Formatting…" }
            ]}]
          }
        }"#;
        let hooks = parse_hooks_from_settings_json(
            raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].status_message.as_deref(), Some("Formatting…"));
        assert!(!hooks[0].once, "absent once must default to false");
    }

    #[test]
    fn once_and_status_message_parse_together() {
        let raw = r#"{
          "hooks": {
            "PreToolUse": [{ "matcher": "Write|Edit", "hooks": [
              { "type": "command", "command": "guard.sh",
                "once": true, "statusMessage": "Guarding" }
            ]}]
          }
        }"#;
        let hooks = parse_hooks_from_settings_json(
            raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        assert_eq!(hooks.len(), 1);
        assert!(hooks[0].once);
        assert_eq!(hooks[0].status_message.as_deref(), Some("Guarding"));
        assert_eq!(
            hooks[0]
                .if_condition
                .as_ref()
                .and_then(|c| c.pattern.string_at("")),
            Some("Write|Edit"),
        );
    }

    #[test]
    fn async_runtime_options_parse_with_millisecond_timeout() {
        let raw = r#"{
          "hooks": {
            "Stop": [{ "hooks": [
              { "type": "command", "command": "notify.sh", "async": true,
                "asyncRewake": true, "asyncTimeout": 2500,
                "rewakeMessage": "Review the completed hook." }
            ]}]
          }
        }"#;
        let hooks = parse_hooks_from_settings_json(
            raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert_eq!(hooks.len(), 1);
        assert!(!hooks[0].blocking);
        assert!(hooks[0].async_rewake);
        assert_eq!(hooks[0].async_timeout, Some(Duration::from_millis(2500)));
        assert_eq!(
            hooks[0].rewake_message.as_deref(),
            Some("Review the completed hook.")
        );
    }

    // ---- http hook parsing (schemas/hooks.ts:97-126) -----------------------

    #[test]
    fn http_hook_parses_to_http_executor_with_url_headers_timeout() {
        let raw = r#"{
          "hooks": {
            "PreToolUse": [{ "matcher": "Write|Edit", "hooks": [
              { "type": "http",
                "url": "https://hooks.example.com/pre",
                "headers": { "Authorization": "Bearer t", "X-Env": "prod" },
                "timeout": 12 }
            ]}]
          }
        }"#;
        let hooks = parse_hooks_from_settings_json(
            raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].events, vec![HookEventType::PreToolUse]);
        assert_eq!(
            hooks[0].source,
            HookSource::Settings(lingxi_core::types::SettingsScope::Project)
        );
        // The per-hook timeout is carried onto the definition (seconds).
        assert_eq!(hooks[0].timeout, Some(Duration::from_secs(12)));
        let HookExecutor::Http {
            url,
            method,
            headers,
            allowed_env_vars: _,
            timeout,
        } = &hooks[0].executor
        else {
            panic!("expected Http executor, got {:?}", hooks[0].executor);
        };
        assert_eq!(url, "https://hooks.example.com/pre");
        // claude-code always POSTs the hook input JSON.
        assert_eq!(method, "POST");
        assert_eq!(
            headers.get("Authorization").map(String::as_str),
            Some("Bearer t")
        );
        assert_eq!(headers.get("X-Env").map(String::as_str), Some("prod"));
        // The per-hook timeout is mirrored onto the executor's `timeout`.
        assert_eq!(*timeout, Duration::from_secs(12));
        // The hook name reflects the URL (primary user-supplied field).
        assert_eq!(hooks[0].name, "https://hooks.example.com/pre");
        // No matcher-independent matcher pattern is lost.
        assert_eq!(
            hooks[0]
                .if_condition
                .as_ref()
                .and_then(|c| c.pattern.string_at("")),
            Some("Write|Edit"),
        );
    }

    #[test]
    fn http_hook_parses_allowed_env_vars() {
        let raw = r#"{
          "hooks": {
            "PreToolUse": [{ "hooks": [
              { "type": "http",
                "url": "https://h.test/post",
                "headers": { "Authorization": "Bearer $MY_TOKEN" },
                "allowedEnvVars": ["MY_TOKEN", "OTHER"] }
            ]}]
          }
        }"#;
        let hooks = parse_hooks_from_settings_json(
            raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        let HookExecutor::Http {
            allowed_env_vars, ..
        } = &hooks[0].executor
        else {
            panic!("expected Http executor");
        };
        assert_eq!(
            allowed_env_vars,
            &vec!["MY_TOKEN".to_string(), "OTHER".to_string()]
        );
    }

    #[test]
    fn agent_hook_parses_model_override() {
        let raw = r#"{
          "hooks": {
            "PreToolUse": [{ "hooks": [
              { "type": "agent", "prompt": "vet", "model": "claude-haiku-4-5" }
            ]}]
          }
        }"#;
        let hooks = parse_hooks_from_settings_json(
            raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        let HookExecutor::Agent { model, .. } = &hooks[0].executor else {
            panic!("expected Agent executor");
        };
        assert_eq!(model.as_deref(), Some("claude-haiku-4-5"));
    }

    #[test]
    fn http_hook_without_headers_defaults_to_empty_and_zero_timeout() {
        // No `headers` and no `timeout`: headers default empty, executor timeout
        // is ZERO (the executor then falls back to its HTTP default), and the
        // definition-level timeout is None.
        let raw = r#"{
          "hooks": {
            "PostToolUse": [{ "hooks": [
              { "type": "http", "url": "https://h.test/post" }
            ]}]
          }
        }"#;
        let hooks = parse_hooks_from_settings_json(
            raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].timeout, None);
        let HookExecutor::Http {
            url,
            headers,
            timeout,
            ..
        } = &hooks[0].executor
        else {
            panic!("expected Http executor");
        };
        assert_eq!(url, "https://h.test/post");
        assert!(headers.is_empty());
        assert_eq!(*timeout, Duration::ZERO);
    }

    #[test]
    fn http_hook_missing_url_is_skipped() {
        let raw = r#"{ "hooks": { "PreToolUse": [{ "hooks": [
            { "type": "http", "timeout": 5 }
        ]}]}}"#;
        let hooks = parse_hooks_from_settings_json(
            raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert!(hooks.is_empty(), "an http entry without a url is skipped");
    }

    // ---- agent hook parsing (schemas/hooks.ts:128-163) ---------------------

    #[test]
    fn agent_hook_parses_to_agent_executor_with_prompt_and_default_type() {
        let raw = r#"{
          "hooks": {
            "Stop": [{ "hooks": [
              { "type": "agent",
                "prompt": "Verify that unit tests ran and passed.",
                "timeout": 90 }
            ]}]
          }
        }"#;
        let hooks = parse_hooks_from_settings_json(
            raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::Local),
        )
        .unwrap();
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].events, vec![HookEventType::Stop]);
        assert_eq!(
            hooks[0].source,
            HookSource::Settings(lingxi_core::types::SettingsScope::Local)
        );
        assert_eq!(hooks[0].timeout, Some(Duration::from_secs(90)));
        let HookExecutor::Agent {
            agent_type,
            prompt,
            model: _,
        } = &hooks[0].executor
        else {
            panic!("expected Agent executor, got {:?}", hooks[0].executor);
        };
        assert_eq!(
            prompt.string_at(""),
            Some("Verify that unit tests ran and passed.")
        );
        // The schema carries no agent type; the crate default is used.
        assert_eq!(agent_type, "general-purpose");
    }

    #[test]
    fn agent_hook_missing_prompt_is_skipped() {
        let raw = r#"{ "hooks": { "Stop": [{ "hooks": [
            { "type": "agent", "timeout": 30 }
        ]}]}}"#;
        let hooks = parse_hooks_from_settings_json(
            raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert!(
            hooks.is_empty(),
            "an agent entry without a prompt is skipped"
        );
    }

    // ---- prompt hook parsing (schemas/hooks.ts:67-95) + mixed batches ------

    #[test]
    fn prompt_hook_parses_to_prompt_executor_with_prompt_and_model() {
        // `prompt` is the inline single-turn LLM evaluator (execPromptHook.ts).
        let raw = r#"{
          "hooks": {
            "PreToolUse": [{ "matcher": "Bash", "hooks": [
              { "type": "prompt",
                "prompt": "Is $ARGUMENTS a safe command?",
                "model": "claude-sonnet-4-6",
                "timeout": 15 }
            ]}]
          }
        }"#;
        let hooks = parse_hooks_from_settings_json(
            raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].events, vec![HookEventType::PreToolUse]);
        assert_eq!(
            hooks[0].source,
            HookSource::Settings(lingxi_core::types::SettingsScope::Project)
        );
        assert_eq!(hooks[0].timeout, Some(Duration::from_secs(15)));
        let HookExecutor::Prompt { prompt, model, .. } = &hooks[0].executor else {
            panic!("expected Prompt executor, got {:?}", hooks[0].executor);
        };
        assert_eq!(prompt.string_at(""), Some("Is $ARGUMENTS a safe command?"));
        assert_eq!(model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(hooks[0].name, "prompt");
        assert_eq!(
            hooks[0]
                .if_condition
                .as_ref()
                .and_then(|c| c.pattern.string_at("")),
            Some("Bash"),
        );
    }

    #[test]
    fn prompt_hook_without_model_defaults_to_none() {
        let raw = r#"{ "hooks": { "PostToolUse": [{ "hooks": [
            { "type": "prompt", "prompt": "evaluate this" }
        ]}]}}"#;
        let hooks = parse_hooks_from_settings_json(
            raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert_eq!(hooks.len(), 1);
        let HookExecutor::Prompt { prompt, model, .. } = &hooks[0].executor else {
            panic!("expected Prompt executor");
        };
        assert_eq!(prompt.string_at(""), Some("evaluate this"));
        assert_eq!(*model, None);
    }

    #[test]
    fn prompt_hook_retains_exact_utf16_source() {
        let raw = r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"prompt","prompt":"check \ud800 $ARGUMENTS"}]}]}}"#;
        let hooks = parse_hooks_from_settings_json(
            raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        let HookExecutor::Prompt { prompt, .. } = &hooks[0].executor else {
            panic!("expected Prompt executor");
        };
        let expected = Utf16JsonProjection::parse(r#""check \ud800 $ARGUMENTS""#).unwrap();
        assert_eq!(prompt.string_units(""), expected.string_units(""));
        let serialized = serde_json::to_string(&hooks[0]).unwrap();
        assert!(serialized.contains(r#""prompt":"check \ud800 $ARGUMENTS""#));
    }

    #[test]
    fn prompt_hook_missing_prompt_is_skipped() {
        let raw = r#"{ "hooks": { "PreToolUse": [{ "hooks": [
            { "type": "prompt", "model": "claude-sonnet-4-6" }
        ]}]}}"#;
        let hooks = parse_hooks_from_settings_json(
            raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert!(
            hooks.is_empty(),
            "a prompt entry without a prompt is skipped"
        );
    }

    /// SH-04: an `mcp_tool` settings entry used to fall through `build_executor`
    /// and be dropped SILENTLY. The oracle's five hook schemas are a
    /// `z0("type", …)` discriminated union, so it accepts the entry like any
    /// other; the port must now load it, name it `${server}/${tool}` (oracle
    /// `y2e`, 2.1.238 @ 296901596), and carry `input` / `timeout` / `once` /
    /// `statusMessage` / the group matcher.
    #[test]
    fn mcp_tool_hook_parses_to_mcp_tool_executor() {
        let raw = r#"{
          "hooks": {
            "PostToolUse": [{ "matcher": "Write|Edit", "hooks": [
              { "type": "mcp_tool",
                "server": "linter",
                "tool": "format_file",
                "input": { "path": "${tool_input.file_path}", "fix": true },
                "timeout": 12,
                "statusMessage": "Formatting…",
                "once": true }
            ]}]
          }
        }"#;
        let hooks = parse_hooks_from_settings_json(
            raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        assert_eq!(
            hooks.len(),
            1,
            "an mcp_tool entry must no longer be dropped"
        );
        assert_eq!(hooks[0].events, vec![HookEventType::PostToolUse]);
        assert_eq!(hooks[0].timeout, Some(Duration::from_secs(12)));
        assert!(hooks[0].once);
        assert_eq!(hooks[0].status_message.as_deref(), Some("Formatting…"));
        assert_eq!(
            hooks[0]
                .if_condition
                .as_ref()
                .and_then(|c| c.pattern.string_at("")),
            Some("Write|Edit"),
        );
        // Oracle hook display/name for `mcp_tool` is `${server}/${tool}`.
        assert_eq!(hooks[0].name, "linter/format_file");
        let HookExecutor::McpTool {
            server,
            tool,
            input,
        } = &hooks[0].executor
        else {
            panic!("expected McpTool executor, got {:?}", hooks[0].executor);
        };
        assert_eq!(server, "linter");
        assert_eq!(tool, "format_file");
        // `input` is carried verbatim; the `${path}` interpolation is applied at
        // invocation time, so the raw template must survive the load.
        assert_eq!(
            input.get_property("path").and_then(|value| value.as_str()),
            Some("${tool_input.file_path}"),
        );
        assert_eq!(
            input.get_property("fix").and_then(|value| value.as_bool()),
            Some(true),
        );
    }

    #[test]
    fn mcp_tool_input_retains_exact_lone_utf16_units_from_raw_settings() {
        let raw = r#"{ "hooks": { "PreToolUse": [{ "hooks": [
            { "type": "mcp_tool", "server": "audit", "tool": "check",
              "input": { "path": "\ud800" } }
        ]}]}}"#;
        let hooks = parse_hooks_from_settings_json(
            raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        let HookExecutor::McpTool { input, .. } = &hooks[0].executor else {
            panic!("expected McpTool executor");
        };
        assert_eq!(input.string_units("/path").unwrap(), vec![0xd800]);
        assert_eq!(input.to_json_string().unwrap(), r#"{"path":"\ud800"}"#);
    }

    /// An omitted optional input loads as an empty object.
    #[test]
    fn mcp_tool_hook_without_input_defaults_to_empty_object() {
        let raw = r#"{ "hooks": { "PreToolUse": [{ "hooks": [
            { "type": "mcp_tool", "server": "audit", "tool": "check" }
        ]}]}}"#;
        let hooks = parse_hooks_from_settings_json(
            raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert_eq!(hooks.len(), 1);
        let HookExecutor::McpTool { input, .. } = &hooks[0].executor else {
            panic!("expected McpTool executor");
        };
        assert!(input
            .object_entries()
            .is_some_and(|entries| entries.is_empty()));
    }

    #[test]
    fn mcp_tool_null_input_keeps_the_optional_empty_object_behavior() {
        let raw = r#"{ "hooks": { "PreToolUse": [{ "hooks": [
            { "type": "mcp_tool", "server": "audit", "tool": "check", "input": null }
        ]}]}}"#;
        let hooks = parse_hooks_from_settings_json(
            raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        let HookExecutor::McpTool { input, .. } = &hooks[0].executor else {
            panic!("expected McpTool executor");
        };
        assert!(input
            .object_entries()
            .is_some_and(|entries| entries.is_empty()));
    }

    /// `server` and `tool` are REQUIRED (`H()`, not `.optional()`), so an entry
    /// missing either is skipped — the same rule the `command`/`url`/`prompt`
    /// arms apply to their own required field.
    #[test]
    fn mcp_tool_hook_missing_server_or_tool_is_skipped() {
        let no_tool = r#"{ "hooks": { "PreToolUse": [{ "hooks": [
            { "type": "mcp_tool", "server": "audit" }
        ]}]}}"#;
        assert!(parse_hooks_from_settings_json(
            no_tool,
            HookSource::Settings(lingxi_core::types::SettingsScope::User)
        )
        .unwrap()
        .is_empty());
        let no_server = r#"{ "hooks": { "PreToolUse": [{ "hooks": [
            { "type": "mcp_tool", "tool": "check" }
        ]}]}}"#;
        assert!(parse_hooks_from_settings_json(
            no_server,
            HookSource::Settings(lingxi_core::types::SettingsScope::User)
        )
        .unwrap()
        .is_empty());
    }

    #[test]
    fn mixed_command_http_agent_prompt_in_one_group_all_parse() {
        // One matcher group carrying a command, an http, an agent, and a prompt
        // hook. All four now parse to their executors.
        let raw = r#"{
          "hooks": {
            "PreToolUse": [{ "matcher": "Bash", "hooks": [
              { "type": "command", "command": "./guard.sh" },
              { "type": "http", "url": "https://h.test/hook" },
              { "type": "agent", "prompt": "vet it" },
              { "type": "prompt", "prompt": "is $ARGUMENTS safe?" }
            ]}]
          }
        }"#;
        let hooks = parse_hooks_from_settings_json(
            raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        assert_eq!(hooks.len(), 4, "command + http + agent + prompt all parse");
        // Every parsed hook keeps the group's matcher.
        for h in &hooks {
            assert_eq!(
                h.if_condition
                    .as_ref()
                    .and_then(|c| c.pattern.string_at("")),
                Some("Bash"),
            );
        }
        assert!(matches!(hooks[0].executor, HookExecutor::Command { .. }));
        assert!(matches!(hooks[1].executor, HookExecutor::Http { .. }));
        assert!(matches!(hooks[2].executor, HookExecutor::Agent { .. }));
        assert!(matches!(hooks[3].executor, HookExecutor::Prompt { .. }));
    }

    // ---- SH-06: the `shell` selector ---------------------------------------

    /// `shell:"bash"` / `shell:"powershell"` reach `HookExecutor::Command`.
    /// Before SH-06 the loader had no `shell` field at all, so BOTH of these
    /// parsed to an executor that could only ever run `/bin/sh -c`.
    #[test]
    fn shell_selector_reaches_the_command_executor() {
        let raw = r#"{ "hooks": { "PreToolUse": [{ "hooks": [
            { "type": "command", "command": "a.sh", "shell": "bash" },
            { "type": "command", "command": "b.ps1", "shell": "powershell" },
            { "type": "command", "command": "c.sh" }
        ]}]}}"#;
        let hooks = parse_hooks_from_settings_json(
            raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert_eq!(hooks.len(), 3);
        let shells: Vec<Option<crate::definition::HookShell>> = hooks
            .iter()
            .map(|h| match &h.executor {
                HookExecutor::Command { shell, .. } => *shell,
                other => panic!("expected Command, got {other:?}"),
            })
            .collect();
        assert_eq!(
            shells,
            vec![
                Some(crate::definition::HookShell::Bash),
                Some(crate::definition::HookShell::Powershell),
                // Omitted stays `None` so the spawn path can apply `Otr()`.
                None,
            ],
        );
    }

    /// `shell:Mr(w$u)` is a zod ENUM: a value outside `["bash","powershell"]`
    /// fails the schema, which drops the whole entry. Defaulting instead would
    /// silently run a typo'd `"shell": "pwsh"` hook through `/bin/sh`.
    #[test]
    fn an_out_of_enum_shell_value_drops_the_hook() {
        let raw = r#"{ "hooks": { "PreToolUse": [{ "hooks": [
            { "type": "command", "command": "a.sh", "shell": "pwsh" },
            { "type": "command", "command": "b.sh" }
        ]}]}}"#;
        let hooks = parse_hooks_from_settings_json(
            raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert_eq!(hooks.len(), 1, "only the valid entry survives");
        assert_eq!(hooks[0].name, "b.sh");
    }

    // ---- #41: managed-policy hook gate (vBr / h$) --------------------------

    /// Minimal `{ "hooks": { "Stop": [{ "hooks": [command] }] } }` so a gated
    /// load that allows the source produces exactly one hook.
    fn one_stop_command() -> String {
        one_command("Stop")
    }

    #[test]
    fn policy_gate_default_allows_every_source() {
        // The default-constructed gate (no managed policy, not safe-mode) is the
        // faithful no-policy path: every tier loads.
        let gate = HookPolicyGate::default();
        for src in [
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
            HookSource::Settings(lingxi_core::types::SettingsScope::Project),
            HookSource::Settings(lingxi_core::types::SettingsScope::Local),
            HookSource::Settings(lingxi_core::types::SettingsScope::Managed),
            HookSource::Plugin,
            HookSource::FrontMatter,
        ] {
            assert!(gate.allows_source(src), "default gate must allow {src:?}");
        }
        assert!(!gate.managed_only());
    }

    #[test]
    fn policy_disable_all_hooks_blocks_every_tier_including_managed() {
        // vBr branch A: `policySettings.disableAllHooks===true` → `{}` (no hooks
        // at all). Even the managed tier is suppressed.
        let policy = r#"{ "disableAllHooks": true }"#;
        let gate = HookPolicyGate::from_policy_settings_json(Some(policy), false, false);
        assert!(gate.policy_disable_all_hooks);
        for src in [
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
            HookSource::Settings(lingxi_core::types::SettingsScope::Project),
            HookSource::Settings(lingxi_core::types::SettingsScope::Managed),
        ] {
            assert!(
                !gate.allows_source(src),
                "disableAllHooks must block {src:?}"
            );
        }
    }

    #[test]
    fn allow_managed_hooks_only_keeps_only_managed_tier() {
        // vBr branch B: `policySettings.allowManagedHooksOnly===true` → only the
        // managed tier's hooks survive.
        let policy = r#"{ "allowManagedHooksOnly": true }"#;
        let gate = HookPolicyGate::from_policy_settings_json(Some(policy), false, false);
        assert!(gate.managed_only());
        assert!(gate.allows_source(HookSource::Settings(
            lingxi_core::types::SettingsScope::Managed
        )));
        for src in [
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
            HookSource::Settings(lingxi_core::types::SettingsScope::Project),
            HookSource::Plugin,
        ] {
            assert!(!gate.allows_source(src), "managed-only must block {src:?}");
        }
    }

    #[test]
    fn safe_mode_collapses_to_managed_only() {
        // vBr branch B fold: `Bl()` (CLAUDE_CODE_SAFE_MODE / --safe-mode) is OR'd
        // with allowManagedHooksOnly → only the managed tier loads.
        let gate = HookPolicyGate::from_policy_settings_json(None, true, false);
        assert!(gate.safe_mode);
        assert!(gate.managed_only());
        assert!(gate.allows_source(HookSource::Settings(
            lingxi_core::types::SettingsScope::Managed
        )));
        assert!(!gate.allows_source(HookSource::Settings(
            lingxi_core::types::SettingsScope::User
        )));
    }

    #[test]
    fn settings_tier_disable_all_hooks_collapses_to_managed_only() {
        // vBr branch D: `ts().disableAllHooks===true` keeps the managed tier
        // (returns `e?.hooks`), unlike branch A (policy disableAllHooks) which
        // drops it. So the managed tier still loads.
        let gate = HookPolicyGate::from_policy_settings_json(None, false, true);
        assert!(gate.settings_disable_all_hooks);
        assert!(gate.managed_only());
        assert!(gate.allows_source(HookSource::Settings(
            lingxi_core::types::SettingsScope::Managed
        )));
        assert!(!gate.allows_source(HookSource::Settings(
            lingxi_core::types::SettingsScope::Project
        )));
    }

    #[test]
    fn strict_plugin_only_collapses_to_managed_only() {
        // vBr branch C: `iS("hooks")` (strictPluginOnlyCustomization) → managed
        // tier only. Sourced at the composition root and set on the gate field.
        let gate = HookPolicyGate {
            strict_plugin_only: true,
            ..HookPolicyGate::default()
        };
        assert!(gate.managed_only());
        assert!(gate.allows_source(HookSource::Settings(
            lingxi_core::types::SettingsScope::Managed
        )));
        assert!(!gate.allows_source(HookSource::Settings(
            lingxi_core::types::SettingsScope::User
        )));
    }

    #[test]
    fn policy_disable_all_hooks_takes_precedence_over_managed_only() {
        // Branch A must win over B: when BOTH disableAllHooks and
        // allowManagedHooksOnly are set, even the managed tier is dropped.
        let policy = r#"{ "disableAllHooks": true, "allowManagedHooksOnly": true }"#;
        let gate = HookPolicyGate::from_policy_settings_json(Some(policy), false, false);
        assert!(!gate.allows_source(HookSource::Settings(
            lingxi_core::types::SettingsScope::Managed
        )));
        assert!(!gate.allows_source(HookSource::Settings(
            lingxi_core::types::SettingsScope::User
        )));
    }

    #[test]
    fn malformed_policy_json_fails_open() {
        // The binary's `e?.` optional chaining treats missing/undefined policy
        // as no gating. A malformed JSON parses to the all-`None` default ⇒ no
        // gating beyond the runtime signals (here both false ⇒ everything loads).
        let gate = HookPolicyGate::from_policy_settings_json(Some("not json {"), false, false);
        assert!(!gate.policy_disable_all_hooks);
        assert!(!gate.allow_managed_hooks_only);
        assert!(gate.allows_source(HookSource::Settings(
            lingxi_core::types::SettingsScope::User
        )));
    }

    #[test]
    fn falsy_policy_flag_does_not_gate() {
        // `=== !0` is strict `true`: `disableAllHooks: false` must NOT gate.
        let policy = r#"{ "disableAllHooks": false, "allowManagedHooksOnly": false }"#;
        let gate = HookPolicyGate::from_policy_settings_json(Some(policy), false, false);
        assert!(!gate.policy_disable_all_hooks);
        assert!(!gate.managed_only());
        assert!(gate.allows_source(HookSource::Settings(
            lingxi_core::types::SettingsScope::User
        )));
    }

    #[test]
    fn gated_load_suppresses_without_parsing_when_blocked() {
        // A blocked source returns an empty vec WITHOUT parsing — a managed
        // disableAllHooks suppresses the user tier.
        let gate = HookPolicyGate::from_policy_settings_json(
            Some(r#"{ "disableAllHooks": true }"#),
            false,
            false,
        );
        let raw = one_stop_command();
        let hooks = parse_hooks_from_settings_json_gated(
            &raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
            gate,
        )
        .unwrap();
        assert!(
            hooks.is_empty(),
            "disableAllHooks must suppress the user tier"
        );
    }

    #[test]
    fn gated_load_managed_only_keeps_managed_drops_user() {
        let gate = HookPolicyGate::from_policy_settings_json(
            Some(r#"{ "allowManagedHooksOnly": true }"#),
            false,
            false,
        );
        let raw = one_stop_command();
        // Managed tier loads.
        let managed = parse_hooks_from_settings_json_gated(
            &raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::Managed),
            gate,
        )
        .unwrap();
        assert_eq!(managed.len(), 1);
        assert_eq!(
            managed[0].source,
            HookSource::Settings(lingxi_core::types::SettingsScope::Managed)
        );
        // User tier is suppressed.
        let user = parse_hooks_from_settings_json_gated(
            &raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
            gate,
        )
        .unwrap();
        assert!(user.is_empty());
    }

    #[test]
    fn gated_load_under_default_gate_equals_ungated() {
        // With the default (no-policy) gate, the gated entry point is
        // byte-identical to the ungated one.
        let raw = one_stop_command();
        let gated = parse_hooks_from_settings_json_gated(
            &raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
            HookPolicyGate::default(),
        )
        .unwrap();
        let ungated = parse_hooks_from_settings_json(
            &raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert_eq!(gated.len(), ungated.len());
        assert_eq!(gated.len(), 1);
        assert_eq!(gated[0].name, ungated[0].name);
        assert_eq!(gated[0].source, ungated[0].source);
    }
}
