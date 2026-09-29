//! System-prompt and leading-context assembly.

use super::*;

fn prompt_tool_descriptions(
    wire_tools: &[serde_json::Value],
) -> Vec<lingxi_core::host::PromptToolDescription> {
    let mut seen = std::collections::HashSet::new();
    wire_tools
        .iter()
        .filter(|tool| {
            !tool
                .get("defer_loading")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        })
        .filter_map(|tool| {
            let name = tool.get("name")?.as_str()?.trim();
            let description = tool.get("description")?.as_str()?;
            if name.is_empty() || !seen.insert(name.to_owned()) {
                return None;
            }
            Some(lingxi_core::host::PromptToolDescription {
                name: name.to_owned(),
                description: description.to_owned(),
            })
        })
        .collect()
}

fn valid_prompt_snapshot(snapshot: &lingxi_core::host::PromptSnapshot) -> bool {
    !snapshot.system_prompt.is_empty()
        && snapshot.system_prompt.iter().all(|part| !part.is_empty())
        && snapshot.tools.iter().all(|tool| !tool.name.is_empty())
}

impl ConversationOrchestrator {
    /// Assemble the system prompt this orchestrator would send on the next
    /// turn, WITHOUT running a turn (no API call, no message mutation).
    ///
    /// Honors the same `system_prompt_override` bypass as [`Self::run_turn`]:
    /// returns the override verbatim when set, otherwise the freshly assembled
    /// prompt (cwd / git / file-tree / **memory** / tool-name context). This is
    /// a read-only introspection seam — it lets a host/composition-root test
    /// prove that its injected [`crate::prompt::MemoryHierarchyProvider`]
    /// (e.g. a controlled `StaticMemoryProvider`, or the production
    /// `real_provider()`) actually reaches the system prompt, without a live
    /// model round-trip.
    pub async fn assemble_system_prompt_preview(&self) -> String {
        self.effective_system_prompt().await
    }

    /// Assemble the live default system prompt without applying the main
    /// thread's `--system-prompt` / adopted-agent overrides.
    ///
    /// In-process teammates use the same default prompt builder as Claude Code,
    /// then append their teammate addendum and optional custom agent prompt.
    /// Keeping this as a read-only renderer avoids copying the dynamic cwd/git/
    /// memory/tool assembly into the task-handler leaf.
    pub async fn assemble_default_system_prompt_preview(&self) -> String {
        self.build_system_prompt().await
    }

    /// Adopt a `--agent`-resolved definition for the MAIN conversation loop
    /// (claude-code `bde(agentDef.agentType)` + `mainThreadAgentDefinition`).
    /// Called ONCE at startup by the composition root when `--agent` resolves to
    /// a catalog hit — the resolution runs against the FINAL agent catalog after
    /// this orchestrator is already `Arc`-wrapped, so the seam is interior-mutable.
    /// After this, `agent_type` rides every main-thread lifecycle hook payload,
    /// `system_prompt` (when `Some`) becomes the main-loop system prompt on every
    /// query — `--system-prompt` (`system_prompt_override`) still winning — and
    /// `tool_policy` / `disallowed_tools` narrow the advertised tool pool (claude
    /// `HJ(agentDef,to,!1,!0)`).
    ///
    /// `model_override` is the agent's frontmatter `model` ALREADY resolved to a
    /// concrete wire id and gated by the caller (claude-code
    /// `if(!userSpecifiedModel&&y.model&&y.model!=="inherit"){jb(Zo(y.model))}` —
    /// the `!userSpecifiedModel` / `!=="inherit"` checks live at the composition
    /// root, which owns `--model`). When `Some`, it replaces the session model
    /// (profile cleared: agent frontmatter carries a bare id, no provider profile).
    pub async fn set_main_thread_agent(
        &self,
        agent_type: String,
        system_prompt: Option<String>,
        tool_policy: agent::AgentToolPolicy,
        disallowed_tools: Vec<String>,
        model_override: Option<String>,
    ) {
        if let Some(model) = model_override {
            let mut s = self.session.lock().await;
            s.model = model;
            s.model_profile = None;
        }
        *self.lifecycle_runtime.main_thread_agent.write().await = Some(MainThreadAgentState {
            agent_type,
            system_prompt,
            tool_policy,
            disallowed_tools,
        });
    }

    /// Replace the frontmatter-hook bucket owned by the main-thread agent.
    /// Normal startup installs it once; hot resume calls this again so the
    /// previous session's hooks are removed before the resumed agent's hooks
    /// become visible.
    pub async fn replace_main_thread_agent_hooks(&self, definitions: &[hooks::HookDefinition]) {
        let previous = self
            .lifecycle_runtime
            .main_thread_agent_hook_id
            .lock()
            .await
            .take();
        if let Some(previous) = previous {
            self.hooks.clear_agent_hooks(previous).await;
        }
        if definitions.is_empty() {
            return;
        }
        let agent_id = lingxi_core::types::AgentId::new();
        self.hooks
            .register_agent_hooks(agent_id, definitions, false)
            .await;
        *self
            .lifecycle_runtime
            .main_thread_agent_hook_id
            .lock()
            .await = Some(agent_id);
    }

    /// Restore the main-thread agent selected by a resumed session. Prefer the
    /// immutable, integrity-checked transcript snapshot; legacy transcripts
    /// fall back to the current live catalog by agent type. A missing or
    /// unresolvable selection explicitly restores default behavior instead of
    /// retaining state from the session that was previously mounted.
    pub(crate) async fn restore_main_thread_agent_from_resume(
        &self,
        wanted: Option<String>,
        snapshot: Option<serde_json::Value>,
    ) {
        let mut resolved = match (wanted.as_deref(), snapshot) {
            (Some(wanted), Some(value)) => serde_json::from_value::<agent::AgentDefinition>(value)
                .ok()
                .filter(|definition| definition.agent_type == wanted),
            _ => None,
        };

        if resolved.is_none() {
            if let (Some(wanted), Some(catalog)) = (
                wanted.as_deref(),
                self.lifecycle_runtime.agent_catalog.as_ref(),
            ) {
                let catalog = catalog.read().await;
                // CC 2.1.218 resolves the resumed agentType by EXACT equality
                // only — no bare-name/suffix fallback (that belongs to the
                // `--agent` startup surface, not resume).
                resolved = catalog
                    .iter()
                    .find(|definition| definition.agent_type == wanted)
                    .cloned();
            }
        }

        match resolved {
            Some(definition) => {
                // (cc 2.1.218 `mvo`) ORIGIN TRUST — the RESUME surface. The
                // resumed `agent-setting` snapshot carries the definition's
                // `frontmatter_hooks` verbatim, so without this check a
                // definition whose hooks were correctly REFUSED at `--agent`
                // time would be silently installed on the next resume of that
                // session. Evaluated BEFORE the fields are moved below.
                let hooks_trusted =
                    agent::hooks_trust::agent_hooks_origin_trusted(&definition, &self.cwd);
                if !hooks_trusted {
                    agent::hooks_trust::report_untrusted_hooks(
                        &definition,
                        &self.cwd,
                        agent::hooks_trust::HooksTrustSurface::MainThread,
                        false,
                    );
                }
                // (gap218 #43 / cc 2.1.218 `NQe`) Adopt the resumed agent's
                // frontmatter `model`, resolved to a wire id — the hot-resume twin
                // of the composition root's COLD-resume gate. Applied ONLY when the
                // user did NOT pass `--model` (`apply_resumed_agent_model` is
                // `!default_model_explicit`, set at the root) AND the agent declares
                // a concrete model (`AgentModel != Inherit`, oracle `i.model &&
                // i.model!=="inherit"`); an explicit `--model` is never overridden.
                // `resolve_user_specified_model` is `Zo` (alias → wire id).
                // Evaluated BEFORE `definition.*` moves into the call below.
                let model_override = if self.config.apply_resumed_agent_model {
                    match &definition.model {
                        agent::AgentModel::Alias(spec) | agent::AgentModel::Explicit(spec) => {
                            Some(agent::model_resolution::resolve_user_specified_model(spec))
                        }
                        agent::AgentModel::Inherit => None,
                    }
                } else {
                    None
                };
                self.set_main_thread_agent(
                    definition.agent_type,
                    definition.system_prompt,
                    definition.tools,
                    definition.disallowed_tools,
                    model_override,
                )
                .await;
                if hooks_trusted {
                    self.replace_main_thread_agent_hooks(&definition.frontmatter_hooks)
                        .await;
                } else {
                    // `QEt`'s untrusted arm ends in `b1r(void 0)` — it CLEARS the
                    // main-thread bucket rather than leaving it alone. Skipping
                    // the clear would strand the PREVIOUS session's hooks: an
                    // in-place resume from a trusted folder A into an untrusted
                    // session B would keep A's hook commands firing under B.
                    self.replace_main_thread_agent_hooks(&[]).await;
                }
            }
            None => {
                if let Some(wanted) = wanted {
                    tracing::warn!(
                        "Resumed session had agent \"{wanted}\" but it is no longer available. Using default behavior."
                    );
                }
                *self.lifecycle_runtime.main_thread_agent.write().await = None;
                self.replace_main_thread_agent_hooks(&[]).await;
            }
        }
    }

    /// The adopted main-thread agent's `agentType` (claude-code `MB()`), or
    /// `None` when no `--agent` was applied. Threaded into main-thread lifecycle
    /// hook payloads.
    pub(crate) async fn main_thread_agent_type(&self) -> Option<String> {
        self.lifecycle_runtime
            .main_thread_agent
            .read()
            .await
            .as_ref()
            .map(|a| a.agent_type.clone())
    }

    /// The system prompt for the next query, applying claude-code `nre`
    /// precedence: `overrideSystemPrompt` (`--system-prompt`) wins; else the
    /// adopted main-thread agent's prompt (`mainThreadAgentDefinition`
    /// `.getSystemPrompt()`); else the freshly assembled default.
    pub(crate) async fn effective_system_prompt(&self) -> String {
        if let Some(custom) = &self.config.system_prompt_override {
            return custom.clone();
        }
        if self.prompt_snapshot_eligible() {
            if let Some(snapshot) = self.prompt_runtime.prompt_snapshot.lock().await.clone() {
                if valid_prompt_snapshot(&snapshot) {
                    // Carved-slate freezes only the static getSystemPrompt
                    // members. `systemContext` (currently the git-status block
                    // in this port) is intentionally recomputed for every
                    // request, so a cwd/context change never gets baked into
                    // the cacheable prefix.
                    let mut prompt = snapshot.system_prompt.join("\n\n");
                    self.append_dynamic_system_context(&mut prompt).await;
                    return prompt;
                }
            }
        }
        let main_thread_prompt = self
            .lifecycle_runtime
            .main_thread_agent
            .read()
            .await
            .as_ref()
            .and_then(|agent| agent.system_prompt.clone());
        let mut prompt = match main_thread_prompt {
            Some(prompt) => prompt,
            None => self.build_system_prompt().await,
        };
        if let Ok(profile) = self.prompt_runtime.app_agent_prompt_profile.read() {
            if let Some(profile) = profile.as_ref() {
                if !profile.instructions.trim().is_empty() {
                    prompt.push_str("\n\n# App Agent Profile\n");
                    prompt.push_str(&format!("Revision: {}\n", profile.revision));
                    prompt.push_str(&profile.instructions);
                }
            }
        }
        prompt
    }

    /// Oracle `lje(e)` (`src_169588164.js`) — whether the static-system-prompt
    /// snapshot is active for this session:
    ///
    /// ```js
    /// function lje(e){
    ///   if (e.systemPromptSnapshot === false) return false;
    ///   return e.systemPromptSnapshot === true
    ///       || a.CLAUDE_CODE_SESSION_KIND === "bg"
    ///       || a.CLAUDE_CODE_REMOTE
    ///       || !U2();                       // U2() = a.CLAUDE_CODE_SIMPLE
    /// }
    /// ```
    ///
    /// An explicit `--system-prompt-snapshot off` overrules every disjunct;
    /// `on` forces it regardless of session kind. With the flag absent the
    /// default is `!simple`, i.e. ON — 2.1.252's `tengu_carved_slate` rollout
    /// flag GRADUATED and no longer exists anywhere in the 2.1.270 binary
    /// (verified: 2347 distinct `tengu_` flags present, that one absent). The
    /// port had kept requiring it, so this whole recorded-prompt feature —
    /// implemented, tested, and restored on resume — could never switch on.
    ///
    /// `CLAUDE_CODE_REMOTE` is deliberately omitted: remote surfaces are out of
    /// scope for this port, and it is a pure disjunct, so the only behaviour it
    /// could add is for a session that is remote AND simple.
    pub(crate) fn snapshot_gate(
        explicit: Option<bool>,
        background_session: bool,
        simple_mode: bool,
    ) -> bool {
        if explicit == Some(false) {
            return false;
        }
        explicit == Some(true) || background_session || !simple_mode
    }

    /// Whether the static-system-prompt snapshot gate is active for this main
    /// conversation. Auxiliary query sources and custom prompts are excluded —
    /// upstream applies those at the recording seam (`swt`), and the port
    /// carries them here because one predicate feeds both record and reuse.
    pub(crate) fn prompt_snapshot_eligible(&self) -> bool {
        let simple = lingxi_core::host::env::is_env_truthy(
            std::env::var("CLAUDE_CODE_SIMPLE").ok().as_deref(),
        );
        // `CLAUDE_CODE_SESSION_KIND` in claude-code; the port already spells it
        // `LINGXI_SESSION_KIND` at its other reader (`tool_api::defer`).
        let background_session = std::env::var("LINGXI_SESSION_KIND").ok().as_deref() == Some("bg");
        let source = crate::config::sanitize_query_source(&self.config.query_source);
        Self::snapshot_gate(
            lingxi_core::host::session_flags::system_prompt_snapshot(),
            background_session,
            simple,
        ) && self.config.system_prompt_override.is_none()
            && source != "auxiliary"
            && !source.starts_with("auxiliary:")
    }

    /// The resume path must never manufacture a missing snapshot. This flag is
    /// set by cold/hot resume restoration and cleared by `/clear`.
    pub(crate) fn prompt_snapshot_resume(&self) -> bool {
        self.prompt_runtime
            .prompt_snapshot_resume
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Persist the first eligible request's static prompt and initial inline
    /// tool descriptions. State is published before disk I/O so an in-memory
    /// host still gets stable semantics when persistence is unavailable.
    pub(crate) async fn record_prompt_snapshot_if_needed(
        &self,
        system: Option<&str>,
        wire_tools: &[serde_json::Value],
    ) {
        if !self.prompt_snapshot_eligible()
            || self.prompt_snapshot_resume()
            || system.is_none_or(str::is_empty)
        {
            return;
        }
        let system = system.unwrap_or_default();
        let static_prompt = self
            .current_system_context_block()
            .await
            .and_then(|dynamic| {
                let suffix = format!("\n\n{dynamic}");
                system.strip_suffix(&suffix).map(str::to_owned)
            })
            .unwrap_or_else(|| system.to_owned());
        let snapshot = lingxi_core::host::PromptSnapshot {
            system_prompt: vec![static_prompt],
            tools: prompt_tool_descriptions(wire_tools),
        };
        let mut slot = self.prompt_runtime.prompt_snapshot.lock().await;
        if slot.is_some() {
            return;
        }
        *slot = Some(snapshot.clone());
        drop(slot);
        self.persist_prompt_snapshot_attachment(&snapshot).await;
    }

    /// After a successful, non-API-error response, append a replacement
    /// snapshot only for newly seen non-deferred inline tools. Existing names
    /// retain their original descriptions and order forever.
    pub(crate) async fn record_inline_prompt_tools_after_success(
        &self,
        wire_tools: &[serde_json::Value],
    ) {
        if !self.prompt_snapshot_eligible() || self.prompt_snapshot_resume() {
            return;
        }
        let additions = prompt_tool_descriptions(wire_tools);
        if additions.is_empty() {
            return;
        }
        let snapshot = {
            let mut slot = self.prompt_runtime.prompt_snapshot.lock().await;
            let Some(snapshot) = slot.as_mut() else {
                return;
            };
            let known: std::collections::HashSet<String> = snapshot
                .tools
                .iter()
                .map(|tool| tool.name.clone())
                .collect();
            let mut added = false;
            for tool in additions {
                if !known.contains(&tool.name) {
                    snapshot.tools.push(tool);
                    added = true;
                }
            }
            added.then(|| snapshot.clone())
        };
        if let Some(snapshot) = snapshot {
            self.persist_prompt_snapshot_attachment(&snapshot).await;
        }
    }

    async fn persist_prompt_snapshot_attachment(
        &self,
        snapshot: &lingxi_core::host::PromptSnapshot,
    ) {
        let mut payload = serde_json::Map::new();
        payload.insert(
            "type".to_string(),
            serde_json::Value::String("prompt_snapshot".to_string()),
        );
        payload.insert(
            "systemPrompt".to_string(),
            serde_json::json!(snapshot.system_prompt),
        );
        if !snapshot.tools.is_empty() {
            payload.insert("tools".to_string(), serde_json::json!(snapshot.tools));
        }
        self.persist_hook_attachment_to_jsonl(serde_json::Value::Object(payload))
            .await;
    }

    /// Install a host-approved app Agent Profile as an additive prompt layer.
    /// The next turn observes it; the current in-flight request keeps its
    /// already-built prompt.
    pub fn set_app_agent_prompt_profile(
        &self,
        revision: u64,
        instructions: String,
    ) -> Result<(), String> {
        if instructions.len() > 32 * 1024 {
            return Err("app Agent Profile exceeds 32 KiB".into());
        }
        let mut profile = self
            .prompt_runtime
            .app_agent_prompt_profile
            .write()
            .map_err(|_| "app Agent Profile lock is poisoned".to_string())?;
        if profile
            .as_ref()
            .is_some_and(|current| revision < current.revision)
        {
            return Err("app Agent Profile revision moved backwards".into());
        }
        *profile = Some(AppAgentPromptProfile {
            revision,
            instructions,
        });
        Ok(())
    }

    /// Remove the app-specific additive prompt layer when an app session is
    /// closed or the host switches back to the ordinary conversation.
    pub fn clear_app_agent_prompt_profile(&self) -> Result<(), String> {
        self.prompt_runtime
            .app_agent_prompt_profile
            .write()
            .map_err(|_| "app Agent Profile lock is poisoned".to_string())?
            .take();
        Ok(())
    }

    /// Read-only introspection seam for the ordinary per-turn additional
    /// context (`claudeMd` / `userEmail` / `currentDate`). Companion to
    /// [`Self::assemble_system_prompt_preview`] — lets a host/composition-root
    /// test prove an injected memory provider reaches the additional-context
    /// message without a live model round-trip.
    pub async fn additional_context_preview(&self) -> Option<String> {
        self.additional_context_message()
            .await
            .and_then(|m| match m {
                ConversationMessage::User { content, .. } => {
                    content.into_iter().find_map(|b| match b {
                        lingxi_core::types::ContentBlock::Text { text } => Some(text),
                        _ => None,
                    })
                }
                _ => None,
            })
    }

    /// Prepend the fixed runtime context followed by the ordinary per-turn
    /// additional context. Keeping this in one helper prevents retry paths from
    /// drifting in ordering or accidentally dropping the mobile snapshot.
    pub(crate) async fn prepend_leading_context(&self, messages: &mut Vec<ConversationMessage>) {
        if let Some(ctx_msg) = self.additional_context_message().await {
            messages.insert(0, ctx_msg);
        }
        if let Some(workspace) = self.mobile_workspace_environment_message() {
            messages.insert(0, workspace);
        }
        if let Some(runtime) = self.mobile_runtime_environment_message().await {
            messages.insert(0, runtime);
        }
    }

    fn mobile_workspace_environment_message(&self) -> Option<ConversationMessage> {
        let environment = self.mobile_runtime_environment.as_ref()?;
        let cwd = self.session_cwd.cwd();
        let model_cwd = match &self.mobile_workspace_cwd_resolver {
            Some(resolver) => resolver(&cwd),
            None => Some(cwd.to_string_lossy().into_owned()),
        };
        let reminder = environment.render_workspace_system_reminder(model_cwd.as_deref())?;
        Some(ConversationMessage::user_meta(MessageId::new(), reminder))
    }

    pub(crate) fn prompt_is_interactive(&self) -> bool {
        self.mobile_runtime_environment.as_ref().map_or(
            self.config.interactive_session,
            |environment| {
                !matches!(
                    environment.host.launch_mode,
                    lingxi_core::host::MobileLaunchMode::ScheduledHeadless
                )
            },
        )
    }

    /// Prepend a transient call-scoped reminder without displacing the fixed
    /// mobile runtime snapshot from index zero.
    ///
    /// Desktop callers retain the historical index-zero behavior. Mobile
    /// callers place date/deferred-tool deltas immediately after the fixed
    /// runtime reminder, keeping that cache-stable prefix in one position on
    /// initial, retry, and fallback requests.
    pub(crate) fn prepend_transient_leading_context(
        &self,
        messages: &mut Vec<ConversationMessage>,
        reminder: ConversationMessage,
    ) {
        let mut index = usize::from(messages.first().is_some_and(|message| {
            Self::is_mobile_runtime_environment_message(message)
                || self
                    .mobile_runtime_environment_message
                    .as_ref()
                    .is_some_and(|runtime| runtime == message)
        }));
        if self.mobile_runtime_environment.is_some()
            && messages.get(index).is_some_and(|message| {
                matches!(message, ConversationMessage::User { content, .. } if content.iter().any(
                    |block| matches!(block, lingxi_core::types::ContentBlock::Text { text } if text.starts_with("<system-reminder>\nMobile workspace context"))
                ))
            })
        {
            index += 1;
        }
        messages.insert(index, reminder);
    }

    /// Reattach all call-scoped context after rebuilding a request from raw
    /// session history (for example after a context-overflow retry).
    pub(crate) async fn reattach_outgoing_context(
        &self,
        messages: &mut Vec<ConversationMessage>,
        deferred_tools_reminder: Option<&ConversationMessage>,
        date_change_reminder: Option<&ConversationMessage>,
        turn_reminders: &[ConversationMessage],
    ) {
        self.prepend_leading_context(messages).await;
        if let Some(reminder) = deferred_tools_reminder {
            self.prepend_transient_leading_context(messages, reminder.clone());
        }
        if let Some(reminder) = date_change_reminder {
            self.prepend_transient_leading_context(messages, reminder.clone());
        }
        messages.extend(turn_reminders.iter().cloned());
    }

    /// Read-only test/host preview of the fixed runtime reminder.
    pub async fn mobile_runtime_environment_preview(&self) -> Option<String> {
        self.mobile_runtime_environment_message()
            .await
            .and_then(|message| Self::text_content(&message))
    }

    /// PathAtlas S3: map the model-visible session cwd to the directory the
    /// prompt-side filesystem probes must actually read.
    ///
    /// On mobile the session cwd is a mobile-linux GUEST path
    /// (`engine-mobile`'s `model_cwd` comes from `workspace_mount.guest_path`),
    /// so probing it verbatim reads a directory that does not exist on the
    /// host. Desktop never installs a resolver, so `probe_cwd == cwd` there —
    /// byte-identical (INERT INVARIANT).
    ///
    /// Both prompt-side memory readers go through here: the system prompt
    /// ([`Self::build_prompt_context`]) and the per-turn `claudeMd` context
    /// message ([`Self::additional_context_message`]). Keeping ONE derivation
    /// is the point — the second reader having its own (raw, unresolved) copy
    /// is what kept every mobile workspace `LINGXI.md` out of the model.
    fn prompt_probe_cwd(&self, cwd: &std::path::Path) -> std::path::PathBuf {
        match &self.prompt_probe_cwd_resolver {
            Some(resolver) => resolver(cwd),
            None => cwd.to_path_buf(),
        }
    }

    /// Build the per-turn system prompt by gathering cwd / git / file
    /// tree / memory / tool-name context and calling
    /// [`crate::prompt::assemble_system_prompt`]. Bypassed when
    /// `OrchestratorConfig::system_prompt_override` is `Some(_)`.
    /// Build the per-turn [`crate::prompt::SystemPromptContext`] (cwd / env /
    /// git / tools / memory / …). Shared by [`Self::build_system_prompt`] and
    /// [`Self::additional_context_message`] — the latter re-emits the env block
    /// in the first user message when `--exclude-dynamic-system-prompt-sections`
    /// is set, so the construction (and its env-field probes) lives in ONE place.
    async fn build_prompt_context(&self) -> crate::prompt::SystemPromptContext {
        use crate::prompt::{FileTree, SystemPromptContext};

        // Task 5 (worktree 206 session-cwd plumbing): read the LIVE
        // `self.session_cwd` — the SAME cell `EnterWorktree`/`ExitWorktree`
        // swap on the tool side — not the frozen `self.cwd`. The env block's
        // `Primary working directory:` line, the file tree, the git-status
        // probe, and the memory hierarchy below all derive from `cwd`, so this
        // one substitution re-derives the ENTIRE prompt context from the
        // post-swap worktree every turn. `self.session_cwd` defaults to a
        // private, never-swapped cell equal to `self.cwd` when
        // `with_session_cwd` was never called, so this is byte-identical to
        // before for every caller that doesn't wire it (INERT INVARIANT).
        let cwd = self.session_cwd.cwd();
        // PathAtlas S3: when the session cwd is a mobile-linux guest path,
        // the probes below (memory hierarchy, git status, file tree,
        // worktree check) must read the HOST directory backing it while the
        // env block's `Primary working directory:` keeps displaying the
        // guest path the model actually uses. Desktop never sets the
        // resolver, so `probe_cwd == cwd` there — byte-identical.
        let probe_cwd = self.prompt_probe_cwd(&cwd);
        // The `<env>` model-identity line ("You are powered by the model named
        // …") must reflect the CURRENT model, not the launch model. `/model`
        // switches update `session.model` (see `OrchestratorHandle::switch_model`
        // → `SessionState::model`), while `config.model` stays frozen at
        // startup. Reading `config.model` here froze the injected identity, so a
        // switched-to model (e.g. Fable 5) still saw "You are Opus 4.8" in its
        // system prompt and reported the stale identity. The outgoing REQUEST
        // model already re-snapshots `session.model` each turn; this aligns the
        // prompt identity with it. Locked briefly and released — every
        // `build_system_prompt` caller builds the prompt BEFORE taking the
        // session lock, so there is no reentrancy.
        let model = self.session.lock().await.model.clone();
        let memory_files = self.memory.load(&probe_cwd).await;

        let (git, _) = self.cached_git_status(&probe_cwd).await;

        // Tool name extraction: ToolRegistry's `all_names()` is the
        // unfiltered set (builtin + plugin + MCP). M5-03 uses the
        // unfiltered list because the registry's enable-filter requires
        // a `ToolStaticContext` that's only meaningful at dispatch time.
        // tools_block::format sorts alphabetically inside.
        let tool_names: Vec<String> = self.tools.all_names();

        let shell = crate::prompt::env_meta::detect_shell();

        // DIV-1: worktree detection — `hf()!==null` in claude-code. Detect a
        // worktree by checking for the `gitdir` file that git creates in worktree
        // checkouts (a file rather than a directory at .git). Computed before `cwd`
        // is moved into the context struct below.
        let in_worktree = probe_cwd.join(".git").is_file();

        SystemPromptContext {
            cwd,
            // `Platform: ${je.platform}` — claude-code emits the node
            // `process.platform` value (`darwin`/`linux`/`win32`), NOT Rust's
            // `std::env::consts::OS` (`macos`/`linux`/`windows`). Map the two
            // divergent names so the env line is byte-exact.
            platform: node_platform_name(std::env::consts::OS).to_string(),
            // SYSPROMPT.1: port TS getMarketingNameForModel / getKnowledgeCutoff
            // (`utils/model/model.ts:570`, `constants/prompts.ts:712`) so the
            // model line + cutoff sentence match claude-code instead of being
            // stubbed to None. Sourced from the LIVE `session.model` (above) so
            // `/model` switches take effect for the identity block.
            //
            // NON-Claude fallback: `marketing_name_for_model` only names Claude
            // ids, so a switched-to non-Claude model (deepseek/gemini/…) got the
            // weak id-only "powered by the model {id}." form. Fall back to the
            // catalog display name so EVERY turn's identity line uses the strong
            // "powered by the model named {name}." form with the CURRENT model.
            // Gated on non-Claude so Claude ids keep byte-parity (an unknown
            // Claude id stays id-only exactly like claude-code).
            model_marketing_name: crate::prompt::env_meta::marketing_name_for_model(&model)
                .map(String::from)
                .or_else(|| {
                    (!model.to_ascii_lowercase().contains("claude"))
                        .then(|| crate::provider_adapter::display_name_for_model(&model))
                        .flatten()
                }),
            knowledge_cutoff: crate::prompt::env_meta::knowledge_cutoff_for_model(&model)
                .map(String::from),
            model,
            shell,
            // SYSPROMPT.1: `uname -sr` (TS getUnameSR) e.g. "Darwin 25.3.0",
            // falling back to "<os> <arch>" on Windows / spawn failure.
            os_version: crate::prompt::env_meta::os_version_string(),
            git_status: git,
            in_worktree,
            file_tree: FileTree::default(),
            memory_files,
            tool_names,
            // `nz()` non-empty: at least one model-invocable prompt skill
            // exists. Sourced from the attached skill-listing provider (same
            // source as the per-turn skill reminder); `None` provider ⇒ false.
            skills_available: match &self.prompt_runtime.skill_listing {
                Some(provider) => !provider.skill_entries().await.is_empty(),
                None => false,
            },
            // A scheduled mobile runtime shares a process with the foreground
            // conversation, so `interactive_session` intentionally remains
            // process-interactive. The typed per-orchestrator launch mode is
            // authoritative for prompt guidance and avoids advertising `!`
            // commands or other live-UI actions to a headless Cron run.
            is_interactive: self.prompt_is_interactive(),
            // `# Memory` section gate (claude-code `tengu_moth_copse`, default
            // OFF): the memory feature is active iff a memory prefetch is wired
            // (`memory_prefetch.is_some()`), and the section points the model at
            // exactly the user memdir the prefetch scans. `None` ⇒ section
            // omitted (byte-identical to the pre-memory prompt).
            memory_dir: self
                .prompt_runtime
                .memory_prefetch
                .as_ref()
                .and_then(|p| p.user_memdir())
                .map(std::path::Path::to_path_buf),
            // `--exclude-dynamic-system-prompt-sections`: when set, `assemble`
            // OMITS the env block from the system prompt (it is re-emitted in the
            // first-user-message context reminder via `env_reminder_section`).
            exclude_dynamic_sections: self.config.exclude_dynamic_system_prompt_sections,
        }
    }

    /// The output style in force for THIS turn: a `/output-style` switch made
    /// during the session, else the boot setting.
    ///
    /// Read per turn rather than captured once, mirroring upstream, where the
    /// `/output-style` handler writes `outputStyle` into localSettings and the
    /// next prompt assembly reads it back. `Some("default")` is an explicit
    /// choice and must win over an inherited custom style — treating it as
    /// "unset" would make switching BACK to the default impossible.
    pub(crate) async fn active_output_style_setting(&self) -> Option<String> {
        if let Some(chosen) = self.prompt_runtime.live_output_style.lock().await.clone() {
            return Some(chosen);
        }
        self.config.output_style.clone()
    }

    pub(super) async fn resolve_active_output_style(
        &self,
    ) -> Option<outputstyles::ResolvedOutputStyle> {
        let setting = self.active_output_style_setting().await;
        if let Some(registry) = &self.prompt_runtime.output_style_registry {
            if let Some(style) = registry.read().await.resolve(setting.as_deref()) {
                return Some(style);
            }
        }
        outputstyles::resolve_output_style(setting.as_deref(), &self.config.output_style_dirs)
    }

    /// Assemble the full system-prompt STRING from the static prompt members
    /// plus the current dynamic `systemContext` block.
    /// Bypassed when `OrchestratorConfig::system_prompt_override` is `Some(_)`.
    pub(super) async fn build_system_prompt(&self) -> String {
        let mut prompt = self.build_static_system_prompt().await;
        self.append_dynamic_system_context(&mut prompt).await;
        prompt
    }

    /// Build only the cacheable `getSystemPrompt` members. The git-status
    /// suffix is kept out so a carved-slate snapshot can reuse this prefix
    /// while still receiving live dynamic system context on every request.
    pub(super) async fn build_static_system_prompt(&self) -> String {
        use crate::prompt::{assemble_system_prompt_with_style, ActiveOutputStyle};
        let ctx = self.build_prompt_context().await;
        // OUTSTYLE.2/.3: when a non-default output style is active — a builtin
        // OR a custom disk style discovered under `output_style_dirs` — inject
        // its `# Output Style: <name>` section (TS getOutputStyleSection). A
        // `None`/`"default"`/unknown style resolves to `None`, leaving the prompt
        // byte-identical to the styleless path (empty `output_style_dirs` ⇒
        // builtin-only, as before).
        let resolved = self.resolve_active_output_style().await;
        let style = resolved.as_ref().map(|r| ActiveOutputStyle {
            name: r.name.as_str(),
            prompt: r.prompt.as_str(),
            keep_coding_instructions: r.keep_coding_instructions,
        });
        assemble_system_prompt_with_style(&ctx, style)
    }

    /// Return the current dynamic `systemContext` contribution. Today the
    /// port models Claude's `gitStatus` member; the exclusion flag omits the
    /// whole context just as the upstream custom/static path does.
    async fn current_system_context_block(&self) -> Option<String> {
        if self.config.exclude_dynamic_system_prompt_sections {
            return None;
        }
        let cwd = self.session_cwd.cwd();
        let probe_cwd = self.prompt_probe_cwd(&cwd);
        self.cached_git_status(&probe_cwd).await.1
    }

    async fn append_dynamic_system_context(&self, prompt: &mut String) {
        if let Some(block) = self.current_system_context_block().await {
            prompt.push_str("\n\n");
            prompt.push_str(&block);
        }
    }

    /// R-P1c/R-P1d: the leading `additionalContext` (`# claudeMd` / `# userEmail`
    /// / `# currentDate`) meta user message, or `None` when nothing is sourceable.
    ///
    /// 1:1 with claude-code `A6n(messages, userContext)` (binary offset
    /// ~205838418): when the `userContext` object is non-empty it PREPENDS one
    /// `isMeta` user message whose body is
    /// ```text
    /// <system-reminder>
    /// As you answer the user's questions, you can use the following context:
    /// # {key}
    /// {value}
    /// …                          (one `# {key}\n{value}` per entry, joined by `\n`)
    ///
    ///       IMPORTANT: this context may or may not be relevant to your tasks. You should not respond to this context unless it is highly relevant to your task.
    /// </system-reminder>
    /// ```
    /// (the IMPORTANT line is indented by exactly six spaces).
    ///
    /// The `userContext` keys, in claude-code insertion order (`pS`,
    /// binary offset ~197202100): `claudeMd` (the assembled LINGXI.md memory
    /// block — [`memory_block::format`]), `userEmail`
    /// (`The user's email address is {email}.`, only when configured), and
    /// `currentDate` (`Today's date is {YYYY-MM-DD}.`, always present). The
    /// `attachedProject` key (CLAUDE_PROJECT_TOOL) is not modelled.
    ///
    /// NOTE: `gitStatus` is NOT here — it belongs to the SEPARATE `systemContext`
    /// that claude-code folds into the SYSTEM PROMPT (see `build_system_prompt`),
    /// not this `userContext` message.
    ///
    /// Like the per-turn reminders, this is recomputed and prepended to the
    /// OUTGOING snapshot each turn (claude-code calls `A6n` on every `callModel`);
    /// it is never persisted to `session.history` / JSONL.
    pub(crate) async fn additional_context_message(&self) -> Option<ConversationMessage> {
        // `claudeMd` value = the assembled memory block (preamble + `Contents
        // of …:` blocks). Empty when no LINGXI.md files are loaded.
        //
        // Task 5 (worktree 206 session-cwd plumbing): read the LIVE
        // `self.session_cwd.cwd()`, not the frozen `self.cwd` — this reminder
        // is already recomputed fresh every turn (no cache), but reading the
        // frozen field would still show the pre-swap directory's LINGXI.md
        // files after `EnterWorktree`.
        //
        // PathAtlas S3: probe the HOST directory backing the (possibly guest)
        // session cwd — same hop `build_prompt_context` makes. This message is
        // the ONLY render path for the memory block (`prompt/mod.rs` no longer
        // splices it into the system prompt), so reading the raw guest path
        // here meant a mobile workspace `LINGXI.md` reached the model NOWHERE.
        let probe_cwd = self.prompt_probe_cwd(&self.session_cwd.cwd());
        // Build the entries in claude-code insertion order; each is `# key\nvalue`.
        let mut entries: Vec<String> = Vec::with_capacity(4);
        // `--exclude-dynamic-system-prompt-sections`: the per-machine env block
        // (cwd / env / git / OS / shell) is OMITTED from the static system prompt
        // (see `assemble_system_prompt_with_style`) and re-emitted HERE in the
        // first-user-message context reminder, so the system prompt stays
        // identical across machines (prompt-cache reuse) while the model still
        // sees the env. Built from the SAME `build_prompt_context` the system
        // prompt uses. (`false` ⟶ skipped, byte-identical to before this flag.)
        //
        // claude-code: "Only applies with the default system prompt (ignored with
        // --system-prompt)." A custom system prompt already bypasses the static
        // env block, so re-emitting it here under `--system-prompt` would leak
        // per-machine env into the first user message that the oracle never sends.
        // Gate the env-block re-emission on the absence of a system-prompt override
        // so the exclude-dynamic flag is a complete no-op when a custom prompt is
        // active. (The claudeMd / userEmail / currentDate entries below stay
        // unconditional — they are unrelated to this flag.)
        //
        // When the env block is re-emitted we already load memory inside
        // `build_prompt_context`; reuse that snapshot instead of loading twice.
        let exclude_env = self.config.exclude_dynamic_system_prompt_sections
            && self.config.system_prompt_override.is_none();
        let lingxi_md = if exclude_env {
            let ctx = self.build_prompt_context().await;
            let env = crate::prompt::env_block::format(&ctx);
            // `env_block::format` already begins with its own `# Environment\n`
            // heading, so key the entry as `Environment` and strip that leading
            // heading — the userContext renderer prepends `# {key}\n`, and a raw
            // `# env\n{env}` would DOUBLE the heading (`# env\n# Environment\n…`).
            let body = env.strip_prefix("# Environment\n").unwrap_or(&env).trim();
            if !body.is_empty() {
                entries.push(format!("# Environment\n{body}"));
            }
            crate::prompt::memory_block::format(&ctx.memory_files)
        } else {
            let memory_files = self.memory.load(&probe_cwd).await;
            crate::prompt::memory_block::format(&memory_files)
        };
        if !lingxi_md.is_empty() {
            entries.push(format!("# claudeMd\n{lingxi_md}"));
        }
        if let Some(email) = self
            .config
            .user_email
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            entries.push(format!("# userEmail\nThe user's email address is {email}."));
        }
        // `currentDate` is unconditional, but its date is session-memoized.
        // Midnight rollover is communicated exclusively by `date_change`; the
        // leading cacheable context entry must stay byte-stable.
        let session_id = self.session.lock().await.session_id;
        let session_date = self.session_start_date(session_id);
        entries.push(format!("# currentDate\nToday's date is {}.", session_date));

        // `A6n` returns the messages unchanged when the context object is empty.
        // `currentDate` is always present, so `entries` is never empty — but keep
        // the guard for faithfulness to the `Object.entries(t).length===0` check.
        if entries.is_empty() {
            return None;
        }

        let body = entries.join("\n");
        // NOTE: the IMPORTANT line is indented by EXACTLY six spaces (claude-code
        // `A6n`). Those spaces must NOT sit at the start of a continued (`\`)
        // string line — Rust's line-continuation strips leading whitespace — so
        // the `\n\n      IMPORTANT` segment is written without a preceding `\`.
        let important = "      IMPORTANT: this context may or may not be relevant to your tasks. \
You should not respond to this context unless it is highly relevant to your task.";
        let content = format!(
            "<system-reminder>\n\
As you answer the user's questions, you can use the following context:\n\
{body}\n\n{important}\n</system-reminder>\n"
        );
        // claude-code `A6n` sets `isMeta:!0` on this message. It is sent to the
        // wire (the wire conversion does not drop meta user messages) but never
        // persisted to JSONL (it is only prepended to the OUTGOING snapshot).
        Some(ConversationMessage::user_meta(MessageId::new(), content))
    }

    /// Resolve this session's plan file path (206 `ON(agentId)` →
    /// `<plansDir>/<slug>.md`). The plans directory is resolved by
    /// [`Self::plans_dir`] (206 `iT`): the `plansDirectory` settings override
    /// (relative to the project root, with a within-root containment check) when
    /// present, else the default `<project-root>/.lingxi/plans/`. The slug is the
    /// session's UUID (206's slug is likewise session-specific — exact bytes are
    /// not observable, the structure is). Uses the bare UUID (not the `sess:`
    /// display form) so the filename has no `:` separator, matching
    /// `computed_transcript_path`.
    /// The session's plan file, preferring the shared identity when the host
    /// published one (2.1.266 `ay(void 0)`); otherwise the pre-slug derivation.
    pub(crate) fn session_plan_file_path(&self, session_id: &SessionId) -> String {
        if let Some(plan_files) = self.config.plan_files.as_ref() {
            if let Some(path) = plan_files.plan_file(None) {
                return path.to_string_lossy().into_owned();
            }
        }
        Self::plan_file_path(
            session_id,
            &self.cwd,
            self.config.plans_directory.as_deref(),
        )
    }

    pub fn plan_file_path(
        session_id: &SessionId,
        project_root: &std::path::Path,
        plans_directory: Option<&str>,
    ) -> String {
        Self::plans_dir(project_root, plans_directory)
            .join(format!("{}.md", session_id.as_uuid()))
            .to_string_lossy()
            .into_owned()
    }

    /// Resolve the plans DIRECTORY — 1:1 with the binary's `iT`:
    ///
    /// ```js
    /// iT=Or(function(){
    ///   let r=Wn().plansDirectory;
    ///   if(r){
    ///     let n=Ct(),o=Pne.resolve(n,r);
    ///     if(W5_(o,n))return o;
    ///     C(`plansDirectory must be within project root: ${r}`,{level:"error"})
    ///   }
    ///   return KPp()  // Pne.join(mn(),"plans")
    /// })
    /// ```
    ///
    /// When `plansDirectory` is set: resolve it against the project root
    /// (absolute values are used verbatim, `path.resolve` semantics), normalize
    /// `.`/`..` lexically, and accept it only if it is WITHIN the project root
    /// (`W5_`'s primary check `o === n || o.startsWith(n + sep)`) and passes
    /// the hardened protected-dir / same-repo-root checks. On rejection, fall
    /// through to [`Self::default_plans_dir`].
    ///
    /// DIVERGENCE (default location): upstream's `KPp()` is
    /// `<config-home>/plans/` (`~/.claude/plans/`). LingXi's file tools only
    /// write inside their trusted set (the project cwd plus additional working
    /// dirs), so a plan file under the config home is rejected with
    /// `ToolError::PathBlocked`, the model falls back to pasting the plan into
    /// the chat, and `ExitPlanMode` reads back an EMPTY plan. The default is
    /// therefore `<project-root>/.lingxi/plans/`, inside the trusted set.
    pub fn plans_dir(
        project_root: &std::path::Path,
        plans_directory: Option<&str>,
    ) -> std::path::PathBuf {
        if let Some(r) = plans_directory.filter(|s| !s.is_empty()) {
            // `path.resolve(project_root, r)`: absolute `r` wins; else join.
            let candidate = if std::path::Path::new(r).is_absolute() {
                std::path::PathBuf::from(r)
            } else {
                project_root.join(r)
            };
            let resolved = crate::turn_loop::normalize_lexically(&candidate);
            let root = crate::turn_loop::normalize_lexically(project_root);
            // `W5_` primary: `o === n || o.startsWith(n + sep)` — component-wise
            // prefix containment on the normalized paths (so a `../escape` that
            // popped above `root` is rejected).
            if confined_path_components(&root, &resolved).is_some() {
                if plans_dir_passes_hardening(project_root, &root, &resolved) {
                    return resolved;
                }
                tracing::warn!("plansDirectory rejected by hardening guard: {r}");
                return Self::default_plans_dir(project_root);
            }
            tracing::error!("plansDirectory must be within project root: {r}");
        }
        Self::default_plans_dir(project_root)
    }

    /// The default plans directory — `<project-root>/.lingxi/plans/`.
    ///
    /// Project-local on purpose: the file tools' trusted set is the project cwd
    /// plus additional working dirs, so this is the one location the plan-mode
    /// reminder can name and the model can actually write. Repos that do not
    /// want plans in `git status` ignore `.lingxi/`.
    pub fn default_plans_dir(project_root: &std::path::Path) -> std::path::PathBuf {
        project_root.join(branding::DOT_DIR).join("plans")
    }

    async fn mobile_runtime_environment_message(&self) -> Option<ConversationMessage> {
        if let Some(message) = &self.mobile_runtime_environment_message {
            return Some(message.clone());
        }
        let environment = self.mobile_runtime_environment.as_ref()?;
        Some(ConversationMessage::user_meta(
            MessageId::new(),
            environment.render_system_reminder(),
        ))
    }
}

// Plans-directory confinement helpers live with prompt/plan path assembly.
//
// `.lingxi` is deliberately ABSENT: it is where the default plans directory
// lives (`<project-root>/.lingxi/plans`), so listing it would reject an
// explicit `plansDirectory: ".lingxi/plans"` and fall back to the byte-identical
// default with a spurious "rejected by hardening guard" warning.
pub(super) const PROTECTED_PLANS_DIR_COMPONENTS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".bzr",
    ".jj",
    ".sl",
    ".claude",
    ".cargo",
    "node_modules",
];

pub(super) fn normalize_guard_component(component: &std::ffi::OsStr) -> Option<String> {
    let text = component
        .to_str()?
        .trim_end_matches(['.', ' '])
        .to_ascii_lowercase();
    (!text.is_empty()).then_some(text)
}

pub(super) fn path_relative_components(
    root: &std::path::Path,
    candidate: &std::path::Path,
    case_insensitive: bool,
) -> Option<Vec<std::ffi::OsString>> {
    let mut candidate_components = candidate.components();
    for root_component in root.components() {
        let candidate_component = candidate_components.next()?;
        let equal = if case_insensitive {
            root_component
                .as_os_str()
                .to_string_lossy()
                .eq_ignore_ascii_case(&candidate_component.as_os_str().to_string_lossy())
        } else {
            root_component == candidate_component
        };
        if !equal {
            return None;
        }
    }
    Some(
        candidate_components
            .map(|component| component.as_os_str().to_os_string())
            .collect(),
    )
}

pub(super) fn confined_path_components(
    root: &std::path::Path,
    candidate: &std::path::Path,
) -> Option<Vec<std::ffi::OsString>> {
    path_relative_components(root, candidate, cfg!(windows))
}

pub(super) fn plans_dir_has_protected_component(
    root: &std::path::Path,
    candidate: &std::path::Path,
) -> bool {
    confined_path_components(root, candidate).is_none_or(|components| {
        components.iter().any(|name| {
            normalize_guard_component(name).is_some_and(|name| {
                PROTECTED_PLANS_DIR_COMPONENTS
                    .iter()
                    .any(|protected| name == *protected)
            })
        })
    })
}

pub(super) fn deepest_existing_ancestor(path: &std::path::Path) -> Option<&std::path::Path> {
    path.ancestors().find(|ancestor| ancestor.exists())
}

pub(super) fn metadata_is_link_like(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        return metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    }
    #[cfg(not(windows))]
    false
}

pub(super) fn has_symlink_component_between(
    root: &std::path::Path,
    existing: &std::path::Path,
) -> Option<bool> {
    let mut current = root.to_path_buf();
    for component in confined_path_components(root, existing)? {
        current.push(component);
        let meta = std::fs::symlink_metadata(&current).ok()?;
        if metadata_is_link_like(&meta) {
            return Some(true);
        }
    }
    Some(false)
}

pub(super) fn nearest_repo_root(path: &std::path::Path) -> Option<std::path::PathBuf> {
    path.ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
        .and_then(|ancestor| std::fs::canonicalize(ancestor).ok())
}

pub(super) fn plans_dir_passes_hardening(
    project_root: &std::path::Path,
    normalized_root: &std::path::Path,
    normalized_candidate: &std::path::Path,
) -> bool {
    if plans_dir_has_protected_component(normalized_root, normalized_candidate) {
        return false;
    }

    let canonical_root = match std::fs::canonicalize(project_root) {
        Ok(path) => path,
        Err(_) => return false,
    };
    let existing = match deepest_existing_ancestor(normalized_candidate) {
        Some(path) => path,
        None => return false,
    };
    if !existing.is_dir() {
        return false;
    }
    if has_symlink_component_between(normalized_root, existing) != Some(false) {
        return false;
    }

    let canonical_existing = match std::fs::canonicalize(existing) {
        Ok(path) => path,
        Err(_) => return false,
    };
    if confined_path_components(&canonical_root, &canonical_existing).is_none() {
        return false;
    }

    let project_repo_root = nearest_repo_root(&canonical_root);
    let candidate_repo_root = nearest_repo_root(&canonical_existing);
    match (project_repo_root, candidate_repo_root) {
        (Some(project), Some(candidate)) => project == candidate,
        (None, None) => true,
        _ => false,
    }
}

// ── Task 7 helpers ──────────────────────────────────────────────────────────
