//! Hook definition + executor / source taxonomy (spec §9.2, §9.3).

use crate::events::HookEventType;
use lingxi_core::types::HookId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

/// A registered hook: which events to fire on, optional matcher, how to
/// execute, and where the definition was loaded from.
///
/// Hooks are evaluated in priority-descending order; the executor stops
/// processing further hooks for the same event as soon as one returns a
/// [`crate::HookDecision::Block`] decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookDefinition {
    /// Stable engine-assigned ID for telemetry and cancellation.
    pub id: HookId,
    /// Human-readable name (used in logs and user messages).
    pub name: String,
    /// Event kinds this hook subscribes to.
    pub events: Vec<HookEventType>,
    /// Optional regex / matcher applied before executing the hook.
    /// When `None` the hook fires unconditionally for its subscribed events.
    pub if_condition: Option<HookCondition>,
    /// How to actually run the hook body.
    pub executor: HookExecutor,
    /// Where the hook definition came from (used for trust / display).
    pub source: HookSource,
    /// If `true` the engine awaits the result before continuing the in-flight
    /// action. If `false` the hook is dispatched to the background async
    /// registry and the engine proceeds immediately.
    pub blocking: bool,
    /// Optional wall-clock timeout. `None` means use the executor's default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<Duration>,
    /// Higher values execute first. Ties resolved by registration order.
    pub priority: i32,
    /// If `true`, the hook runs once and is removed after a successful
    /// execution (claude-code `once` field; `schemas/hooks.ts:51-54`).
    ///
    /// The executor removes the definition from its registry after a successful
    /// run. Defaults to `false`.
    #[serde(default)]
    pub once: bool,
    /// Custom status message shown in the spinner while the hook runs
    /// (claude-code `statusMessage` field; `schemas/hooks.ts:47-50`).
    ///
    /// The executor emits this through the live hook observer and the TUI keeps
    /// it out of transcript history. Defaults to `None` (use the engine's
    /// generic running message).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_message: Option<String>,
    /// Re-enter the agent loop when this asynchronous hook completes.
    #[serde(default)]
    pub async_rewake: bool,
    /// Wall-clock limit for asynchronous execution. This is independent from
    /// the foreground `timeout` and defaults to 15 seconds at dispatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub async_timeout: Option<Duration>,
    /// Bounded meta-context supplied when an async completion reawakens the
    /// agent. Empty values fall back to the normal async-hook response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rewake_message: Option<String>,
}

impl HookDefinition {
    /// The hook's tool-name matcher pattern, if any (B3).
    ///
    /// In claude-code a `Pre/PostToolUse` hook declares a `matcher` string
    /// (e.g. `"Write|Edit"`, `"^Bash$"`, `"*"`) that gates whether the hook
    /// fires for a given tool. Here that pattern is canonically carried on
    /// [`Self::if_condition`] as the [`HookCondition::pattern`] of a condition
    /// whose [`HookCondition::match_tool_name`] is `true` — that is how the
    /// settings loader (`loader.rs`) and the orchestrator's `list_hooks`
    /// (`handle_impl.rs`) already model it. This accessor surfaces that pattern
    /// as the `Option<String>`-shaped "matcher" the B3 spec calls for, WITHOUT
    /// adding a new struct field (which would break the explicit
    /// `HookDefinition { … }` literals constructed in the locked test-harness
    /// and orchestrator integration tests).
    ///
    /// Returns `None` when the hook has no condition, or has an
    /// `if`-condition that is NOT a tool-name matcher (`match_tool_name ==
    /// false`) — such a condition is a permission-rule / input matcher,
    /// surfaced separately by [`Self::if_pattern`].
    #[must_use]
    pub fn matcher(&self) -> Option<&str> {
        self.if_condition
            .as_ref()
            .filter(|c| c.match_tool_name)
            .map(|c| c.pattern.as_str())
    }

    /// The hook's `if`-condition permission-rule string, if any (e.g.
    /// `"Bash(git push:*)"`).
    ///
    /// This is the per-hook `if` field from claude-code's hook schema
    /// (`schemas/hooks.ts:35` `IfConditionSchema`): a permission-rule-syntax
    /// pattern evaluated against the event's tool name + tool input by
    /// [`crate::matcher::matches_if_condition`] in
    /// [`crate::registry::HookRegistry::match_event`]. It is INDEPENDENT of the
    /// tool-name [`Self::matcher`] (the parent group `matcher`); a hook may
    /// declare both, and claude-code requires BOTH to pass before the hook
    /// fires (`utils/hooks.ts:1681-1685` then `:1808-1850`). Returns `None` when
    /// the hook carries no `if`-condition.
    #[must_use]
    pub fn if_pattern(&self) -> Option<&str> {
        self.if_condition
            .as_ref()
            .and_then(|c| c.if_pattern.as_deref())
    }
}

/// claude-code's command-hook `shell` selector (`schemas/hooks.ts`;
/// oracle 2.1.238 @ 282293705).
///
/// ```js
/// shell:Mr(w$u).optional().describe("Shell interpreter. 'bash' uses your $SHELL
///   (bash/zsh/sh); 'powershell' uses pwsh. Defaults to bash (powershell on
///   Windows without Git Bash).")
/// ```
/// with `w$u=["bash","powershell"]` (oracle @ 282293098). Identical in 2.1.220 —
/// this is an old port gap, not 238 drift.
///
/// The selector is only consulted for the SHELL form of a command hook (no
/// `args`); the exec form (`args` present) spawns the executable directly and is
/// never wrapped (oracle spawn head @ 296948400: `if(I) spawn(I[0],I[1],…)` is
/// taken FIRST, ahead of both shell branches).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HookShell {
    /// `'bash' uses your $SHELL (bash/zsh/sh)`. On POSIX this is Node's
    /// `shell: true`, i.e. `/bin/sh -c <command>`.
    Bash,
    /// `'powershell' uses pwsh`. Spawns the resolved PowerShell executable with
    /// `[-NoProfile, -NonInteractive, (-ExecutionPolicy Bypass), -Command, <cmd>]`.
    Powershell,
}

impl HookShell {
    /// Parse the wire spelling. `None` for any value outside `w$u`, which is
    /// how zod's `Mr(w$u)` rejects the entry.
    #[must_use]
    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "bash" => Some(Self::Bash),
            "powershell" => Some(Self::Powershell),
            _ => None,
        }
    }
}

/// How the engine actually invokes a hook body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HookExecutor {
    /// Spawn an external process. Hook payload is delivered via stdin; the
    /// process replies with a JSON [`crate::HookResponse`] on stdout.
    Command {
        /// Executable path or PATH-resolvable command name.
        command: String,
        /// Positional CLI arguments.
        args: Vec<String>,
        /// Environment variables to set for the child process.
        env: HashMap<String, String>,
        /// Working directory for the child; `None` inherits the engine's cwd.
        cwd: Option<PathBuf>,
        /// claude-code `shell` (`w$u=["bash","powershell"]`, oracle 2.1.238 @
        /// 282293705). `None` = the field was omitted, which upstream resolves
        /// at spawn time as `e.shell ?? Otr()` where
        /// `Otr()=Sh()?"bash":"powershell"` and `Sh()` is "not Windows, or Git
        /// Bash was found" — see [`crate::executor::default_hook_shell`].
        /// Ignored entirely for the exec form (`args` non-empty).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        shell: Option<HookShell>,
    },
    /// POST the event payload to an HTTP endpoint and parse the JSON body
    /// as the [`crate::HookResponse`]. Subject to the [`crate::SsrfGuard`].
    Http {
        /// Endpoint URL (scheme must satisfy the SSRF guard).
        url: String,
        /// HTTP method (typically `"POST"`).
        method: String,
        /// Request headers to attach. Values may reference environment variables
        /// via `$VAR`/`${VAR}`; only names in [`Self::Http::allowed_env_vars`]
        /// are interpolated (claude-code `cHm`).
        headers: HashMap<String, String>,
        /// Env-var names that header values may interpolate (`allowedEnvVars`,
        /// `schemas/hooks.ts`). A `$VAR`/`${VAR}` reference whose name is NOT in
        /// this list resolves to an empty string (claude-code `cHm` warns +
        /// substitutes `""`); a name present resolves to the process env value
        /// (`process.env[VAR] ?? ""`). Empty ⇒ no interpolation (every `$VAR`
        /// reference is blanked).
        allowed_env_vars: Vec<String>,
        /// Request timeout enforced by the executor.
        timeout: Duration,
    },
    /// Run plugin-supplied JavaScript in the deny-by-default sandbox
    /// (claude-code function hooks; upstream runs these in a bundled worker).
    ///
    /// The body receives the event payload as `input` and returns a JSON value.
    /// ⛔ It gets NO host capabilities — see [`crate::function_hook`] for the
    /// threat model and why nothing may be bound into that context.
    Function {
        /// The hook body. Evaluated as a function body, so it `return`s.
        source: String,
        /// Per-hook wall-clock budget (upstream `budgetMs`). `None` uses
        /// [`crate::function_hook::DEFAULT_FUNCTION_HOOK_BUDGET`].
        budget_ms: Option<u64>,
    },
    /// Fork an agent of the named type, running the supplied prompt. The
    /// agent's tool result becomes the hook response.
    Agent {
        /// Agent type discriminator.
        agent_type: String,
        /// Prompt text supplied to the spawned agent.
        prompt: String,
        /// Optional model override (`schemas/hooks.ts:128-163` `model`). When
        /// `None` the spawned verifier inherits the spawner's default model.
        model: Option<String>,
    },
    /// Evaluate an inline single-turn LLM query and map the model's
    /// `{ok, reason?}` JSON to a hook decision (claude-code
    /// `utils/hooks/execPromptHook.ts`; `schemas/hooks.ts:67-95`).
    ///
    /// The query is run through the injected
    /// [`crate::prompt_executor::HookPromptRunner`] (wired on the
    /// [`crate::HookExecutorImpl`] via `with_prompt_runner`). When no runner is
    /// wired the arm is a structured "not wired" no-op — it never blocks.
    Prompt {
        /// Prompt text to evaluate. May contain `$ARGUMENTS` placeholders that
        /// the executor substitutes with the serialized event payload
        /// (`addArgumentsToPrompt`; `execPromptHook.ts:35`).
        prompt: String,
        /// Optional model override (`schemas/hooks.ts:81-86`). When `None` the
        /// runner falls back to its default small-fast model
        /// (`getSmallFastModel()`; `execPromptHook.ts:79`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// `continueOnBlock` (`schemas/hooks.ts`): when the hook blocks
        /// (`ok:false`), whether the turn may still continue. Drives the prompt
        /// executor's `prevent_continuation = !continue_on_block`. Default false
        /// (a block ends the turn).
        #[serde(default)]
        continue_on_block: bool,
    },
    /// Call a tool on an already-configured MCP server (claude-code
    /// `McpToolHookSchema`, settings `"type": "mcp_tool"`).
    ///
    /// Oracle 2.1.238 @ 282295711 (identical in 2.1.220 — this is an old port
    /// gap, not 238 drift):
    ///
    /// ```js
    /// be({type:At("mcp_tool").describe("MCP tool hook type"),
    ///     server:H().describe("Name of an already-configured MCP server to invoke"),
    ///     tool:H().describe("Name of the tool on that server to call"),
    ///     input:lo(H(),Fn()).optional().describe('Arguments passed to the MCP tool. String values
    ///       support ${path} interpolation from the hook input JSON (e.g. "${tool_input.file_path}").'),
    ///     if:Pxn(), timeout:…, statusMessage:…, once:…})
    /// ```
    ///
    /// The five hook schemas are a `z0("type", …)` DISCRIMINATED UNION, so the
    /// oracle accepts an `mcp_tool` entry like any other; the port's loader used
    /// to fall through its `build_executor` match and drop the entry SILENTLY.
    /// This variant exists so the entry loads: the hook registers, matches, and
    /// honors `matcher` / `if` / `once` / `statusMessage` / `timeout` like every
    /// other hook.
    ///
    /// RESIDUAL (documented, not silent): actually invoking the tool needs an
    /// MCP client by server NAME, which the `hooks` crate has no seam for (it
    /// depends on `traits`, whose `McpTransport` is a raw per-connection
    /// transport, not a name-addressed invoker). Until a runner is injected the
    /// executor arm returns the same structured "not wired" error the
    /// [`Self::Command`] and [`Self::Prompt`] arms return when THEIR runner is
    /// absent — it never blocks a turn. The `${path}` interpolation of the
    /// variant's `input` map belongs to that invocation step and is therefore
    /// also deferred rather than implemented with no call site.
    McpTool {
        /// `server`: name of an already-configured MCP server to invoke.
        server: String,
        /// `tool`: name of the tool on that server to call.
        tool: String,
        /// `input`: arguments passed to the MCP tool. String values support
        /// `${path}` interpolation from the hook input JSON (e.g.
        /// `"${tool_input.file_path}"`), applied at invocation time.
        #[serde(default)]
        input: HashMap<String, Value>,
    },
    /// Dispatch to an in-process Rust handler registered via
    /// [`crate::HookExecutorImpl::register_builtin`].
    Builtin {
        /// Handler ID, looked up in the executor's builtin table.
        handler_id: String,
    },
}

/// Optional matcher applied before executing a hook.
///
/// A single pattern flagged for either tool-name or input matching.
///
/// When `match_tool_name` is `true`, [`Self::pattern`] is the B3 tool-name
/// matcher (`"Write|Edit"`, `"^Bash$"`, `"*"`, …) and is evaluated by
/// [`crate::matcher::matches_pattern`] against the event's tool name in
/// [`crate::registry::HookRegistry::match_event`]. This is the MATCHER half of
/// B3 and is fully ported.
///
/// When `match_input` is `true`, [`Self::if_pattern`] carries a permission-rule
/// string (e.g. `"Bash(git push:*)"`) matched against the tool name + tool
/// input — the `if`-condition half of B3. That rule-content matching is now
/// PORTED ([`crate::matcher::matches_if_condition`], reusing the permission
/// crate's `PermissionRuleValue::from_rule_string` + per-tool content matchers)
/// and gates firing in [`crate::registry::HookRegistry::match_event`].
///
/// The tool-name [`Self::pattern`] (group `matcher`) and the [`Self::if_pattern`]
/// (`if`-condition) are INDEPENDENT — a hook may carry both, and both must pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookCondition {
    /// Pattern text of the B3 tool-name matcher, evaluated when
    /// `match_tool_name` is `true`. (The `if`-condition lives in
    /// [`Self::if_pattern`], not here.)
    pub pattern: String,
    /// If `true` [`Self::pattern`] is the B3 tool-name matcher (evaluated).
    pub match_tool_name: bool,
    /// If `true` an [`Self::if_pattern`] permission-rule string is present and
    /// matched against the tool name + serialized `tool_input` (the
    /// `if`-condition half of B3 — now ported). Kept in sync with
    /// `if_pattern.is_some()`.
    pub match_input: bool,
    /// The `if`-condition permission-rule string (claude-code per-hook `if`
    /// field, `schemas/hooks.ts:35`). `None` when the hook has no `if`. Additive
    /// (`#[serde(default)]`) so existing serialized hooks deserialize unchanged,
    /// and `skip_serializing_if` keeps a `None` byte-identical to the pre-`if`
    /// serialization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub if_pattern: Option<String>,
}

/// Origin of a hook definition. Used by the engine to display trust info to
/// the user and by policy code to decide whether a hook can run at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HookSource {
    /// A settings tier's `hooks.json` — `~/.lingxi/`, the committed project
    /// file, the developer-local override, or managed policy.
    Settings(lingxi_core::types::SettingsScope),
    /// Hook supplied by an installed plugin.
    Plugin,
    /// Hook embedded in an agent file's front-matter.
    FrontMatter,
    /// Hook registered at runtime by another part of the engine for the
    /// duration of a single session.
    Session,
    /// Hook registered as part of a skill bundle.
    Skill,
}

impl HookSource {
    /// Claude-compatible source-family label used by deferred hook records.
    /// Plugin/skill identity is not part of the legacy definition wire shape,
    /// so those sources retain their trusted family rather than fabricating a
    /// package name.
    #[must_use]
    pub const fn deferred_label(self) -> &'static str {
        match self {
            Self::Settings(_) => "settings",
            Self::Plugin => "plugin",
            Self::FrontMatter => "agent",
            Self::Session => "session",
            Self::Skill => "skill",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::HookSource;

    /// Pins the family label every source projects to. The four settings tiers
    /// collapse to a single `"settings"` family: on this wire the producer is
    /// observable and the settings rung is not.
    ///
    /// That collapse is now structural — the rung lives inside
    /// `Settings(_)`, so there is no longer a way to spell the four tiers apart
    /// here by accident. The four rows below therefore guard less than they did
    /// when each tier was its own variant; what they still pin is that a
    /// settings-backed hook reports `"settings"` and not one of the producer
    /// labels. A newly added producer is caught by
    /// [`HookSource::deferred_label`]'s exhaustive `match`.
    #[test]
    fn deferred_label_folds_the_settings_tiers_and_keeps_each_origin_distinct() {
        for (source, expected) in [
            (
                HookSource::Settings(lingxi_core::types::SettingsScope::User),
                "settings",
            ),
            (
                HookSource::Settings(lingxi_core::types::SettingsScope::Project),
                "settings",
            ),
            (
                HookSource::Settings(lingxi_core::types::SettingsScope::Local),
                "settings",
            ),
            (
                HookSource::Settings(lingxi_core::types::SettingsScope::Managed),
                "settings",
            ),
            (HookSource::Plugin, "plugin"),
            (HookSource::FrontMatter, "agent"),
            (HookSource::Session, "session"),
            (HookSource::Skill, "skill"),
        ] {
            assert_eq!(source.deferred_label(), expected, "{source:?}");
        }
    }
}
