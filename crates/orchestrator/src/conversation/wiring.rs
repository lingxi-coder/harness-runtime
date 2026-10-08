//! Construction, builder wiring, and small façade accessors.

use super::*;

impl ConversationOrchestrator {
    /// Construct a new orchestrator with a fresh in-memory session and
    /// BOTH batched + streaming API clients wired.
    ///
    /// Argument order: same as `new`, but inserts `streaming_api` right
    /// after `api`. Use this when the streaming path is needed
    /// (`run_turn_streaming`); otherwise `new` is shorter.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new_with_streaming(
        config: OrchestratorConfig,
        api: Arc<dyn OrchestratorApiClient>,
        streaming_api: Arc<dyn StreamingApiClient>,
        tools: Arc<ToolRegistry>,
        hooks: Arc<HookExecutor>,
        perms: Arc<dyn PermissionGate>,
        output: Arc<dyn OutputStream>,
        memory: Arc<dyn crate::prompt::MemoryHierarchyProvider>,
        cwd: std::path::PathBuf,
    ) -> Self {
        // M5-14 Task 10: emit release markers once per process lifetime.
        telemetry::emit_release_markers_once();
        // Publish the session interactivity to the process-global flag (the port's
        // `getIsNonInteractiveSession()` analog) so prompt builders without a
        // `ToolUseContext` — e.g. the `AgentTool` fork gate — see the right mode.
        // `is_non_interactive_session == !interactive_session`: interactive CLI
        // prompt/request semantics are distinct from the permission-gate
        // transport, so TUI/REPL sessions can carry interactive guidance even
        // when `interactive_permissions` remains on the headless default.
        lingxi_core::host::session_flags::set_non_interactive_session(!config.interactive_session);
        // claude-code `Dx(()=>HR(rt()))` — publish the session main-loop model
        // the `OO()` todo/task tool gate reads back through `J$e()`. This init
        // value is the launch model; `build_wire_tools` republishes it from the
        // live session each turn, so `/model` switches and resumes move the
        // gate. Seeding it here matters because the reminder producer can run
        // before the first wire-tools assembly.
        tools.set_main_loop_model(
            crate::conversation::tooling_impl::canonical_main_loop_model(&config.model, None),
        );
        // Publish the session-scoped tool-search gate (Claude Code `$U()`) so the
        // request builder branches `tool_reference` normalization on the SESSION
        // decision, not on whether a given request's toolset carries a
        // `ToolSearch` declaration. This init value uses no resolved profile yet
        // (the CLI resolves it post-construction via `switch_model`); the
        // per-request assembly refreshes it once the model/profile are known, so
        // any side query fired before the first main-loop turn still sees the
        // session's mode+provider decision rather than the bare `false` default.
        lingxi_core::host::session_flags::set_tool_search_enabled(
            tools.deferral().mode().is_enabled()
                && tool_search_supported_for_request(&config.model, None),
        );
        let session = SessionState::empty(SessionId::new(), config.model.clone());
        let main_reports = super::main_reports_impl::MainReportInbox::new(session.session_id);
        let invoked_skill_session_id = session.session_id.to_string();
        let current_effort = config.effort.clone();
        let current_reasoning_selection = current_effort
            .as_ref()
            .map(|effort| lingxi_core::host::ReasoningSelection::Level { id: effort.clone() })
            .unwrap_or(lingxi_core::host::ReasoningSelection::Automatic);
        let current_effort_explicit = current_effort.is_some();
        let ultracode = config.ultracode;
        Self {
            config,
            api,
            streaming_api,
            query_chain_id: uuid::Uuid::new_v4().to_string(),
            tools,
            hooks,
            perms,
            output,
            session: Arc::new(Mutex::new(session)),
            session_end_fired: Mutex::new(HashSet::new()),
            mod_turn: std::sync::Mutex::new(None),
            mod_session_event_sender: std::sync::Mutex::new(None),
            mod_session_measure_sampler: Arc::new(
                mod_session_measure_sampler::ModSessionMeasureSampler::default(),
            ),
            mod_ui_selection: std::sync::Mutex::new(None),
            mod_surface_roster: std::sync::Arc::new(
                crate::mod_surface_roster::ModSurfaceRoster::default(),
            ),
            mod_settings_reader: None,
            mod_command_catalog: None,
            invoked_skill_session_guard: compaction::invoked_skills::InvokedSkillSessionGuard::new(
                invoked_skill_session_id,
            ),
            turn_gate: Arc::new(Mutex::new(())),
            streaming_tool_dispatch_owner: std::sync::OnceLock::new(),
            main_reports,
            model_switch_gate: Mutex::new(()),
            dynamic_workflows_gate: lingxi_core::host::session_flags::DynamicWorkflowsGate::default(
            ),
            workflow_size_guideline:
                lingxi_core::host::session_flags::WorkflowSizeGuidelineState::default(),
            current_cwd: Arc::new(std::sync::Mutex::new(cwd.clone())),
            session_cwd: tool_api::SessionCwd::new(cwd.clone(), vec![cwd.clone()]),
            prompt_probe_cwd_resolver: None,
            mobile_runtime_environment_message: None,
            mobile_runtime_environment: None,
            mobile_workspace_cwd_resolver: None,
            cwd,
            config_home: None,
            workspace_trusted: true,
            hooks_restricted: false,
            transcript: TranscriptStore::new(),
            prompt_runtime: PromptRuntime::new(),
            compaction_runtime: CompactionRuntime::new(),
            lifecycle_runtime: LifecycleRuntime::new(),
            computer_runtime: crate::native_computer::ComputerRuntime::default(),
            tool_execution_journal: None,
            model_runtime: ModelRuntime::new(
                current_effort,
                current_reasoning_selection,
                current_effort_explicit,
                ultracode,
            ),
            memory,
            should_exit: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            fast_mode: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            mcp_registry: None,
            task_registry: None,
            mod_agent_name_registry: None,
            model_resolution_context_provider: None,
            ide_handle: None,
            fork_spawner: None,
            fork_budget: None,
            hook_agent_inheritance: None,
            bg_session_forker: None,
            repo_root_reloader: None,
            recap_runner: None,
            file_history: None,
            orphan_forced_decisions: Mutex::new(std::collections::HashMap::new()),
            mid_turn_input: std::sync::OnceLock::new(),
            cancel_reason: std::sync::OnceLock::new(),
            end_conversation_slot: None,
            loop_wakeup_armed_slot: None,
            turn_span: crate::turn_span::TurnSpanTally::default(),
            coordinator_mode: None,
            #[cfg(test)]
            coordinator_simple_mode_override: None,
            #[cfg(test)]
            coordinator_pool_override: None,
            tool_pool_denied_names: std::sync::RwLock::new(Vec::new()),
            main_agent_tool_names: std::sync::RwLock::new(None),
        }
    }

    /// Attach a [`JsonlWriter`] for byte-equivalent session persistence.
    /// Builder-style — used by the CLI binary (M5-12) and integration tests.
    #[must_use]
    pub fn with_jsonl_writer(mut self, writer: Arc<JsonlWriter>) -> Self {
        self.transcript.jsonl_writer = Some(writer);
        self
    }

    /// Use the host's live remote UI attachment roster for this session.
    #[must_use]
    pub fn with_mod_surface_roster(
        mut self,
        roster: std::sync::Arc<crate::mod_surface_roster::ModSurfaceRoster>,
    ) -> Self {
        self.mod_surface_roster = roster;
        self
    }

    /// Attach the shared main-loop pre-call preparer.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_model_call_preparer(mut self, preparer: Arc<dyn ModelCallPreparer>) -> Self {
        self.model_runtime.model_call_preparer = Some(preparer);
        self
    }

    /// Enable the shared image sidecar for non-vision primary models. Passing
    /// `false` keeps the preparer installed so media receives an actionable
    /// disabled error instead of falling through to a generic capability error.
    #[must_use]
    pub fn with_vision_delegation(mut self, enabled: bool) -> Self {
        self.model_runtime.model_call_preparer = Some(Arc::new(
            crate::vision_model_call::VisionModelCallPreparer::with_enabled(enabled),
        ));
        self
    }

    /// Whether a [`JsonlWriter`] has been wired via [`Self::with_jsonl_writer`].
    /// (M3 cc2.1.198) Probe for the `--no-session-persistence` boot gate: the
    /// composition root leaves the slot `None` so nothing persists to disk.
    #[must_use]
    pub fn has_jsonl_writer(&self) -> bool {
        self.transcript.jsonl_writer.is_some()
    }

    /// Share the per-session read-file-state registry (claude-code's
    /// `readFileState` map) with the file tools. Builder-style — the
    /// composition root creates ONE
    /// [`tool_api::read_file_state::ReadFileStateMap`], passes a clone into the
    /// file tools' [`tool_api::BuiltinToolContext`], and hands the SAME `Arc`
    /// here, so a tool's `readFileState.set` (Read/Edit/Write/NotebookEdit) is
    /// visible to the orchestrator's post-compact restore, `/files`, and the
    /// staleness consumers — 1:1 with claude-code's single per-session map on
    /// the `ToolUseContext` (P1-06). Overwrites the fresh, unshared default the
    /// constructor allocated. Wired at the desktop + mobile composition roots;
    /// tests that need a live registry can call this with a map they also seed.
    #[must_use]
    pub fn with_read_state_map(mut self, map: tool_api::read_file_state::ReadFileStateMap) -> Self {
        self.prompt_runtime.read_state_map = map;
        self
    }

    /// Attach the resolved claude-home (`$LINGXI_CONFIG_DIR ?? ~/.claude`) so
    /// hook payloads carry a deterministically-computed `transcript_path` even
    /// when no [`JsonlWriter`] is wired (the production case). Builder-style —
    /// wired at the composition root (`engine-desktop` / `engine-mobile`).
    #[must_use]
    pub fn with_config_home(mut self, config_home: std::path::PathBuf) -> Self {
        self.config_home = Some(config_home);
        self
    }

    /// Supply the host's live settings sources to Mod calls.
    #[must_use]
    pub fn with_mod_settings_reader(
        mut self,
        reader: Arc<dyn hooks::mods::ModSettingsReader>,
    ) -> Self {
        self.mod_settings_reader = Some(reader);
        self
    }

    /// Supply the host's shared slash-command catalog to Mod calls.
    #[must_use]
    pub fn with_mod_command_catalog(
        mut self,
        catalog: Arc<dyn hooks::mods::ModCommandCatalog>,
    ) -> Self {
        self.mod_command_catalog = Some(catalog);
        self
    }

    /// Wire the resolved workspace-trust state for `/goal`.
    #[must_use]
    pub fn with_workspace_trusted(mut self, trusted: bool) -> Self {
        self.workspace_trusted = trusted;
        self
    }

    /// Wire the resolved hook-policy restriction state for `/goal`.
    #[must_use]
    pub fn with_hooks_restricted(mut self, restricted: bool) -> Self {
        self.hooks_restricted = restricted;
        self
    }

    /// Share the composition root's mutable-cwd cell — the SAME `Arc` the
    /// [`crate::OrchestratorCwdChangedFirer`] writes on a Bash `cd` — so hook
    /// payloads read the post-`cd` directory. Builder-style; wired at the
    /// desktop composition root. Without it the default private cell (over the
    /// static `cwd`) never moves, so hooks read the init cwd exactly as before.
    #[must_use]
    pub fn with_current_cwd(mut self, cell: Arc<std::sync::Mutex<std::path::PathBuf>>) -> Self {
        self.current_cwd = cell;
        self
    }

    /// Share the composition root's switchable [`tool_api::SessionCwd`] — the
    /// SAME `Arc` handed to `BuiltinToolContext`, which `EnterWorktree`/
    /// `ExitWorktree` (Task 6-8 of the worktree-206 plan) swap. Builder-style;
    /// wired at the desktop/mobile composition roots.
    ///
    /// Conditional rules use the current cwd on every fresh trigger walk.
    /// Their session delivery marks survive a worktree swap.
    ///
    /// Without this call the default private `SessionCwd` (over the
    /// constructor's static `cwd`, never swapped) stands, so every cwd-derived
    /// section reads the boot cwd exactly as before — the INERT INVARIANT.
    #[must_use]
    pub fn with_session_cwd(mut self, session_cwd: Arc<tool_api::SessionCwd>) -> Self {
        self.session_cwd = session_cwd;
        self
    }

    /// Share the session-owned dynamic-workflow gate with slash commands, the
    /// Workflow tool, and TUI config surfaces.
    #[must_use]
    pub fn with_dynamic_workflows_gate(
        mut self,
        gate: lingxi_core::host::session_flags::DynamicWorkflowsGate,
    ) -> Self {
        self.dynamic_workflows_gate = gate;
        self
    }

    /// Share the session-owned workflow-size setting with slash commands, the
    /// Workflow tool, and TUI config surfaces.
    #[must_use]
    pub fn with_workflow_size_guideline(
        mut self,
        state: lingxi_core::host::session_flags::WorkflowSizeGuidelineState,
    ) -> Self {
        self.workflow_size_guideline = state;
        self
    }

    /// PathAtlas S3 (mobile-linux): resolve the model-visible session cwd to
    /// the HOST directory the prompt probes should run against.
    ///
    /// When the session cwd is a guest path (`/workspace/<id>`), the env
    /// block must DISPLAY it verbatim while the memory hierarchy, git-status
    /// probe, and file tree must read the host directory that backs it. This
    /// resolver performs that guest→host hop in [`Self::build_prompt_context`];
    /// unset (every desktop caller), probes run on the session cwd itself —
    /// byte-identical to before.
    #[must_use]
    pub fn with_prompt_probe_cwd_resolver(
        mut self,
        resolver: Arc<dyn Fn(&std::path::Path) -> std::path::PathBuf + Send + Sync>,
    ) -> Self {
        self.prompt_probe_cwd_resolver = Some(resolver);
        self
    }

    /// Attach the stable mobile host/tool-runtime snapshot for this engine.
    ///
    /// Rendering happens exactly once here. The snapshot describes the stable
    /// configured runtime; registered tool schemas remain authoritative for
    /// each agent's effective `Shell` availability.
    #[must_use]
    pub fn with_mobile_runtime_environment(
        mut self,
        environment: lingxi_core::host::mobile_runtime_environment::MobileRuntimeEnvironment,
    ) -> Self {
        self.mobile_runtime_environment_message = Some(ConversationMessage::user_meta(
            MessageId::new(),
            environment.render_system_reminder(),
        ));
        self.mobile_runtime_environment = Some(environment);
        self
    }

    /// Attach the model-visible mobile cwd mapper. Host-native backing paths
    /// that are not covered by a guest mount are omitted by the environment's
    /// sanitizer rather than exposed to the model.
    #[must_use]
    pub fn with_mobile_workspace_cwd_resolver(
        mut self,
        resolver: Arc<dyn Fn(&std::path::Path) -> Option<String> + Send + Sync>,
    ) -> Self {
        self.mobile_workspace_cwd_resolver = Some(resolver);
        self
    }

    /// Override the orchestrator's session id. Builder-style — used at the
    /// composition root so the boot-canonical session id (also handed to the
    /// leaf hook firers / subagent spawner for a consistent `transcript_path`)
    /// matches the orchestrator's live session. Without this the constructor's
    /// fresh [`SessionId::new`] stands (test/library callers).
    #[must_use]
    pub fn with_session_id(self, session_id: SessionId) -> Self {
        // `session` is behind an `Arc<Mutex>`; this builder runs before any turn
        // (single owner at construction), so a blocking lock is safe and avoids
        // making the builder async.
        {
            let mut s = self
                .session
                .try_lock()
                .expect("with_session_id runs at construction, before any turn holds the lock");
            s.session_id = session_id;
        }
        self.main_reports.reset_for_builder(session_id);
        self.invoked_skill_session_guard
            .replace(session_id.to_string());
        self
    }

    /// Attach a [`cost::CostTracker`] so `snapshot_cost` returns
    /// real numbers. Without this, `snapshot_cost` keeps the M5-10
    /// zero-shaped stub shape. This construction-only builder must run before
    /// the tracker is used to issue any budget reservations. (M6-06)
    #[must_use]
    pub fn with_cost_tracker(mut self, tracker: Arc<cost::CostTracker>) -> Self {
        let session_id = self
            .session
            .try_lock()
            .expect("with_cost_tracker runs at construction")
            .session_id;
        tracker.adopt_active_session_for_builder(session_id);
        *self
            .model_runtime
            .cost_scope
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(tracker.session_scope(session_id));
        self.model_runtime.cost_tracker = Some(tracker);
        self
    }

    /// Attach a `/usage` Loops provider (typically the live cron scheduler).
    #[must_use]
    pub fn with_loop_usage(
        mut self,
        provider: Arc<dyn lingxi_core::host::LoopUsageProvider>,
    ) -> Self {
        self.model_runtime.loop_usage = Some(provider);
        self
    }

    /// Optional form used by composition roots that may have cron disabled.
    #[must_use]
    pub fn with_loop_usage_opt(
        mut self,
        provider: Option<Arc<dyn lingxi_core::host::LoopUsageProvider>>,
    ) -> Self {
        self.model_runtime.loop_usage = provider;
        self
    }

    /// Attach the session's observer pairing table.
    ///
    /// Must be the SAME `Arc` handed to `TaskRegistry::set_observer_pairings`:
    /// the registry files a pairing when it spawns an observer, and
    /// `ObserverReport` resolves against this one. Two tables would look wired
    /// and report "not armed" forever.
    #[must_use]
    pub fn with_observer_pairings(
        mut self,
        table: Arc<lingxi_core::host::observer_pairing::ObserverPairings>,
    ) -> Self {
        self.model_runtime.observer_pairings = Some(table);
        self
    }

    /// Attach the host's shared output book. Inert unless explicitly wired.
    #[must_use]
    pub fn with_workflow_output_scopes(
        mut self,
        scopes: Arc<dyn lingxi_core::host::WorkflowOutputScopes>,
    ) -> Self {
        self.model_runtime.output_scopes = Some(scopes);
        self
    }

    /// Attach an app-owned durable session switcher for hot clear/resume.
    /// The switcher hydrates and claims a destination before the live
    /// orchestrator session identity changes; legacy hosts leave it unset.
    #[must_use]
    pub fn with_cost_session_switcher(
        mut self,
        switcher: Arc<dyn crate::conversation::CostSessionSwitcher>,
    ) -> Self {
        self.model_runtime.cost_session_switcher = Some(switcher);
        self
    }

    /// Optional form used by composition roots that preserve legacy
    /// ephemeral construction when durable session persistence is disabled.
    #[must_use]
    pub fn with_cost_session_switcher_opt<T>(mut self, switcher: Option<Arc<T>>) -> Self
    where
        T: crate::conversation::CostSessionSwitcher + 'static,
    {
        self.model_runtime.cost_session_switcher =
            switcher.map(|switcher| switcher as Arc<dyn crate::conversation::CostSessionSwitcher>);
        self
    }

    /// Attach the host-side process presence/inbox binder that must follow an
    /// owned hot-session commit even when the initiating client disappears.
    #[must_use]
    pub fn with_session_activation_observer(
        mut self,
        observer: Arc<dyn crate::conversation::SessionActivationObserver>,
    ) -> Self {
        self.lifecycle_runtime.session_activation_observer = Some(observer);
        self
    }

    /// Publish the weak self-link used solely by owned clear/resume commits.
    /// Production composition calls this immediately after `Arc::new`; direct
    /// value-based library constructors retain their historical inline path.
    pub fn attach_owned_session_switches(self: &Arc<Self>) {
        let _ = self
            .lifecycle_runtime
            .session_switch_owner
            .set(Arc::downgrade(self));
    }

    /// Close session-switch admission and wait for every registered prepare or
    /// post-prepare owner to finish. Hosts call this after request producers
    /// stop and before session-state coordinators close.
    pub async fn close_and_drain_session_switches(&self) -> Vec<String> {
        self.close_main_report_admission().await;
        self.lifecycle_runtime
            .session_switch_supervisor
            .close_and_drain()
            .await
    }

    /// Share the API-call counter with other in-process model entry points,
    /// such as local-app vision delegation, so one `/cost` snapshot includes
    /// every billable request made on the session's behalf.
    #[must_use]
    pub fn with_api_calls_counter(mut self, counter: Arc<std::sync::atomic::AtomicU32>) -> Self {
        self.model_runtime.api_calls_recorded = counter;
        self
    }

    /// Whether a [`cost::CostTracker`] has been wired via
    /// [`Self::with_cost_tracker`]. (M6-06)
    #[must_use]
    pub fn has_cost_tracker(&self) -> bool {
        self.model_runtime.cost_tracker.is_some()
    }

    /// Attach an analytics bus so the live turn loop fires
    /// `tengu_api_success` per completed API response (M7). Without this the
    /// cost tracker still accrues totals but emits no analytics event — matching
    /// the pre-M7 behavior (and library/test callers that don't want telemetry).
    /// The desktop composition root passes the SAME bus it gives the provider
    /// adapter (which fires `tengu_api_*`), so all telemetry shares one sink set.
    #[must_use]
    pub fn with_analytics_bus(mut self, bus: Arc<telemetry::AnalyticsBus>) -> Self {
        self.model_runtime.analytics_bus = Some(bus);
        self
    }

    /// Attach an MCP registry so `list_mcp_servers` reports real data.
    /// Without this, the trait method returns `vec![]`. (M6-07)
    #[must_use]
    pub fn with_mcp_registry(mut self, mcp: Arc<mcp::McpRegistry>) -> Self {
        self.mcp_registry = Some(mcp);
        self
    }

    /// Share the execution registry with Mod agent enumeration and waits.
    #[must_use]
    pub fn with_task_registry(
        mut self,
        registry: Arc<dyn lingxi_core::host::task_registry::TaskRegistryHandle>,
    ) -> Self {
        self.task_registry = Some(registry);
        self
    }

    /// Bind the actual Agent routing store to direct Mod list calls.
    #[must_use]
    pub fn with_mod_agent_name_registry(
        mut self,
        registry: Arc<dyn lingxi_core::host::agent_name_registry::AgentNameRegistry>,
    ) -> Self {
        self.mod_agent_name_registry = Some(registry);
        self
    }

    /// Share the host's model route authority and refresh the initial tool gate.
    #[must_use]
    pub fn with_model_resolution_context_provider(
        mut self,
        provider: Arc<dyn agent::model_resolution::ModelResolutionContextProvider>,
    ) -> Self {
        self.model_resolution_context_provider = Some(provider);
        let initial_route = self
            .session
            .try_lock()
            .ok()
            .map(|session| (session.model.clone(), session.model_profile.clone()));
        if let Some((model, profile)) = initial_route {
            self.refresh_main_loop_model_for_route(&model, profile.as_deref());
        } else {
            // A shared session is already in use; its route will be refreshed
            // by profile seeding or the next wire-tools snapshot.
            self.tools.set_main_loop_model(None);
        }
        self
    }

    /// Attach the provider-neutral local IDE lifecycle handle.
    #[must_use]
    pub fn with_ide_handle(mut self, ide: Arc<dyn lingxi_core::host::IdeHandle>) -> Self {
        self.ide_handle = Some(ide);
        self
    }

    /// Attach a hook registry so `list_hooks` reports real data.
    /// Wrapped in `RwLock` because `HookRegistry::register` is `&mut self`
    /// and the CLI binary may register hooks past the initial load.
    /// (M6-07)
    #[must_use]
    pub fn with_hook_registry(
        mut self,
        hooks: Arc<tokio::sync::RwLock<hooks::HookRegistry>>,
    ) -> Self {
        self.lifecycle_runtime.hook_registry = Some(hooks);
        self
    }

    /// Attach the host's live plugin output-style registry.
    #[must_use]
    pub fn with_output_style_registry(
        mut self,
        styles: Arc<tokio::sync::RwLock<outputstyles::OutputStyleRegistry>>,
    ) -> Self {
        self.prompt_runtime.output_style_registry = Some(styles);
        self
    }

    /// Attach an agent catalog so `list_agents` reports real data.
    /// Wrapped in `RwLock` so the CLI binary can append/reload agents
    /// without rebuilding the orchestrator. (M6-07)
    #[must_use]
    pub fn with_agent_catalog(
        mut self,
        agents: Arc<tokio::sync::RwLock<Vec<agent::AgentDefinition>>>,
    ) -> Self {
        self.lifecycle_runtime.agent_catalog = Some(agents);
        self
    }

    /// Attach a skill-listing provider so the per-turn `skill_listing`
    /// system-reminder enumerates the model-invocable skills (SKILLLIST.1).
    /// Without this the reminder is never injected (the model cannot discover
    /// skills autonomously). The desktop binary wires a `CommandRegistry`-backed
    /// provider at the composition root.
    #[must_use]
    pub fn with_skill_listing(
        mut self,
        provider: Arc<dyn crate::prompt::skill_listing::SkillListingProvider>,
    ) -> Self {
        self.prompt_runtime.skill_listing = Some(provider);
        self
    }

    /// Whether a skill-listing provider has been wired via
    /// [`Self::with_skill_listing`]. (SKILLLIST.1)
    #[must_use]
    pub fn has_skill_listing(&self) -> bool {
        self.prompt_runtime.skill_listing.is_some()
    }

    /// Attach a memory prefetcher so the per-turn `relevant_memories` surfacing
    /// reminder is injected (P0.1). Without this the surfacing channel is a
    /// strict no-op ([`Self::relevant_memory_reminder_messages`] returns empty),
    /// keeping the locked fixtures byte-identical — the LingXi equivalent of
    /// claude-code's `tengu_moth_copse`-default-false gate (here: "is a prefetch
    /// wired at all"). Wired at the composition root once a real
    /// selector-backed prefetch lands.
    #[must_use]
    pub fn with_memory_prefetch(mut self, prefetch: Arc<memory::prefetch::MemoryPrefetch>) -> Self {
        self.prompt_runtime.memory_prefetch = Some(prefetch);
        self
    }

    /// Wire the EndConversation end-request slot (shared with the tool). The
    /// composition root passes the same `Arc` it gave
    /// [`crate::end_conversation_tool::EndConversationTool::new`]; the turn loop
    /// reads+consumes it after tool execution to terminate the conversation.
    /// Only set when the feature is enabled — `None` keeps the turn loop
    /// byte-identical.
    #[must_use]
    pub fn with_end_conversation_slot(
        mut self,
        slot: crate::end_conversation_tool::EndConversationSlot,
    ) -> Self {
        self.end_conversation_slot = Some(slot);
        self
    }

    /// Wire the `/loop` wakeup-armed flag (shared with `ScheduleWakeup`). The
    /// composition root passes the same `Arc` it took from
    /// `tool_cron::register_all_with_auth`; the turn loop reads+consumes it
    /// after tool execution. `None` keeps the turn loop byte-identical.
    #[must_use]
    pub fn with_loop_wakeup_armed_slot(
        mut self,
        slot: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        self.loop_wakeup_armed_slot = Some(slot);
        self
    }

    /// Wire the live coordinator-mode flag (`Ci()`). Unwired sessions never
    /// take the unknown-tool coordinator suffix.
    #[must_use]
    pub fn with_coordinator_mode(
        mut self,
        mode: std::sync::Arc<dyn lingxi_core::host::coordinator_mode::CoordinatorModeHandle>,
    ) -> Self {
        self.coordinator_mode = Some(mode);
        self
    }

    /// Whether this session is currently in coordinator mode and not simple
    /// mode (`Ci() && !CLAUDE_CODE_SIMPLE`).
    #[must_use]
    pub(crate) fn is_coordinator_session(&self) -> bool {
        #[cfg(test)]
        let simple_mode = self
            .coordinator_simple_mode_override
            .unwrap_or_else(Self::coordinator_simple_mode);
        #[cfg(not(test))]
        let simple_mode = Self::coordinator_simple_mode();
        self.is_coordinator_mode_enabled() && !simple_mode
    }

    pub(crate) fn is_coordinator_mode_enabled(&self) -> bool {
        self.coordinator_mode
            .as_ref()
            .is_some_and(|mode| mode.is_enabled())
    }

    fn coordinator_simple_mode() -> bool {
        lingxi_core::host::env::is_env_truthy(std::env::var(branding::SIMPLE_ENV).ok().as_deref())
    }

    #[cfg(test)]
    pub(crate) fn with_coordinator_simple_mode_for_test(mut self, simple: bool) -> Self {
        self.coordinator_simple_mode_override = Some(simple);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_coordinator_pool_for_test(mut self, brief: bool, extra: &[&str]) -> Self {
        self.coordinator_pool_override = Some((brief, extra.iter().map(|s| (*s).into()).collect()));
        self
    }

    /// 2.1.263 `Idr`: qbt base, PR subscriptions, MCP comms, brief and extra tools.
    /// Unlike the Ldt suffix, pool assembly does not bypass filtering in simple mode.
    pub(crate) fn is_coordinator_pool_tool(&self, tool: &dyn tool_api::Tool) -> bool {
        let name = tool.name();
        if crate::streaming_executor::is_coordinator_redirect_excluded(name)
            || name.ends_with("subscribe_pr_activity")
            || name.ends_with("unsubscribe_pr_activity")
            || tool.mcp_role() == Some("comms")
        {
            return true;
        }
        #[cfg(test)]
        if let Some((brief, extra)) = &self.coordinator_pool_override {
            return Self::coordinator_optional_tool(tool, *brief, extra.iter().map(String::as_str));
        }
        let extra = std::env::var("LINGXI_COORDINATOR_EXTRA_TOOLS")
            .or_else(|_| std::env::var("CLAUDE_CODE_COORDINATOR_EXTRA_TOOLS"))
            .unwrap_or_default();
        Self::coordinator_optional_tool(
            tool,
            lingxi_core::host::session_flags::brief_mode_enabled(),
            extra.split(',').map(str::trim).filter(|s| !s.is_empty()),
        )
    }

    fn coordinator_optional_tool<'a>(
        tool: &dyn tool_api::Tool,
        brief: bool,
        extra: impl Iterator<Item = &'a str>,
    ) -> bool {
        (brief && matches!(tool.name(), "SendUserMessage" | "SendUserFile"))
            || extra.into_iter().any(|name| {
                name == tool.name()
                    || Some(name) == tool.underlying_v1_tool_name()
                    || Some(name) == tool.family_parent_tool_name()
            })
    }

    pub(crate) fn is_tool_pool_denied(&self, tool: &dyn tool_api::Tool) -> bool {
        let denied = self
            .tool_pool_denied_names
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        denied.iter().any(|rule| {
            std::iter::once(tool.name())
                .chain(tool.underlying_v1_tool_name())
                .chain(tool.family_parent_tool_name().filter(|name| {
                    self.tools.find_registered(name).is_some_and(|parent| {
                        parent.is_enabled(&tool_api::tool_trait::ToolStaticContext {
                            main_loop_model: self.tools.main_loop_model(),
                            ..Default::default()
                        })
                    })
                }))
                .chain(tool.aliases().iter().copied())
                .any(|name| permission::tool_wide_name_matches(rule, name))
        })
    }

    /// Look up a tool the main loop may actually dispatch.
    ///
    /// Coordinator sessions keep their assembled pool and treat every other
    /// registered name as unknown so `Ldt` can take the `Y7e` worker-redirect
    /// arm. The shared registry stays full — workers still resolve from
    /// `available_tools`.
    /// Is this tool available to the model right now? A tool-pool DENY hides it,
    /// which is what the advertised tool list and the reminders want to know.
    #[must_use]
    pub(crate) fn find_dispatchable_tool(
        &self,
        name: &str,
    ) -> Option<std::sync::Arc<dyn tool_api::tool_trait::Tool>> {
        self.lookup_tool(name, true)
    }

    /// The lookup a `tool_use` DISPATCH must use: identical, except a tool-pool
    /// deny does not hide the tool.
    ///
    /// Upstream 2.1.270 (`GT` / `tBn`, `src_169588164.js`) makes a tool-wide
    /// deny the FIRST branch of the authorization walk and returns a permission
    /// DECISION from it — `Permission to use ${e.name} has been denied.` — so
    /// the tool stays dispatchable. Hiding it here instead sent the call down
    /// the unknown-tool path: the model was told `tool not found`, and the
    /// permission decision never ran at all.
    ///
    /// Safe because the deny is re-decided downstream: `policy.rs`'s
    /// `authorize_inner` matches the tool-wide deny set first and returns
    /// `deny_with_rule`, so `tool.call` is never reached.
    #[must_use]
    pub(crate) fn find_tool_for_dispatch(
        &self,
        name: &str,
    ) -> Option<std::sync::Arc<dyn tool_api::tool_trait::Tool>> {
        self.lookup_tool(name, false)
    }

    /// Resolve `$.tool.call` against the live Native-equivalent active tool
    /// catalog, after the `Mx` wrapped-V1 filter. Returns the canonical tool
    /// handle itself so later validation, permission checks, and execution use
    /// the same Arc even if a dynamic catalog changes during the call.
    pub(crate) async fn resolve_mod_tool_call_tool(
        &self,
        requested_name: &str,
    ) -> Option<std::sync::Arc<dyn tool_api::tool_trait::Tool>> {
        self.filtered_available_tools()
            .await
            .into_iter()
            .find(|tool| {
                let wrapper = tool.native_mod_tool_batch_wrapper_facts();
                let hidden_wrapped_v1 = wrapper.underlying_v1_tool_name.is_some()
                    && wrapper.entry_field_name.is_some()
                    && wrapper.per_entry_hook_inputs_is_function
                    && wrapper.reassemble_is_function;
                !hidden_wrapped_v1
                    && (tool.name() == requested_name || tool.aliases().contains(&requested_name))
            })
    }

    fn lookup_tool(
        &self,
        name: &str,
        honor_pool_deny: bool,
    ) -> Option<std::sync::Arc<dyn tool_api::tool_trait::Tool>> {
        let tool = self.tools.find_by_name(name)?;
        if self
            .main_agent_tool_names
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|names| !names.contains(tool.name()))
        {
            return None;
        }
        let ctx = tool_api::tool_trait::ToolStaticContext {
            main_loop_model: self.tools.main_loop_model(),
            ..Default::default()
        };
        if !tool.is_enabled(&ctx)
            || (honor_pool_deny && self.is_tool_pool_denied(tool.as_ref()))
            || (self.is_coordinator_mode_enabled() && !self.is_coordinator_pool_tool(tool.as_ref()))
        {
            return None;
        }
        Some(tool)
    }

    /// Whether a memory prefetcher has been wired via
    /// [`Self::with_memory_prefetch`]. (P0.1)
    #[must_use]
    pub fn has_memory_prefetch(&self) -> bool {
        self.prompt_runtime.memory_prefetch.is_some()
    }

    /// Wire the EXPERIMENTAL_SKILL_SEARCH skill-discovery prefetcher. The
    /// composition root calls this ONLY when the flag is ON (default OFF), so an
    /// unwired build keeps [`Self::start_skill_discovery_prefetch`] +
    /// [`Self::skill_discovery_reminder_message`] strict no-ops and the locked
    /// fixtures byte-identical (the presence of the prefetch IS the gate, exactly
    /// like `with_memory_prefetch`).
    #[must_use]
    pub fn with_skill_discovery_prefetch(
        mut self,
        prefetch: Arc<skill_api::SkillDiscoveryPrefetch>,
    ) -> Self {
        self.prompt_runtime.skill_discovery_prefetch = Some(prefetch);
        self
    }

    /// Whether a skill-discovery prefetcher has been wired via
    /// [`Self::with_skill_discovery_prefetch`] (EXPERIMENTAL_SKILL_SEARCH).
    #[must_use]
    pub fn has_skill_discovery_prefetch(&self) -> bool {
        self.prompt_runtime.skill_discovery_prefetch.is_some()
    }

    /// Wire the standalone session-memory extractor (§6.5). `None` (the default)
    /// keeps it inert. The composition root builds the handle (gated, default
    /// off) via [`crate::prompt::build_session_memory_handle`].
    #[must_use]
    pub fn with_session_memory(mut self, handle: Arc<SessionMemoryHandle>) -> Self {
        self.compaction_runtime.session_memory = Some(handle);
        self
    }

    /// Wire the source of completed background (`async`) hook responses, folded
    /// back into the next turn by [`Self::async_hook_response_mod_messages`]
    /// (claude-code `getAsyncHookResponseAttachments`). Without it that method
    /// is a strict no-op.
    #[must_use]
    pub fn with_async_hook_responses(
        mut self,
        provider: Arc<dyn crate::prompt::async_hook_response::AsyncHookResponseProvider>,
    ) -> Self {
        self.prompt_runtime.async_hook_responses = Some(provider);
        self
    }

    /// T35: wire the source of terminal background tasks, folded back into the
    /// next turn as a `<task-notification>` reminder by
    /// [`Self::task_notification_reminder_messages`] (claude-code's per-task-type
    /// `enqueue*Notification`). Without it that method is a strict no-op.
    #[must_use]
    pub fn with_task_notifications(
        mut self,
        provider: Arc<dyn crate::prompt::task_notification::TaskNotificationProvider>,
    ) -> Self {
        if let (Ok(runtime), Some(mut events)) = (
            tokio::runtime::Handle::try_current(),
            provider.subscribe_task_lifecycle(),
        ) {
            let output = Arc::downgrade(&self.output);
            let analytics_bus = self.model_runtime.analytics_bus.clone();
            self.prompt_runtime.task_lifecycle_relay = Some(
                tokio_util::task::AbortOnDropHandle::new(runtime.spawn(async move {
                    while let Some(event) = events.recv().await {
                        let Some(output) = output.upgrade() else {
                            break;
                        };
                        if event.get("type").and_then(serde_json::Value::as_str)
                            == Some("task_feature")
                        {
                            if let Some(bus) = analytics_bus.as_ref() {
                                let mut metadata = telemetry::LogEventMetadata::new();
                                if let Some(feature) = event
                                    .get("feature_name")
                                    .and_then(serde_json::Value::as_str)
                                {
                                    metadata.insert(
                                        "feature_name".into(),
                                        telemetry::AnalyticsValue::String(feature.into()),
                                    );
                                    let error =
                                        event.get("error_code").and_then(serde_json::Value::as_str);
                                    if let Some(error) = error {
                                        metadata.insert(
                                            "error_code".into(),
                                            telemetry::AnalyticsValue::String(error.into()),
                                        );
                                    }
                                    bus.log_event(
                                        if error.is_some() {
                                            "tengu_feature_bad"
                                        } else {
                                            "tengu_feature_ok"
                                        },
                                        metadata,
                                    )
                                    .await;
                                }
                            }
                        } else {
                            output.emit_task_lifecycle(&event).await;
                        }
                    }
                })),
            );
        }
        self.prompt_runtime.task_notifications = Some(provider);
        self
    }

    /// Wire the source of the `Stop` / `SubagentStop` hook `background_tasks` +
    /// `session_crons` snapshot (claude-code `Lic(taskRegistry.all())` /
    /// `Mic()`). Without it both fields stay `None` and the keys are omitted on
    /// every payload (the no-registry default — byte-identical to today).
    #[must_use]
    pub fn with_stop_hook_snapshot(
        mut self,
        provider: Arc<dyn crate::stop_hook_snapshot::StopHookSnapshotProvider>,
    ) -> Self {
        self.lifecycle_runtime.stop_hook_snapshot = Some(provider);
        self
    }

    /// Wire the mid-turn input source consulted by the streaming turn loop at
    /// each cancel-check point to drain queued user input WITHIN the running turn
    /// (claude-code's query.ts mid-turn injection). Without it the mid-turn drain
    /// is a strict no-op, so a turn with no source wired is byte-identical to
    /// today. Builder form (fresh construction). See
    /// [`Self::set_mid_turn_input`] for the post-`Arc` (`&self`) form the bridge
    /// uses with its per-connection queue.
    #[must_use]
    pub fn with_mid_turn_input(
        self,
        source: Arc<dyn crate::prompt::mid_turn_input::MidTurnInputSource>,
    ) -> Self {
        self.set_mid_turn_input(source);
        self
    }

    /// Wire the abort-reason flag shared with the queue adapter so the streaming
    /// loop can tell a `Now`-command abort from a user Ctrl+C/ESC interrupt.
    /// Without it every abort is labeled a user interrupt (today's behavior).
    /// Builder form. See [`Self::set_cancel_reason`] for the `&self` form.
    #[must_use]
    pub fn with_cancel_reason(self, flag: crate::prompt::mid_turn_input::CancelReasonFlag) -> Self {
        self.set_cancel_reason(flag);
        self
    }

    /// The per-turn tallies the `/loop` no-op fold reads (see
    /// [`crate::turn_span`]): reset at every turn's start, snapshotted at its
    /// completion edge.
    #[must_use]
    pub fn turn_span(&self) -> &crate::turn_span::TurnSpanTally {
        &self.turn_span
    }

    /// Finding #73: wire the V2 task source consulted by the per-turn
    /// `task_reminder` ([`Self::todo_reminder_message`], the binary's `B4p`).
    /// Without it the V2 reminder still fires when its counters/gates are met,
    /// but renders with no task items (base text only). The V1 (`todo_reminder`)
    /// path reads `session.todos` directly and needs no provider.
    #[must_use]
    pub fn with_todo_reminder_tasks(
        mut self,
        provider: Arc<dyn crate::prompt::todo_reminder::TodoReminderTaskProvider>,
    ) -> Self {
        self.prompt_runtime.todo_reminder_tasks = Some(provider);
        self
    }

    /// Whether an MCP registry has been wired via
    /// [`Self::with_mcp_registry`]. (M6-07)
    #[must_use]
    pub fn has_mcp_registry(&self) -> bool {
        self.mcp_registry.is_some()
    }

    /// Whether the wired MCP registry has the OAuth seam injected
    /// ([`mcp::registry::OAuthDeps`] via `McpRegistry::with_oauth`). `false`
    /// when no registry is wired OR the registry lacks OAuth (static-header
    /// fallback only). Lets the desktop composition-root test confirm OAuth is
    /// production-reachable end to end.
    #[must_use]
    pub fn has_mcp_oauth(&self) -> bool {
        self.mcp_registry.as_ref().is_some_and(|r| r.has_oauth())
    }

    /// Whether the wired MCP registry has a Cross-App-Access provider injected
    /// ([`mcp::registry::XaaConfigProvider`] inside [`mcp::registry::OAuthDeps`]).
    /// `false` when no registry/OAuth is wired OR no `xaaIdp` settings were
    /// present (XAA stays opt-in, falling through to the actionable hard-fail).
    /// Lets the desktop composition-root test confirm the XAA config layer is
    /// production-reachable once configured.
    #[must_use]
    pub fn has_mcp_xaa(&self) -> bool {
        self.mcp_registry.as_ref().is_some_and(|r| r.has_xaa())
    }

    /// Whether a hook registry has been wired via
    /// [`Self::with_hook_registry`]. (M6-07)
    #[must_use]
    pub fn has_hook_registry(&self) -> bool {
        self.lifecycle_runtime.hook_registry.is_some()
    }

    /// Whether an agent catalog has been wired via
    /// [`Self::with_agent_catalog`]. (M6-07)
    #[must_use]
    pub fn has_agent_catalog(&self) -> bool {
        self.lifecycle_runtime.agent_catalog.is_some()
    }

    /// Attach a [`compaction::CompactionOrchestrator`] so
    /// `force_compact` performs real history compaction. Without this,
    /// `force_compact` retains the M5-10 no-op shape. (M6-08)
    #[must_use]
    pub fn with_compaction(mut self, compactor: Arc<compaction::CompactionOrchestrator>) -> Self {
        self.compaction_runtime.compaction = Some(compactor);
        self
    }

    /// Attach the shared [`sidequery::CacheSafeParamsSlot`] the turn drivers
    /// write after every successful API call (In-Loop Compaction Batch 6).
    ///
    /// Pass the SAME `Arc` that was handed to
    /// [`compaction::Autocompactor::with_forked_runner`] at the composition
    /// root, so the forked autocompact summarizer reads the prefix this
    /// orchestrator produced. Without this the slot stays empty and the
    /// summarizer falls back to a no-cache forked call.
    #[must_use]
    pub fn with_cache_safe_slot(mut self, slot: Arc<sidequery::CacheSafeParamsSlot>) -> Self {
        self.model_runtime.cache_safe_slot = Some(slot);
        self
    }

    /// Attach the `/fork` background-agent spawner (the composition root's
    /// `BackgroundAgentSpawner`), so [`OrchestratorHandle::fork_conversation`]
    /// can spawn a detached background agent. Without it, `/fork` fails with a
    /// clear `ActionFailed`.
    #[must_use]
    pub fn with_fork_spawner(
        mut self,
        spawner: Arc<dyn lingxi_core::host::subagent_spawn::SubagentSpawner>,
    ) -> Self {
        self.fork_spawner = Some(spawner);
        self
    }

    /// Attach the owning session's enforced tool and shared budget handles for Agent hooks.
    #[must_use]
    pub fn with_hook_agent_inheritance(
        mut self,
        inheritance: lingxi_core::host::SubagentInheritance,
    ) -> Self {
        self.hook_agent_inheritance = Some(inheritance);
        self
    }

    /// Attach the budget enforcer a `/fork`-spawned background agent inherits
    /// (`SubagentInheritance::budget`). Pair with [`Self::with_fork_spawner`].
    #[must_use]
    pub fn with_fork_budget(
        mut self,
        budget: Arc<dyn lingxi_core::host::budget::BudgetEnforcerHandle>,
    ) -> Self {
        self.fork_budget = Some(budget);
        self
    }

    /// Attach the 2.1.212 `/fork` (`vAd`) background-session forker (the CLI
    /// composition root's `CliBgSessionForker`), so
    /// [`OrchestratorHandle::fork_to_background_session`] can copy the live
    /// conversation into a new background session. Without it, that `/fork`
    /// variant fails with a clear `ActionFailed`.
    #[must_use]
    pub fn with_bg_session_forker(
        mut self,
        forker: Arc<dyn lingxi_core::host::bg_session_forker::BgSessionForker>,
    ) -> Self {
        self.bg_session_forker = Some(forker);
        self
    }

    /// Attach the composition-root catalog reconciler used by
    /// [`Self::register_repo_root`].
    #[must_use]
    pub fn with_repo_root_reloader(
        mut self,
        reloader: Arc<dyn lingxi_core::host::RepoRootReloader>,
    ) -> Self {
        self.repo_root_reloader = Some(reloader);
        self
    }

    /// Attach the `/recap` side-query runner. Pass the SAME
    /// [`sidequery::ForkedAgentRunner`] `Arc` handed to
    /// [`compaction::Autocompactor::with_forked_runner`] at the composition root
    /// (clone it BEFORE that move) so recap replays the identical cache-safe
    /// prefix the summarizer would. Without it, `/recap` fails gracefully.
    #[must_use]
    pub fn with_recap_runner(mut self, runner: Arc<sidequery::ForkedAgentRunner>) -> Self {
        self.recap_runner = Some(runner);
        self
    }

    /// (`/rewind`) Share the file-history checkpoint store — the SAME
    /// `Arc<session::FileHistory>` the CLI holds for restore + picker rows. The
    /// turn loop then snapshots each turn and tracks tool edits.
    #[must_use]
    pub fn with_file_history(mut self, file_history: Arc<session::FileHistory>) -> Self {
        self.file_history = Some(file_history);
        self
    }

    /// (`/fast`) Share the session's fast-mode flag with this orchestrator — the
    /// SAME `Arc<AtomicBool>` the request-building `ProviderApiAdapter` holds, so
    /// the handle's `set_fast_mode` flip is seen by the adapter on the next
    /// turn. Without it the flag is a private always-`false` default (no request
    /// ever carries `speed`).
    #[must_use]
    pub fn with_fast_mode(mut self, flag: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
        self.fast_mode = flag;
        self
    }

    /// Wire the passive `<new-diagnostics>` source (the LSP diagnostic registry)
    /// so each turn surfaces newly-reported LSP diagnostics to the model. See
    /// [`Self::new_diagnostics_source`] / [`Self::new_diagnostics_reminder_message`].
    #[must_use]
    pub fn with_new_diagnostics_source(
        mut self,
        source: Arc<dyn lingxi_core::host::NewDiagnosticsSource>,
    ) -> Self {
        self.prompt_runtime.new_diagnostics_source = Some(source);
        self
    }

    /// Whether a [`compaction::CompactionOrchestrator`] has been
    /// wired via [`Self::with_compaction`]. (M6-08)
    #[must_use]
    pub fn has_compaction(&self) -> bool {
        self.compaction_runtime.compaction.is_some()
    }

    /// Construct a new orchestrator with a fresh in-memory session and
    /// the BATCHED API client only — the streaming field is wired with
    /// the [`NoStreamingApiClient`] stub so any `run_turn_streaming`
    /// call surfaces "no streaming client configured" rather than
    /// panicking. M5-02 / M5-03 callers continue to use this signature
    /// unchanged.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        config: OrchestratorConfig,
        api: Arc<dyn OrchestratorApiClient>,
        tools: Arc<ToolRegistry>,
        hooks: Arc<HookExecutor>,
        perms: Arc<dyn PermissionGate>,
        output: Arc<dyn OutputStream>,
        memory: Arc<dyn crate::prompt::MemoryHierarchyProvider>,
        cwd: std::path::PathBuf,
    ) -> Self {
        Self::new_with_streaming(
            config,
            api,
            Arc::new(NoStreamingApiClient),
            tools,
            hooks,
            perms,
            output,
            memory,
            cwd,
        )
    }

    /// Whether any registered hook subscribes to the `Notification` event.
    ///
    /// Cheap gate read for callers that want to avoid arming a fire path with
    /// no subscriber — e.g. the CLI repl's idle-prompt timer only arms when
    /// this is `true`, mirroring how `engine-desktop`'s settings watcher only
    /// arms the `ConfigChange` fire when a subscriber exists. A `true` here
    /// reports event-type subscription only (a declared matcher may still
    /// filter the hook out at fire time), which is exactly what the gate needs.
    pub async fn has_notification_hook(&self) -> bool {
        self.hooks
            .has_hooks_for(&hooks::events::HookEventType::Notification)
            .await
    }

    /// Read the `should_exit` flag set by `/exit` (M5-10 / M5-13).
    ///
    /// The REPL checks this after each dispatch and breaks the loop if
    /// `true`. The flag is set via
    /// [`lingxi_core::host::OrchestratorHandle::request_exit`]; once set it
    /// never resets (idempotent `/exit`).
    pub fn current_should_exit(&self) -> bool {
        self.should_exit.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Hermetic override for the roots [`Self::nested_memory_reminder_messages`]
    /// probes. See [`Self::nested_memory_roots`].
    #[must_use]
    pub fn with_nested_memory_roots(
        mut self,
        home: std::path::PathBuf,
        managed_dir: Option<std::path::PathBuf>,
    ) -> Self {
        self.prompt_runtime.nested_memory_roots = Some((home, managed_dir));
        self
    }

    /// Borrow the in-memory session (read-write lock surrogate). Useful for tests.
    #[must_use]
    pub fn session(&self) -> Arc<Mutex<SessionState>> {
        self.session.clone()
    }

    /// Snapshot the live conversation history for an embedding host that owns
    /// a separate durable session store.
    pub async fn snapshot_history(&self) -> Vec<ConversationMessage> {
        self.session.lock().await.history.clone()
    }

    /// Restore history before the first turn of a newly constructed
    /// orchestrator. The session id and all host-owned runtime state remain
    /// local to this orchestrator; only the ordered message transcript is
    /// adopted.
    pub async fn restore_history(&self, history: Vec<ConversationMessage>) -> Result<(), String> {
        let mut session = self.session.lock().await;
        if !session.history.is_empty() {
            return Err("cannot restore Agent history after a turn has started".into());
        }
        session.history = history;
        Ok(())
    }

    /// Return registered tool names in registry partition order, before the
    /// model-visible enablement and permission filters.
    #[must_use]
    pub fn tool_names(&self) -> Vec<String> {
        self.tools.all_names()
    }

    /// Names from the same ordered, filtered catalog used by model requests.
    /// Headless initialization must not advertise disabled registered tools.
    pub async fn advertised_tool_names(&self) -> Vec<String> {
        self.filtered_available_tools()
            .await
            .into_iter()
            .map(|tool| tool.name().to_owned())
            .collect()
    }

    /// The session's DEFAULT main-loop model — the boot-configured model the
    /// stream-json `set_model` control_request resolves `"default"` (or an
    /// absent model) back to (claude-code `getDefaultMainLoopModel()`), so a
    /// client can revert a prior `set_model` override.
    #[must_use]
    pub fn default_model(&self) -> String {
        self.config.model.clone()
    }
}
