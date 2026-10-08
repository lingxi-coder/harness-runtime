//! Static description of an agent (a.k.a. subagent type).
//!
//! An [`AgentDefinition`] is parsed from a markdown frontmatter file or
//! synthesized in code; it carries the tool policy, permission mode, model
//! preference, and worktree requirement that downstream subsystems consult
//! when a new agent is spawned. See spec §10.2.

pub use lingxi_core::host::subagent_spawn::ObserverSpec;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Self-contained description of an agent type used by
/// [`crate::context::SubagentContext`] and [`crate::pool::StateMachinePool`].
///
/// In addition to the resolver-consumed fields (`tools`, `permission_mode`,
/// `model`, `worktree_requirement`), this struct also carries the full set of
/// claude-code `BaseAgentDefinition` frontmatter fields parsed from markdown /
/// JSON agent files: `disallowed_tools`, `skills`, `required_mcp_servers`,
/// `background`, `isolation`, `memory`, `effort`, `initial_prompt`, and
/// `color`. Several of these are stored-but-not-yet-consumed (field-level
/// parity); see the per-field rustdoc and `catalog.rs`. See spec §10.2.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDefinition {
    /// Stable agent kind label (e.g. `"general"`, `"reviewer"`, `"fork"`).
    pub agent_type: String,
    /// Free-form guidance shown to the orchestrator about when to dispatch
    /// this agent.
    pub when_to_use: String,
    /// Tool exposure policy resolved by
    /// [`crate::tool_resolver::AgentToolResolver`].
    pub tools: AgentToolPolicy,
    /// Hard upper bound on turns this agent may take before forced shutdown.
    pub max_turns: u32,
    /// Model preference. Resolved against the host's model catalog at spawn.
    pub model: AgentModel,
    /// Permission mode applied to tool calls — see [`AgentPermissionMode`].
    pub permission_mode: AgentPermissionMode,
    /// Origin of the definition (built-in, user file, plugin, ...).
    pub source: AgentSource,
    /// Installed provider identity and seat for Mods `agent.offer`. This is
    /// host-owned plugin metadata, never parsed from agent frontmatter or
    /// persisted in a transcript. Non-plugin providers are derived from
    /// `source` at listing time.
    #[serde(skip)]
    pub offer_provider: Option<serde_json::Value>,
    /// Directory the definition was loaded from. Used to resolve relative
    /// includes (e.g. system-prompt fragments).
    pub base_dir: PathBuf,
    /// Optional system-prompt body to prepend to the per-spawn template.
    pub system_prompt: Option<String>,
    /// MCP servers the agent should connect to.
    pub mcp_servers: Vec<AgentMcpServerSpec>,
    /// Hooks declared inside the agent's frontmatter.
    pub frontmatter_hooks: Vec<hooks::HookDefinition>,
    /// Optional emoji or short icon for the UI.
    pub icon: Option<String>,
    /// Allow-list of tool names appended to the resolver output. Often used
    /// to expose MCP-server tools without expanding `AgentToolPolicy::All`.
    pub allowed_tools: Vec<String>,
    /// Worktree policy (required, optional, none) — see
    /// [`crate::worktree_policy::create_worktree_or_degrade`].
    pub worktree_requirement: Option<WorktreeRequirement>,
    /// Deny-list of tool names (claude `disallowedTools`). Field-level parity:
    /// parsed and stored, but not yet consumed by
    /// [`crate::tool_resolver::AgentToolResolver`] (a separate follow-up).
    #[serde(default)]
    pub disallowed_tools: Vec<String>,
    /// Skill names to preload (claude `skills` — parsed from
    /// comma-separated/list frontmatter).
    #[serde(default)]
    pub skills: Vec<String>,
    /// MCP server name patterns that must be configured for the agent to be
    /// available (claude `requiredMcpServers`). NOTE: claude does NOT parse
    /// this from markdown/JSON frontmatter — it is set only on
    /// built-in/programmatic defs. The catalog/JSON parsers leave it empty.
    #[serde(default)]
    pub required_mcp_servers: Vec<String>,
    /// Always run as a background task when spawned (claude `background`).
    #[serde(default)]
    pub background: bool,
    /// Omit user/project/local instructions only when running as a subagent.
    #[serde(default, rename = "omitInstructions")]
    pub omit_instructions: bool,
    /// Isolation mode selected for the agent. Availability is enforced by
    /// the host execution policy when the agent is spawned.
    #[serde(default)]
    pub isolation: Option<AgentIsolation>,
    /// Persistent memory scope (claude `memory`). Parsed, stored, and EXECUTED:
    /// when set, [`crate::tool_resolver::AgentToolResolver::resolve`] injects the
    /// auto-memory tools (`Read`/`Write`/`Edit`) into the spawned agent's tool
    /// pool, mirroring claude's `isAutoMemoryEnabled` → Write/Edit/Read
    /// injection (the scope selects only WHERE memory lives, not which tools).
    #[serde(default)]
    pub memory: Option<lingxi_core::types::WritableScope>,
    /// Reasoning effort preference, validated against the selected provider.
    #[serde(default)]
    pub effort: Option<AgentEffort>,
    /// Prepended to the first user turn (claude `initialPrompt`). The raw
    /// (untrimmed) value is kept when its trimmed form is non-empty.
    #[serde(default)]
    pub initial_prompt: Option<String>,
    /// Validated agent color name (claude `color`, one of [`AGENT_COLORS`]).
    /// Stored distinct from the `icon` emoji field.
    #[serde(default)]
    pub color: Option<String>,
    /// Optional companion agent that observes this agent's run.
    ///
    /// The schema is versioned so persisted snapshots can evolve without
    /// invalidating older transcripts. `None` is the legacy/default behavior.
    #[serde(default)]
    pub observer: Option<ObserverSpec>,
    /// `experimental.cacheTtl` — prompt-cache TTL for this agent's requests.
    ///
    /// claude-code 2.1.267 (`src_163219561.js` @1339284): *"Prompt cache TTL for
    /// this agent's requests (\"5m\" or \"1h\") when no `subagentPromptCacheTtl`
    /// setting or env var is set."* Consumed upstream as `agentCacheTtlOverride`
    /// (@3483567); the complete value is carried to the provider cache resolver.
    ///
    /// `None` means the frontmatter said nothing — env/setting decides, exactly
    /// as before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_ttl: Option<AgentCacheTtl>,
}

/// `experimental.cacheTtl` value domain: upstream accepts exactly `"5m"`/`"1h"`.
///
/// `FiveMinutes` is Anthropic's default ephemeral lifetime, so it maps to a
/// plain breakpoint; `OneHour` is the one that changes the wire (`ttl:"1h"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentCacheTtl {
    /// `"5m"` — default ephemeral lifetime; emits no `ttl` key.
    #[serde(rename = "5m")]
    FiveMinutes,
    /// `"1h"` — emits `ttl:"1h"` (claude-code `should1hCacheTTL`).
    #[serde(rename = "1h")]
    OneHour,
}

impl AgentCacheTtl {
    /// Parse a frontmatter value. Unknown strings are REJECTED, not defaulted:
    /// a typo'd `"60m"` silently reading as `"5m"` would look like the feature
    /// working while doing nothing.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "5m" => Some(Self::FiveMinutes),
            "1h" => Some(Self::OneHour),
            _ => None,
        }
    }
}

/// Strategy the [`crate::tool_resolver::AgentToolResolver`] uses to project
/// the parent agent's tool set onto the child.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentToolPolicy {
    /// Inherit every tool from the parent. `use_exact_tools` controls whether
    /// the child receives the parent's exact tool instances or fresh ones.
    All {
        /// `true` when the child must reuse the parent's `Arc<dyn Tool>`
        /// references (e.g. to share state); `false` when it may be re-bound.
        use_exact_tools: bool,
    },
    /// Inherit only the explicitly named tools.
    Explicit(Vec<String>),
    /// Inherit every tool except the explicitly named ones.
    Except(Vec<String>),
}

/// Model resolution strategy for an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentModel {
    /// Use whichever model the parent picked.
    Inherit,
    /// Resolve via a logical alias (e.g. `"sonnet"`, `"haiku-fast"`).
    Alias(String),
    /// Use the specific model id verbatim.
    Explicit(String),
}

/// Permission mode applied to tool calls inside the subagent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentPermissionMode {
    /// Bubble prompts up to the parent agent for approval.
    Bubble,
    /// Run isolated — never prompt; deny if not pre-approved.
    Isolated,
    /// Use the session's default permission mode.
    Default,
    /// Allow edits without prompting, but still ask for higher-risk actions.
    AcceptEdits,
    /// Never ask; deny unresolved asks.
    DontAsk,
    /// Full bypass permissions mode.
    BypassPermissions,
    /// Auto-approve every tool call (sandboxed contexts).
    Auto,
    /// Plan-only mode — read-only tools only.
    Plan,
}

/// Where an agent definition came from.
///
/// The settings rungs are the shared [`lingxi_core::types::SettingsScope`]; the rest are
/// this subsystem's own producers.
///
/// `AdditionalDirectory` stays a separate variant rather than collapsing into
/// `Settings(Project)`: [`crate::hooks_trust::source_is_self_trusting`] is a
/// security gate in which `Settings(Project)` is the ONLY value that must prove
/// folder trust, and merging the two would drag `--add-dir` agents onto the
/// wrong side of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgentSource {
    /// Compiled into the binary.
    BuiltIn,
    /// Read from a settings tier's `agents/` directory.
    Settings(lingxi_core::types::SettingsScope),
    /// Supplied by the `--agents` flag for this run.
    Flag,
    /// Loaded by an installed plugin.
    Plugin,
    /// Discovered under an `--add-dir` directory.
    AdditionalDirectory,
}

/// Isolation mode for an agent. The host decides which execution modes it supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentIsolation {
    /// Run in an isolated git worktree (claude `'worktree'`).
    Worktree,
    /// Run in a remote environment provided by the host.
    Remote,
}

/// Reasoning effort preference passed to the shared LLM request boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentEffort {
    /// A named level from [`EFFORT_LEVELS`].
    Level(String),
    /// An integer thinking budget, when supported by the selected provider.
    Numeric(i64),
}

impl AgentEffort {
    /// A scalar effort preference for the shared request boundary: a named
    /// level or an integer budget. The SDK owns its provider-specific projection.
    #[must_use]
    pub fn to_wire(&self) -> serde_json::Value {
        match self {
            AgentEffort::Level(s) => serde_json::Value::String(s.clone()),
            AgentEffort::Numeric(n) => serde_json::Value::Number((*n).into()),
        }
    }

    /// Parse a JSON effort preference: a level from [`EFFORT_LEVELS`] or an
    /// integer. `None` for an invalid value.
    #[must_use]
    pub fn from_json(value: &serde_json::Value) -> Option<AgentEffort> {
        match value {
            serde_json::Value::String(s) if EFFORT_LEVELS.contains(&s.as_str()) => {
                Some(AgentEffort::Level(s.clone()))
            }
            serde_json::Value::Number(n) => n.as_i64().map(AgentEffort::Numeric),
            _ => None,
        }
    }
}

/// Named reasoning efforts supported by the shared LLM protocol.
/// The selected provider validates which levels its model accepts.
pub const EFFORT_LEVELS: [&str; 7] = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// Coerce a YAML/JSON frontmatter value into an [`AgentEffort`], mirroring
/// claude `parseEffortValue` (effort.ts:71-87):
/// - `undefined`/`null`/empty-string -> `None`
/// - integer number -> `Numeric`
/// - string whose lowercased form is in [`EFFORT_LEVELS`] -> `Level`
/// - else `parseInt(str, 10)`; if an integer -> `Numeric`, else `None`
#[must_use]
pub fn parse_effort_value(value: &serde_yaml::Value) -> Option<AgentEffort> {
    match value {
        // value === undefined / null
        serde_yaml::Value::Null => None,
        // typeof value === 'number' && isValidNumericEffort(value)
        // (isValidNumericEffort = Number.isInteger)
        serde_yaml::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(AgentEffort::Numeric(i))
            } else {
                // Non-integer number (float): falls through to String(value)
                // path. parseInt of a float string truncates to its integer
                // part, matching JS `parseInt(String(1.5), 10) === 1`.
                let s = n.to_string();
                parse_effort_from_string(&s)
            }
        }
        serde_yaml::Value::String(s) => {
            // value === '' -> undefined
            if s.is_empty() {
                return None;
            }
            parse_effort_from_string(s)
        }
        serde_yaml::Value::Bool(b) => {
            // String(true)='true' / String(false)='false' -> not a level,
            // parseInt -> NaN -> None.
            let s = if *b { "true" } else { "false" };
            parse_effort_from_string(s)
        }
        // Arrays/maps: String(value) is non-numeric -> None.
        _ => None,
    }
}

/// String branch of [`parse_effort_value`]: lowercased level, else
/// `parseInt(str, 10)`.
fn parse_effort_from_string(value: &str) -> Option<AgentEffort> {
    let lower = value.to_lowercase();
    if EFFORT_LEVELS.contains(&lower.as_str()) {
        return Some(AgentEffort::Level(lower));
    }
    // JS parseInt(str, 10): consumes an optional sign and leading digits,
    // stopping at the first non-digit; NaN if no leading integer.
    parse_int_radix10(value).map(AgentEffort::Numeric)
}

/// JS-`parseInt(str, 10)` semantics: skip leading whitespace, take an optional
/// `+`/`-`, then consume leading ASCII digits; ignore any trailing garbage;
/// return `None` (NaN) when no digits are found.
pub(crate) fn parse_int_radix10(s: &str) -> Option<i64> {
    let t = s.trim_start();
    let mut chars = t.chars().peekable();
    let mut out = String::new();
    if let Some(&c) = chars.peek() {
        if c == '+' || c == '-' {
            out.push(c);
            chars.next();
        }
    }
    let mut saw_digit = false;
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            out.push(c);
            saw_digit = true;
            chars.next();
        } else {
            break;
        }
    }
    if !saw_digit {
        return None;
    }
    out.parse::<i64>().ok()
}

/// Reference to an MCP server an agent should connect to.
///
/// Mirrors claude's `AgentMcpServerSpecSchema` union — `string | record` — with
/// the record kept RAW: validation + conversion to scoped
/// [`mcp::McpServerConfig`]s happens in
/// [`crate::mcp_servers::agent_mcp_specs_to_scoped_configs`] (claude `obs`),
/// which is also where a record with more than one key is rejected
/// (`Object.entries(r).length !== 1`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentMcpServerSpec {
    /// String entry: refer to a server already configured by name in the host
    /// (skipped by the scoped-config conversion, claude `typeof r==="string"`).
    ByName(String),
    /// Record entry: `{ name: config }` with the config body in the same shape
    /// as one `.mcp.json` server entry, held raw until conversion.
    Record(serde_json::Map<String, serde_json::Value>),
}

/// Whether the agent needs an isolated git worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorktreeRequirement {
    /// Spawn must fail if a worktree cannot be created.
    Required,
    /// Try to create one; fall back to in-place execution on failure.
    Optional,
    /// Don't bother with worktrees.
    None,
}

impl AgentDefinition {
    /// `true` if this is the special `fork` agent that runs a byte-exact
    /// copy of the parent's prompt cache. See spec §10.4.
    #[must_use]
    pub fn is_fork(&self) -> bool {
        self.agent_type == "fork"
    }
}

#[cfg(test)]
mod cache_ttl_tests {
    use super::AgentCacheTtl;

    #[test]
    fn only_the_two_upstream_values_parse() {
        assert_eq!(AgentCacheTtl::parse("5m"), Some(AgentCacheTtl::FiveMinutes));
        assert_eq!(AgentCacheTtl::parse("1h"), Some(AgentCacheTtl::OneHour));
        assert_eq!(AgentCacheTtl::parse(" 1h "), Some(AgentCacheTtl::OneHour));
    }

    /// ⛔ Unknown values must be REJECTED, never defaulted. A `"60m"` that
    /// quietly read as `"5m"` would look like the feature working while doing
    /// nothing — and `"1h"`-looking typos are the likely mistake.
    #[test]
    fn unknown_values_are_rejected_not_defaulted() {
        for raw in ["60m", "1hr", "1H", "3600", "", "true", "5 m"] {
            assert_eq!(AgentCacheTtl::parse(raw), None, "{raw:?} must not parse");
        }
    }

    /// The serde spellings are the frontmatter spellings, not the Rust names.
    #[test]
    fn serde_round_trips_the_upstream_spellings() {
        let one = serde_json::to_string(&AgentCacheTtl::OneHour).unwrap();
        assert_eq!(one, "\"1h\"");
        assert_eq!(
            serde_json::from_str::<AgentCacheTtl>("\"5m\"").unwrap(),
            AgentCacheTtl::FiveMinutes
        );
    }
}

#[cfg(test)]
mod effort_tests {
    use super::*;

    #[test]
    fn named_efforts_match_the_shared_protocol() {
        for level in EFFORT_LEVELS {
            let parsed = AgentEffort::from_json(&serde_json::json!(level)).unwrap();
            let wire = parsed.to_wire();
            let mut request = llm_runtime::protocol::LlmRequest::default();
            request.set_effort(Some(wire)).unwrap();
            let effort = request.input.thinking.unwrap().effort.unwrap();
            assert_eq!(
                serde_json::to_value(effort).unwrap(),
                serde_json::json!(level)
            );
        }
        for raw in ["unsupported", "7", "7junk", "1.5"] {
            assert!(AgentEffort::from_json(&serde_json::json!(raw)).is_none());
        }
    }
}
