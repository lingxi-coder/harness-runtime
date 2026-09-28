use super::cleanup::McpCleanupGuard;
use super::{
    workflow_transcript_subdir_override, PoolSubagentSpawner, APPEND_SUBAGENT_PROMPT_GATE_ENV,
    APPEND_SUBAGENT_PROMPT_VALUE_ENV, FUSION_RESERVED_AGENT_TYPE,
};
use crate::builtins::WORKFLOW_SUBAGENT_TYPE;
use crate::context::SubagentContext;
use crate::definition::{
    AgentDefinition, AgentModel, AgentPermissionMode, AgentSource, AgentToolPolicy,
};
use crate::display::{AgentColor, AgentDisplay};
use permission::PermissionMode;
use platform_api::subagent_spawn::{
    SubagentInheritance, SubagentListingEntry, SubagentSpawnError, SubagentSpawnRequest,
};
use protocol::{AgentId, ConversationMessage, MessageId};
use std::collections::HashMap;
use std::sync::Arc;

/// Unicode `Pd` (dash punctuation) — the exact set `tools/agent`'s
/// `is_pd_dash` (agent.rs) enumerates, mirrored here because the two crates
/// are siblings with no dependency edge between them. Keep the two lists
/// byte-identical: they define which spellings the reserved-name guard and
/// the Fusion intercept agree on.
pub(super) fn is_reserved_name_pd_dash(c: char) -> bool {
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

/// [Round-12 finding 6] Whether `agent_type` names the reserved Fusion
/// surface — under the SAME normalization `tools/agent`'s `call` intercept
/// applies (`normalize_agent_type`: lowercase, then strip every whitespace
/// char, `_`, and Unicode-Pd dash), not a byte-for-byte compare against the
/// literal.
///
/// The intercept fires for `Fusion`, `FUSION`, `fu-sion`, `fu_sion`,
/// `fusion-`, … so the reserved-name guards in this crate must cover exactly
/// that set: a literal-only compare let a disk agent named `Fusion` stay in
/// the Agent listing and stay resolvable here, while every dispatch of that
/// name was silently turned into a Fusion panel run — one name meaning two
/// different agents depending on the entry point.
///
/// Names that merely CONTAIN `fusion` are unaffected: `fusion-agent`
/// normalizes to `fusionagent`, `confusion` to `confusion`.
#[must_use]
pub fn normalizes_to_fusion(agent_type: &str) -> bool {
    agent_type
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|c| !(c.is_whitespace() || *c == '_' || is_reserved_name_pd_dash(*c)))
        .eq(FUSION_RESERVED_AGENT_TYPE.chars())
}

/// (CLI-15) The operator-supplied suffix appended to every Task-tool subagent's
/// system prompt, or `None` when the flag was not passed or its gate is off.
///
/// Oracle @292360822, inside the subagent query builder:
///
/// ```js
/// Xt=UWf(vt,C??!1,(d?.suppressScratchpad||d?.isolatedContext)??!1),
/// Zt=!C&&!d?.isolatedContext
///    &&Un(process.env.CLAUDE_CODE_ENABLE_APPEND_SUBAGENT_PROMPT)
///    &&r.options.appendSubagentSystemPrompt
///    ?Rm([...Xt,r.options.appendSubagentSystemPrompt]):Xt
/// ```
///
/// `Rm` is the identity brand (`function Rm(e){return e}`, @290379857), so the
/// text becomes one more SECTION at the end of the prompt array. LingXi renders
/// the subagent prompt as a single string, and its section separator is `\n\n`
/// (the same join the subagent `<env>` block already uses), so the splice site
/// appends `"\n\n" + suffix`.
///
/// `Un` is the env-truthiness predicate (`{1,true,yes,on}` after
/// lower-case + trim), NOT mere presence — `…=0` leaves the suffix off.
///
/// Two oracle guards have no LingXi analogue at this seam and are therefore not
/// replicated: `C` is the caller's `useExactTools` and `d?.isolatedContext` is
/// an isolated-context spawn override; neither concept exists in
/// `SubagentSpawnRequest`. Both suppress the append upstream, so LingXi's
/// version is strictly wider — recorded rather than guessed at.
#[must_use]
pub fn append_subagent_system_prompt_suffix() -> Option<String> {
    if !platform_api::env::is_env_truthy(
        std::env::var(APPEND_SUBAGENT_PROMPT_GATE_ENV)
            .ok()
            .as_deref(),
    ) {
        return None;
    }
    // `&&r.options.appendSubagentSystemPrompt` — an empty string is falsy in
    // JS, so it does not append either.
    std::env::var(APPEND_SUBAGENT_PROMPT_VALUE_ENV)
        .ok()
        .filter(|v| !v.is_empty())
}

/// Render an [`AgentDefinition`]'s tool policy into the human "tools
/// description" claude-code shows for an agent type (`AgentTool/prompt.ts:15-37`
/// `getToolsDescription`). The Rust [`AgentToolPolicy`] folds TS's `tools`
/// (allowlist) + `disallowedTools` (denylist) into one enum, so the mapping is:
/// - `All { .. }` → `"All tools"` (no restrictions)
/// - `Explicit(names)` → `names.join(", ")` (allowlist), `"None"` if empty
/// - `Except(names)` → `"All tools except {names.join(", ")}"` (denylist)
#[must_use]
pub fn tools_description(def: &AgentDefinition) -> String {
    match &def.tools {
        AgentToolPolicy::All { .. } => "All tools".to_string(),
        AgentToolPolicy::Explicit(names) => {
            if names.is_empty() {
                "None".to_string()
            } else {
                names.join(", ")
            }
        }
        AgentToolPolicy::Except(names) => {
            format!("All tools except {}", names.join(", "))
        }
    }
}

/// Merge a flat slice of [`AgentDefinition`]s into the deduplicated
/// [`SubagentListingEntry`] set the dynamic Agent listing renders — the single
/// source of truth shared by the inline tool-prompt path
/// (`PoolSubagentSpawner::listing_entries`) and the `agent_listing_delta`
/// attachment path (the orchestrator's per-turn reminder).
///
/// Precedence is claude-code's later-wins: when two definitions share an
/// `agent_type`, the LAST one in `defs` wins. Callers therefore pass built-ins
/// FIRST and the user/project catalog AFTER (built-in < user < project). Each
/// entry's model is left unresolved (the listing only needs type / when-to-use
/// / tools). Output is sorted by `agent_type` for deterministic bytes (agent
/// load order is nondeterministic — plugin load races, MCP async connect —
/// matching TS `getAgentListingDeltaAttachment`'s sort, attachments.ts:1543).
#[must_use]
pub fn agent_listing_entries(defs: &[AgentDefinition]) -> Vec<SubagentListingEntry> {
    let mut by_type: HashMap<String, &AgentDefinition> = HashMap::new();
    for def in defs {
        if def.agent_type == platform_api::FUSION_PANEL_TYPE {
            continue;
        }
        // `workflow-subagent` is NOT a catalog agent. The oracle declares it
        // (`bn`, src_173804794.js @34591) inside the workflow chunk and hands it
        // straight to the workflow runtime; `cre()` — the built-in roster the
        // listing is built from — never contains it, so no oracle session has
        // ever advertised it to the model. The port keeps it in
        // `builtin_agent_definitions` as the workflow path's resolution
        // registry, which put an extra
        // `- workflow-subagent: Internal subagent for workflow script
        // orchestration. (Tools: All tools except SendUserMessage, Agent,
        // Workflow)` line into BOTH model-facing catalogs (the inline Agent tool
        // prompt and the `agent_listing_delta` reminder) and made an internal
        // type selectable via `subagent_type`. Drop it here — the one place both
        // catalogs are built — rather than from the registry the workflow runtime
        // resolves against.
        if def.agent_type == WORKFLOW_SUBAGENT_TYPE {
            continue;
        }
        // [Finding 25] `fusion` is reserved for the Fusion Agent surface (see
        // `PoolSubagentSpawner::lookup_definition`'s matching 0c case):
        // advertising a disk agent under this name would promise a
        // definition that can never be dispatched, since `tools/agent`'s
        // `call` intercepts the name into the multi-model panel before any
        // catalog lookup runs. Drop it from the listing rather than show the
        // model an entry point that always resolves to something else.
        // [Round-12 finding 6] Same NORMALIZED predicate the intercept uses,
        // so `Fusion` / `fu-sion` / `fusion-` are dropped as well — a
        // literal-only compare advertised those with the user's own
        // `when_to_use` while every dispatch became a Fusion run.
        if normalizes_to_fusion(&def.agent_type) {
            tracing::warn!(
                agent_type = %def.agent_type,
                "dropping a disk agent named `fusion` from the Agent listing: \
                 the name is reserved for the Fusion Agent surface"
            );
            continue;
        }
        // Later-wins: a same-typed definition later in the slice overrides.
        by_type.insert(def.agent_type.clone(), def);
    }
    let mut entries: Vec<SubagentListingEntry> = by_type
        .into_values()
        .map(|def| SubagentListingEntry {
            tools_description: tools_description(def),
            agent_type: def.agent_type.clone(),
            when_to_use: def.when_to_use.clone(),
            // `whenToUseLean` rides along unresolved: which of the two texts a
            // line renders is `U2n`'s decision, taken per RENDER against the
            // model being rendered for, not per catalog build.
            when_to_use_lean: crate::builtins::when_to_use_lean(def).map(str::to_string),
        })
        .collect();
    entries.sort_by(|a, b| a.agent_type.cmp(&b.agent_type));
    entries
}

/// claude 2.1.238 `NJa` (@290291941) — filter a definition slice down to the
/// agent types that are UNAVAILABLE because every tool they may use is denied:
///
/// ```js
/// function NJa(e,t){return e.filter((r)=>{
///   if(r.source!=="built-in"||!r.tools||r.tools.length===0||att(r.tools)!==null)return!0;
///   return r.tools.some((n)=>{ if(n==="*")return!1;
///     let o=Lp(n).toolName; return!ak(t,{name:o})&&_Tv(o) })})}
/// ```
///
/// Guard-by-guard:
/// * `r.source!=="built-in"` — only BUILT-IN definitions are subject; a user /
///   project / plugin agent is never withheld for this reason.
/// * `!r.tools` — a TS definition with no `tools` (the port's
///   [`AgentToolPolicy::Except`], i.e. `disallowedTools`-only) is skipped.
/// * `r.tools.length===0` — an empty explicit list is skipped.
/// * `att(r.tools)!==null` (@290070773) — `att` returns non-null unless the list
///   contains `"*"`, so a wildcard list ([`AgentToolPolicy::All`]) is skipped.
/// * the surviving case is a non-empty, wildcard-free explicit allow-list; the
///   agent stays available iff SOME entry is both un-denied (`!ak(t,{name:o})`,
///   a deny rule matched against the bare tool NAME ⇒ the port's tool-wide deny
///   names, matched with [`permission::tool_wide_name_matches`] — the SAME
///   matcher [`crate::tool_resolver::resolve_subagent_tools`] uses to strip the
///   spawn's pool, so this predicate cannot disagree with the pool the agent
///   would actually get) and usable
///   (`_Tv(o) = o!==cm||Vs(wjr)` — `WebFetch` additionally needs the
///   `allow_web_fetch` entitlement,
///   [`crate::builtins::web_fetch_policy_allowed`]).
///
/// `Lp(n).toolName` strips a rule's content (`Bash(git:*)` → `Bash`), so an
/// allow-list entry written in rule form resolves to its tool name here too.
///
/// Definitions are de-duplicated later-wins first, matching
/// [`agent_listing_entries`], so a catalog entry that overrides a built-in is
/// judged (and, being non-built-in, exempted) in the built-in's place.
#[must_use]
pub fn tools_denied_agent_types(
    defs: &[AgentDefinition],
    tool_wide_deny: &[String],
) -> Vec<String> {
    let mut by_type: HashMap<String, &AgentDefinition> = HashMap::new();
    for def in defs {
        by_type.insert(def.agent_type.clone(), def);
    }
    let mut out: Vec<String> = by_type
        .into_values()
        .filter(|def| every_tool_denied(def, tool_wide_deny))
        .map(|def| def.agent_type.clone())
        .collect();
    out.sort();
    out
}

/// claude `mdr(e,t)` (@290291941) = `NJa([e],t).length===0` — the single-agent
/// arm of [`tools_denied_agent_types`].
pub(super) fn every_tool_denied(def: &AgentDefinition, tool_wide_deny: &[String]) -> bool {
    // `r.source!=="built-in"` ⇒ kept (never withheld).
    if !matches!(def.source, AgentSource::BuiltIn) {
        return false;
    }
    // `!r.tools` / `r.tools.length===0` / `att(r.tools)!==null` ⇒ kept.
    let AgentToolPolicy::Explicit(names) = &def.tools else {
        return false;
    };
    if names.is_empty() || names.iter().any(|n| n == "*") {
        return false;
    }
    // `!r.tools.some(...)` — no entry is both un-denied and usable.
    !names.iter().any(|name| {
        let tool = rule_tool_name(name);
        let denied = tool_wide_deny
            .iter()
            .any(|d| permission::tool_wide_name_matches(d, tool));
        // `_Tv(o) = o !== cm || Vs(wjr)`
        let usable = tool != crate::builtins::WEB_FETCH_TOOL_NAME
            || crate::builtins::web_fetch_policy_allowed();
        !denied && usable
    })
}

/// claude `Lp(n).toolName` — the bare tool name of a permission-rule-shaped
/// string (`Bash(git status:*)` → `Bash`); a plain name is returned unchanged.
pub(super) fn rule_tool_name(rule: &str) -> &str {
    match rule.find('(') {
        Some(i) => rule[..i].trim_end(),
        None => rule,
    }
}

/// Apply an `agent.spawn` hook's `modified_input` to a spawn request.
///
/// Pure so the rewrite rules are testable without standing up a spawner. Only
/// the four fields upstream allows are honoured; anything else in the object is
/// ignored rather than reflected, so a hook cannot reach fields it was never
/// given authority over by guessing their names.
#[must_use]
pub(crate) fn apply_spawn_rewrite(
    request: &SubagentSpawnRequest,
    modified_input: Option<&serde_json::Value>,
) -> Result<Option<SubagentSpawnRequest>, String> {
    let Some(updated) = modified_input.and_then(serde_json::Value::as_object) else {
        return Ok(None);
    };
    let mut rewritten = request.clone();
    let mut changed = Vec::new();
    if let Some(value) = updated.get("agent_type").and_then(|v| v.as_str()) {
        if value != rewritten.subagent_type {
            rewritten.subagent_type = value.to_string();
            changed.push("agent_type");
        }
    }
    if let Some(value) = updated.get("model") {
        let next = value.as_str().map(ToString::to_string);
        if next != rewritten.model {
            rewritten.model = next;
            changed.push("model");
        }
    }
    if let Some(value) = updated.get("cwd") {
        let next = value.as_str().map(ToString::to_string);
        if next != rewritten.cwd {
            rewritten.cwd = next;
            changed.push("cwd");
        }
    }
    // ⚠️ `background` is deliberately NOT rewritable here, though upstream lists
    // it. In this port the consumer sits ABOVE the hook: `should_run_in_background`
    // has already branched in the Agent tool, and the builder takes `persistent`
    // as a caller parameter rather than reading `request.run_in_background`.
    // Accepting the field would log "rewritten by a hook" and change nothing —
    // an advertised capability that silently does not work, which is worse than
    // an absent one. Honouring it means moving the hook above that branch, which
    // is the same change the async-path ordering gap needs.
    // claude-code: a hook that sets cwd on a worktree-isolated spawn is
    // self-contradictory — the worktree IS the working directory. Upstream
    // refuses rather than silently picking one, and so does this.
    if rewritten.cwd != request.cwd && rewritten.isolation.as_deref() == Some("worktree") {
        return Err(
            "A plugin's agent.spawn hook set cwd on a spawn isolated in a worktree; \
             cwd and isolation: \"worktree\" are mutually exclusive."
                .to_string(),
        );
    }
    if changed.is_empty() {
        return Ok(None);
    }
    // Upstream logs which fields a hook rewrote (`Yvn`). A silent rewrite of the
    // agent type or cwd is exactly what an operator needs to see.
    tracing::info!(
        agent_type = %request.subagent_type,
        rewritten = %changed.join(", "),
        "agent.spawn: rewritten by a hook"
    );
    Ok(Some(rewritten))
}

impl PoolSubagentSpawner {
    /// Resolve a spawn's [`AgentDefinition`] from `subagent_type`, including its
    /// model preference.
    ///
    /// First looks the definition up by precedence (see [`Self::lookup_definition`]),
    /// then resolves its [`AgentModel`] to a concrete wire model id via
    /// [`crate::model_resolution::resolve_agent_model`] (when a `parent_model`
    /// is supplied): `Inherit`→parent model; a bare family alias→the parent's exact
    /// id when same-tier, else the family's concrete default id. Without a
    /// `parent_model` the model string is left RAW (legacy behavior). The caller
    /// computes `parent_model` via [`Self::effective_parent_model`] (the request's
    /// `parent_model_override` — the LIVE / immediate-parent model — else the
    /// spawner's boot/live default).
    pub(super) async fn resolve_definition(
        &self,
        subagent_type: &str,
        parent_model: Option<&str>,
    ) -> AgentDefinition {
        self.resolve_definition_with_profile(subagent_type, parent_model, None, None)
            .await
    }
    pub(super) async fn resolve_definition_with_profile(
        &self,
        subagent_type: &str,
        parent_model: Option<&str>,
        _parent_model_profile: Option<&str>,
        parent_provider_first_party: Option<bool>,
    ) -> AgentDefinition {
        let mut def = self.lookup_definition(subagent_type).await;
        // An explicit `parent_model` (the request override) wins; otherwise fall
        // back to the spawner's own live/boot default. `None` on BOTH ⇒ the model
        // string is left RAW (legacy: the runner emits `Inherit`→`"inherit"`).
        let parent = parent_model
            .map(str::to_string)
            .or_else(|| self.resolved_default_model());
        if let Some(parent_model) = parent.as_deref() {
            // 2.1.198 `GAe`: the built-in Explore definition's model is derived
            // from the SESSION model (inherit, capped at "opus" for
            // fable/mythos-class firstParty sessions) BEFORE the normal
            // alias/Inherit resolution. Non-Explore / non-built-in definitions
            // pass through unchanged.
            def.model = crate::model_resolution::resolve_builtin_explore_model(
                &def,
                parent_model,
                parent_provider_first_party.unwrap_or(self.session_provider_first_party),
            );
            def.model = AgentModel::Explicit(self.resolve_model_pref(&def.model, parent_model));
        }
        def
    }
    /// Look up the [`AgentDefinition`] for `subagent_type` by precedence.
    ///
    /// Precedence (claude-code parity — later wins): file catalog
    /// (user/project) overrides built-ins. An unknown type defaults to
    /// `general-purpose`; a last-resort all-tools stub covers the impossible
    /// empty-built-ins case.
    ///
    /// NOTE: in claude-code the unknown→general-purpose fallback only fires when
    /// `subagent_type` is OMITTED (`effectiveType ?? GENERAL_PURPOSE`,
    /// AgentTool.tsx:322); an EXPLICIT unknown type throws `Agent type 'x' not
    /// found`. That distinction is enforced UPSTREAM in `AgentTool::call`
    /// (tools/agent), which validates an explicit type against `agent_listing()`
    /// before spawning, so this method only ever receives an omitted (→
    /// general-purpose) or a resolvable type from the tool path. Internal
    /// callers that bypass the tool still get the permissive fallback.
    pub(super) async fn lookup_definition(&self, subagent_type: &str) -> AgentDefinition {
        // 0. Fork path (codex #5): the synthetic FORK_AGENT is resolved FIRST,
        // unconditionally, so a user agent literally named "fork" cannot shadow
        // it (claude uses the synthetic FORK_AGENT on the fork path, never the
        // catalog — forkSubagent.ts:60-71 / AgentTool.tsx:335). It is NOT in the
        // 6-element built-in vec (claude does not register it in builtInAgents).
        if subagent_type == platform_api::fork_subagent::FORK_SUBAGENT_TYPE {
            return crate::builtins::fork_agent_definition();
        }
        // 0b. Hidden Fusion panel: resolved BEFORE the catalog so a user agent
        // named `fusion-panel` cannot shadow the synthetic definition.
        if subagent_type == platform_api::FUSION_PANEL_TYPE {
            return crate::builtins::fusion_panel_definition();
        }
        // 0c. [Finding 25] `fusion` is reserved for the Fusion Agent surface:
        // tools/agent's `call` intercepts any subagent_type normalizing to
        // `fusion` into a multi-model panel BEFORE the catalog lookup, so a
        // disk agent whose name normalizes to it can never be dispatched by
        // any spelling. Drop it here too — for any caller that resolves a
        // definition directly instead of going through that intercept — by
        // falling through past the catalog to the same general-purpose
        // fallback a wholly unknown type gets, matching the fork /
        // fusion-panel precedent of never letting a user file shadow the
        // reserved name. [Round-12 finding 6] The predicate is the shared
        // NORMALIZED one, not a literal compare: the intercept covers
        // `Fusion` / `fu-sion` / `fusion-` too, and a narrower guard here
        // made one name resolve to two different agents.
        if normalizes_to_fusion(subagent_type) {
            if let Some(catalog) = self.agent_catalog.get() {
                if let Some(shadow) = catalog
                    .read()
                    .await
                    .iter()
                    .find(|d| normalizes_to_fusion(&d.agent_type))
                {
                    tracing::warn!(
                        agent_type = %shadow.agent_type,
                        "a disk agent is named `fusion` (under the Fusion \
                         intercept's normalization), which is reserved for \
                         the Fusion Agent surface and can never be dispatched; \
                         rename it so it is not silently unreachable"
                    );
                }
            }
            if let Some(def) = self.builtins.get("general-purpose").cloned() {
                return def;
            }
            return Self::fallback_definition(subagent_type);
        }
        // 1. File catalog (user/project) wins on collision.
        if let Some(catalog) = self.agent_catalog.get() {
            if let Some(def) = catalog
                .read()
                .await
                .iter()
                .find(|d| d.agent_type == subagent_type)
                .cloned()
            {
                return def;
            }
        }
        // 2. Built-in by exact type.
        if let Some(def) = self.builtins.get(subagent_type).cloned() {
            return def;
        }
        // 3. Unknown type → general-purpose (matches claude-code's default).
        if let Some(def) = self.builtins.get("general-purpose").cloned() {
            return def;
        }
        // 4. Last resort (built-ins somehow empty): a permissive stub.
        Self::fallback_definition(subagent_type)
    }
    /// Minimal all-tools definition used only when neither the catalog nor the
    /// built-ins can supply one (built-ins always include `general-purpose`, so
    /// this is defensive). Uses the high built-in turn cap, not the old
    /// `max_turns: 1`, so a fallback agent can still run a tool-using loop.
    pub(super) fn fallback_definition(subagent_type: &str) -> AgentDefinition {
        AgentDefinition {
            cache_ttl: None,
            agent_type: subagent_type.into(),
            when_to_use: String::new(),
            tools: AgentToolPolicy::All {
                use_exact_tools: false,
            },
            max_turns: crate::builtins::BUILTIN_AGENT_MAX_TURNS,
            model: AgentModel::Inherit,
            permission_mode: AgentPermissionMode::Bubble,
            source: AgentSource::BuiltIn,
            base_dir: "built-in".into(),
            system_prompt: None,
            mcp_servers: vec![],
            frontmatter_hooks: vec![],
            icon: None,
            allowed_tools: vec![],
            worktree_requirement: None,
            // Defensive stub: no extended frontmatter — all defaults.
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
    /// Resolve a spawn's advertised tool schemas + dispatch allow-list from the
    /// live registry per `agent_def`'s [`AgentToolPolicy`]. Returns
    /// `(tool_schemas, allowed_tool_names)`. Unset registry → `(empty, empty)`
    /// (no tools advertised, allow-list guard skipped).
    pub(super) async fn resolve_tools(
        &self,
        agent_def: &AgentDefinition,
        // The resolved subagent's own recursion depth — gates its `Agent` tool
        // against Claude's configured maximum spawn depth. Threaded from
        // `request.depth`.
        depth: u32,
        // §24b — this spawn's per-agent MCP tools (claude `Agr`'s `Fe`),
        // already connected + built by [`Self::mcp_tool_builder`]. Appended by
        // [`crate::tool_resolver::AgentToolResolver::resolve`] step (4) AFTER
        // every drop/filter, exactly like every other MCP tool. Empty when the
        // definition declared no `mcpServers` or no builder is wired.
        agent_mcp_tools: &[Arc<dyn tool_api::Tool>],
    ) -> Result<(Vec<serde_json::Value>, Vec<String>), SubagentSpawnError> {
        let Some(registry) = self.tool_registry.get() else {
            return Ok((Vec::new(), Vec::new()));
        };
        // Delegate to the shared resolver (single source of truth, also used by
        // the in-process teammate handler). The tool-wide deny names come from the
        // boot policy via the set-once cell (UNFILLED / EMPTY ⇒ no tools dropped,
        // regression-safe). The `default_model` only anchors the model-gated tool
        // prompt for an `AgentModel::Inherit` def; on the production spawn path the
        // def's model is already resolved to `Explicit` (so the param is inert
        // there), hence the LIVE default (provider else boot snapshot) is a
        // faithful anchor for the direct-call / Inherit case without needing the
        // per-request override threaded here.
        let empty: Vec<String> = Vec::new();
        let denied = self.tool_wide_deny_names.get().unwrap_or(&empty);
        let default_model = self.resolved_default_model();
        let coordinator_mode = self
            .coordinator_mode
            .get()
            .is_some_and(|mode| mode.is_enabled());
        crate::tool_resolver::resolve_subagent_tools(
            registry.as_ref(),
            agent_def,
            denied,
            default_model.as_deref(),
            depth,
            coordinator_mode,
            agent_mcp_tools,
        )
        .await
        .map_err(|e| SubagentSpawnError::Internal(e.to_string()))
    }
    /// Build the child context from a RESOLVED [`AgentDefinition`] + the
    /// caller's task prompt, plus the optional fork carriers.
    ///
    /// Non-fork path (`fork_*` both `None`): the agent body becomes the system
    /// prompt with the appended `Notes:` trailer, and the task `prompt` is the
    /// first (and only) user message — byte-identical to before codex #5.
    ///
    /// Fork path (codex #5):
    /// - `fork_parent_system_prompt = Some` → use the parent's already-rendered
    ///   bytes VERBATIM as the system prompt and SKIP the `Notes:` trailer
    ///   (re-appending it would bust the prompt cache; claude passes
    ///   `override.systemPrompt` verbatim with no
    ///   `enhanceSystemPromptWithEnvDetails`, AgentTool.tsx:622-623).
    /// - `fork_context_messages = Some` → seed `ctx.fork_context_messages` with
    ///   the byte-exact forked prefix and leave `prompt_messages` EMPTY (the
    ///   directive is already the trailing Text block inside that prefix, built
    ///   by `build_forked_messages`; `runner.rs` replays
    ///   `fork_context_messages ++ prompt_messages`, so `[]` prompt_messages
    ///   yields exactly the forked prefix — AgentTool.tsx:630 / spec note (A)).
    #[cfg(test)]
    pub(super) fn make_subagent_context(
        def: AgentDefinition,
        prompt: &str,
        fork_context_messages: Option<Vec<ConversationMessage>>,
        fork_parent_system_prompt: Option<String>,
    ) -> SubagentContext {
        Self::make_subagent_context_with_id(
            def,
            prompt,
            fork_context_messages,
            fork_parent_system_prompt,
            AgentId::new(),
        )
    }
    pub(super) fn make_subagent_context_with_id(
        def: AgentDefinition,
        prompt: &str,
        fork_context_messages: Option<Vec<ConversationMessage>>,
        fork_parent_system_prompt: Option<String>,
        agent_id: AgentId,
    ) -> SubagentContext {
        // System prompt: fork path uses the parent's rendered bytes verbatim
        // (no trailer); non-fork path = agent body + the `Notes:` trailer
        // (claude `enhanceSystemPromptWithEnvDetails`). A `None` body on the
        // non-fork path stays `None` (no body, no trailer).
        let rendered_system_prompt: Option<Arc<str>> = match &fork_parent_system_prompt {
            Some(parent) => Some(Arc::from(parent.as_str())),
            None => def.system_prompt.as_deref().map(|body| {
                Arc::from(format!(
                    "{body}\n\n{}\n\n{}",
                    Self::SUBAGENT_CONSENT_PARAGRAPH,
                    Self::SUBAGENT_NOTES_TRAILER
                ))
            }),
        };
        // Fork path seeds prompt_messages EMPTY (the directive lives in the fork
        // prefix); non-fork path seeds it with the task prompt user message.
        let is_fork = fork_context_messages.is_some();
        let prompt_messages = if is_fork {
            vec![]
        } else {
            vec![ConversationMessage::user(
                MessageId::new(),
                prompt.to_string(),
            )]
        };
        SubagentContext {
            task_registry: None,
            refusal_fallback_chain: Vec::new(),
            agent_id,
            parent_agent_id: None,
            agent_name: None,
            team_name: None,
            agent_definition: def,
            prompt_messages,
            fork_context_messages,
            allowed_tools: vec![],
            worktree_handle: None,
            // Set by `build_subagent_context` from the resolved isolation/cwd.
            cwd: None,
            is_async: false,
            persistent: false,
            can_show_permission_prompts: false,
            // Filled by `build_subagent_context` from the owning spawner.
            session_interactive: None,
            origin_session_id: None,
            mcp_clients: vec![],
            transcript_subdir: "/tmp".into(),
            transcript_fs: None,
            resumed_history: None,
            rendered_system_prompt,
            mobile_runtime_environment_reminder: None,
            mobile_runtime_workspace_reminder: None,
            content_replacement_state: None,
            agent_memory: None,
            display: AgentDisplay {
                color: AgentColor::Cyan,
                icon: None,
            },
            // Set by `build_subagent_context` from `request.model_profile`.
            model_profile: None,
            // Set by `spawn` from `self.api_client` / `inherit.tool_invoker` /
            // `inherit.budget` just before pool allocation. `tool_schemas` +
            // `allowed_tools` are overwritten by `spawn` from `resolve_tools`
            // over the live registry per the resolved definition's policy.
            api_client: None,
            tool_invoker: None,
            new_diagnostics_source: None,
            tool_schemas: vec![],
            // Overwritten by `spawn` from `request.schema` (like `tool_schemas`).
            schema: None,
            // Overwritten by `build_subagent_context` from `request.structured_output_mode`.
            structured_output_mode: platform_api::subagent_spawn::StructuredOutputMode::Forced,
            structured_output_parse_retries: 0,
            budget: None,
            // Filled by `spawn` from the set-once `hook_executor` / `skill_loader`
            // cells (None when unfilled — tests / minimal builds). `hook_session_id`
            // / `hook_cwd` carry the boot-set values.
            hook_executor: None,
            strict_plugin_only_hooks: false,
            skill_loader: None,
            hook_session_id: protocol::SessionId::nil(),
            hook_cwd: std::path::PathBuf::new(),
            // Default 0; `build_subagent_context` overwrites it with `request.depth`.
            depth: 0,
            observer: None,
            // Set by `build_subagent_context` from the clamped spawn `mode` /
            // definition permission mode (non-fork only). `None` = inherit the
            // live/boot gate mode.
            permission_mode_override: None,
            frozen_command_denies: Vec::new(),
            max_output_tokens_per_turn: None,
            max_input_bytes_per_turn: None,
            query_source_label: None,
            correlation_id: None,
            model_attempt: None,
        }
    }
    /// Resolve the full subagent catalog (built-ins overlaid by the file
    /// catalog, claude-code later-wins precedence) into listing entries for
    /// the dynamic Agent tool prompt. Delegates to the crate-level
    /// [`crate::agent_listing_entries`] free fn (shared with the
    /// `agent_listing_delta` attachment path) after snapshotting the catalog.
    pub(super) async fn listing_entries(&self) -> Vec<SubagentListingEntry> {
        // Snapshot built-ins + any wired catalog into one slice, then run the
        // shared merge. Built-ins are listed first; the shared fn applies
        // later-wins precedence so a same-named catalog entry overrides them.
        let mut defs: Vec<AgentDefinition> = self.builtins.values().cloned().collect();
        if let Some(catalog) = self.agent_catalog.get() {
            defs.extend(catalog.read().await.iter().cloned());
        }
        crate::agent_listing_entries(&defs)
    }
    /// Build the child [`SubagentContext`] for a spawn: definition resolution +
    /// caller model override + inheritance (tool invoker / budget / api seam) +
    /// hook cells + transcript dir + per-spawn tool resolution. Shared by the
    /// one-shot [`SubagentSpawner::spawn`] and the resumable
    /// [`StreamingSubagentSpawner::spawn_persistent`].
    ///
    /// `persistent = true` makes the runner "come to rest" after each terminal
    /// turn-set — it parks awaiting the next inbound `UserMessage` (delivered via
    /// [`StateMachinePool::send_event`]) instead of returning — and marks it
    /// async (background-scheduled). This is the basis of the resumable
    /// background local_agent (claude-code `run_in_background` + comes-to-rest).
    /// Returns the built context alongside this spawn's §24b agent-scoped MCP
    /// teardown handles (empty unless the definition declared `mcpServers`
    /// AND a builder is wired) — the caller runs them
    /// ([`crate::agent_mcp_tools::run_agent_mcp_cleanups`]) once the spawn's
    /// run concludes, mirroring claude `Agr`'s `cleanup` closure.
    pub(super) async fn build_subagent_context(
        &self,
        request: &SubagentSpawnRequest,
        inherit: SubagentInheritance,
        persistent: bool,
    ) -> Result<
        (
            SubagentContext,
            Vec<crate::agent_mcp_tools::AgentMcpCleanupHandle>,
        ),
        SubagentSpawnError,
    > {
        self.build_subagent_context_with_id(request, inherit, persistent, None, None)
            .await
    }
    /// Run the `agent.spawn` function hooks and return the possibly-rewritten
    /// request (claude-code `_Bo`, @2955987).
    ///
    /// A hook may deny the spawn, or rewrite `subagent_type` / `model` / `cwd` /
    /// `run_in_background`.
    ///
    /// 🚨 The rewrite is applied HERE, before anything is derived from the
    /// request — deliberately, and it is what makes upstream's "re-check the
    /// permission rules after a rewrite" step unnecessary rather than skipped.
    /// Definition resolution, model resolution, the bypass clamps and tool
    /// policy all read the request AFTER this point, so they re-derive from the
    /// rewritten values on their own. ⛔ Do not move this later and add a
    /// separate re-check: a hook that rewrote `subagent_type` to an agent whose
    /// frontmatter declares `permissionMode: bypassPermissions` would then be
    /// clamped against the OLD type.
    pub(super) async fn apply_agent_spawn_hook(
        &self,
        request: &SubagentSpawnRequest,
        origin_session_id: Option<protocol::SessionId>,
    ) -> Result<Option<SubagentSpawnRequest>, SubagentSpawnError> {
        // `RuntimeLink::get` already hands back an owned `Arc`; the
        // `OnceLock` this arrived on borrows and needs a `.cloned()`.
        let executor = match self.hook_executor.get() {
            Some(executor) => executor,
            // Sealed and empty: the host drained its children while this spawn
            // was in flight. Reading that as "no hook is registered" is how a
            // plugin's `HookDecision::Block` would turn into an allow, so the
            // spawn is refused instead — the host is going away regardless.
            None if self.hook_executor.is_sealed() => {
                return Err(SubagentSpawnError::Runtime(
                    "SubagentSpawner: the host released its hook executor; refusing to spawn \
                     without consulting agent.spawn"
                        .to_string(),
                ));
            }
            // Never filled: this host has no hook executor at all, which is the
            // same as upstream running with no `agent.spawn` hook registered.
            None => return Ok(None),
        };
        let event = hooks::events::HookEvent::AgentSpawn {
            agent_type: request.subagent_type.clone(),
            model: request.model.clone(),
            cwd: request.cwd.clone(),
            background: request.run_in_background,
            parent_agent_id: request.creator_agent_id,
        };
        let aggregate = executor
            .execute(
                event,
                hooks::HookContext {
                    session_id: origin_session_id.unwrap_or(self.hook_session_id),
                    cwd: self.hook_cwd.clone(),
                    ..Default::default()
                },
            )
            .await;

        if matches!(aggregate.decision, Some(hooks::HookDecision::Block)) {
            return Err(SubagentSpawnError::DeniedByHook(
                aggregate
                    .reason
                    .unwrap_or_else(|| "no reason given".to_string()),
            ));
        }

        // `modified_input` carries the rewrite, reusing the same field every
        // other hook kind uses to mutate what it gates.
        let rewritten = apply_spawn_rewrite(request, aggregate.modified_input.as_ref())
            .map_err(SubagentSpawnError::DeniedByHook)?;

        // 🚨 RE-CHECK the deny rule against the REWRITTEN type.
        //
        // `Agent(<type>)` is evaluated in the tool layer, above this spawner, so
        // it saw the type the MODEL asked for. A hook that rewrites
        // `subagent_type` would otherwise reach a type the operator's rules
        // explicitly deny — and if that type's frontmatter declares
        // `permissionMode: bypassPermissions`, reach it WITH bypass. Rewriting
        // early makes the clamps re-derive, but it cannot re-run a rule that
        // lives above the hook; only this can.
        if let Some(next) = rewritten.as_ref() {
            if next.subagent_type != request.subagent_type {
                if let Some(gate) = self.permission_gate.get() {
                    if let Some(source) = gate.agent_type_deny(&next.subagent_type).await {
                        return Err(SubagentSpawnError::DeniedByHook(format!(
                            "an agent.spawn hook rewrote this spawn to agent type \"{}\", which \
                             a permission rule denies ({source}). Dispatch it directly.",
                            next.subagent_type
                        )));
                    }
                }
            }
        }
        Ok(rewritten)
    }
    pub(super) async fn build_subagent_context_with_id(
        &self,
        request: &SubagentSpawnRequest,
        inherit: SubagentInheritance,
        persistent: bool,
        restored_agent_id: Option<AgentId>,
        identity_reservation: Option<Arc<crate::pool::IdentityReservation>>,
    ) -> Result<
        (
            SubagentContext,
            Vec<crate::agent_mcp_tools::AgentMcpCleanupHandle>,
        ),
        SubagentSpawnError,
    > {
        // `agent.spawn` runs FIRST: everything below derives from `request`, so
        // a rewrite here is re-derived by definition resolution, the bypass
        // clamps and tool policy without any of them knowing a hook ran.
        let restored_transcript = restored_agent_id.and_then(|agent_id| {
            self.allocated_transcript_paths
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&agent_id)
                .cloned()
        });
        let workflow_dir = workflow_transcript_subdir_override();
        let origin_session_id = restored_transcript
            .as_ref()
            .and_then(|(_, owner)| *owner)
            .or_else(|| self.resolved_origin_session_id(request));
        let rewritten = self
            .apply_agent_spawn_hook(request, origin_session_id)
            .await?;
        let request = rewritten.as_ref().unwrap_or(request);

        // The parent / main-loop model this spawn resolves against: the request's
        // `parent_model_override` (the LIVE session model at top level / the
        // immediate parent subagent's resolved model when nested — threaded by
        // `AgentTool`, claude `AgentTool.tsx:418`) else the spawner's boot/live
        // default. Computed ONCE and threaded into definition + model-override +
        // tool resolution so all three agree on the same anchor.
        let parent_selection = self.effective_parent_selection(request);
        let has_explicit_provider_model = request
            .model
            .as_deref()
            .is_some_and(|model| !model.trim().is_empty())
            && request
                .model_profile
                .as_deref()
                .is_some_and(|profile| !profile.trim().is_empty());
        if parent_selection.is_none()
            && self.default_model_selection_provider.get().is_some()
            && !has_explicit_provider_model
        {
            return Err(SubagentSpawnError::Runtime(
                "live session model/provider selection is unavailable".to_string(),
            ));
        }
        let parent_model = parent_selection
            .as_ref()
            .map(|selection| selection.model.clone());
        let mut def = self
            .resolve_definition_with_profile(
                &request.subagent_type,
                parent_model.as_deref(),
                parent_selection
                    .as_ref()
                    .and_then(|selection| selection.model_profile.as_deref()),
                parent_selection
                    .as_ref()
                    .map(|selection| selection.provider_first_party),
            )
            .await;
        // Per-spawn system-prompt override (workflow xBp / DBp): replace the
        // resolved definition's body with the caller's override BEFORE the Notes
        // trailer is appended by `make_subagent_context`.
        if let Some(override_prompt) = &request.system_prompt_override {
            def.system_prompt = Some(override_prompt.clone());
        }
        // Per-spawn disallowed-tools union (workflow §6): augment the resolved
        // definition's deny list with the caller's additional names. Dedup so a
        // builtin that already has "SendUserMessage" doesn't double-list it.
        if !request.additional_disallowed_tools.is_empty() {
            for name in &request.additional_disallowed_tools {
                if !def.disallowed_tools.contains(name) {
                    def.disallowed_tools.push(name.clone());
                }
            }
        }
        // AgentTool spawn-surface parity: an explicit `model` from the caller
        // (TS schema `model: 'sonnet' | 'opus' | 'haiku'`) takes precedence over
        // the definition's model frontmatter (AgentTool.tsx:86).
        // `model_profile` pins the CHILD only when accompanied by an explicit
        // child model. Without `request.model` it is a parent hint and must not
        // leak onto a definition that resolves to a different model.
        let mut accepted_request_model_profile = request
            .model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .and(request.model_profile.clone());
        if let Some(model_pref) = request.model.as_deref() {
            // Dual-LLM dual-PROVIDER routing: when the caller pinned a provider
            // profile (`model_profile`), `request.model` is ALREADY the concrete
            // provider-local wire model (the candidate's resolved `request_model`,
            // e.g. `gpt-4o` / `gemini-1.5-pro`). The family-alias logic in
            // `resolve_agent_model` (alias→parent-tier matching, parent region
            // prefix) is Claude-shaped and would mangle a foreign concrete id, so
            // it is BYPASSED here: the model is used verbatim as `Explicit`. The
            // `profile` (set just below from `request.model_profile`) selects the
            // provider in `messages_create_*_in`.
            if request.model_profile.is_some() {
                let (resolved, accepted) =
                    self.resolve_provider_model_pref(model_pref, parent_model.as_deref())?;
                def.model = AgentModel::Explicit(resolved);
                if !accepted {
                    accepted_request_model_profile = None;
                }
            } else {
                let requested = AgentModel::Alias(model_pref.to_string());
                def.model = match parent_model.as_deref() {
                    Some(parent) => {
                        AgentModel::Explicit(self.resolve_model_pref(&requested, parent))
                    }
                    None => requested,
                };
            }
        }
        // Per-spawn effort override (claude-code workflow `agent({effort})` →
        // `me={...ie,effort:ae}`): a level/integer opt overrides the resolved
        // definition's effort frontmatter. Ignored when unparseable.
        if let Some(effort) = &request.effort {
            if let Some(parsed) = crate::definition::AgentEffort::from_json(effort) {
                def.effort = Some(parsed);
            }
        }
        let is_fork_spawn =
            request.fork_parent_system_prompt.is_some() || request.fork_context_messages.is_some();
        let effective_permission_mode = if is_fork_spawn {
            None
        } else {
            // (parity 2.1.212) The Agent/Task `mode` call param is DEPRECATED and
            // ignored: claude reads the PARENT's live mode (`_=yn(l),y=_.mode`)
            // and never consults the spawn param. The child therefore inherits the
            // parent's live permission mode (`self.permission_mode`), with the
            // agent-definition frontmatter as the ONLY override source. Pass `None`
            // for the requested spawn mode so `request.mode` — carried for
            // back-compat — is never applied.
            crate::permission_mode::effective_child_mode(
                None,
                self.permission_mode,
                def.permission_mode,
                self.spawn_bypass_gates.get().copied().unwrap_or_default(),
                &mut |m| tracing::warn!("{m}"),
            )
        };
        if effective_permission_mode == Some(PermissionMode::Plan) {
            def.permission_mode = AgentPermissionMode::Plan;
        }
        // Fork carriers (codex #5): on the fork path `fork_context_messages`
        // carries the byte-exact forked prefix and `fork_parent_system_prompt`
        // the parent's rendered system prompt; both `None` for a normal spawn.
        let mut ctx = Self::make_subagent_context_with_id(
            def,
            &request.prompt,
            request.fork_context_messages.clone(),
            request.fork_parent_system_prompt.clone(),
            restored_agent_id.unwrap_or_default(),
        );
        ctx.session_interactive = self.session_interactive;
        ctx.origin_session_id = origin_session_id;
        // Hand the child the refusal-fallback chain. Upstream's subagents share
        // the main thread's cascade because they share its query generator;
        // here the loops are separate, so it is passed down.
        ctx.refusal_fallback_chain = self.refusal_fallback_chain.clone();
        // Append the subagent `<env>` block (claude-code 2.1.186 `tIm`, after the
        // `Notes:` trailer) on the NON-fork path only — the fork path replays the
        // parent's rendered prompt verbatim with no `enhanceSystemPromptWithEnvDetails`.
        // Rendered with THIS spawn's resolved model id so a model-override agent's
        // env line matches the model it actually runs as. Unfilled cell ⇒ no-op.
        if !is_fork_spawn {
            if let (Some(render), Some(body)) = (
                self.subagent_env_renderer.get(),
                ctx.rendered_system_prompt.as_ref(),
            ) {
                let model_id = crate::runner::resolve_model(&ctx);
                // Per-agent cwd (worktree isolation / explicit `cwd`) → the env
                // block's `Working directory` + the "git worktree" notice, so the
                // isolated agent forms absolute paths under it.
                let cwd_override = request.cwd.as_deref().map(std::path::Path::new);
                let env = render(&model_id, cwd_override);
                ctx.rendered_system_prompt = Some(Arc::from(format!("{body}\n\n{env}")));
            }
        }
        // Per-spawn system-prompt addendum (workflow HBp/IBp NOTE): appended
        // AFTER the Notes trailer + env block so it is the final content the model
        // sees. Used when the caller specifies an explicit agentType in a workflow
        // agent() call.
        if let Some(addendum) = &request.system_prompt_addendum {
            if let Some(body) = ctx.rendered_system_prompt.as_ref() {
                ctx.rendered_system_prompt = Some(Arc::from(format!("{body}{addendum}")));
            }
        }
        // (CLI-15) `--append-subagent-system-prompt <prompt>`: the operator's
        // suffix, appended to EVERY Task-tool subagent's system prompt and
        // therefore to nested subagents too (they spawn through this same
        // function in the same process).
        if let Some(suffix) = append_subagent_system_prompt_suffix() {
            if let Some(body) = ctx.rendered_system_prompt.as_ref() {
                ctx.rendered_system_prompt = Some(Arc::from(format!("{body}\n\n{suffix}")));
            }
        }
        // Hand the child the parent's tool invoker + budget enforcer + our model
        // API seam (recursion-lock / budget-inheritance invariants).
        ctx.parent_agent_id = request.creator_agent_id;
        ctx.task_registry = self.task_registry.get().and_then(std::sync::Weak::upgrade);
        ctx.tool_invoker = Some(inherit.tool_invoker);
        let child_budget = ctx
            .origin_session_id
            .and_then(|session_id| inherit.budget.scoped_for_session(session_id))
            .unwrap_or_else(|| Arc::clone(&inherit.budget));
        ctx.budget = Some(child_budget);
        ctx.api_client.clone_from(&self.api_client);
        // Per-spawn provider routing (dual-LLM dual-PROVIDER): the runner passes
        // this as the `profile` arg of the api client's `messages_create_*_in`
        // methods so the round-trip targets the candidate's resolved provider.
        let resolved_model = crate::runner::resolve_model(&ctx);
        ctx.model_profile = accepted_request_model_profile.or_else(|| {
            parent_selection
                .as_ref()
                .filter(|selection| selection.model == resolved_model)
                .and_then(|selection| selection.model_profile.clone())
        });
        // G4/G5: thread the runner's hook executor + skill loader + hook context
        // seed from the set-once cells (None ⇒ runner skips those steps).
        ctx.hook_executor = self.hook_executor.get();
        ctx.strict_plugin_only_hooks = self
            .strict_plugin_only_hooks
            .get()
            .copied()
            .unwrap_or(false);
        ctx.skill_loader = self.skill_loader.get();
        ctx.hook_session_id = ctx.origin_session_id.unwrap_or(self.hook_session_id);
        ctx.hook_cwd = self.hook_cwd.clone();
        // A RESTORE seeds the child from its recovered conversation, replacing
        // prompt + fork-context + preload (see `SubagentContext::resumed_history`).
        ctx.resumed_history = request.resumed_history.clone();
        // Seed the child's REAL transcript_subdir when the host wired one.
        let restored_subdir = restored_transcript
            .and_then(|(path, _)| path.parent().map(std::path::Path::to_path_buf));
        let subagents_dir = if let Some(pinned) = restored_subdir.or(workflow_dir) {
            Some(pinned)
        } else {
            ctx.origin_session_id
                .zip(self.subagents_dir_for_session_provider.as_ref())
                .map(|(session_id, provider)| provider(session_id))
                .transpose()?
                .or_else(|| self.resolved_subagents_dir())
        };
        if let Some(subagents_dir) = subagents_dir {
            ctx.transcript_subdir = subagents_dir;
            // Only wire the writer alongside a REAL subagents dir — writing a
            // transcript into the `/tmp` placeholder would scatter files a
            // resume could never find.
            ctx.transcript_fs = self.transcript_fs.clone();
        }
        // Resolve THIS spawn's advertised tools + dispatch allow-list.
        // This child's recursion depth (claude `spawnDepth`): the Agent tool
        // stamped it as parent.depth + 1. Drives the resolver's `Agent` depth-gate
        // and is threaded by the runner into the child's dispatched tools.
        ctx.depth = request.depth;
        ctx.observer.clone_from(&request.observer);
        // §24b: connect + build this spawn's per-agent inline `mcpServers`
        // (claude `Agr`) BEFORE resolving the tool pool, so the pool's step
        // (4) (`AgentToolResolver::resolve`'s `agent_mcp_tools` append) can
        // include them. Unwired builder (tests / minimal builds) ⇒ empty —
        // byte-identical legacy.
        let mut agent_mcp = match self.mcp_tool_builder.get() {
            Some(builder) => {
                builder(
                    ctx.agent_id,
                    ctx.agent_definition.clone(),
                    identity_reservation.clone().map(|reservation| {
                        reservation as crate::agent_mcp_tools::AgentMcpConstructionLease
                    }),
                )
                .await
            }
            None => crate::agent_mcp_tools::AgentMcpToolSet::default(),
        };
        if let Some(reservation) = identity_reservation {
            // Keep a restored identity reserved through asynchronous MCP
            // teardown too, including failed/cancelled context construction.
            for cleanup in &mut agent_mcp.cleanups {
                let run = cleanup.run.clone();
                let reservation = reservation.clone();
                cleanup.run = Arc::new(move || {
                    let reservation = reservation.clone();
                    let future = run();
                    Box::pin(async move {
                        let _reservation = reservation;
                        future.await
                    })
                });
            }
        }
        // [round-5 finding 11, one layer up] The builder above just CONNECTED
        // this spawn's MCP servers, and `resolve_tools` below is both an
        // `.await` and a `?`. A rejected tool policy (or a drop while
        // resolving) used to discard the handles right here, before any
        // caller had seen them — no guard, no owner, no teardown. Own them
        // from the instant they exist and hand them out at the `Ok` below.
        let mut mcp_guard = McpCleanupGuard::new(
            std::mem::take(&mut agent_mcp.cleanups),
            ctx.agent_definition.agent_type.clone(),
        );
        let (tool_schemas, allowed_tools) = self
            .resolve_tools(&ctx.agent_definition, request.depth, &agent_mcp.tools)
            .await?;
        ctx.tool_schemas = tool_schemas;
        ctx.allowed_tools = allowed_tools;
        // Per-agent working directory (claude-code `me = cwd ?? worktreePath`):
        // resolve this before rendering the mutable mobile workspace reminder.
        ctx.cwd = request.cwd.as_ref().map(std::path::PathBuf::from);
        ctx.new_diagnostics_source = self
            .new_diagnostics_source_factory
            .as_ref()
            .map(|factory| factory(ctx.cwd.as_deref()));
        ctx.mobile_runtime_environment_reminder = self
            .mobile_runtime_environment
            .as_ref()
            .map(|environment| Arc::from(environment.render_system_reminder()));
        ctx.mobile_runtime_workspace_reminder =
            self.mobile_runtime_environment
                .as_ref()
                .and_then(|environment| {
                    let cwd = match &self.mobile_workspace_cwd_provider {
                        Some(provider) => provider(ctx.cwd.as_deref()),
                        None => ctx
                            .cwd
                            .as_deref()
                            .map(|path| path.to_string_lossy().into_owned()),
                    };
                    environment
                        .render_workspace_system_reminder(cwd.as_deref())
                        .map(Arc::from)
                });
        ctx.schema = request.schema.clone();
        ctx.structured_output_mode = request.structured_output_mode;
        ctx.structured_output_parse_retries = request.structured_output_parse_retries;
        // Preserve the spawn's human identity on every dispatched tool call.
        // Claude's per-agent async-local context exposes `getAgentName()` and
        // `getTeammateContext()?.teamName`; SendMessage and the V2 task tools
        // key mailbox senders/owners on these display names, not on the pool's
        // internal AgentId. The background wrapper already registers the same
        // request.name on the shared mailbox, so carrying it here closes the
        // reverse (child -> peer/lead) attribution path as well.
        ctx.agent_name = request.name.clone();
        ctx.team_name = request.team_name.clone();
        // Per-agent working directory (claude-code `me = cwd ?? worktreePath`):
        // the AgentTool resolves `isolation:"worktree"` to a freshly-created
        // worktree path (or honours an explicit `cwd`) and threads it via
        // `request.cwd`. Set it on the context so the runner threads it into every
        // dispatched tool's `cwd`. `None` ⇒ the shared session workspace (legacy).
        // Per-spawn permission mode (claude-code 2.1.212): the Agent `mode` call
        // param is DEPRECATED and ignored — the child inherits the parent's live
        // permission-mode anchor (claude `_=yn(l),y=_.mode`), and ONLY the agent
        // definition's own permission mode may override it. The resulting override
        // (or `None`, meaning "inherit the live mode unchanged") is threaded into
        // the child's tool-dispatch permission checks (via
        // `SubagentContext::permission_mode_override` → `SubagentInvocationContext`
        // → the gate's `PermissionCheckContext`). The fork path replays the parent's
        // rendered context verbatim, so it never applies a mode override.
        ctx.permission_mode_override =
            effective_permission_mode.map(|m| crate::permission_mode::wire_mode_str(m).to_string());
        // Carry the fork-time command-deny snapshot through to the runner, which
        // replays it on every dispatched tool call (claude `freezeCommandDenies`).
        // Only the fork path populates it; every other spawn leaves it empty and
        // the dispatch path is unchanged.
        //
        // This is the consumer the field never had: it was computed, persisted to
        // the scoping sidecar and read back into the spawn request, but nothing
        // ever APPLIED it — so a settings edit made while a fork was parked could
        // silently widen what the resumed fork was allowed to run.
        ctx.frozen_command_denies = request.frozen_command_denies.clone();
        ctx.max_output_tokens_per_turn = request.max_output_tokens_per_turn;
        ctx.max_input_bytes_per_turn = request.max_input_bytes_per_turn;
        ctx.query_source_label = request.query_source_label.clone();
        ctx.model_attempt = request.model_attempt.clone();
        // G011: thread the caller's correlation id (Fusion's `{run_id}:p{index}`)
        // onto the child so its transcript can be matched back to a run.
        ctx.correlation_id = request.correlation_id.clone();
        if let Some(turns) = request.max_turns_override {
            if turns > 0 {
                ctx.agent_definition.max_turns = ctx.agent_definition.max_turns.min(turns);
            }
        }
        // A persistent (background/resumable) agent parks after each turn-set;
        // `is_async` marks background scheduling (vs the foreground one-shot).
        ctx.persistent = persistent;
        ctx.is_async = persistent;
        Ok((ctx, mcp_guard.take()))
    }
}
