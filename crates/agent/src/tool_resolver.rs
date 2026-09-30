//! Resolve the tool set exposed to a subagent.
//!
//! [`AgentToolResolver`] projects the parent agent's tool set onto the child
//! according to the child's [`AgentToolPolicy`], appends the per-agent MCP
//! tools, drops the always-disallowed agent-tool set + the per-definition
//! denylist, and then applies the canonical plan-safe filter when the child runs in
//! [`AgentPermissionMode::Plan`]. See spec §10.8.
//!
//! ## Always-disallowed default drop (claude `ALL_AGENT_DISALLOWED_TOOLS`)
//!
//! Mirroring claude-code `filterToolsForAgent` (`AgentTool/agentToolUtils.ts`),
//! every subagent pool has the agent-management / plan-mode tools stripped by
//! default: `TaskOutput`, `ExitPlanMode`, `EnterPlanMode`, `AskUserQuestion`,
//! `ConnectGitHub`, `WaitForMcpServers`, `ScheduleWakeup` (claude `_qd`).
//! `Workflow` is additionally dropped for non-ant subagents (claude
//! `...(USER_TYPE !== 'ant' ? [WORKFLOW_TOOL_NAME] : [])`).
//!
//! `TaskStop` is NOT in that set — it is allowed to subagents. And `Agent` is
//! NOT flat-denied either: it is DEPTH-GATED in [`AgentToolResolver::resolve`]
//! per claude's `if(isAgentTool(a)) return depth < maxSpawnDepth`. As of 2.1.219
//! the default maximum depth is 3 (was 1 through 2.1.217), with
//! `CLAUDE_CODE_MAX_SUBAGENT_SPAWN_DEPTH` providing the same override as
//! Claude Code. The fork `use_exact_tools` bypass is exempt (fork recursion is
//! governed by `AgentTool`'s `is_in_fork_child` message guard).
//!
//! ## Per-definition `disallowedTools` subtraction (claude `resolveAgentTools`)
//!
//! After the always-disallowed drop, the agent definition's own
//! [`AgentDefinition::disallowed_tools`] (claude `disallowedTools` frontmatter,
//! `loadAgentsDir.ts:676-681`) is subtracted from the pool — see
//! `agentToolUtils.ts:149-160`. Each spec's trailing `(rule content)` is
//! stripped before comparison (claude `permissionRuleValueFromString`); LingXi
//! extracts the base tool name as the prefix before the first `(`.
//!
//! ## `Agent(x)` deny semantics are NOT handled here
//!
//! claude's `Agent(x)` rule content carries `allowedAgentTypes` metadata which
//! restricts WHICH agent TYPES may be launched — it operates on the spawnable
//! agent-type LIST (claude `filterDeniedAgents` / `getDenyRuleForAgent`,
//! `permissions.ts:308-343`), NOT on the tool pool. It never adds the `Agent`
//! tool to a child. LingXi's resolver does not model `allowedAgentTypes` at all
//! and so structurally cannot wrongly add `Agent` to a child based on it.

use crate::definition::{AgentDefinition, AgentModel, AgentPermissionMode, AgentToolPolicy};
use std::collections::HashSet;
use std::sync::Arc;
use thiserror::Error;
use tool_api::Tool;

/// Stateless utility that computes the effective tool set for an agent
/// spawn from the agent definition plus the surrounding tool sets.
pub struct AgentToolResolver;

/// Coordinator workers must not be able to route through the generic MCP
/// dispatcher or inspect MCP auth state, even when those base-list tools do
/// not carry the per-server `role:"comms"` marker themselves. Keep this
/// name-based gate local to `agent`: the crate deliberately has no dependency
/// on `tools/mcp`, and resource helper tools are intentionally not part of it.
fn coordinator_worker_tool_allowed(tool: &dyn Tool) -> bool {
    tool.mcp_role() != Some("comms") && !matches!(tool.name(), "MCP" | "McpAuth")
}

/// Errors while converting an agent definition's explicit tool policy into the
/// child-visible schema/allow-list.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum ToolResolutionError {
    /// A definition requested tool names the parent tool pool cannot resolve.
    #[error("unknown explicit agent tool(s): {0}")]
    UnknownExplicitTools(String),
    /// A non-empty explicit policy was valid syntactically but every requested
    /// tool was removed by default deny rules, plan mode, or policy filters.
    #[error("explicit agent tools resolved to an empty set after filtering: {0}")]
    EmptyExplicitToolSet(String),
}

impl AgentToolResolver {
    /// Tools every subagent has stripped by default, mirroring claude-code's
    /// `ALL_AGENT_DISALLOWED_TOOLS` (`constants/tools.ts:36-46`). `is_ant`
    /// gates the `Agent` entry: when `true` (claude `USER_TYPE === 'ant'`) the
    /// `Agent` tool is KEPT so nested agents may spawn further agents.
    ///
    /// The tool names are hardcoded string literals because the `agent` crate
    /// has no path-dep on `tools/*` (verified via `agent/Cargo.toml`), so the
    /// `*_TOOL_NAME` consts are unreachable — `builtins.rs` already hardcodes
    /// the same names. `Workflow` is dropped for non-ant subagents (claude
    /// v2.1.186 `HDd` ant-gates it exactly like `Agent`).
    #[must_use]
    pub fn all_agent_disallowed_tools(is_ant: bool) -> Vec<&'static str> {
        // PARITY: binary 2.1.191 `_qd(e)` (cc_all.txt:16041079):
        //   `function _qd(e){return new Set([eW,WD,Kz,nm,Kst,tHe,
        //                                    ...e!=="ant"?[av]:[],Kh])}`
        // resolving the minified consts (grounded in the same binary):
        //   eW="TaskOutput"  WD="ExitPlanMode"  Kz="EnterPlanMode"
        //   nm="AskUserQuestion"  Kst="ConnectGitHub"  tHe="WaitForMcpServers"
        //   av="Workflow" (non-ant only)  Kh="ScheduleWakeup"
        // `nHe=_qd("external")` is the set the subagent tool-filter consults
        // (`if(nHe.has(a.name))return!1`, cc_all.txt:19974083) — so ScheduleWakeup,
        // ConnectGitHub and WaitForMcpServers ARE flatly denied to every subagent.
        // ScheduleWakeup denial is load-bearing here: the WakeupSchedulerCell is
        // process-global, so an un-denied ScheduleWakeup would let a default
        // subagent schedule a real wakeup. (The companion advisory `nke`/NKE_BASE
        // in runner.rs is only a message; the real removal happens HERE.)
        let mut names = vec![
            "TaskOutput",
            "ExitPlanMode",
            "EnterPlanMode",
            "AskUserQuestion",
            "ConnectGitHub",
            "WaitForMcpServers",
            "ScheduleWakeup",
            // NOTE: the binary's `_qd` set excludes `TaskStop` AND `Agent`.
            // `TaskStop` is allowed to subagents (so it is NOT listed here), and
            // `Agent` is NOT flat-denied — it is depth-GATED in `resolve()` via
            // Claude's configured maximum spawn depth.
        ];
        // claude: `...(USER_TYPE !== 'ant' ? [WORKFLOW_TOOL_NAME] : [])` (`av`,
        // non-ant only) — ant subagents keep `Workflow`. `Agent` is depth-gated
        // (both ant + non-ant), NOT in this flat-deny set.
        if !is_ant {
            names.push("Workflow");
        }
        names
    }

    /// `true` when `USER_TYPE == "ant"` (claude `process.env.USER_TYPE ===
    /// 'ant'`, EXACT match — not the truthy allowlist). Matches the existing
    /// repo convention (`tools/task/src/task.rs`).
    fn is_user_ant() -> bool {
        std::env::var("USER_TYPE").is_ok_and(|v| v == "ant")
    }

    /// Compute the effective tool list for a subagent.
    ///
    /// * `agent_def` — the spawning agent's definition (drives the policy +
    ///   the per-definition `disallowed_tools` denylist).
    /// * `parent_tools` — tools the parent agent had access to.
    /// * `agent_mcp_tools` — tools surfaced by the agent's MCP servers; these
    ///   normally pass (claude returns `true` for `mcp__` names before any
    ///   disallowed check, `agentToolUtils.ts:82-85`), so they are appended
    ///   AFTER the always-disallowed/per-definition drops. Coordinator workers
    ///   additionally omit entries carrying `role:"comms"` and the generic
    ///   `MCP`/`McpAuth` routing tools.
    /// * `coordinator_mode` — when true, coordinator workers do not receive
    ///   coordinator-only (`role:"comms"`) MCP tools from either the shared
    ///   registry pool or the per-agent inline MCP pool.
    ///
    /// Pipeline (claude `resolveAgentTools` order-equivalent):
    /// 1. policy projection ([`AgentToolPolicy`]) — only removes tools;
    /// 2. always-disallowed drop (`Agent`/`TaskOutput`/… gated by `USER_TYPE`);
    /// 3. per-definition `disallowed_tools` subtraction (base-name match);
    /// 4. append per-agent MCP tools (coordinator workers omit `comms`);
    /// 5. Plan-mode safe-tool narrowing (LingXi-local last step).
    ///
    /// ## `use_exact_tools` full bypass (claude `runAgent.ts:500-502`)
    ///
    /// When the policy is [`AgentToolPolicy::All`] with `use_exact_tools == true`
    /// (the synthetic `FORK_AGENT`), claude SKIPS `resolveAgentTools` /
    /// `filterToolsForAgent` entirely — `resolvedTools = availableTools` — so the
    /// fork child keeps the parent's EXACT unfiltered tool pool. This is
    /// load-bearing: it (a) preserves the cache-identical API prefix and (b)
    /// keeps the `Agent` tool the recursion guard (`isInForkChild`) assumes is
    /// present. We therefore return `parent_tools` verbatim with NO
    /// always-disallowed strip, NO per-definition subtraction, and NO Plan-mode
    /// narrowing, except that coordinator workers still omit shared `comms`
    /// MCP tools and generic MCP routing/auth tools so the lead-only routing
    /// invariant holds. (Per-agent MCP tools are not appended on this path either:
    /// claude's fork passes `availableTools = toolUseContext.options.tools`
    /// untouched.)
    #[must_use]
    pub fn resolve(
        agent_def: &AgentDefinition,
        parent_tools: &[Arc<dyn Tool>],
        agent_mcp_tools: &[Arc<dyn Tool>],
        // The resolved subagent's own recursion depth (claude `agentContext.depth`
        // / `spawnDepth`): the main thread spawns depth-1 children. Gates the
        // `Agent` tool against the configured maximum spawn depth below.
        depth: u32,
        coordinator_mode: bool,
    ) -> Vec<Arc<dyn Tool>> {
        // claude `runAgent.ts:500-502`: `useExactTools ? availableTools : …`.
        // The fork child bypasses ALL filtering, keeping the parent's exact pool.
        if let AgentToolPolicy::All {
            use_exact_tools: true,
        } = &agent_def.tools
        {
            return if coordinator_mode {
                parent_tools
                    .iter()
                    .filter(|tool| coordinator_worker_tool_allowed(tool.as_ref()))
                    .cloned()
                    .collect()
            } else {
                parent_tools.to_vec()
            };
        }

        let mut tools = match &agent_def.tools {
            AgentToolPolicy::All { use_exact_tools: _ } => parent_tools.to_vec(),
            AgentToolPolicy::Explicit(names) => parent_tools
                .iter()
                .filter(|t| names.contains(&t.name().to_string()))
                .cloned()
                .collect(),
            AgentToolPolicy::Except(names) => parent_tools
                .iter()
                .filter(|t| !names.contains(&t.name().to_string()))
                .cloned()
                .collect(),
        };

        // Shared MCP tools are already in `parent_tools`, unlike inline MCP
        // tools which are appended below. Apply the coordinator worker gate to
        // the projected parent pool before the common deny/depth passes.
        if coordinator_mode {
            tools.retain(|tool| coordinator_worker_tool_allowed(tool.as_ref()));
        }

        // (1b) Auto-memory tool injection (claude `isAutoMemoryEnabled` →
        // Write/Edit/Read). When a subagent declares a `memory:` scope
        // (`user`/`project`/`local`), claude treats auto-memory as enabled for
        // that agent and guarantees the memory read/write tools are present so
        // the agent can actually read from and write to its scoped auto-memory
        // store — regardless of the agent's `tools:` policy. The scope selects
        // only WHERE memory lives, not WHICH tools are injected, so all three
        // scopes inject the same `Read`/`Write`/`Edit` set. Injection happens
        // right after policy projection so the injected tools remain subject to
        // every downstream filter: an explicit `disallowedTools: [Write]` still
        // wins (step 3), and Plan-mode safe-tool narrowing (step 5) still strips
        // `Write`/`Edit` (keeping `Read`). The tools are pulled from the parent
        // pool (the memory agent's parent always exposes them); if the parent
        // pool lacks one, that tool is simply not injected.
        // (review #13) Match claude's full condition `fm() && n.memory &&
        // o!==void 0`, not just `n.memory`: (a) honor the global auto-memory
        // killswitch via `auto_memory_enabled()`, so `CLAUDE_CODE_DISABLE_AUTO_MEMORY`
        // / `CLAUDE_CODE_SIMPLE` suppress injection as claude does — otherwise a
        // restricted agent would be granted Read/Write/Edit the user's killswitch
        // meant to withhold; and (b) inject ONLY for an EXPLICIT tools list
        // (`o!==void 0`). For `All` the tools are already present (no-op); for
        // `Except` claude does NOT inject, so an `Except`-excluded memory tool
        // must stay excluded.
        if agent_def.memory.is_some()
            && auto_memory_enabled()
            && matches!(agent_def.tools, AgentToolPolicy::Explicit(_))
        {
            for want in ["Read", "Write", "Edit"] {
                if !tools.iter().any(|t| t.name() == want) {
                    if let Some(injected) = parent_tools.iter().find(|t| t.name() == want) {
                        tools.push(injected.clone());
                    }
                }
            }
        }

        // (2) Always-disallowed default drop (claude filterToolsForAgent →
        // ALL_AGENT_DISALLOWED_TOOLS.has()). Runs for ALL policies because
        // claude's filterToolsForAgent runs on `availableTools` regardless of
        // the agent's `tools` policy. Applied BEFORE the MCP extend so
        // `mcp__*` tools are never touched (they are appended after).
        let disallowed = Self::all_agent_disallowed_tools(Self::is_user_ant());
        tools.retain(|t| !disallowed.contains(&t.name()));

        // (2b) Agent recursion depth-gate — introduced in Claude 2.1.217 and
        // raised to a default maximum depth of 3 in 2.1.219.
        // `if(isAgentTool(a)) return depth < getMaxSubagentSpawnDepth()`.
        // The default is 3 (depths 0-2 may spawn; a depth-3 child may not),
        // with `CLAUDE_CODE_MAX_SUBAGENT_SPAWN_DEPTH` as the override.
        // Applies to ALL subagents (ant + non-ant). The `use_exact_tools` fork
        // bypass (returned above) is exempt — fork recursion is governed by the
        // `is_in_fork_child` message guard in `AgentTool`.
        if depth >= lingxi_core::host::subagent_spawn::max_subagent_spawn_depth() {
            tools.retain(|t| t.name() != "Agent");
        }

        // (3) Per-definition `disallowedTools` subtraction (claude
        // resolveAgentTools disallowedToolSet, agentToolUtils.ts:149-160). Each
        // spec's trailing `(rule content)` is stripped to its base tool name
        // (claude `permissionRuleValueFromString` → `toolName`); the bare-name
        // case is what custom agents use in practice.
        if !agent_def.disallowed_tools.is_empty() {
            let def_disallowed: HashSet<&str> = agent_def
                .disallowed_tools
                .iter()
                .map(|spec| tool_name_from_spec(spec))
                .collect();
            tools.retain(|t| !def_disallowed.contains(t.name()));
        }

        // (4) Per-agent MCP tools always pass (claude returns true for
        // `mcp__*` before any disallowed check) — append after the drops.
        // Coordinator workers still apply the same comms/generic dispatcher
        // gate to this pool, preventing an inline MCP definition from
        // reintroducing a bypass after the shared pool was filtered.
        if coordinator_mode {
            tools.extend(
                agent_mcp_tools
                    .iter()
                    .filter(|tool| coordinator_worker_tool_allowed(tool.as_ref()))
                    .cloned(),
            );
        } else {
            tools.extend(agent_mcp_tools.iter().cloned());
        }

        // (5) Plan-mode narrowing (LingXi-local last step; only further
        // narrows, so leaving it last is byte-safe). Reuse permission's
        // canonical safe-tool set (which includes teammate communication/task
        // metadata) while preserving this resolver's existing read-only web
        // tools, which are not part of the auto-mode classifier allowlist.
        if agent_def.permission_mode == AgentPermissionMode::Plan {
            tools.retain(|tool| {
                permission::is_plan_safe_tool(tool.name())
                    || matches!(tool.name(), "WebSearch" | "WebFetch")
            });
        }
        tools
    }
}

/// Extract the base tool name from a `disallowedTools` spec, stripping any
/// trailing `(rule content)` — mirrors claude `permissionRuleValueFromString`'s
/// `toolName` extraction (e.g. `"Bash(rm -rf)"` → `"Bash"`). The bare-name
/// case (`"Bash"`) is returned unchanged (trimmed).
fn tool_name_from_spec(spec: &str) -> &str {
    spec.split('(').next().unwrap_or(spec).trim()
}

/// Mirror of claude `fm()` / `isAutoMemoryEnabled` for the auto-memory tool
/// injection gate (review #13). Auto-memory is DISABLED — and so the Read/Write/
/// Edit injection is suppressed — when the killswitch env is set:
/// `CLAUDE_CODE_DISABLE_AUTO_MEMORY` / `LINGXI_DISABLE_AUTO_MEMORY` truthy, or
/// `CLAUDE_CODE_SIMPLE` / `LINGXI_SIMPLE` set. The remaining `fm()` arms
/// (settings-level `autoMemoryEnabled:false`, non-interactive `Rl()`, and the
/// remote-without-memdir case) are not yet threaded into the resolver — a
/// documented follow-up; the env killswitch is the user-facing control CC
/// documents and is honored here.
fn auto_memory_enabled() -> bool {
    fn truthy(name: &str) -> bool {
        std::env::var(name).ok().is_some_and(|v| {
            let v = v.trim().to_ascii_lowercase();
            !matches!(v.as_str(), "" | "0" | "false" | "no" | "off")
        })
    }
    fn is_set(name: &str) -> bool {
        std::env::var(name)
            .ok()
            .is_some_and(|v| !v.trim().is_empty())
    }
    !(truthy("CLAUDE_CODE_DISABLE_AUTO_MEMORY")
        || truthy("LINGXI_DISABLE_AUTO_MEMORY")
        || is_set("CLAUDE_CODE_SIMPLE")
        || is_set("LINGXI_SIMPLE"))
}

/// Resolve a subagent spawn's advertised tool SCHEMAS + dispatch allow-list from
/// a live registry per `agent_def`'s [`AgentToolPolicy`]. Returns
/// `(tool_schemas, allowed_tool_names)`.
///
/// This is the single source of truth shared by the one-shot/persistent
/// [`crate::handle::PoolSubagentSpawner`] and the in-process teammate handler —
/// both must advertise the same `assembleToolPool`-equivalent pool (claude-code
/// `runAgent.ts`): [`AgentToolResolver::resolve`] over the registry's
/// `available_tools`, then the tool-wide deny filter
/// (`filterToolsByDenyRules`), then wire serialization keyed on the subagent's
/// resolved `model` (so a model-gated tool prompt tracks the child's model).
///
/// The allow-list includes each resolved tool's `aliases()` so the runner's
/// dispatch guard accepts the SAME surface the inherited `RegistryToolInvoker`
/// does (e.g. `AgentTool`'s legacy `"Task"`); the advertised schemas stay
/// canonical-name-only. An empty `tool_wide_deny` drops nothing.
pub async fn resolve_subagent_tools(
    registry: &tool_api::ToolRegistry,
    agent_def: &AgentDefinition,
    tool_wide_deny: &[String],
    default_model: Option<&str>,
    // The resolved subagent's own recursion depth — gates its `Agent` tool
    // against Claude's configured maximum. Threaded from the spawn request.
    depth: u32,
    // Whether this spawn is a coordinator worker. Coordinator workers hide
    // `role:"comms"` MCP tools while ordinary sessions retain them.
    coordinator_mode: bool,
    // §24b — this spawn's already-connected per-agent MCP tools (claude
    // `Agr`'s `Fe`), passed straight through to
    // [`AgentToolResolver::resolve`]'s `agent_mcp_tools` parameter. Empty for
    // every caller that has none (byte-identical legacy).
    agent_mcp_tools: &[Arc<dyn Tool>],
) -> Result<(Vec<serde_json::Value>, Vec<String>), ToolResolutionError> {
    use tool_api::tool_trait::{PromptOptions, ToolStaticContext};

    let parent_tools = registry.available_tools(&ToolStaticContext::default());
    if let AgentToolPolicy::Explicit(names) = &agent_def.tools {
        // Validate names against the registered, policy-visible catalog. A
        // registered tool may be temporarily disabled (for example LSP when
        // its transport is unavailable). Availability still controls schemas
        // and dispatch below; it must not make a valid definition unknown.
        let unknown: Vec<String> = names
            .iter()
            .filter(|name| registry.find_by_name(name).is_none())
            .cloned()
            .collect();
        if !unknown.is_empty() {
            return Err(ToolResolutionError::UnknownExplicitTools(
                unknown.join(", "),
            ));
        }
    }
    let mut resolved = AgentToolResolver::resolve(
        agent_def,
        &parent_tools,
        agent_mcp_tools,
        depth,
        coordinator_mode,
    );
    if !tool_wide_deny.is_empty() {
        resolved.retain(|t| {
            !tool_wide_deny
                .iter()
                .any(|d| permission::tool_wide_name_matches(d, t.name()))
        });
    }
    if let AgentToolPolicy::Explicit(names) = &agent_def.tools {
        if !names.is_empty() && resolved.is_empty() {
            return Err(ToolResolutionError::EmptyExplicitToolSet(names.join(", ")));
        }
    }
    let allowed: Vec<String> = resolved
        .iter()
        .flat_map(|t| {
            std::iter::once(t.name().to_string())
                .chain(t.aliases().iter().map(|a| (*a).to_string()))
        })
        .collect();
    let model = match &agent_def.model {
        AgentModel::Explicit(id) | AgentModel::Alias(id) => Some(id.clone()),
        AgentModel::Inherit => default_model.map(str::to_string),
    };
    let schemas = tool_api::wire::tools_to_wire(
        &resolved,
        &PromptOptions {
            include_examples: true,
            model,
            model_profile: None,
        },
    )
    .await;
    Ok((schemas, allowed))
}

/// Add the tool capabilities every in-process teammate receives regardless of
/// a custom agent definition's tool policy.
///
/// Claude Code 2.1.241 constructs teammate definitions as custom tools plus
/// `SendMessage`, and (when the root surface exposes the complete task-list
/// suite) `TaskCreate`/`TaskGet`/`TaskUpdate`/`TaskList`. The returned boolean is
/// the oracle's `hasTaskListTools` gate used by auto-claim.
pub fn augment_teammate_tool_policy(
    registry: &tool_api::ToolRegistry,
    definition: &mut AgentDefinition,
) -> bool {
    use tool_api::tool_trait::ToolStaticContext;

    const SEND_MESSAGE: &str = "SendMessage";
    const TASK_TOOLS: [&str; 4] = ["TaskCreate", "TaskGet", "TaskUpdate", "TaskList"];
    let available: HashSet<String> = registry
        .available_tools(&ToolStaticContext::default())
        .iter()
        .map(|tool| tool.name().to_string())
        .collect();
    let has_task_list_tools = TASK_TOOLS.iter().all(|name| available.contains(*name));
    let mandatory = std::iter::once(SEND_MESSAGE)
        .filter(|name| available.contains(*name))
        .chain(TASK_TOOLS.into_iter().filter(|_| has_task_list_tools))
        .collect::<Vec<_>>();

    match &mut definition.tools {
        AgentToolPolicy::Explicit(names) => {
            for name in mandatory {
                if !names.iter().any(|existing| existing == name) {
                    names.push(name.to_string());
                }
            }
        }
        AgentToolPolicy::Except(excluded) => {
            excluded.retain(|name| !mandatory.contains(&name.as_str()));
        }
        AgentToolPolicy::All { .. } => {}
    }
    has_task_list_tools
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::{AgentModel, AgentSource};
    use async_trait::async_trait;
    use serde_json::Value;
    use tool_api::context::ToolUseContext;
    use tool_api::progress::ToolProgressSender;
    use tool_api::tool_trait::{
        DescriptionOptions, PromptOptions, ToolCallResult, ToolError, ToolStaticContext,
    };

    /// Minimal stub tool exposing only `name()` + `aliases()` (the surface the
    /// resolver inspects).
    struct StubTool {
        name: &'static str,
        role: Option<&'static str>,
        enabled: bool,
    }

    #[async_trait]
    impl Tool for StubTool {
        fn name(&self) -> &str {
            self.name
        }
        fn input_schema(&self) -> &Value {
            static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
            SCHEMA.get_or_init(|| serde_json::json!({"type": "object"}))
        }
        fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
            self.enabled
        }
        fn mcp_role(&self) -> Option<&str> {
            self.role
        }
        fn max_result_size_chars(&self) -> usize {
            1024
        }
        fn is_concurrency_safe(&self, _input: &Value) -> bool {
            true
        }
        fn is_read_only(&self, _input: &Value) -> bool {
            true
        }
        async fn check_permissions(
            &self,
            _input: &Value,
            _ctx: &ToolUseContext,
        ) -> permission::PermissionResult {
            permission::PermissionResult::Allow {
                reason: permission::PermissionDecisionReason::Other {
                    reason: "test".into(),
                },
                updated_input: None,
                update_destination: None,
                metadata: permission::result::PermissionMetadata::default(),
            }
        }
        async fn description(&self, _input: &Value, _opts: &DescriptionOptions) -> String {
            self.name.into()
        }
        async fn prompt(&self, _opts: &PromptOptions) -> String {
            self.name.into()
        }
        async fn call(
            &self,
            _input: Value,
            _ctx: ToolUseContext,
            _tx: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            unreachable!("not invoked in this test")
        }
    }

    fn tool(name: &'static str) -> Arc<dyn Tool> {
        Arc::new(StubTool {
            name,
            role: None,
            enabled: true,
        })
    }

    fn comms_tool(name: &'static str) -> Arc<dyn Tool> {
        Arc::new(StubTool {
            name,
            role: Some("comms"),
            enabled: true,
        })
    }

    #[tokio::test]
    async fn registered_disabled_tool_does_not_prevent_other_explicit_tools() {
        let mut registry = tool_api::ToolRegistry::new();
        registry.register_builtin(tool("Read"));
        registry.register_builtin(Arc::new(StubTool {
            name: "LSP",
            role: None,
            enabled: false,
        }));
        let definition = agent_def(AgentToolPolicy::Explicit(vec!["Read".into(), "LSP".into()]));
        let (schemas, allowed) =
            resolve_subagent_tools(&registry, &definition, &[], None, 0, false, &[])
                .await
                .expect("disabled LSP must not block the builder");
        assert_eq!(allowed, vec!["Read"]);
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0]["name"], "Read");
        let only_disabled = agent_def(AgentToolPolicy::Explicit(vec!["LSP".into()]));
        assert!(matches!(
            resolve_subagent_tools(&registry, &only_disabled, &[], None, 0, false, &[],).await,
            Err(ToolResolutionError::EmptyExplicitToolSet(_))
        ));
    }

    #[tokio::test]
    async fn explicit_tool_catalog_validation_preserves_policy_filters() {
        let mut registry = tool_api::ToolRegistry::new();
        registry.register_builtin(tool("Read"));
        registry.register_builtin(tool("LSP"));
        let definition = agent_def(AgentToolPolicy::Explicit(vec!["Read".into(), "LSP".into()]));
        let (_, allowed) = resolve_subagent_tools(&registry, &definition, &[], None, 0, false, &[])
            .await
            .unwrap();
        assert_eq!(allowed, vec!["LSP", "Read"]);
        let (_, allowed) =
            resolve_subagent_tools(&registry, &definition, &["LSP".into()], None, 0, false, &[])
                .await
                .unwrap();
        assert_eq!(allowed, vec!["Read"]);
        registry.set_session_tool_allowlist(&["Read".into()]);
        assert!(matches!(
            resolve_subagent_tools(&registry, &definition, &[], None, 0, false, &[],).await,
            Err(ToolResolutionError::UnknownExplicitTools(_))
        ));
    }

    fn pool(names: &[&'static str]) -> Vec<Arc<dyn Tool>> {
        names.iter().map(|n| tool(n)).collect()
    }

    fn names(tools: &[Arc<dyn Tool>]) -> Vec<String> {
        tools.iter().map(|t| t.name().to_string()).collect()
    }

    /// Build an `AgentDefinition` with the given tool policy + spawn-path
    /// defaults.
    fn agent_def(tools: AgentToolPolicy) -> AgentDefinition {
        AgentDefinition {
            cache_ttl: None,
            agent_type: "test".into(),
            when_to_use: String::new(),
            tools,
            max_turns: 1,
            model: AgentModel::Inherit,
            permission_mode: AgentPermissionMode::Bubble,
            source: AgentSource::BuiltIn,
            base_dir: "/tmp".into(),
            system_prompt: None,
            mcp_servers: vec![],
            frontmatter_hooks: vec![],
            icon: None,
            allowed_tools: vec![],
            worktree_requirement: None,
            disallowed_tools: vec![],
            skills: vec![],
            required_mcp_servers: vec![],
            background: false,
            isolation: None,
            memory: None,
            effort: None,
            initial_prompt: None,
            color: None,
            observer: None,
        }
    }

    fn all_policy() -> AgentToolPolicy {
        AgentToolPolicy::All {
            use_exact_tools: false,
        }
    }

    #[test]
    fn teammate_policy_adds_send_message_and_complete_task_suite() {
        let mut registry = tool_api::ToolRegistry::new();
        for name in [
            "Read",
            "SendMessage",
            "TaskCreate",
            "TaskGet",
            "TaskUpdate",
            "TaskList",
        ] {
            registry.register_builtin(tool(name));
        }
        let mut definition = agent_def(AgentToolPolicy::Explicit(vec!["Read".into()]));

        assert!(augment_teammate_tool_policy(&registry, &mut definition));
        let AgentToolPolicy::Explicit(names) = definition.tools else {
            panic!("expected explicit teammate tool policy");
        };
        assert_eq!(
            names,
            vec![
                "Read".to_string(),
                "SendMessage".to_string(),
                "TaskCreate".to_string(),
                "TaskGet".to_string(),
                "TaskUpdate".to_string(),
                "TaskList".to_string(),
            ]
        );
    }

    #[test]
    fn teammate_policy_does_not_advertise_partial_task_suite() {
        let mut registry = tool_api::ToolRegistry::new();
        for name in ["Read", "SendMessage", "TaskList"] {
            registry.register_builtin(tool(name));
        }
        let mut definition = agent_def(AgentToolPolicy::Explicit(vec!["Read".into()]));

        assert!(!augment_teammate_tool_policy(&registry, &mut definition));
        let AgentToolPolicy::Explicit(names) = definition.tools else {
            panic!("expected explicit teammate tool policy");
        };
        assert_eq!(names, vec!["Read", "SendMessage"]);
    }

    #[test]
    fn teammate_policy_removes_mandatory_tools_from_except_denylist() {
        let mut registry = tool_api::ToolRegistry::new();
        for name in [
            "SendMessage",
            "TaskCreate",
            "TaskGet",
            "TaskUpdate",
            "TaskList",
        ] {
            registry.register_builtin(tool(name));
        }
        let mut definition = agent_def(AgentToolPolicy::Except(vec![
            "Bash".into(),
            "SendMessage".into(),
            "TaskUpdate".into(),
        ]));

        assert!(augment_teammate_tool_policy(&registry, &mut definition));
        let AgentToolPolicy::Except(excluded) = definition.tools else {
            panic!("expected except teammate tool policy");
        };
        assert_eq!(excluded, vec!["Bash"]);
    }

    // ── pure core: all_agent_disallowed_tools(is_ant) ──
    // Tested directly (no env) to avoid the process-global USER_TYPE race.

    #[test]
    fn core_non_ant_set_excludes_agent_and_task_stop() {
        // `Agent` is depth-gated (not flat-denied) and `TaskStop` is allowed to
        // subagents — neither is in the disallowed set (claude `_qd`). The
        // plan-mode / agent-management tools + non-ant `Workflow` ARE.
        let set = AgentToolResolver::all_agent_disallowed_tools(false);
        assert!(
            !set.contains(&"Agent"),
            "Agent is depth-gated, not flat-denied"
        );
        assert!(
            !set.contains(&"TaskStop"),
            "TaskStop is allowed to subagents"
        );
        assert!(set.contains(&"TaskOutput"));
        assert!(set.contains(&"ExitPlanMode"));
        assert!(set.contains(&"EnterPlanMode"));
        assert!(set.contains(&"AskUserQuestion"));
        // non-ant: Workflow is dropped from subagent pools (binary HDd ant-gate).
        assert!(set.contains(&"Workflow"));
    }

    /// The binary's `_qd`/`nHe` set (cc_all.txt:16041079) flatly denies
    /// `ScheduleWakeup`, `ConnectGitHub`, and `WaitForMcpServers` to every
    /// subagent. Denying `ScheduleWakeup` is load-bearing: the process-global
    /// `WakeupSchedulerCell` means an un-denied call would schedule a real wakeup.
    #[test]
    fn core_denies_schedule_wakeup_and_friends() {
        for is_ant in [false, true] {
            let set = AgentToolResolver::all_agent_disallowed_tools(is_ant);
            assert!(
                set.contains(&"ScheduleWakeup"),
                "ScheduleWakeup must be denied to subagents (is_ant={is_ant})"
            );
            assert!(set.contains(&"ConnectGitHub"));
            assert!(set.contains(&"WaitForMcpServers"));
        }
    }

    #[test]
    fn core_ant_omits_agent_and_workflow_keeps_rest() {
        let set = AgentToolResolver::all_agent_disallowed_tools(true);
        assert!(
            !set.contains(&"Agent"),
            "Agent is depth-gated, never flat-denied"
        );
        // ant: Workflow is kept (allowed for ant subagents), like Agent.
        assert!(!set.contains(&"Workflow"));
        assert!(
            !set.contains(&"TaskStop"),
            "TaskStop is allowed to subagents"
        );
        assert!(set.contains(&"TaskOutput"));
    }

    // ── resolve(): always-disallowed drop ──
    // NOTE: these rely on the default env (USER_TYPE unset). They must NOT run
    // concurrently with a test that sets USER_TYPE=ant in the same process; we
    // therefore test the ant branch via the pure core above, never via env.

    #[test]
    fn all_policy_keeps_agent_at_depth_0() {
        // `Agent` is no longer flat-denied: a depth-0 caller keeps it.
        let parent = pool(&["Read", "Bash", "Agent"]);
        let resolved = AgentToolResolver::resolve(&agent_def(all_policy()), &parent, &[], 0, false);
        let got = names(&resolved);
        assert!(
            got.contains(&"Agent".to_string()),
            "Agent kept at depth 0 (< default 1)"
        );
        assert!(got.contains(&"Read".to_string()));
        assert!(got.contains(&"Bash".to_string()));
    }

    #[test]
    fn all_policy_strips_workflow_for_non_ant() {
        // The registered Workflow tool must not leak into subagent pools (binary
        // HDd: Workflow disallowed for USER_TYPE !== "ant").
        let parent = pool(&["Read", "Workflow", "Bash"]);
        let resolved = AgentToolResolver::resolve(&agent_def(all_policy()), &parent, &[], 0, false);
        let got = names(&resolved);
        assert!(
            !got.contains(&"Workflow".to_string()),
            "Workflow must be stripped from non-ant subagents"
        );
        assert!(got.contains(&"Read".to_string()));
        assert!(got.contains(&"Bash".to_string()));
    }

    #[test]
    fn all_policy_strips_other_disallowed() {
        let parent = pool(&[
            "Read",
            "ExitPlanMode",
            "EnterPlanMode",
            "AskUserQuestion",
            "TaskStop",
            "TaskOutput",
        ]);
        let resolved = AgentToolResolver::resolve(&agent_def(all_policy()), &parent, &[], 0, false);
        // The plan-mode / agent-management tools are stripped; `TaskStop` is NOT
        // in the disallowed set (allowed to subagents), so it survives.
        assert_eq!(
            names(&resolved),
            vec!["Read".to_string(), "TaskStop".to_string()]
        );
    }

    #[test]
    fn mcp_tools_always_survive() {
        // An mcp__ tool is appended AFTER the drop and is never filtered. The
        // parent-pool Agent tool is kept at depth 0 (depth-gated, not flat-denied).
        let parent = pool(&["Read", "Agent"]);
        let mcp = pool(&["mcp__x__y"]);
        let resolved =
            AgentToolResolver::resolve(&agent_def(all_policy()), &parent, &mcp, 0, false);
        let got = names(&resolved);
        assert!(got.contains(&"mcp__x__y".to_string()));
        assert!(got.contains(&"Agent".to_string()), "Agent kept at depth 0");
        assert!(got.contains(&"Read".to_string()));
    }

    #[test]
    fn coordinator_worker_filters_only_comms_mcp_tools() {
        let parent = vec![
            tool("Read"),
            tool("MCP"),
            tool("McpAuth"),
            tool("ListMcpResourcesTool"),
            tool("ReadMcpResourceTool"),
            tool("ReadMcpResourceDirTool"),
            comms_tool("mcp__comms__send"),
        ];
        let mcp = vec![tool("mcp__ordinary__run"), comms_tool("mcp__comms__send")];
        let def = agent_def(all_policy());
        let worker = AgentToolResolver::resolve(&def, &parent, &mcp, 0, true);
        assert!(names(&worker).contains(&"mcp__ordinary__run".to_string()));
        assert!(!names(&worker).contains(&"mcp__comms__send".to_string()));
        for denied in ["MCP", "McpAuth"] {
            assert!(!names(&worker).contains(&denied.to_string()));
        }
        for retained in [
            "Read",
            "ListMcpResourcesTool",
            "ReadMcpResourceTool",
            "ReadMcpResourceDirTool",
        ] {
            assert!(names(&worker).contains(&retained.to_string()));
        }
        let ordinary = AgentToolResolver::resolve(&def, &parent, &mcp, 0, false);
        assert!(names(&ordinary).contains(&"mcp__comms__send".to_string()));
        assert!(names(&ordinary).contains(&"MCP".to_string()));
        assert!(names(&ordinary).contains(&"McpAuth".to_string()));
    }

    #[test]
    fn explicit_policy_keeps_agent_when_below_depth() {
        // An agent that explicitly lists `Agent` keeps it below the depth cap
        // (no longer flat-denied). At the default cap of 3 the gate drops it.
        let parent = pool(&["Read", "Agent"]);
        let def = agent_def(AgentToolPolicy::Explicit(vec![
            "Read".to_string(),
            "Agent".to_string(),
        ]));
        let kept = AgentToolResolver::resolve(&def, &parent, &[], 0, false);
        assert_eq!(names(&kept), vec!["Read".to_string(), "Agent".to_string()]);
        let gated = AgentToolResolver::resolve(&def, &parent, &[], 3, false);
        assert_eq!(
            names(&gated),
            vec!["Read".to_string()],
            "Agent gated at depth 3"
        );
    }

    // ── resolve(): per-definition disallowed_tools subtraction ──

    #[test]
    fn disallowed_tools_subtracts() {
        let parent = pool(&["Read", "Bash"]);
        let mut def = agent_def(all_policy());
        def.disallowed_tools = vec!["Bash".to_string()];
        let resolved = AgentToolResolver::resolve(&def, &parent, &[], 0, false);
        assert_eq!(names(&resolved), vec!["Read".to_string()]);
    }

    #[test]
    fn disallowed_tools_strips_rule_content() {
        // claude permissionRuleValueFromString strips the `(rule)` pattern; we
        // match on the base tool name.
        let parent = pool(&["Read", "Bash"]);
        let mut def = agent_def(all_policy());
        def.disallowed_tools = vec!["Bash(rm -rf)".to_string()];
        let resolved = AgentToolResolver::resolve(&def, &parent, &[], 0, false);
        assert_eq!(names(&resolved), vec!["Read".to_string()]);
    }

    // ── resolve(): Plan-mode safe-tool narrowing still applies last ──

    #[test]
    fn plan_mode_readonly_narrowing_applies_last() {
        let parent = pool(&["Read", "Bash", "Grep", "Agent"]);
        let def = AgentDefinition {
            permission_mode: AgentPermissionMode::Plan,
            ..agent_def(all_policy())
        };
        let resolved = AgentToolResolver::resolve(&def, &parent, &[], 0, false);
        let got = names(&resolved);
        // Agent dropped by the always-disallowed set; Bash dropped by the
        // Plan-mode safe-tool narrowing; only Read+Grep survive from this pool.
        assert!(!got.contains(&"Agent".to_string()));
        assert!(!got.contains(&"Bash".to_string()));
        assert!(got.contains(&"Read".to_string()));
        assert!(got.contains(&"Grep".to_string()));
    }

    #[test]
    fn plan_mode_keeps_teammate_communication_and_task_metadata_tools() {
        let parent = pool(&[
            "Read",
            "Bash",
            "SendMessage",
            "TaskCreate",
            "TaskGet",
            "TaskUpdate",
            "TaskList",
        ]);
        let def = AgentDefinition {
            permission_mode: AgentPermissionMode::Plan,
            ..agent_def(all_policy())
        };

        let resolved = AgentToolResolver::resolve(&def, &parent, &[], 0, false);

        assert_eq!(
            names(&resolved),
            vec![
                "Read",
                "SendMessage",
                "TaskCreate",
                "TaskGet",
                "TaskUpdate",
                "TaskList",
            ]
        );
    }

    #[test]
    fn tool_name_from_spec_extracts_base_name() {
        assert_eq!(tool_name_from_spec("Bash"), "Bash");
        assert_eq!(tool_name_from_spec("Bash(rm -rf)"), "Bash");
        assert_eq!(tool_name_from_spec("Read (foo)"), "Read");
    }

    // ── resolve(): use_exact_tools FULL bypass (claude runAgent.ts:500-502) ──
    // The fork child keeps the parent's EXACT unfiltered pool: no
    // always-disallowed strip, no per-definition subtraction, no Plan-mode
    // narrowing, no MCP append.

    fn exact_policy() -> AgentToolPolicy {
        AgentToolPolicy::All {
            use_exact_tools: true,
        }
    }

    #[test]
    fn use_exact_tools_keeps_agent_and_full_pool() {
        // With useExactTools the Agent / TaskOutput / etc. that resolve() would
        // otherwise strip are KEPT — the recursion guard relies on Agent being
        // present, and the cache prefix relies on the pool being identical.
        let parent = pool(&[
            "Read",
            "Bash",
            "Agent",
            "TaskOutput",
            "ExitPlanMode",
            "EnterPlanMode",
            "AskUserQuestion",
            "TaskStop",
        ]);
        let resolved =
            AgentToolResolver::resolve(&agent_def(exact_policy()), &parent, &[], 0, false);
        // Child pool == parent pool, byte-for-byte (same order, same set).
        assert_eq!(names(&resolved), names(&parent));
    }

    #[test]
    fn use_exact_tools_ignores_per_definition_disallowed() {
        // Even a per-definition disallowedTools entry is bypassed on the exact
        // path (claude skips resolveAgentTools entirely).
        let parent = pool(&["Read", "Bash", "Agent"]);
        let mut def = agent_def(exact_policy());
        def.disallowed_tools = vec!["Bash".to_string()];
        let resolved = AgentToolResolver::resolve(&def, &parent, &[], 0, false);
        assert_eq!(names(&resolved), names(&parent));
    }

    #[test]
    fn use_exact_tools_ignores_plan_mode_narrowing_and_mcp() {
        // Plan-mode narrowing and the MCP append are also bypassed: the child
        // pool is the parent pool verbatim regardless of permission mode, and
        // the fork path passes availableTools untouched (no MCP extend).
        let parent = pool(&["Read", "Bash", "Agent"]);
        let mcp = pool(&["mcp__x__y"]);
        let def = AgentDefinition {
            permission_mode: AgentPermissionMode::Plan,
            ..agent_def(exact_policy())
        };
        let resolved = AgentToolResolver::resolve(&def, &parent, &mcp, 0, false);
        assert_eq!(names(&resolved), names(&parent));
        assert!(!names(&resolved).contains(&"mcp__x__y".to_string()));
    }

    #[test]
    fn use_exact_tools_coordinator_gate_blocks_generic_dispatcher_and_auth() {
        let parent = vec![
            tool("Read"),
            tool("MCP"),
            tool("McpAuth"),
            tool("ListMcpResourcesTool"),
            tool("ReadMcpResourceTool"),
            tool("ReadMcpResourceDirTool"),
            comms_tool("mcp__comms__send"),
        ];
        let def = agent_def(exact_policy());
        let worker = AgentToolResolver::resolve(&def, &parent, &[], 0, true);
        let worker_names = names(&worker);
        for denied in ["MCP", "McpAuth", "mcp__comms__send"] {
            assert!(!worker_names.contains(&denied.to_string()));
        }
        for retained in [
            "Read",
            "ListMcpResourcesTool",
            "ReadMcpResourceTool",
            "ReadMcpResourceDirTool",
        ] {
            assert!(worker_names.contains(&retained.to_string()));
        }

        let ordinary = AgentToolResolver::resolve(&def, &parent, &[], 0, false);
        assert_eq!(names(&ordinary), names(&parent));
    }

    #[test]
    fn use_exact_tools_false_still_filters() {
        // Sanity: the bypass is gated on use_exact_tools==true; the false branch
        // keeps the always-disallowed strip. `ScheduleWakeup` is flat-denied (so
        // still stripped); `Agent` is NOT flat-denied — it is depth-gated, so it
        // is KEPT here at depth 0 (0 < 5) and only dropped at depth >= 5.
        let parent = pool(&["Read", "Agent", "ScheduleWakeup"]);
        let resolved = AgentToolResolver::resolve(&agent_def(all_policy()), &parent, &[], 0, false);
        assert!(
            !names(&resolved).contains(&"ScheduleWakeup".to_string()),
            "flat-deny still applies on the non-exact branch"
        );
        assert!(
            names(&resolved).contains(&"Agent".to_string()),
            "Agent is kept at depth 0 (depth-gated, not flat-denied)"
        );
    }

    // ── Agent recursion depth-gate (Claude 2.1.219, default max depth 3) ──

    #[test]
    fn agent_tool_kept_for_top_level_caller() {
        // The top-level caller (depth 0) keeps `Agent` and may spawn a child.
        let parent = pool(&["Read", "Agent", "Bash"]);
        let resolved = AgentToolResolver::resolve(&agent_def(all_policy()), &parent, &[], 0, false);
        assert!(names(&resolved).contains(&"Agent".to_string()));
    }

    #[test]
    fn agent_tool_dropped_at_default_depth_3_and_beyond() {
        // Claude 2.1.219 defaults the maximum spawn depth to 3, so `Agent` is
        // advertised at depths 0-2 and dropped at 3 and deeper.
        let parent = pool(&["Read", "Agent", "Bash"]);
        for depth in [0u32, 1, 2] {
            let resolved =
                AgentToolResolver::resolve(&agent_def(all_policy()), &parent, &[], depth, false);
            assert!(
                names(&resolved).contains(&"Agent".to_string()),
                "Agent must survive at depth {depth} (< 3 by default)"
            );
        }
        for depth in [3u32, 5, 12] {
            let resolved =
                AgentToolResolver::resolve(&agent_def(all_policy()), &parent, &[], depth, false);
            assert!(
                !names(&resolved).contains(&"Agent".to_string()),
                "Agent must be dropped at depth {depth} (>= 3 by default)"
            );
            // Non-Agent tools are unaffected by the depth-gate.
            assert!(names(&resolved).contains(&"Read".to_string()));
        }
    }

    // ── Auto-memory tool injection (claude isAutoMemoryEnabled → Write/Edit/Read) ──

    /// Set an agent's memory scope on top of the spawn-path defaults.
    fn agent_def_with_memory(
        tools: AgentToolPolicy,
        memory: lingxi_core::types::WritableScope,
    ) -> AgentDefinition {
        AgentDefinition {
            memory: Some(memory),
            ..agent_def(tools)
        }
    }

    #[test]
    fn memory_injects_read_write_edit_into_explicit_pool() {
        // An explicit `tools: [Bash]` agent that declares `memory: project` still
        // gets Read/Write/Edit injected so it can read from and write to memory.
        let parent = pool(&["Read", "Write", "Edit", "Bash", "Grep"]);
        let def = agent_def_with_memory(
            AgentToolPolicy::Explicit(vec!["Bash".to_string()]),
            lingxi_core::types::WritableScope::Project,
        );
        let resolved = AgentToolResolver::resolve(&def, &parent, &[], 0, false);
        let got = names(&resolved);
        assert!(got.contains(&"Bash".to_string()));
        assert!(
            got.contains(&"Read".to_string()),
            "Read injected for memory"
        );
        assert!(
            got.contains(&"Write".to_string()),
            "Write injected for memory"
        );
        assert!(
            got.contains(&"Edit".to_string()),
            "Edit injected for memory"
        );
        assert!(
            !got.contains(&"Grep".to_string()),
            "Grep not part of memory set"
        );
    }

    #[test]
    fn memory_scope_does_not_inject_for_except_policy() {
        // (review #13) claude injects auto-memory tools only for an EXPLICIT
        // tools list (`o!==void 0`). An `Except` agent that excludes Write must
        // NOT have Write re-added by the memory scope — the exclusion wins.
        let parent = pool(&["Read", "Write", "Edit", "Bash"]);
        let def = agent_def_with_memory(
            AgentToolPolicy::Except(vec!["Write".to_string()]),
            lingxi_core::types::WritableScope::Project,
        );
        let resolved = AgentToolResolver::resolve(&def, &parent, &[], 0, false);
        let got = names(&resolved);
        assert!(
            !got.contains(&"Write".to_string()),
            "Except-excluded Write must not be re-injected by memory scope"
        );
        // The non-excluded parent tools remain.
        assert!(got.contains(&"Read".to_string()) && got.contains(&"Bash".to_string()));
    }

    #[test]
    fn no_memory_scope_does_not_inject() {
        // Without a `memory:` scope, an explicit-tools agent keeps exactly its
        // requested tools — nothing is injected.
        let parent = pool(&["Read", "Write", "Edit", "Bash"]);
        let def = agent_def(AgentToolPolicy::Explicit(vec!["Bash".to_string()]));
        let resolved = AgentToolResolver::resolve(&def, &parent, &[], 0, false);
        assert_eq!(names(&resolved), vec!["Bash".to_string()]);
    }

    #[test]
    fn memory_injection_no_duplicates_when_already_present() {
        // If the explicit pool already lists the memory tools, injection must not
        // duplicate them.
        let parent = pool(&["Read", "Write", "Edit"]);
        let def = agent_def_with_memory(
            AgentToolPolicy::Explicit(vec![
                "Read".to_string(),
                "Write".to_string(),
                "Edit".to_string(),
            ]),
            lingxi_core::types::WritableScope::User,
        );
        let resolved = AgentToolResolver::resolve(&def, &parent, &[], 0, false);
        assert_eq!(
            names(&resolved),
            vec!["Read".to_string(), "Write".to_string(), "Edit".to_string()]
        );
    }

    #[test]
    fn memory_injection_all_three_scopes_inject_same_set() {
        // The scope selects only WHERE memory lives, not WHICH tools — all three
        // scopes inject the identical Read/Write/Edit set.
        let parent = pool(&["Read", "Write", "Edit", "Bash"]);
        for scope in [
            lingxi_core::types::WritableScope::User,
            lingxi_core::types::WritableScope::Project,
            lingxi_core::types::WritableScope::Local,
        ] {
            let def =
                agent_def_with_memory(AgentToolPolicy::Explicit(vec!["Bash".to_string()]), scope);
            let got = names(&AgentToolResolver::resolve(&def, &parent, &[], 0, false));
            for want in ["Read", "Write", "Edit"] {
                assert!(
                    got.contains(&want.to_string()),
                    "{want} must be injected for scope {scope:?}"
                );
            }
        }
    }

    #[test]
    fn memory_injection_only_pulls_available_parent_tools() {
        // Injection is best-effort from the parent pool: a memory tool the parent
        // does not expose is simply not injected (no panic, no phantom tool).
        let parent = pool(&["Read", "Bash"]); // no Write/Edit in parent
        let def = agent_def_with_memory(
            AgentToolPolicy::Explicit(vec!["Bash".to_string()]),
            lingxi_core::types::WritableScope::Local,
        );
        let got = names(&AgentToolResolver::resolve(&def, &parent, &[], 0, false));
        assert!(got.contains(&"Read".to_string()));
        assert!(got.contains(&"Bash".to_string()));
        assert!(!got.contains(&"Write".to_string()));
        assert!(!got.contains(&"Edit".to_string()));
    }

    #[test]
    fn memory_injected_write_edit_respect_per_definition_disallow() {
        // An explicit `disallowedTools: [Write]` still wins over memory injection
        // (injection happens before the per-definition subtraction).
        let parent = pool(&["Read", "Write", "Edit", "Bash"]);
        let mut def = agent_def_with_memory(
            AgentToolPolicy::Explicit(vec!["Bash".to_string()]),
            lingxi_core::types::WritableScope::Project,
        );
        def.disallowed_tools = vec!["Write".to_string()];
        let got = names(&AgentToolResolver::resolve(&def, &parent, &[], 0, false));
        assert!(got.contains(&"Read".to_string()));
        assert!(got.contains(&"Edit".to_string()));
        assert!(
            !got.contains(&"Write".to_string()),
            "explicit disallow wins"
        );
    }

    #[test]
    fn memory_injected_tools_respect_plan_mode_narrowing() {
        // Plan-mode safe-tool narrowing still strips the injected Write/Edit while
        // keeping the read-only Read — memory writes do not bypass plan mode.
        let parent = pool(&["Read", "Write", "Edit", "Bash"]);
        let def = AgentDefinition {
            permission_mode: AgentPermissionMode::Plan,
            memory: Some(lingxi_core::types::WritableScope::Project),
            ..agent_def(AgentToolPolicy::Explicit(vec!["Bash".to_string()]))
        };
        let got = names(&AgentToolResolver::resolve(&def, &parent, &[], 0, false));
        assert!(got.contains(&"Read".to_string()), "read-only Read survives");
        assert!(
            !got.contains(&"Write".to_string()),
            "Write stripped in plan mode"
        );
        assert!(
            !got.contains(&"Edit".to_string()),
            "Edit stripped in plan mode"
        );
    }

    #[test]
    fn task_stop_allowed_to_subagents_at_all_depths() {
        // `TaskStop` is NOT in the disallowed set (claude `_qd` excludes it), so
        // it is available to subagents regardless of recursion depth.
        let parent = pool(&["Read", "TaskStop", "Agent"]);
        for depth in [0u32, 5, 9] {
            let resolved =
                AgentToolResolver::resolve(&agent_def(all_policy()), &parent, &[], depth, false);
            assert!(
                names(&resolved).contains(&"TaskStop".to_string()),
                "TaskStop must be available at depth {depth}"
            );
        }
    }
}
