//! Hook registry — indexes hook definitions by source and dispatches matches
//! to the executor in priority order.

use crate::definition::{HookDefinition, HookSource};
use crate::events::{HookEvent, HookEventType};
use crate::matcher::{
    comma_mode_for, is_bare_mcp_server_matcher, matches_if_condition, matches_pattern_with,
};
use lingxi_core::host::SubagentInheritance;
use lingxi_core::types::{AgentId, HookId, PluginId, SessionId};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use telemetry::otel::SerializedTraceContext;

/// Host-owned logical route of the agent whose hook is being evaluated.
/// This metadata is never decoded from the command/HTTP event payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookModelSelection {
    /// Provider-local logical model id.
    pub model: String,
    /// Configured provider connection paired with the model.
    pub model_profile: Option<String>,
}

/// Per-call context handed to hooks alongside the event payload.
///
/// M1.4 shape carried only `session_id`, `agent_id`, `cwd`. M5-06 extends
/// it with `transcript_path`, `permission_mode`, `agent_type`, and
/// `inherit` (the subagent capability bundle used by the Agent-arm
/// executor). All new fields are `Option` (or default-empty `PathBuf`) so
/// existing call sites adopt them with `..Default::default()` rather than
/// having to populate everything.
#[derive(Clone, Default)]
pub struct HookContext {
    /// Current logical route of the immediate agent, independent of serving
    /// refusal fallbacks or a separately bound root session.
    pub model_selection: Option<HookModelSelection>,
    /// Captured terminal-owner epoch. An old callback or dispatch must not
    /// consume a newer restored run's snapshot under the same child id.
    pub subagent_stop_epoch: Option<lingxi_core::types::MessageId>,
    /// Owning agent's spawn depth; absent for a root/session hook.
    pub agent_depth: Option<u32>,
    /// Optional lease for visible side effects produced by this hook dispatch.
    /// Tool dispatches set it to their executor generation; lifecycle and
    /// standalone hook calls leave it absent.
    pub publication_guard: Option<Arc<dyn crate::attachment::HookPublicationGuard>>,
    /// Live history for prompt evaluation; never serialized into command/HTTP input.
    pub prompt_transcript: Option<crate::PromptHookTranscript>,
    /// Session this event belongs to.
    pub session_id: SessionId,
    /// Active agent at the moment of dispatch (`None` for engine-global
    /// events like `Setup`).
    pub agent_id: Option<AgentId>,
    /// Engine cwd at the moment of dispatch.
    pub cwd: PathBuf,
    /// UUID correlating a user prompt with every subsequent hook event until
    /// the next prompt, spliced into the shared hook-input base as `prompt_id`
    /// (oracle `createBaseHookInput`, 2.1.238 minified `c_` @ BIN off
    /// 296935693: `{session_id, transcript_path, cwd, prompt_id:Vut()??void 0,
    /// permission_mode, agent_id, agent_type, effort}`). The same value the
    /// OpenTelemetry `prompt.id` attribute carries, so hook output joins to
    /// OTel events at prompt grain. `None` — the additive default — omits the
    /// key, faithful to "absent until the first user input of the process
    /// lifetime".
    pub prompt_id: Option<String>,
    /// Path to the on-disk transcript file backing this session. Used by
    /// HTTP / command hooks to splice into their payload (`transcript_path`
    /// per claude-code `BaseHookInputSchema`). M5-06.
    pub transcript_path: PathBuf,
    /// Current permission mode (`"default" | "plan" | "acceptEdits" | …`).
    /// `None` when not applicable (e.g. engine-global hooks). M5-06.
    pub permission_mode: Option<String>,
    /// Subagent role identifier when the dispatch happened inside a
    /// subagent context (`general-purpose`, `code-reviewer`, …). M5-06.
    pub agent_type: Option<String>,
    /// Subagent capability bundle the Agent-arm executor inherits when it
    /// spawns. `None` for hooks that never reach the Agent arm. M5-06.
    pub inherit: Option<SubagentInheritance>,
    /// Stable project root injected into Command hooks as `LINGXI_PROJECT_DIR`
    /// (B2). claude-code derives this from `getProjectRoot()` — the real repo
    /// root, deliberately *not* updated when entering a git worktree — so a
    /// hook script's `$LINGXI_PROJECT_DIR` always resolves to the repo root
    /// (`utils/hooks.ts:813-816`). When `None`, the Command arm falls back to
    /// `cwd`, the faithful approximation until the orchestrator populates a
    /// distinct project-root concept (that wiring lives in `turn_loop.rs`,
    /// out of this crate's scope). Additive / `..Default::default()`-compatible.
    pub project_dir: Option<PathBuf>,
    /// `true` when this dispatch is a *re-entry* of the Stop hook after a
    /// previous Stop hook returned a blocking error and the agent loop
    /// continued one extra turn (B4). claude-code threads `stop_hook_active`
    /// into the Stop hook payload (`query.ts:1300`) and uses it as the
    /// infinite-loop guard: when already `true`, a fresh Stop block must NOT
    /// loop again (`query.ts:1297` rationale). Additive default `false`.
    pub stop_hook_active: bool,
    /// The text of the final assistant message of the turn, spliced into the
    /// `Stop` / `UserPromptSubmit` payload (claude-code `BaseHookInputSchema`
    /// surfaces the transcript; B1 payloads consume this). `None` when no
    /// assistant message is available (e.g. a `UserPromptSubmit` fired at
    /// prompt ingress before any model reply). Additive default.
    pub last_assistant_message: Option<String>,
    /// Path to the agent-scoped transcript file when the dispatch happened
    /// inside a subagent context. Distinct from [`Self::transcript_path`]
    /// (the session transcript); `None` outside a subagent. Additive default.
    pub agent_transcript_path: Option<PathBuf>,
    /// Active reasoning-effort level for the current turn, spliced into the
    /// base hook input shape as `effort: { level }` (claude-code
    /// `createBaseHookInput`, minified `vd`). `None` — the additive default —
    /// when the firing scope has no effort signal, which is faithful for
    /// session-lifecycle hooks and models that do not support the effort
    /// parameter (claude-code's `Lw(model)` gate): in both cases the binary
    /// omits the `effort` key entirely. The orchestrator populates this only
    /// for tool-use-context hooks on effort-capable models. Additive default.
    pub effort: Option<crate::hook_payload::EffortLevel>,
    /// Controlling-terminal width/height at dispatch, injected into Command
    /// hooks as `COLUMNS`/`LINES` (#43). claude-code reads
    /// `{columns,rows}=process.stdout` and sets `if(L)P.COLUMNS=String(L)` /
    /// `if(D)P.LINES=String(D)` (BIN off 205727903) — i.e. each is set ONLY when
    /// truthy (non-zero / present). The hooks crate has no TTY of its own, so the
    /// composition root passes the engine's `process.stdout` size here (`None`
    /// for headless / non-TTY hosts, matching `process.stdout.columns` being
    /// `undefined`); the Command arm then sets the env var only when `Some(n)` and
    /// `n != 0` (the binary's falsy guard). Additive default `None`.
    pub terminal_columns: Option<u16>,
    /// Controlling-terminal height — see [`Self::terminal_columns`]. Injected as
    /// the `LINES` env var (#43). Additive default `None`.
    pub terminal_rows: Option<u16>,
    /// Serialized OpenTelemetry parent trace context for this dispatch. The
    /// orchestrator captures it at the synchronous turn seam; deferred/background
    /// hook re-entry restores the same parent before re-running side effects, and
    /// Command hooks can forward the matching `TRACEPARENT` env var to children.
    /// `None` omits trace propagation entirely, matching the current gate-off
    /// behavior.
    pub trace_context: Option<SerializedTraceContext>,
    /// Current session title at the moment of dispatch, threaded into
    /// `UserPromptSubmit` and `SessionStart` payloads as `session_title`
    /// (binary-confirmed at BIN off 201745825). `None` when no title is
    /// available or not applicable (most event types). Additive default `None`.
    pub session_title: Option<String>,
    /// Background-task snapshot stamped onto `Stop` / `SubagentStop` payloads
    /// (claude `Lic(taskRegistry.all())`, spread `...m` after
    /// `last_assistant_message`). `None` — the additive default — omits the
    /// `background_tasks` key (claude `m = void 0` when no tool-use context);
    /// `Some(vec)` emits the array (possibly empty `[]`). The orchestrator
    /// populates this from the live task registry; the filter
    /// (status running|pending, not `isBackgrounded === false`) and the
    /// per-element truncation (1000 chars) are applied by that wiring, not by
    /// the payload struct. Additive default `None`.
    pub background_tasks: Option<Vec<crate::hook_payload::HookBackgroundTask>>,
    /// Session-cron snapshot stamped onto `Stop` / `SubagentStop` payloads
    /// (claude `Mic()`, spread immediately after `background_tasks`). Same
    /// None-vs-`Some([])` semantics as [`Self::background_tasks`]. Additive
    /// default `None`.
    pub session_crons: Option<Vec<crate::hook_payload::HookSessionCron>>,
}

/// In-memory registry of hook definitions, sharded by their declared source.
///
/// Plugin-supplied hooks are tracked separately so they can be wholesale
/// removed by `unregister_plugin` when a plugin unloads. Front-matter hooks
/// are indexed by their owning agent so the registry can scope them to that
/// agent's lifetime — that index is plumbed in Plan 09 alongside the agent
/// loader.
pub struct HookRegistry {
    /// Shared session Mod host, installed when the first plugin Mod loads.
    mod_host: Option<Arc<crate::mods::ModHost>>,
    mod_background_context: Option<Weak<dyn crate::mods::ModSessionContext>>,
    mod_model_policy: Option<Arc<dyn crate::mods::ModSettingsReader>>,
    sources: HashMap<HookSource, Vec<HookDefinition>>,
    plugin: HashMap<PluginId, Vec<HookDefinition>>,
    /// Session-scoped named hooks keyed by session id then logical hook name.
    /// Used by runtime-installed hooks that must be replaceable/clearable as a
    /// unit (for example `/goal`'s temporary Stop hook).
    session_named: HashMap<SessionId, HashMap<String, HookDefinition>>,
    /// Frontmatter hooks scoped to the agent (claude `addSessionHook` keyed on
    /// the agent id, registerFrontmatterHooks.ts). Registered by
    /// [`Self::register_agent_hooks`] when a subagent starts and removed wholesale
    /// by [`Self::clear_agent_hooks`] when it ends (claude `clearSessionHooks`,
    /// runAgent.ts finally). Iterated by [`Self::match_event`] / [`Self::has_hooks_for`]
    /// so an agent's frontmatter hooks actually fire while it runs.
    frontmatter: HashMap<AgentId, Vec<HookDefinition>>,
    /// Whether a frontmatter bucket belongs to a real subagent. Buckets
    /// registered with `is_agent == false` are main-thread/session buckets;
    /// real subagent buckets are eligible only for their exact owner.
    frontmatter_is_agent: HashMap<AgentId, bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookSourceReplaceResult {
    pub replaced_count: usize,
    pub file_changed_matchers_before: Vec<String>,
    pub file_changed_matchers_after: Vec<String>,
}

impl HookRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            mod_host: None,
            mod_background_context: None,
            mod_model_policy: None,
            sources: HashMap::new(),
            plugin: HashMap::new(),
            session_named: HashMap::new(),
            frontmatter: HashMap::new(),
            frontmatter_is_agent: HashMap::new(),
        }
    }

    /// Expose the session's Mod host without keeping the registry lock while
    /// a handler waits on tool execution or user interaction.
    pub fn mod_host(&self) -> Option<Arc<crate::mods::ModHost>> {
        self.mod_host.clone()
    }

    /// The live session binding to attach to a newly created Mod worker
    /// before its first `plugin.register` policy check.
    pub fn mod_background_context(&self) -> Option<Weak<dyn crate::mods::ModSessionContext>> {
        self.mod_background_context.clone()
    }

    pub fn mod_model_policy(&self) -> Option<Arc<dyn crate::mods::ModSettingsReader>> {
        self.mod_model_policy.clone()
    }

    pub fn attach_mod_model_policy(&mut self, reader: Arc<dyn crate::mods::ModSettingsReader>) {
        if let Some(host) = &self.mod_host {
            host.attach_model_policy(reader.clone());
        }
        self.mod_model_policy = Some(reader);
    }

    /// Interim safety gate until managed PreToolUse hooks can run before the
    /// Mod chain and again after a rewritten call. A matching managed hook must
    /// never be bypassed by a Mod's short-circuit answer.
    pub fn has_managed_pre_tool_use_match(&self, event: &HookEvent, ctx: &HookContext) -> bool {
        self.match_event(event, ctx).into_iter().any(|hook| {
            hook.source == HookSource::Settings(lingxi_core::types::SettingsScope::Managed)
        })
    }

    /// Install the host shared by plugin lifecycle and event dispatch.
    pub fn set_mod_host(&mut self, host: Arc<crate::mods::ModHost>) {
        if let Some(context) = &self.mod_background_context {
            host.attach_background_context(context.clone());
        }
        if let Some(policy) = &self.mod_model_policy {
            host.attach_model_policy(policy.clone());
        }
        self.mod_host = Some(host);
    }

    /// Attach the owning session for callbacks fired after a hook has settled.
    pub fn attach_mod_background_context(
        &mut self,
        context: Weak<dyn crate::mods::ModSessionContext>,
    ) {
        if let Some(host) = &self.mod_host {
            host.attach_background_context(context.clone());
        }
        self.mod_background_context = Some(context);
    }

    /// Stop dispatching through a Mod host when no modules remain active.
    pub fn clear_mod_host(&mut self) {
        self.mod_host = None;
    }

    /// Register a hook under its declared source.
    pub fn register(&mut self, hook: HookDefinition) {
        warn_if_bare_mcp_matcher(&hook);
        self.sources.entry(hook.source).or_default().push(hook);
    }

    /// Atomically replace one settings-backed source bucket without touching
    /// plugin / session / frontmatter / other source families.
    ///
    /// Intended for desktop settings edits: user/project/local hooks can be
    /// rebuilt off-thread, validated, then swapped in one write once the
    /// candidate is known-good. Returns the pre/post `FileChanged` matcher sets
    /// so the composition root can decide whether to restart the watcher.
    pub fn replace_source_hooks(
        &mut self,
        source: HookSource,
        hooks: Vec<HookDefinition>,
    ) -> HookSourceReplaceResult {
        let before = self.file_changed_matchers();
        for hook in &hooks {
            warn_if_bare_mcp_matcher(hook);
        }
        let replaced_count = self.sources.get(&source).map_or(0, Vec::len);
        if hooks.is_empty() {
            self.sources.remove(&source);
        } else {
            self.sources.insert(source, hooks);
        }
        let after = self.file_changed_matchers();
        HookSourceReplaceResult {
            replaced_count,
            file_changed_matchers_before: before,
            file_changed_matchers_after: after,
        }
    }

    /// Register a batch of hooks owned by `plugin_id`. Replaces any previous
    /// registration under that plugin.
    pub fn register_plugin_hooks(&mut self, plugin_id: PluginId, hooks: Vec<HookDefinition>) {
        for hook in &hooks {
            warn_if_bare_mcp_matcher(hook);
        }
        self.plugin.insert(plugin_id, hooks);
    }

    /// Drop every hook owned by `plugin_id` (used when a plugin unloads).
    pub fn unregister_plugin(&mut self, plugin_id: &PluginId) {
        self.plugin.remove(plugin_id);
    }

    /// Insert or replace one named runtime hook scoped to `session_id`.
    /// Returns the previous definition, if the slot was already populated.
    pub fn upsert_session_named_hook(
        &mut self,
        session_id: SessionId,
        name: String,
        mut hook: HookDefinition,
    ) -> Option<HookDefinition> {
        warn_if_bare_mcp_matcher(&hook);
        hook.name = name.clone();
        hook.source = HookSource::Session;
        self.session_named
            .entry(session_id)
            .or_default()
            .insert(name, hook)
    }

    /// Borrow a named runtime hook scoped to `session_id`, if present.
    #[must_use]
    pub fn get_session_named_hook(
        &self,
        session_id: SessionId,
        name: &str,
    ) -> Option<&HookDefinition> {
        self.session_named.get(&session_id)?.get(name)
    }

    /// Remove one named runtime hook scoped to `session_id`.
    pub fn remove_session_named_hook(
        &mut self,
        session_id: SessionId,
        name: &str,
    ) -> Option<HookDefinition> {
        let removed = self.session_named.get_mut(&session_id)?.remove(name);
        if self
            .session_named
            .get(&session_id)
            .is_some_and(std::collections::HashMap::is_empty)
        {
            self.session_named.remove(&session_id);
        }
        removed
    }

    /// Remove every named runtime hook scoped to `session_id`.
    pub fn clear_session_hooks(&mut self, session_id: SessionId) -> usize {
        self.session_named
            .remove(&session_id)
            .map_or(0, |hooks| hooks.len())
    }

    /// Register an agent's frontmatter hooks, scoped to `agent_id` so they fire
    /// only while that subagent runs and can be cleared wholesale on exit.
    ///
    /// Port of claude-code `registerFrontmatterHooks(setAppState, agentId, hooks,
    /// …, isAgent)` (registerFrontmatterHooks.ts): when `is_agent` is `true`,
    /// every `Stop` subscription on a hook is retargeted to `SubagentStop` —
    /// because a subagent's loop end fires `SubagentStop`, not `Stop` (claude
    /// `executeStopHooks` uses `SubagentStop` when called with an `agentId`). The
    /// retarget rewrites the hook's `events` in place (a hook may subscribe to
    /// multiple events; only the `Stop` entry is rewritten, and de-duplicated if
    /// `SubagentStop` is already present). `is_agent == false` registers the
    /// hooks verbatim (the skill-frontmatter path, scoped to a session id).
    pub fn register_agent_hooks(
        &mut self,
        agent_id: AgentId,
        hooks: &[HookDefinition],
        is_agent: bool,
    ) {
        if hooks.is_empty() {
            return;
        }
        self.frontmatter_is_agent.insert(agent_id, is_agent);
        let bucket = self.frontmatter.entry(agent_id).or_default();
        for hook in hooks {
            let mut hook = hook.clone();
            if is_agent {
                for ev in &mut hook.events {
                    if *ev == crate::events::HookEventType::Stop {
                        *ev = crate::events::HookEventType::SubagentStop;
                    }
                }
                // De-dup a now-doubled `SubagentStop` (a hook that subscribed to
                // BOTH Stop and SubagentStop would otherwise list it twice).
                hook.events.dedup();
            }
            bucket.push(hook);
        }
    }

    /// Remove every frontmatter hook scoped to `agent_id` (claude
    /// `clearSessionHooks(rootSetAppState, agentId)`, runAgent.ts finally).
    /// Returns the number of hooks dropped.
    pub fn clear_agent_hooks(&mut self, agent_id: AgentId) -> usize {
        self.frontmatter_is_agent.remove(&agent_id);
        self.frontmatter
            .remove(&agent_id)
            .map_or(0, |hooks| hooks.len())
    }

    /// Remove the hook with `hook_id` from whichever bucket it lives in,
    /// returning `true` when a hook was actually dropped.
    ///
    /// This is the RUNTIME half of the claude-code `once` field
    /// (`registerSkillHooks.ts:35-36`, `utils/hooks.ts:2918-2919`): a hook
    /// declared `once: true` is removed from its source bucket after it runs
    /// with a *success* outcome, so it never fires again. The executor calls
    /// this only on success — an erroring `once` hook is left in place (it may
    /// succeed on a later dispatch), exactly mirroring TS's
    /// `result.outcome === 'success'` guard around `onHookSuccess`.
    ///
    /// Scans every source bucket plus the plugin and front-matter indexes so a
    /// `once` hook is removed wherever it was registered. `HookId`s are unique
    /// per definition, so at most one entry is dropped.
    pub fn remove_once_hook(&mut self, hook_id: HookId) -> bool {
        for hooks in self.sources.values_mut() {
            if let Some(pos) = hooks.iter().position(|h| h.id == hook_id) {
                hooks.remove(pos);
                return true;
            }
        }
        for hooks in self.plugin.values_mut() {
            if let Some(pos) = hooks.iter().position(|h| h.id == hook_id) {
                hooks.remove(pos);
                return true;
            }
        }
        for hooks in self.session_named.values_mut() {
            if let Some(name) = hooks
                .iter()
                .find_map(|(name, hook)| (hook.id == hook_id).then_some(name.clone()))
            {
                hooks.remove(&name);
                return true;
            }
        }
        for hooks in self.frontmatter.values_mut() {
            if let Some(pos) = hooks.iter().position(|h| h.id == hook_id) {
                hooks.remove(pos);
                return true;
            }
        }
        false
    }

    /// Return every hook subscribed to `event`'s type that ALSO satisfies its
    /// declared matcher, sorted in execution order (highest priority first).
    ///
    /// Mirrors claude-code `getMatchingHooks` (`utils/hooks.ts:1603-1703`):
    /// first the event-type subscription filter, then the per-hook matcher
    /// filter (`hooks.ts:1681-1685`):
    ///
    /// ```ts
    /// const filteredMatchers = matchQuery
    ///   ? hookMatchers.filter(m => !m.matcher || matchesPattern(matchQuery, m.matcher))
    ///   : hookMatchers
    /// ```
    ///
    /// So when this event has a `match_query` (B3, tool-name events), a hook is
    /// dropped only if it DECLARES a matcher that does not match. A hook with
    /// no matcher (`matcher() == None`) — or an empty / `"*"` matcher — always
    /// fires, exactly as before this change. Events with no `match_query`
    /// (e.g. `TaskCompleted`) skip the matcher filter entirely.
    ///
    /// The `if`-condition rule-content matcher (permission rules like
    /// `"Bash(git push:*)"`) is now ALSO enforced (B3 second half): a hook with
    /// an `if`-condition fires only when [`crate::matcher::matches_if_condition`]
    /// also passes for the event's tool name + tool input. This mirrors
    /// claude-code applying the tool-name `matcher` filter
    /// (`utils/hooks.ts:1681-1685`) and THEN the `if`-condition filter
    /// (`utils/hooks.ts:1808-1850`) — both must pass. The `if`-condition is only
    /// evaluable for the four tool events `prepareIfConditionMatcher`
    /// (`utils/hooks.ts:1390-1401`) accepts (`PreToolUse`, `PostToolUse`,
    /// `PostToolUseFailure`, `PermissionRequest`); on any other event a hook
    /// that DECLARES an `if`-condition is dropped (TS `ifMatcher` is `undefined`
    /// → the filter returns `false`). Following TS, the `if`-condition is applied
    /// only to externally-executed hook kinds (Command / Http / Agent / Prompt);
    /// in-process [`crate::HookExecutor::Builtin`] hooks (the analogue of TS
    /// `callback` / `function` hooks) ignore it.
    #[must_use]
    pub fn match_event(&self, event: &HookEvent, ctx: &HookContext) -> Vec<&HookDefinition> {
        let et = event.event_type();
        // Comma-mode (`atf`) widens the simple-pattern matcher (comma-separated
        // lists + the v2.1.195 hyphen) for this event type — claude
        // `matchesPattern`'s `n` argument. The port has no `toolAliases`, so the
        // `r` extra-alias map is `None`.
        let comma_mode = comma_mode_for(&et);
        let match_query = Self::match_query_for(event);
        let if_target = Self::if_match_target(event);
        let keep = |h: &&HookDefinition| -> bool {
            if !h.events.contains(&et) {
                return false;
            }
            // TS `getMatchingHooks`: only apply the tool-name matcher filter when
            // the event yields a `matchQuery`; otherwise every subscribed hook
            // passes. A hook with no matcher always passes.
            let tool_name_ok = match (&match_query, h.matcher()) {
                (Some(query), Some(matcher)) => {
                    matches_pattern_with(query, matcher, comma_mode, None)
                }
                _ => true,
            };
            if !tool_name_ok {
                return false;
            }
            // TS `ifFilteredHooks` (utils/hooks.ts:1808-1850): the `if`-condition
            // gates only externally-executed hook kinds; Builtin (callback /
            // function) hooks bypass it. A hook without an `if`-condition passes.
            if let Some(if_cond) = h.if_pattern() {
                if Self::if_condition_applies(h) {
                    match &if_target {
                        // if-evaluable event → both tool-name + content must match.
                        Some((tool_name, tool_input)) => {
                            if !matches_if_condition(if_cond, tool_name, tool_input) {
                                return false;
                            }
                        }
                        // Non-tool event with an `if`-condition → cannot be
                        // evaluated → drop (TS `if (!ifMatcher) return false`).
                        None => return false,
                    }
                }
            }
            true
        };
        let mut matched: Vec<&HookDefinition> =
            self.sources.values().flatten().filter(keep).collect();
        for hooks in self.plugin.values() {
            matched.extend(hooks.iter().filter(keep));
        }
        if let Some(hooks) = self.session_named.get(&ctx.session_id) {
            matched.extend(hooks.values().filter(keep));
        }
        // Frontmatter hooks are ownership-scoped, not merely lifetime-scoped.
        // A real subagent bucket may fire only when this dispatch carries that
        // exact `agent_id`; main-thread/session buckets (`is_agent == false`)
        // remain eligible only for main-thread dispatches (`agent_id == None`).
        match ctx.agent_id {
            Some(agent_id) => {
                if self
                    .frontmatter_is_agent
                    .get(&agent_id)
                    .copied()
                    .unwrap_or(false)
                {
                    if let Some(hooks) = self.frontmatter.get(&agent_id) {
                        matched.extend(hooks.iter().filter(keep));
                    }
                }
            }
            None => {
                for (agent_id, hooks) in &self.frontmatter {
                    if !self
                        .frontmatter_is_agent
                        .get(agent_id)
                        .copied()
                        .unwrap_or(true)
                    {
                        matched.extend(hooks.iter().filter(keep));
                    }
                }
            }
        }
        matched.sort_by(|a, b| b.priority.cmp(&a.priority));
        drop_http_for_session_events(matched, &et)
    }

    /// Like [`Self::match_event`] but restricted to the frontmatter hooks scoped
    /// to a SINGLE `agent_id` (source / plugin / other-agent buckets excluded).
    ///
    /// Used by the child runner to fire `SubagentStop` against ONLY this agent's
    /// own frontmatter Stop→SubagentStop hooks (claude `runAgent` fires the
    /// subagent's stop hooks inside the child, runAgent.ts), without re-firing
    /// session / plugin `SubagentStop` hooks that the orchestrator-side
    /// chokepoint already covers. Returns `[]` when the agent registered no
    /// frontmatter hooks — a strict no-op for the common (no-frontmatter-hook)
    /// path so the child's behavior is byte-identical to legacy there.
    #[must_use]
    pub fn match_event_agent_scoped(
        &self,
        event: &HookEvent,
        agent_id: AgentId,
    ) -> Vec<&HookDefinition> {
        let Some(bucket) = self.frontmatter.get(&agent_id) else {
            return Vec::new();
        };
        let et = event.event_type();
        // Comma-mode (`atf`) widens the simple-pattern matcher (comma-separated
        // lists + the v2.1.195 hyphen) for this event type — claude
        // `matchesPattern`'s `n` argument. The port has no `toolAliases`, so the
        // `r` extra-alias map is `None`.
        let comma_mode = comma_mode_for(&et);
        let match_query = Self::match_query_for(event);
        let if_target = Self::if_match_target(event);
        let keep = |h: &&HookDefinition| -> bool {
            if !h.events.contains(&et) {
                return false;
            }
            let tool_name_ok = match (&match_query, h.matcher()) {
                (Some(query), Some(matcher)) => {
                    matches_pattern_with(query, matcher, comma_mode, None)
                }
                _ => true,
            };
            if !tool_name_ok {
                return false;
            }
            if let Some(if_cond) = h.if_pattern() {
                if Self::if_condition_applies(h) {
                    match &if_target {
                        Some((tool_name, tool_input)) => {
                            if !matches_if_condition(if_cond, tool_name, tool_input) {
                                return false;
                            }
                        }
                        None => return false,
                    }
                }
            }
            true
        };
        let mut matched: Vec<&HookDefinition> = bucket.iter().filter(keep).collect();
        matched.sort_by(|a, b| b.priority.cmp(&a.priority));
        matched
    }

    /// Match global hooks for an event whose owning agent already fired its own
    /// frontmatter bucket in-child.
    ///
    /// This is the orchestrator-chokepoint twin of [`Self::match_event_agent_scoped`]:
    /// the child runner fires a subagent's OWN frontmatter `Stop`→`SubagentStop`
    /// hooks in-child (agent-scoped, claude `runAgent`), so the chokepoint must
    /// fire session/plugin hooks without re-firing the child's bucket. Other
    /// concurrently-live agents' frontmatter hooks are excluded too: they do
    /// not own this event.
    #[must_use]
    pub fn match_event_excluding_agent(
        &self,
        event: &HookEvent,
        _exclude_agent_id: AgentId,
    ) -> Vec<&HookDefinition> {
        let et = event.event_type();
        // Comma-mode (`atf`) widens the simple-pattern matcher (comma-separated
        // lists + the v2.1.195 hyphen) for this event type — claude
        // `matchesPattern`'s `n` argument. The port has no `toolAliases`, so the
        // `r` extra-alias map is `None`.
        let comma_mode = comma_mode_for(&et);
        let match_query = Self::match_query_for(event);
        let if_target = Self::if_match_target(event);
        let keep = |h: &&HookDefinition| -> bool {
            if !h.events.contains(&et) {
                return false;
            }
            let tool_name_ok = match (&match_query, h.matcher()) {
                (Some(query), Some(matcher)) => {
                    matches_pattern_with(query, matcher, comma_mode, None)
                }
                _ => true,
            };
            if !tool_name_ok {
                return false;
            }
            if let Some(if_cond) = h.if_pattern() {
                if Self::if_condition_applies(h) {
                    match &if_target {
                        Some((tool_name, tool_input)) => {
                            if !matches_if_condition(if_cond, tool_name, tool_input) {
                                return false;
                            }
                        }
                        None => return false,
                    }
                }
            }
            true
        };
        let mut matched: Vec<&HookDefinition> =
            self.sources.values().flatten().filter(keep).collect();
        for hooks in self.plugin.values() {
            matched.extend(hooks.iter().filter(keep));
        }
        matched.sort_by(|a, b| b.priority.cmp(&a.priority));
        matched
    }

    /// Compute the `matchQuery` string for `event`, mirroring the per-event
    /// `switch (hook_event_name)` in claude-code `getMatchingHooks`
    /// (`utils/hooks.ts`, as of 2.1.195) — every case the binary derives a query
    /// for is reproduced here so the declared matcher actually filters that
    /// event. Events with no case (`TeammateIdle`/`TaskCreated`/`TaskCompleted`
    /// and `default`) derive no query and return `None`, so the matcher filter
    /// is skipped (TS `matchQuery ? filter : hookMatchers`) and subscribed hooks
    /// fire regardless of any declared matcher.
    ///
    /// claude's full switch:
    /// - tool events → `tool_name`;
    /// - `UserPromptExpansion` → `command_name`; `SessionStart` → `source`;
    ///   `SessionEnd` → `reason`; `StopFailure` → `error`;
    ///   `SubagentStart`/`SubagentStop` → `agent_type`; `Notification` →
    ///   `notification_type`; `Setup`/`PreCompact`/`PostCompact` → `trigger`;
    /// - `Elicitation`/`ElicitationResult` → `mcp_server_name`; `ConfigChange`
    ///   → `source`; `InstructionsLoaded` → `load_reason`; `FileChanged` →
    ///   `basename(file_path)`; model-switch events → `to_model`.
    ///
    /// `Setup` (trigger `init`/`maintenance`) and `PostCompact` (trigger
    /// `manual`/`auto`) both carry `trigger` on the port variant and derive
    /// their query from it, exactly like `PreCompact`.
    fn match_query_for(event: &HookEvent) -> Option<String> {
        match event {
            HookEvent::PreToolUse { tool_name, .. }
            | HookEvent::PostToolUse { tool_name, .. }
            | HookEvent::PostToolUseFailure { tool_name, .. }
            | HookEvent::PermissionRequest { tool_name, .. }
            | HookEvent::PermissionDenied { tool_name, .. } => Some(tool_name.clone()),
            // claude `i = r.command_name`.
            HookEvent::UserPromptExpansion { command_name, .. } => Some(command_name.clone()),
            // claude `i = r.source` — the session-start trigger source string.
            HookEvent::SessionStart { source, .. } => Some(source.clone()),
            // claude `i = r.reason`.
            HookEvent::SessionEnd { reason, .. } => Some(reason.clone()),
            // claude `i = r.error`.
            HookEvent::StopFailure { error, .. } => Some(error.clone()),
            // claude `i = r.agent_type` — the subagent's type (both events).
            HookEvent::SubagentStart { agent_type, .. }
            | HookEvent::SubagentStop { agent_type, .. } => Some(agent_type.clone()),
            // claude `i = r.notification_type`; the port carries it as `kind`.
            HookEvent::Notification { kind, .. } => Some(kind.clone()),
            // claude `i = r.trigger`; the port's PreCompact carries it as
            // `reason` (e.g. "manual"). `Setup` (init/maintenance) and
            // `PostCompact` (manual/auto) carry the trigger directly.
            HookEvent::PreCompact { reason, .. } => Some(reason.clone()),
            HookEvent::Setup { trigger } => Some(trigger.clone()),
            HookEvent::PostCompact { trigger, .. } => Some(trigger.clone()),
            // claude `i = r.to_model` for both model-switch events.
            HookEvent::PreModelSwitch { to_model, .. }
            | HookEvent::PostModelSwitch { to_model, .. } => Some(to_model.clone()),
            // claude `i = r.mcp_server_name` for both elicitation events.
            HookEvent::Elicitation { server_name, .. }
            | HookEvent::ElicitationResult { server_name, .. } => Some(server_name.clone()),
            // claude `i = r.source` — the settings layer that changed, as its
            // wire (snake_case) string.
            HookEvent::ConfigChange { source, .. } => {
                Some(config_change_source_wire(*source).to_string())
            }
            // claude `i = r.load_reason` — the (snake_case) reload reason.
            HookEvent::InstructionsLoaded { load_reason, .. } => {
                Some(load_reason_wire(*load_reason).to_string())
            }
            // claude `i = path.basename(r.file_path)`.
            HookEvent::FileChanged { path, .. } => {
                path.file_name().map(|n| n.to_string_lossy().into_owned())
            }
            // 2.1.220: DirectoryAdded matchers select the entry-point source,
            // never the directory path.
            HookEvent::DirectoryAdded { source, .. } => Some(source.clone()),
            // Events with no claude query (TeammateIdle/TaskCreated/
            // TaskCompleted, WorktreeCreate/Remove, CwdChanged, …) derive no
            // match query and fire regardless of any declared matcher.
            _ => None,
        }
    }

    /// The `(tool_name, tool_input)` an `if`-condition is evaluated against, or
    /// `None` when the event is not one `prepareIfConditionMatcher`
    /// (`utils/hooks.ts:1390-1401`) accepts.
    ///
    /// Faithful to that guard: ONLY `PreToolUse`, `PostToolUse`,
    /// `PostToolUseFailure`, and `PermissionRequest` yield a matcher. Notably
    /// `PermissionDenied` — which DOES produce a tool-name `matchQuery`
    /// ([`Self::match_query_for`]) — is deliberately excluded here (it also
    /// carries no `tool_input`), so an `if`-conditioned hook subscribed to
    /// `PermissionDenied` is dropped, exactly as in TS.
    fn if_match_target(event: &HookEvent) -> Option<(&str, &serde_json::Value)> {
        match event {
            HookEvent::PreToolUse {
                tool_name,
                tool_input,
                ..
            }
            | HookEvent::PostToolUse {
                tool_name,
                tool_input,
                ..
            }
            | HookEvent::PostToolUseFailure {
                tool_name,
                tool_input,
                ..
            }
            | HookEvent::PermissionRequest {
                tool_name,
                tool_input,
                ..
            } => Some((tool_name.as_str(), tool_input)),
            _ => None,
        }
    }

    /// Whether the `if`-condition filter applies to this hook's executor kind.
    ///
    /// TS gates the `if`-condition on `command` / `prompt` / `agent` / `http`
    /// hooks only (`utils/hooks.ts:1824-1832`); `callback` / `function` hooks
    /// bypass it. [`crate::HookExecutor::Builtin`] is the in-process analogue of
    /// the latter, so it is the sole exempt arm.
    fn if_condition_applies(hook: &HookDefinition) -> bool {
        !matches!(
            hook.executor,
            crate::definition::HookExecutor::Builtin { .. }
        )
    }

    /// Whether ANY registered hook (across every bucket) subscribes to
    /// `event_type`, ignoring per-hook matchers.
    ///
    /// This is the cheap *gate* check (no `HookContext`, no matcher
    /// evaluation, no payload) used to decide whether a best-effort lifecycle
    /// fire is worth arming at all — mirroring how the `engine-desktop`
    /// settings watcher only arms the `ConfigChange` fire path when a
    /// subscriber exists. The idle-prompt timer in the CLI repl consults this
    /// (via `ConversationOrchestrator::has_notification_hook`) so it never
    /// arms a useless timer when no `Notification` hook is registered. A
    /// `true` here does NOT guarantee a hook will run for a *specific* event
    /// (a declared matcher may still filter it out at `execute` time); it only
    /// reports event-type subscription, which is exactly what the gate needs.
    #[must_use]
    pub fn has_hooks_for(&self, event_type: &crate::events::HookEventType) -> bool {
        let subscribed = |h: &&HookDefinition| h.events.contains(event_type);
        self.sources.values().flatten().any(|h| subscribed(&h))
            || self.plugin.values().flatten().any(|h| subscribed(&h))
            || self
                .session_named
                .values()
                .flat_map(std::collections::HashMap::values)
                .any(|h| h.events.contains(event_type))
            || self.frontmatter.values().flatten().any(|h| subscribed(&h))
    }

    /// Snapshot every registered hook across all sources (user / project /
    /// local / managed / plugin / frontmatter / session / skill).
    ///
    /// Used by `OrchestratorHandle::list_hooks` (M6-07) so `/hooks` can
    /// list the registry without exposing the source-sharded internals.
    /// Returned in unspecified order — callers that need stable order
    /// should sort by `name`.
    #[must_use]
    pub fn all_hooks(&self) -> Vec<&HookDefinition> {
        let mut out: Vec<&HookDefinition> = self.sources.values().flatten().collect();
        out.extend(self.plugin.values().flatten());
        out.extend(self.session_named.values().flat_map(|hooks| hooks.values()));
        out.extend(self.frontmatter.values().flatten());
        out
    }

    /// Snapshot every `FileChanged` group matcher currently loaded across all
    /// buckets. The desktop file watcher resolves this same matcher set into
    /// watched paths.
    #[must_use]
    pub fn file_changed_matchers(&self) -> Vec<String> {
        self.all_hooks()
            .into_iter()
            .filter(|hook| hook.events.contains(&HookEventType::FileChanged))
            .filter_map(|hook| hook.matcher().map(ToString::to_string))
            .collect()
    }
}

/// Whether claude-code enables matcher "comma-mode" for `event_type` — the
/// `atf` set (`utils/hooks.ts`, claude-code 2.1.195). For these events the
/// simple-pattern matcher accepts comma-separated lists and the v2.1.195 hyphen
/// char class; every other event uses the classic pipe-only matcher. Mirrors
/// Binary `Bjo` (`getMatchingHooks`): HTTP hooks are NOT supported for the
/// `SessionStart` and `Setup` events, so they are dropped from the matched set
/// (with a skip log) — `R.filter(k=>k.hook.type==="http"?(log,!1):!0)`. A strict
/// no-op for every other event type, so non-session events are byte-identical.
fn drop_http_for_session_events<'a>(
    matched: Vec<&'a HookDefinition>,
    event_type: &HookEventType,
) -> Vec<&'a HookDefinition> {
    let event_label = match event_type {
        HookEventType::SessionStart => "SessionStart",
        HookEventType::Setup => "Setup",
        _ => return matched,
    };
    matched
        .into_iter()
        .filter(|h| match &h.executor {
            crate::definition::HookExecutor::Http { url, .. } => {
                // Byte-locked skip message (binary `Bjo`, em-dash U+2014).
                tracing::debug!(
                    "Skipping HTTP hook {url} \u{2014} HTTP hooks are not supported for {event_label}"
                );
                false
            }
            _ => true,
        })
        .collect()
}

/// Emit claude-code 2.1.195's bare-MCP-server-matcher deprecation
/// (`isBareMcpServerMatcher`) when a hook declares a `mcp__<server>` matcher
/// with no tool suffix. claude warns to use `mcp__<server>__.*` ("See CHANGELOG
/// v2.1.195"). The port surfaces it once at registration time (a natural
/// dedup point) rather than per-dispatch; it is a non-behavioral log.
fn warn_if_bare_mcp_matcher(hook: &HookDefinition) {
    if let Some(matcher) = hook.matcher() {
        if is_bare_mcp_server_matcher(matcher) {
            // Byte-exact wording from the binary `utf` warning (`{level:"warn"}`).
            tracing::warn!(
                matcher = %matcher,
                "Hook matcher `{matcher}` matches no tool (it is compared as an \
                 exact string). To match all tools from this server, use \
                 `{matcher}__.*`. See CHANGELOG v2.1.195."
            );
        }
    }
}

/// The wire (snake_case) string for a [`crate::events::ConfigChangeSource`] —
/// the `source` value claude uses as the `ConfigChange` match query.
fn config_change_source_wire(source: crate::events::ConfigChangeSource) -> &'static str {
    use crate::events::ConfigChangeSource as S;
    match source {
        S::UserSettings => "user_settings",
        S::ProjectSettings => "project_settings",
        S::LocalSettings => "local_settings",
        S::PolicySettings => "policy_settings",
        S::Skills => "skills",
    }
}

/// The wire (snake_case) string for a [`crate::events::InstructionsLoadReason`]
/// — the `load_reason` value claude uses as the `InstructionsLoaded` match
/// query.
fn load_reason_wire(reason: crate::events::InstructionsLoadReason) -> &'static str {
    use crate::events::InstructionsLoadReason as R;
    match reason {
        R::SessionStart => "session_start",
        R::NestedTraversal => "nested_traversal",
        R::PathGlobMatch => "path_glob_match",
        R::Include => "include",
        R::Compact => "compact",
    }
}

impl Default for HookRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod all_hooks_tests {
    use super::*;
    use crate::definition::{HookCondition, HookExecutor, HookSource};
    use crate::events::HookEventType;
    use lingxi_core::types::{HookId, PluginId};

    fn hk(name: &str, event: HookEventType, source: HookSource) -> HookDefinition {
        HookDefinition {
            id: HookId::new(),
            name: name.into(),
            events: vec![event],
            if_condition: None,
            executor: HookExecutor::Builtin {
                handler_id: "noop".into(),
            },
            source,
            blocking: true,
            timeout: None,
            priority: 0,
            once: false,
            status_message: None,
            async_rewake: false,
            async_timeout: None,
            rewake_message: None,
        }
    }

    fn hk_http(name: &str, event: HookEventType) -> HookDefinition {
        let mut h = hk(
            name,
            event,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
        );
        h.executor = HookExecutor::Http {
            url: "https://example.com/hook".into(),
            method: "POST".into(),
            headers: std::collections::HashMap::new(),
            allowed_env_vars: vec![],
            timeout: std::time::Duration::from_secs(5),
        };
        h
    }

    #[test]
    fn all_hooks_returns_empty_for_fresh_registry() {
        let r = HookRegistry::new();
        assert!(r.all_hooks().is_empty());
    }

    // Binary `Bjo`: HTTP hooks are dropped from SessionStart / Setup results
    // (unsupported), but fire normally for other events.
    #[test]
    fn http_hooks_dropped_for_sessionstart_and_setup_only() {
        let mut r = HookRegistry::new();
        r.register(hk_http("http-ss", HookEventType::SessionStart));
        r.register(hk(
            "builtin-ss",
            HookEventType::SessionStart,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
        ));
        r.register(hk_http("http-setup", HookEventType::Setup));
        r.register(hk_http("http-pre", HookEventType::PreToolUse));

        let ss = r.match_event(
            &HookEvent::SessionStart {
                session_id: lingxi_core::types::SessionId::new(),
                source: "cli".into(),
            },
            &HookContext::default(),
        );
        let ss_names: Vec<&str> = ss.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(
            ss_names,
            vec!["builtin-ss"],
            "HTTP hook dropped from SessionStart"
        );

        let setup = r.match_event(
            &HookEvent::Setup {
                trigger: "init".into(),
            },
            &HookContext::default(),
        );
        assert!(
            setup.is_empty(),
            "HTTP hook dropped from Setup; got {setup:?}"
        );

        // Non-session event: HTTP hook is kept.
        let pre = r.match_event(
            &HookEvent::PreToolUse {
                tool_name: "Bash".into(),
                tool_input: serde_json::json!({}),
                tool_use_id: lingxi_core::types::ToolUseId::new(),
            },
            &HookContext::default(),
        );
        let pre_names: Vec<&str> = pre.iter().map(|h| h.name.as_str()).collect();
        assert!(
            pre_names.contains(&"http-pre"),
            "HTTP hook fires for PreToolUse; got {pre_names:?}"
        );
    }

    #[test]
    fn remove_once_hook_drops_from_source_bucket_and_reports() {
        let mut r = HookRegistry::new();
        let keep = hk(
            "keep",
            HookEventType::PostToolUse,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
        );
        let drop = hk(
            "drop",
            HookEventType::PostToolUse,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
        );
        let drop_id = drop.id;
        let absent = HookId::new();
        r.register(keep);
        r.register(drop);

        // Removing a registered hook reports `true` and leaves only the other.
        assert!(r.remove_once_hook(drop_id), "registered hook is removed");
        let names: Vec<&str> = r.all_hooks().iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, vec!["keep"]);

        // Removing again (or removing an unknown id) reports `false`, no panic.
        assert!(!r.remove_once_hook(drop_id), "second removal is a no-op");
        assert!(!r.remove_once_hook(absent), "unknown id is a no-op");
    }

    #[test]
    fn remove_once_hook_drops_from_plugin_bucket() {
        let mut r = HookRegistry::new();
        let h = hk("plugin-once", HookEventType::PreToolUse, HookSource::Plugin);
        let id = h.id;
        r.register_plugin_hooks(lingxi_core::types::PluginId::new(), vec![h]);
        assert!(r.remove_once_hook(id));
        assert!(r.all_hooks().is_empty());
    }

    #[test]
    fn register_agent_hooks_retargets_stop_to_subagent_stop() {
        // G4: isAgent=true converts a Stop subscription to SubagentStop (claude
        // registerFrontmatterHooks isAgent=true).
        let mut r = HookRegistry::new();
        let agent = AgentId::new();
        let stop_hook = hk("agent-stop", HookEventType::Stop, HookSource::FrontMatter);
        r.register_agent_hooks(agent, &[stop_hook], true);
        // It fires on SubagentStop now, NOT Stop.
        let on_subagent_stop = r.match_event(
            &HookEvent::SubagentStop {
                agent_id: agent,
                status: "completed".into(),
                agent_type: String::new(),
            },
            &HookContext {
                agent_id: Some(agent),
                ..Default::default()
            },
        );
        assert_eq!(on_subagent_stop.len(), 1, "Stop retargeted to SubagentStop");
        let on_stop = r.match_event(
            &HookEvent::Stop { reason: "x".into() },
            &HookContext {
                agent_id: Some(agent),
                ..Default::default()
            },
        );
        assert!(on_stop.is_empty(), "no longer fires on plain Stop");
    }

    #[test]
    fn register_agent_hooks_non_agent_keeps_stop() {
        // is_agent=false (the skill-frontmatter path) registers verbatim.
        let mut r = HookRegistry::new();
        let agent = AgentId::new();
        r.register_agent_hooks(
            agent,
            &[hk(
                "skill-stop",
                HookEventType::Stop,
                HookSource::FrontMatter,
            )],
            false,
        );
        let on_stop = r.match_event(
            &HookEvent::Stop { reason: "x".into() },
            &HookContext::default(),
        );
        assert_eq!(on_stop.len(), 1, "Stop preserved when is_agent=false");
    }

    #[test]
    fn clear_agent_hooks_removes_only_that_agents_hooks() {
        let mut r = HookRegistry::new();
        let a = AgentId::new();
        let b = AgentId::new();
        r.register_agent_hooks(
            a,
            &[hk("a1", HookEventType::PreToolUse, HookSource::FrontMatter)],
            true,
        );
        r.register_agent_hooks(
            b,
            &[hk("b1", HookEventType::PreToolUse, HookSource::FrontMatter)],
            true,
        );
        assert_eq!(r.all_hooks().len(), 2);
        // Clearing `a` drops only a1.
        assert_eq!(r.clear_agent_hooks(a), 1);
        let names: Vec<&str> = r.all_hooks().iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, vec!["b1"]);
        // Clearing an unknown agent is a no-op.
        assert_eq!(r.clear_agent_hooks(AgentId::new()), 0);
    }

    #[test]
    fn replace_source_hooks_only_swaps_target_bucket() {
        let mut r = HookRegistry::new();
        r.register(hk(
            "user-stop",
            HookEventType::Stop,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
        ));
        r.register(hk(
            "project-stop",
            HookEventType::Stop,
            HookSource::Settings(lingxi_core::types::SettingsScope::Project),
        ));
        r.register(hk(
            "local-stop",
            HookEventType::Stop,
            HookSource::Settings(lingxi_core::types::SettingsScope::Local),
        ));
        r.register_plugin_hooks(
            PluginId::new(),
            vec![hk("plugin-stop", HookEventType::Stop, HookSource::Plugin)],
        );

        let result = r.replace_source_hooks(
            HookSource::Settings(lingxi_core::types::SettingsScope::Project),
            vec![hk(
                "project-new",
                HookEventType::Stop,
                HookSource::Settings(lingxi_core::types::SettingsScope::Project),
            )],
        );

        assert_eq!(result.replaced_count, 1);
        let names: Vec<&str> = r
            .all_hooks()
            .iter()
            .map(|hook| hook.name.as_str())
            .collect();
        assert!(names.contains(&"user-stop"));
        assert!(names.contains(&"project-new"));
        assert!(names.contains(&"local-stop"));
        assert!(names.contains(&"plugin-stop"));
        assert!(!names.contains(&"project-stop"));
    }

    #[test]
    fn replace_source_hooks_reports_file_changed_matcher_delta() {
        let mut r = HookRegistry::new();
        let mut user = hk(
            "watch-user",
            HookEventType::FileChanged,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
        );
        user.if_condition = Some(HookCondition {
            pattern: ".env".into(),
            match_tool_name: true,
            match_input: false,
            if_pattern: None,
        });
        let mut project = hk(
            "watch-project",
            HookEventType::FileChanged,
            HookSource::Settings(lingxi_core::types::SettingsScope::Project),
        );
        project.if_condition = Some(HookCondition {
            pattern: ".envrc".into(),
            match_tool_name: true,
            match_input: false,
            if_pattern: None,
        });
        r.register(user);
        r.register(project);

        let mut replacement = hk(
            "watch-project-new",
            HookEventType::FileChanged,
            HookSource::Settings(lingxi_core::types::SettingsScope::Project),
        );
        replacement.if_condition = Some(HookCondition {
            pattern: ".mise.toml".into(),
            match_tool_name: true,
            match_input: false,
            if_pattern: None,
        });
        let result = r.replace_source_hooks(
            HookSource::Settings(lingxi_core::types::SettingsScope::Project),
            vec![replacement],
        );
        let mut before = result.file_changed_matchers_before;
        before.sort();
        let mut after = result.file_changed_matchers_after;
        after.sort();

        assert_eq!(before, vec![".env".to_string(), ".envrc".to_string()]);
        assert_eq!(after, vec![".env".to_string(), ".mise.toml".to_string()]);
    }

    #[test]
    fn session_named_hooks_are_scoped_and_replaceable() {
        let mut r = HookRegistry::new();
        let a = SessionId::new();
        let b = SessionId::new();
        let first = hk(
            "old-name",
            HookEventType::Stop,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
        );
        let replaced = hk(
            "other-name",
            HookEventType::Stop,
            HookSource::Settings(lingxi_core::types::SettingsScope::Project),
        );
        let keep = hk(
            "keep",
            HookEventType::Stop,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
        );

        assert!(
            r.upsert_session_named_hook(a, "goal-stop".into(), first)
                .is_none()
        );
        let previous = r
            .upsert_session_named_hook(a, "goal-stop".into(), replaced)
            .expect("existing hook replaced");
        assert_eq!(previous.name, "goal-stop");
        assert_eq!(previous.source, HookSource::Session);
        r.upsert_session_named_hook(b, "goal-stop".into(), keep);

        let match_a = r.match_event(
            &HookEvent::Stop {
                reason: "done".into(),
            },
            &HookContext {
                session_id: a,
                ..Default::default()
            },
        );
        assert_eq!(match_a.len(), 1);
        assert_eq!(match_a[0].name, "goal-stop");
        assert_eq!(match_a[0].source, HookSource::Session);

        let match_b = r.match_event(
            &HookEvent::Stop {
                reason: "done".into(),
            },
            &HookContext {
                session_id: b,
                ..Default::default()
            },
        );
        assert_eq!(match_b.len(), 1);
        assert_eq!(match_b[0].name, "goal-stop");
        assert_ne!(match_b[0].id, match_a[0].id);

        assert!(r.remove_session_named_hook(a, "goal-stop").is_some());
        let after_remove = r.match_event(
            &HookEvent::Stop {
                reason: "done".into(),
            },
            &HookContext {
                session_id: a,
                ..Default::default()
            },
        );
        assert!(after_remove.is_empty());
        assert_eq!(r.clear_session_hooks(b), 1);
    }

    #[test]
    fn agent_scoped_hooks_fire_via_match_event() {
        // The frontmatter bucket fires only in the owning agent context.
        let mut r = HookRegistry::new();
        let agent = AgentId::new();
        r.register_agent_hooks(
            agent,
            &[hk("fm", HookEventType::PreToolUse, HookSource::FrontMatter)],
            true,
        );
        let matched = r.match_event(
            &HookEvent::PreToolUse {
                tool_name: "Bash".into(),
                tool_input: serde_json::json!({}),
                tool_use_id: lingxi_core::types::ToolUseId::new(),
            },
            &HookContext {
                agent_id: Some(agent),
                ..Default::default()
            },
        );
        assert_eq!(matched.len(), 1, "frontmatter hook fires");

        let parent = r.match_event(
            &HookEvent::PreToolUse {
                tool_name: "Bash".into(),
                tool_input: serde_json::json!({}),
                tool_use_id: lingxi_core::types::ToolUseId::new(),
            },
            &HookContext::default(),
        );
        assert!(parent.is_empty(), "subagent hook must not leak to parent");

        let sibling = r.match_event(
            &HookEvent::PreToolUse {
                tool_name: "Bash".into(),
                tool_input: serde_json::json!({}),
                tool_use_id: lingxi_core::types::ToolUseId::new(),
            },
            &HookContext {
                agent_id: Some(AgentId::new()),
                ..Default::default()
            },
        );
        assert!(sibling.is_empty(), "subagent hook must not leak to sibling");
    }

    #[test]
    fn match_event_agent_scoped_only_returns_that_agents_frontmatter() {
        // #9: the runner fires SubagentStop scoped to ITS agent's bucket only —
        // source / plugin / other-agent SubagentStop hooks are excluded so the
        // orchestrator chokepoint isn't double-fired.
        let mut r = HookRegistry::new();
        let a = AgentId::new();
        let b = AgentId::new();
        // a's frontmatter Stop→SubagentStop (the hook we want to fire).
        r.register_agent_hooks(
            a,
            &[hk("a-stop", HookEventType::Stop, HookSource::FrontMatter)],
            true,
        );
        // b's frontmatter SubagentStop — must NOT match for agent a.
        r.register_agent_hooks(
            b,
            &[hk("b-stop", HookEventType::Stop, HookSource::FrontMatter)],
            true,
        );
        // A SESSION-level SubagentStop hook — must NOT match the agent-scoped call.
        r.register(hk(
            "session-stop",
            HookEventType::SubagentStop,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
        ));

        let ev = HookEvent::SubagentStop {
            agent_id: a,
            status: "completed".into(),
            agent_type: String::new(),
        };
        let scoped = r.match_event_agent_scoped(&ev, a);
        let names: Vec<&str> = scoped.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["a-stop"],
            "only agent a's own frontmatter SubagentStop fires: {names:?}"
        );

        // The general agent-context matcher sees global hooks plus agent a's
        // bucket, but never sibling b's bucket.
        let all = r.match_event(
            &ev,
            &HookContext {
                agent_id: Some(a),
                ..Default::default()
            },
        );
        let all_names: Vec<&str> = all.iter().map(|hook| hook.name.as_str()).collect();
        assert!(all_names.contains(&"a-stop"));
        assert!(all_names.contains(&"session-stop"));
        assert!(!all_names.contains(&"b-stop"));
    }

    #[test]
    fn match_event_excluding_agent_omits_all_agent_frontmatter() {
        // The chokepoint fires global hooks only. Each agent's own frontmatter
        // hook is fired in-child; sibling buckets never own this event.
        let mut r = HookRegistry::new();
        let a = AgentId::new();
        let b = AgentId::new();
        // a's frontmatter Stop→SubagentStop — MUST be excluded for agent a.
        r.register_agent_hooks(
            a,
            &[hk("a-stop", HookEventType::Stop, HookSource::FrontMatter)],
            true,
        );
        // b's frontmatter Stop→SubagentStop must not fire for agent a.
        r.register_agent_hooks(
            b,
            &[hk("b-stop", HookEventType::Stop, HookSource::FrontMatter)],
            true,
        );
        // A SESSION-level SubagentStop hook — always fires (the chokepoint owns it).
        r.register(hk(
            "session-stop",
            HookEventType::SubagentStop,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
        ));

        let ev = HookEvent::SubagentStop {
            agent_id: a,
            status: "completed".into(),
            agent_type: String::new(),
        };
        let mut names: Vec<&str> = r
            .match_event_excluding_agent(&ev, a)
            .iter()
            .map(|h| h.name.as_str())
            .collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec!["session-stop"],
            "only global hooks fire at the chokepoint: {names:?}"
        );

        // General agent-context matching includes the owner but not the sibling.
        let all = r.match_event(
            &ev,
            &HookContext {
                agent_id: Some(a),
                ..Default::default()
            },
        );
        let all_names: Vec<&str> = all.iter().map(|hook| hook.name.as_str()).collect();
        assert!(all_names.contains(&"a-stop"));
        assert!(all_names.contains(&"session-stop"));
        assert!(!all_names.contains(&"b-stop"));
    }

    #[test]
    fn match_event_excluding_agent_equals_match_event_when_excluded_agent_has_no_frontmatter() {
        // FakeAgentTool fixtures: the fake child id has NO frontmatter bucket, so
        // excluding it is byte-identical to the general match (fixtures green).
        let mut r = HookRegistry::new();
        r.register(hk(
            "session-stop",
            HookEventType::SubagentStop,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
        ));
        let unknown = AgentId::new();
        let ev = HookEvent::SubagentStop {
            agent_id: unknown,
            status: "completed".into(),
            agent_type: String::new(),
        };
        let excluded: Vec<&str> = r
            .match_event_excluding_agent(&ev, unknown)
            .iter()
            .map(|h| h.name.as_str())
            .collect();
        let general: Vec<&str> = r
            .match_event(&ev, &HookContext::default())
            .iter()
            .map(|h| h.name.as_str())
            .collect();
        assert_eq!(
            excluded, general,
            "no excluded frontmatter ⇒ identical to general"
        );
        assert_eq!(excluded, vec!["session-stop"]);
    }

    #[test]
    fn match_event_agent_scoped_empty_for_unknown_agent() {
        let r = HookRegistry::new();
        let scoped = r.match_event_agent_scoped(
            &HookEvent::SubagentStop {
                agent_id: AgentId::new(),
                status: "completed".into(),
                agent_type: String::new(),
            },
            AgentId::new(),
        );
        assert!(scoped.is_empty(), "no frontmatter hooks ⇒ empty");
    }

    #[test]
    fn all_hooks_unions_source_and_plugin_buckets() {
        let mut r = HookRegistry::new();
        r.register(hk(
            "user-fmt",
            HookEventType::PostToolUse,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
        ));
        r.register(hk(
            "project-lint",
            HookEventType::Stop,
            HookSource::Settings(lingxi_core::types::SettingsScope::Project),
        ));
        r.register_plugin_hooks(
            lingxi_core::types::PluginId::new(),
            vec![hk(
                "plugin-x",
                HookEventType::PreToolUse,
                HookSource::Plugin,
            )],
        );

        let names: Vec<&str> = r.all_hooks().iter().map(|h| h.name.as_str()).collect();
        assert!(names.contains(&"user-fmt"));
        assert!(names.contains(&"project-lint"));
        assert!(names.contains(&"plugin-x"));
        assert_eq!(names.len(), 3);
    }
}

#[cfg(test)]
mod match_event_matcher_tests {
    //! B3 matcher integration: `match_event` drops hooks whose declared
    //! tool-name matcher does not match the event's tool, while no-matcher
    //! hooks keep firing for any subscribed event.
    use super::*;
    use crate::definition::{HookCondition, HookExecutor, HookSource};
    use crate::events::HookEventType;
    use lingxi_core::types::{HookId, ToolUseId};

    fn hook_with(name: &str, event: HookEventType, matcher: Option<&str>) -> HookDefinition {
        HookDefinition {
            id: HookId::new(),
            name: name.into(),
            events: vec![event],
            if_condition: matcher.map(|m| HookCondition {
                pattern: m.into(),
                match_tool_name: true,
                match_input: false,
                if_pattern: None,
            }),
            executor: HookExecutor::Builtin {
                handler_id: "noop".into(),
            },
            source: HookSource::Settings(lingxi_core::types::SettingsScope::User),
            blocking: true,
            timeout: None,
            priority: 0,
            once: false,
            status_message: None,
            async_rewake: false,
            async_timeout: None,
            rewake_message: None,
        }
    }

    /// A hook carrying an `if`-condition (and optional tool-name `matcher`),
    /// executed by a Command arm so the `if`-condition filter actually applies
    /// (Builtin hooks bypass it, mirroring TS `callback`/`function`).
    fn hook_with_if(
        name: &str,
        event: HookEventType,
        matcher: Option<&str>,
        if_cond: &str,
    ) -> HookDefinition {
        HookDefinition {
            id: HookId::new(),
            name: name.into(),
            events: vec![event],
            if_condition: Some(HookCondition {
                pattern: matcher.unwrap_or_default().into(),
                match_tool_name: matcher.is_some(),
                match_input: true,
                if_pattern: Some(if_cond.into()),
            }),
            executor: HookExecutor::Command {
                command: "./noop.sh".into(),
                args: vec![],
                env: std::collections::HashMap::new(),
                cwd: None,
                shell: None,
            },
            source: HookSource::Settings(lingxi_core::types::SettingsScope::User),
            blocking: true,
            timeout: None,
            priority: 0,
            once: false,
            status_message: None,
            async_rewake: false,
            async_timeout: None,
            rewake_message: None,
        }
    }

    fn pre_tool_use_in(tool: &str, input: serde_json::Value) -> HookEvent {
        HookEvent::PreToolUse {
            tool_name: tool.into(),
            tool_input: input,
            tool_use_id: ToolUseId::new(),
        }
    }

    fn pre_tool_use(tool: &str) -> HookEvent {
        HookEvent::PreToolUse {
            tool_name: tool.into(),
            tool_input: serde_json::json!({}),
            tool_use_id: ToolUseId::new(),
        }
    }

    fn matched_names(reg: &HookRegistry, event: &HookEvent) -> Vec<String> {
        reg.match_event(event, &HookContext::default())
            .iter()
            .map(|h| h.name.clone())
            .collect()
    }

    #[test]
    fn write_matcher_hook_does_not_fire_on_bash_event() {
        let mut reg = HookRegistry::new();
        reg.register(hook_with(
            "write-only",
            HookEventType::PreToolUse,
            Some("Write"),
        ));
        // Fires on a Write tool event.
        assert_eq!(
            matched_names(&reg, &pre_tool_use("Write")),
            vec!["write-only"]
        );
        // Dropped on a Bash tool event — the declared matcher does not match.
        assert!(matched_names(&reg, &pre_tool_use("Bash")).is_empty());
    }

    #[test]
    fn no_matcher_hook_fires_on_any_tool_event() {
        let mut reg = HookRegistry::new();
        reg.register(hook_with("always", HookEventType::PreToolUse, None));
        // A hook with no matcher fires regardless of the tool name.
        assert_eq!(matched_names(&reg, &pre_tool_use("Bash")), vec!["always"]);
        assert_eq!(matched_names(&reg, &pre_tool_use("Write")), vec!["always"]);
        assert_eq!(matched_names(&reg, &pre_tool_use("Read")), vec!["always"]);
    }

    #[test]
    fn pipe_matcher_fires_on_any_segment() {
        let mut reg = HookRegistry::new();
        reg.register(hook_with(
            "fmt",
            HookEventType::PreToolUse,
            Some("Write|Edit"),
        ));
        assert_eq!(matched_names(&reg, &pre_tool_use("Write")), vec!["fmt"]);
        assert_eq!(matched_names(&reg, &pre_tool_use("Edit")), vec!["fmt"]);
        assert!(matched_names(&reg, &pre_tool_use("Bash")).is_empty());
    }

    #[test]
    fn star_matcher_fires_on_any_tool() {
        let mut reg = HookRegistry::new();
        reg.register(hook_with("star", HookEventType::PreToolUse, Some("*")));
        assert_eq!(matched_names(&reg, &pre_tool_use("Bash")), vec!["star"]);
        assert_eq!(matched_names(&reg, &pre_tool_use("Write")), vec!["star"]);
    }

    #[test]
    fn regex_matcher_filters_tool_name() {
        let mut reg = HookRegistry::new();
        reg.register(hook_with(
            "bashish",
            HookEventType::PreToolUse,
            Some("^Bash.*"),
        ));
        assert_eq!(matched_names(&reg, &pre_tool_use("Bash")), vec!["bashish"]);
        assert_eq!(
            matched_names(&reg, &pre_tool_use("BashOutput")),
            vec!["bashish"]
        );
        assert!(matched_names(&reg, &pre_tool_use("Write")).is_empty());
    }

    #[test]
    fn matcher_filter_skipped_for_non_tool_event_with_query() {
        // A Stop hook with a (tool-name) matcher still fires on a Stop event,
        // because Stop has no tool match-query (TS leaves matchQuery undefined),
        // so the matcher filter is skipped. This guards the no-regression rule.
        let mut reg = HookRegistry::new();
        reg.register(hook_with("stopper", HookEventType::Stop, Some("Write")));
        let event = HookEvent::Stop {
            reason: "done".into(),
        };
        assert_eq!(matched_names(&reg, &event), vec!["stopper"]);
    }

    #[test]
    fn post_compact_matcher_filters_on_trigger() {
        // P2-04: `PostCompact` derives its matchQuery from `trigger`
        // (`manual`/`auto`), so a hook declaring matcher `"manual"` fires on a
        // manual compaction and is dropped on an auto one.
        let mut reg = HookRegistry::new();
        reg.register(hook_with(
            "on-manual",
            HookEventType::PostCompact,
            Some("manual"),
        ));
        let manual = HookEvent::PostCompact {
            summary: "s".into(),
            tokens_freed: 0,
            trigger: "manual".into(),
        };
        let auto = HookEvent::PostCompact {
            summary: "s".into(),
            tokens_freed: 0,
            trigger: "auto".into(),
        };
        assert_eq!(matched_names(&reg, &manual), vec!["on-manual"]);
        assert!(matched_names(&reg, &auto).is_empty());
    }

    #[test]
    fn model_switch_matcher_filters_on_target_model() {
        let mut reg = HookRegistry::new();
        reg.register(hook_with(
            "opus-only",
            HookEventType::PreModelSwitch,
            Some("claude-opus-4-6"),
        ));
        let event = |to_model: &str| HookEvent::PreModelSwitch {
            from_model: "claude-sonnet-4-6".into(),
            to_model: to_model.into(),
            requested_model: None,
            source: "picker".into(),
            context_tokens: 0,
            prompt_cache_warm: false,
            cache_ttl: "5m".into(),
            estimated_cache_write_usd: 0.0,
            pricing: "default".into(),
        };
        assert_eq!(
            matched_names(&reg, &event("claude-opus-4-6")),
            vec!["opus-only"]
        );
        assert!(matched_names(&reg, &event("claude-haiku-4-5")).is_empty());
    }

    #[test]
    fn setup_matcher_filters_on_trigger() {
        // P2-04: `Setup` derives its matchQuery from `trigger`
        // (`init`/`maintenance`); a hook matcher of `"maintenance"` fires on a
        // maintenance Setup and is dropped on an init one.
        let mut reg = HookRegistry::new();
        reg.register(hook_with(
            "on-maint",
            HookEventType::Setup,
            Some("maintenance"),
        ));
        let maint = HookEvent::Setup {
            trigger: "maintenance".into(),
        };
        let init = HookEvent::Setup {
            trigger: "init".into(),
        };
        assert_eq!(matched_names(&reg, &maint), vec!["on-maint"]);
        assert!(matched_names(&reg, &init).is_empty());
    }

    #[test]
    fn directory_added_matcher_filters_on_source_not_path() {
        let mut reg = HookRegistry::new();
        reg.register(hook_with(
            "sdk-only",
            HookEventType::DirectoryAdded,
            Some("register_repo_root"),
        ));
        let sdk = HookEvent::DirectoryAdded {
            directory: "/work/other".into(),
            source: "register_repo_root".into(),
        };
        let slash = HookEvent::DirectoryAdded {
            directory: "register_repo_root".into(),
            source: "slash_command".into(),
        };
        assert_eq!(matched_names(&reg, &sdk), vec!["sdk-only"]);
        assert!(
            matched_names(&reg, &slash).is_empty(),
            "directory text must not satisfy a source matcher"
        );
    }

    #[test]
    fn matcher_filter_applies_to_plugin_hooks_too() {
        let mut reg = HookRegistry::new();
        reg.register_plugin_hooks(
            lingxi_core::types::PluginId::new(),
            vec![hook_with(
                "plugin-write",
                HookEventType::PreToolUse,
                Some("Write"),
            )],
        );
        assert_eq!(
            matched_names(&reg, &pre_tool_use("Write")),
            vec!["plugin-write"]
        );
        assert!(matched_names(&reg, &pre_tool_use("Bash")).is_empty());
    }

    // ── `if`-condition wiring (B3 second half) ────────────────────────────────

    #[test]
    fn if_condition_fires_only_on_matching_tool_input() {
        // `if: "Bash(git push:*)"` fires on `git push …`, not on `git status`.
        let mut reg = HookRegistry::new();
        reg.register(hook_with_if(
            "guard",
            HookEventType::PreToolUse,
            Some("Bash"),
            "Bash(git push:*)",
        ));
        let pushing = pre_tool_use_in(
            "Bash",
            serde_json::json!({ "command": "git push origin x" }),
        );
        assert_eq!(matched_names(&reg, &pushing), vec!["guard"]);
        let status = pre_tool_use_in("Bash", serde_json::json!({ "command": "git status" }));
        assert!(matched_names(&reg, &status).is_empty());
    }

    #[test]
    fn no_if_condition_fires_on_tool_name_match_alone() {
        // Absent `if` → tool-name match alone (prior behavior preserved).
        let mut reg = HookRegistry::new();
        reg.register(hook_with("plain", HookEventType::PreToolUse, Some("Bash")));
        let any = pre_tool_use_in("Bash", serde_json::json!({ "command": "rm -rf /" }));
        assert_eq!(matched_names(&reg, &any), vec!["plain"]);
    }

    #[test]
    fn both_tool_name_matcher_and_if_must_pass() {
        // matcher "Bash" AND if "Bash(git push:*)" — both gates enforced.
        let mut reg = HookRegistry::new();
        reg.register(hook_with_if(
            "both",
            HookEventType::PreToolUse,
            Some("Bash"),
            "Bash(git push:*)",
        ));
        // tool-name matcher fails (Write != Bash) → dropped even though the
        // event isn't even a git push.
        let on_write = pre_tool_use_in("Write", serde_json::json!({ "file_path": "/a" }));
        assert!(matched_names(&reg, &on_write).is_empty());
        // tool-name matcher passes but the `if` content fails → dropped.
        let wrong_cmd = pre_tool_use_in("Bash", serde_json::json!({ "command": "git pull" }));
        assert!(matched_names(&reg, &wrong_cmd).is_empty());
        // both pass → fires.
        let ok = pre_tool_use_in("Bash", serde_json::json!({ "command": "git push" }));
        assert_eq!(matched_names(&reg, &ok), vec!["both"]);
    }

    #[test]
    fn malformed_if_condition_does_not_fire() {
        // Unbalanced paren → parses as a bare tool name that won't equal "Bash".
        let mut reg = HookRegistry::new();
        reg.register(hook_with_if(
            "bad",
            HookEventType::PreToolUse,
            None,
            "Bash(git push:*",
        ));
        let ev = pre_tool_use_in("Bash", serde_json::json!({ "command": "git push" }));
        assert!(matched_names(&reg, &ev).is_empty());
    }

    #[test]
    fn if_condition_hook_dropped_on_non_if_evaluable_event() {
        // PermissionDenied yields a tool-name matchQuery but is NOT in the
        // prepareIfConditionMatcher set → an `if`-conditioned hook is dropped.
        let mut reg = HookRegistry::new();
        reg.register(hook_with_if(
            "denied-guard",
            HookEventType::PermissionDenied,
            None,
            "Bash(git push:*)",
        ));
        let ev = HookEvent::PermissionDenied {
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({ "command": "git push" }),
            tool_use_id: ToolUseId::new(),
            reason: "no".into(),
        };
        assert!(matched_names(&reg, &ev).is_empty());
    }

    #[test]
    fn builtin_hook_bypasses_if_condition() {
        // A Builtin (callback/function analogue) hook with an `if` ignores it —
        // TS applies the `if` filter only to command/prompt/agent/http hooks.
        let mut reg = HookRegistry::new();
        let mut h = hook_with("builtin", HookEventType::PreToolUse, Some("Bash"));
        // Attach an `if` that would NOT match the event, yet a Builtin bypasses it.
        if let Some(c) = h.if_condition.as_mut() {
            c.if_pattern = Some("Bash(git push:*)".into());
            c.match_input = true;
        }
        reg.register(h);
        let non_matching = pre_tool_use_in("Bash", serde_json::json!({ "command": "git status" }));
        assert_eq!(matched_names(&reg, &non_matching), vec!["builtin"]);
    }

    // ── comma-mode dispatch + non-tool-event match queries (claude 2.1.195) ────

    #[test]
    fn comma_mode_tool_event_applies_comma_separated_matcher() {
        // PreToolUse is in the comma-mode set, so a comma-separated matcher list
        // filters by tool name across both `,` and spaces.
        let mut reg = HookRegistry::new();
        reg.register(hook_with(
            "multi",
            HookEventType::PreToolUse,
            Some("Write, Edit"),
        ));
        assert_eq!(matched_names(&reg, &pre_tool_use("Write")), vec!["multi"]);
        assert_eq!(matched_names(&reg, &pre_tool_use("Edit")), vec!["multi"]);
        assert!(matched_names(&reg, &pre_tool_use("Bash")).is_empty());
    }

    #[test]
    fn elicitation_event_filters_by_server_name() {
        // Elicitation derives its match query from the MCP server name, and the
        // event is in comma-mode so a hyphenated server name matches as a simple
        // entry (the v2.1.195 hyphen).
        let mut reg = HookRegistry::new();
        reg.register(hook_with(
            "srv",
            HookEventType::Elicitation,
            Some("mcp__my-server"),
        ));
        let elicit = |server: &str| HookEvent::Elicitation {
            server_name: server.into(),
            message: "pick one".into(),
            mode: None,
            url: None,
            elicitation_id: None,
            requested_schema: None,
        };
        assert_eq!(
            matched_names(&reg, &elicit("mcp__my-server")),
            vec!["srv"],
            "matching server name fires"
        );
        assert!(
            matched_names(&reg, &elicit("mcp__other")).is_empty(),
            "non-matching server name is filtered out"
        );
    }

    #[test]
    fn file_changed_event_filters_by_basename() {
        // FileChanged derives its query from the changed file's basename.
        let mut reg = HookRegistry::new();
        reg.register(hook_with(
            "watch",
            HookEventType::FileChanged,
            Some("config.toml"),
        ));
        let changed = |p: &str| HookEvent::FileChanged {
            path: std::path::PathBuf::from(p),
            kind: "modify".into(),
        };
        assert_eq!(
            matched_names(&reg, &changed("/proj/src/config.toml")),
            vec!["watch"],
            "basename matches"
        );
        assert!(
            matched_names(&reg, &changed("/proj/src/main.rs")).is_empty(),
            "different basename is filtered out"
        );
    }

    #[test]
    fn session_start_event_filters_by_source() {
        // SessionStart derives its query from the `source` trigger string.
        let mut reg = HookRegistry::new();
        reg.register(hook_with(
            "boot",
            HookEventType::SessionStart,
            Some("startup"),
        ));
        let start = |src: &str| HookEvent::SessionStart {
            session_id: SessionId::new(),
            source: src.into(),
        };
        assert_eq!(matched_names(&reg, &start("startup")), vec!["boot"]);
        assert!(matched_names(&reg, &start("resume")).is_empty());
    }

    #[test]
    fn subagent_start_event_filters_by_agent_type() {
        // SubagentStart derives its query from the subagent type; the event is
        // in comma-mode so a hyphenated agent type matches as a simple entry.
        let mut reg = HookRegistry::new();
        reg.register(hook_with(
            "review",
            HookEventType::SubagentStart,
            Some("code-reviewer"),
        ));
        let start = |ty: &str| HookEvent::SubagentStart {
            agent_id: AgentId::new(),
            agent_type: ty.into(),
            parent_agent_id: None,
        };
        assert_eq!(matched_names(&reg, &start("code-reviewer")), vec!["review"]);
        assert!(matched_names(&reg, &start("general-purpose")).is_empty());
    }

    #[test]
    fn subagent_stop_event_filters_by_agent_type() {
        // SubagentStop now carries `agent_type` and filters by it, like
        // SubagentStart (claude `i = r.agent_type`).
        let mut reg = HookRegistry::new();
        reg.register(hook_with(
            "review-stop",
            HookEventType::SubagentStop,
            Some("code-reviewer"),
        ));
        let stop = |ty: &str| HookEvent::SubagentStop {
            agent_id: AgentId::new(),
            status: "completed".into(),
            agent_type: ty.into(),
        };
        assert_eq!(
            matched_names(&reg, &stop("code-reviewer")),
            vec!["review-stop"],
            "matching agent_type fires"
        );
        assert!(
            matched_names(&reg, &stop("general-purpose")).is_empty(),
            "non-matching agent_type is filtered out"
        );
    }
}
