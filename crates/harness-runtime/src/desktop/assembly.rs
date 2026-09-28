use crate::desktop::ide::DesktopIdeHandle;
use client_adapter::{AdapterPermissionGate, PermissionRequestSink};
use command_api::model::BuiltinCommandHandler;
use command_api::{parse_slash_command, CommandRegistry, RegistrySlashDispatcher};
use cost::CostHydrator;
use orchestrator::model::user_agent::UserAgentEnv;
use orchestrator::test_support::{NoOpPermissionGate, StaticMemoryProvider};
use orchestrator::{
    ConversationOrchestrator, OrchestratorApiClient, OrchestratorConfig, ProviderApiAdapter,
};
use permission::gate::PermissionGate;
use platform_api::{McpTransport, OrchestratorHandle, OutputStream};
use platform_posix::{
    PosixFileSystem, PosixProcess, PosixRuntime, PosixSandbox, PosixWorktreeManager,
};
#[cfg(windows)]
use platform_windows::process::supervisor as shell_supervisor;
#[cfg(windows)]
use platform_windows::WindowsMcpTransport;
use sandbox::runtime_config::Platform as SandboxPlatform;
use skill_api::SkillRegistry;
use std::sync::Arc;
use std::sync::OnceLock;
use tokio::sync::RwLock;
use tool_api::AnthropicRequestBuilder;
use tool_api::SessionCwd;
use tool_api::{BuiltinToolContext, ToolRegistry};

#[cfg(test)]
use super::watcher_test_support;
use super::{
    agent_restore, agent_skill_loader, agent_source_is_trusted, append_mcp_permission_rules,
    append_restricted_builtin_denies, apple_events_override, apply_worktree_launch,
    auto_mode_propose, aws_auth_refresher, background_agent, build_agent_mcp_tool_set,
    build_shared_credential_stack_for_config, capture_legacy_opening_balance, cron_command,
    cron_management, cron_native, cron_scheduler_enabled, desktop_command_registry,
    desktop_fusion_attempts, desktop_fusion_executor, desktop_skill_registry, discover_plugin_set,
    emit_mcp_servers_inventory, emit_mcp_tools_commands_loaded, ephemeral_session_home,
    expand_trusted_dir, file_changed_watch, fork_resume, fusion_command, fusion_recorder,
    is_env_truthy, job_dir_from_env, lingxi_temp_dir, load_ask_user_question_timeout,
    load_blocked_marketplaces, load_boot_permission_tiers_with_flag,
    load_effective_settings_for_config, load_managed_plugin_names,
    load_merged_agent_push_notif_enabled, load_merged_attribution,
    load_merged_bash_output_max_chars, load_merged_disable_all_hooks, load_merged_hooks_restricted,
    load_merged_http_hook_policy, load_merged_output_style, load_merged_settings,
    load_merged_show_thinking_summaries, load_merged_skip_web_fetch_preflight,
    load_merged_task_output_max_chars, load_merged_vision_delegation_enabled,
    load_merged_workflow_keyword_trigger_enabled, load_plugin_configs,
    managed_only_sandbox_overrides, mcp_on_authorization_url, mcp_servers_inventory_payload,
    mcp_tools_commands_loaded_payload, merge_agent_frontmatter_mcp_servers, merge_cli_flag_agents,
    new_desktop_mcp_transport, new_live_sandbox_runner_with_permission_gate, pane_teammate,
    platform_in_enabled_list, plugin_dir_watch_enabled,
    register_desktop_tools_with_fusion_recorder, registered_mcp_tool_count,
    registry_skill_listing_provider, resolve_bash_edit_diff, resolve_llm_stack_with_credentials,
    resolve_memory_feature_gates, resolve_workflow_session_enabled,
    resolve_workflow_size_guideline, ripgrep_override, sandbox_auto_allow_from_settings_tiers,
    sandbox_runtime_config_from_settings_tiers, session_agents, session_kind_for_job_tmp,
    session_read_allowances_for_boot, session_state, session_task_output_dir, settings_watch,
    should_enforce_permissions, skill_loader, spawn_cli_plugin_dir_collection_watch,
    strict_allowlist_override, teammate_backend_selector, AgentMcpMergeGates,
    AsyncHookResponseBuffer, BootPermissionTiers, BuildError,
    CoordinatorTeammateDefinitionResolver, CoordinatorWiring, CredentialStoreAuthProvider,
    DeferredToolInvoker, DesktopBashRunner, DesktopConfig, DesktopHookMcpInvoker,
    DesktopRepoRootReloader, DesktopRuntime, DesktopSessionLifecycle,
    DesktopWebSearchConfigProvider, DesktopWorkflowEvent, DesktopWorkflowEventSink,
    DesktopWorktreeCommandHandler, FusionCatalogRefreshingChatGptConnect,
    FusionCatalogRefreshingCopilotConnect, FusionCatalogRefreshingCredentialWriter,
    FusionCatalogRefreshingOAuthConnect, JsonlWorktreeStatePersister, LlmStack,
    OrchestratorTeammatePromptRenderer, OutputRetryReporter, PluginRuntime,
    ProcessSessionActivationObserver, RegistryStopHookSnapshot, SharedCredentialStack,
    TaskRegistryWorkflowLauncher, TeammateStatusFanout, TEAMMATE_POOL_CAP,
};

pub async fn build(
    cfg: DesktopConfig,
    output: Arc<dyn OutputStream>,
    permission_sink: Arc<dyn PermissionRequestSink>,
) -> Result<DesktopRuntime, BuildError> {
    let shared = build_shared_credential_stack_for_config(&cfg).await?;
    build_with_credential_stack(cfg, output, permission_sink, shared).await
}

/// Build a bridge runtime whose host owns versioned automation execution.
/// No native CLI firer is attached during construction, even before host binding.
pub async fn build_with_host_automation(
    cfg: DesktopConfig,
    output: Arc<dyn OutputStream>,
    permission_sink: Arc<dyn PermissionRequestSink>,
    shared: SharedCredentialStack,
) -> Result<DesktopRuntime, BuildError> {
    cron_native::HOST_AUTOMATION
        .scope(
            true,
            build_with_credential_stack(cfg, output, permission_sink, shared),
        )
        .await
}

/// Build using a host-seeded credential stack; secrets never enter configuration.
pub async fn build_with_credential_stack(
    mut cfg: DesktopConfig,
    output: Arc<dyn OutputStream>,
    permission_sink: Arc<dyn PermissionRequestSink>,
    shared: SharedCredentialStack,
) -> Result<DesktopRuntime, BuildError> {
    let native_cron_seed = cron_native::should_start_native_scheduler(&cfg).then(|| {
        let mut seed = cfg.clone();
        seed.session_writer_lease = None;
        (seed, permission_sink.clone())
    });
    // Consume the construction-only writer claim before any config-derived
    // stack is cloned. Long-lived settings/catalog clones must not retain an
    // obsolete session authority across a hot clear/resume.
    let construction_writer_lease = cfg.session_writer_lease.take();
    let cwd = cfg.cwd.clone();
    let managed_settings_for_strict =
        crate::desktop::settings_watch::managed_settings_raw_tiers().await;
    let effective_settings = load_effective_settings_for_config(&cfg, &managed_settings_for_strict);
    let vision_delegation_enabled = if cfg.restricted {
        effective_settings
            .as_ref()
            .and_then(|settings| settings.settings.vision_delegation_enabled)
            .unwrap_or(true)
    } else {
        load_merged_vision_delegation_enabled(&cwd)
    };

    // On-disk data-retention sweep (claude-code `fWu`). DELETES stale
    // session-file entries (todos/statsig/logs older than the retention period),
    // so it is flag-gated and default-OFF: a no-op unless `LINGXI_RETENTION_SWEEP`
    // is truthy. Runs once at boot on a blocking pool so it never delays startup.
    tokio::task::spawn_blocking(memory::retention::run_startup_retention_sweep);

    // FIX A/B/C: mint the boot-canonical MAIN session id ONCE and derive the
    // session's transcript path + subagents dir from `(lingxi_home, cwd, id)`.
    // claude-code's `createBaseHookInput` (utils/hooks.ts:322) ALWAYS stamps
    // `transcript_path: getTranscriptPathForSession(sessionId)` on EVERY hook
    // payload, and `getAgentTranscriptPath` anchors spawned-subagent transcripts
    // under `<projectDir>/<sessionId>/subagents`. The orchestrator generates its
    // own `SessionId` INSIDE `ConversationOrchestrator::new`, so historically no
    // single id was knowable at boot — the leaf firers / subagent spawner (built
    // BEFORE the orchestrator) fired with an EMPTY `transcript_path` / a `/tmp`
    // subdir. We close that by minting the id here and:
    //   - handing it to the orchestrator via `.with_session_id` (so its live
    //     session matches), and to the firers as the precomputed `transcript_path`;
    //   - handing the subagents dir to the spawner via `with_hook_context`.
    // The path helpers live in `orchestrator::transcript_paths` (a facade over
    // `session::jsonl::path`) so this app needs no direct `session` dep.
    // claude-code `--session-id <uuid>`: honor a host-provided session id when
    // present (already UUID-validated by the host), else mint a fresh one. The
    // override carries through to the transcript path, the firers' precomputed
    // `transcript_path`, and the orchestrator's live `.with_session_id`, so a
    // resumed/SDK-pinned id is consistent everywhere.
    let main_session_id = cfg
        .session_id_override
        .as_deref()
        .and_then(protocol::SessionId::parse_prefixed)
        .unwrap_or_default();
    let main_session_uuid = main_session_id.as_uuid().to_string();
    // (/rewind) One shared file-history checkpoint store: cloned into the
    // orchestrator (per-turn snapshots + pre-edit tool backups) AND the
    // DesktopRuntime (so the CLI can restore + build the picker rows). Backups
    // live under `<lingxi_home>/file-history/<session>/`.
    let file_history = std::sync::Arc::new(session::FileHistory::new(
        cfg.lingxi_home.clone(),
        cwd.clone(),
        main_session_uuid.clone(),
    ));
    let main_transcript_path = orchestrator::transcript_paths::main_transcript_path(
        &cfg.lingxi_home,
        &cwd.to_string_lossy(),
        &main_session_uuid,
    );
    // Create the one ordinary transcript writer before durable session setup.
    // Production later decorates this same writer with the coordinator's
    // durable transaction; legacy/no-persistence hosts keep compatibility.
    let main_jsonl_writer = session::jsonl::writer::JsonlWriter::new(
        main_transcript_path.clone(),
        Arc::new(PosixFileSystem::new(cwd.clone())) as Arc<dyn platform_api::FileSystem>,
    );
    // Resume the live file-history index before the first restored turn. The
    // `/rewind` command can parse snapshots directly from disk, but the edit
    // tools and turn-start snapshotter share this in-memory instance. Leaving
    // it empty on resume caused the next edit to restart at version 1 and lose
    // the previously tracked-file set.
    if let Ok(content) = std::fs::read_to_string(&main_transcript_path) {
        file_history.restore_from_records(session::file_history::parse_snapshot_records(&content));
    }
    let main_subagents_dir = orchestrator::transcript_paths::subagents_dir(
        &cfg.lingxi_home,
        &cwd.to_string_lossy(),
        &main_session_uuid,
    );

    // The credential/provider half of boot lives in `resolve_llm_stack` so the
    // headless one-shot commands share it byte-for-byte. Everything below this
    // point is session-shaped and stays here.
    let LlmStack {
        catalog_registry,
        provider_region,
        http,
        clock,
        mcp_oauth_storage,
        credentials,
        auth,
        subscription,
        resolved_anthropic_api_key,
        is_subscriber,
        openai_oauth_client,
        pricing,
        chains,
        model_providers,
        fusion_catalog_source,
        fusion_catalog_refresher,
        default_listings,
        default_model_id,
        default_model_profile,
        profile_first_party,
        profile_auto_mode_provider,
        first_party_environment_provider,
        provider_availability,
        default_model_fallback,
        model_provenance,
        session_model_restriction,
        model_setting_for_spawns,
        session_provider_first_party,
        session_auto_mode_provider,
        llm_runtime,
        llm_transport,
        cost_estimator,
        subscriber_state,
        credential_origin,
        has_oauth_token,
        ..
    } = resolve_llm_stack_with_credentials(&cfg, shared).await?;

    // Phase 2a CHAINS BRIDGE: translate the assembled `ChainConfig` into main's
    // richer adapter's `fallback_overrides` shape. `assemble` keys each chain by
    // the request/display model id and carries an ordered list of `ChainEntry`;
    // main's adapter routes by model-id through the multi-provider registry, so the
    // `ChainEntry.provider_id` is informational and is dropped here — the per-entry
    // `model` ids are the fallback chain. Cross-provider routing still works
    // because every provider's models are registered in the assembled
    // `ClientConfig`, so a fallback target on another provider resolves by id.
    let fallback_overrides: std::collections::BTreeMap<String, Vec<String>> = chains
        .chains
        .iter()
        .map(|(key, entries)| {
            (
                key.clone(),
                entries.iter().map(|e| e.model.clone()).collect(),
            )
        })
        .collect();
    // Retry override → main's scalar settings_max_retries / settings_backoff_ms.
    let settings_max_retries = chains.retry.as_ref().map(|r| r.max_attempts);
    let settings_backoff_ms = chains.retry.as_ref().map(|r| r.backoff_ms);

    // Build the CONCRETE adapter so it can be coerced to BOTH the orchestrator
    // seam (`OrchestratorApiClient`) and the agent seam (`agent::SubagentApiClient`).
    // `ProviderApiAdapter` impls both. The adapter's own `alias_to_display` map is
    // rebuilt from the client's `available_models()` (whose aliases `assemble`
    // already populated from `chains.aliases`), so the alias map needs no separate
    // pass here.
    //
    // Batch-5 Task 3: attach the live subscription slot (filled by the background
    // profile/roles fetch) so the drive loops read subscriber/enterprise state at
    // call time — `subscriber_state` remains the build-time seed/fallback.
    // M7: one analytics bus shared by the provider adapter (`tengu_api_*`) and the
    // orchestrator (`tengu_api_success` per completed response) so all live
    // telemetry lands on the same sink set — 1:1 with claude-code, where
    // `logEvent` is a single global pipeline.
    let analytics_bus = Arc::new(telemetry::AnalyticsBus::new());
    // metadata.user_id (getAPIMetadata, claude.ts:519): the JSON-string identity
    // `{...extra, device_id, account_uuid, session_id}`. `device_id` =
    // getOrCreateUserID (persisted, stable per install); `session_id` = the main
    // session id (claude-code's getSessionId()); `account_uuid` = "" — the OAuth
    // profile carrying the account UUID is fetched asynchronously in the
    // background and is not available at construction, so this is the faithful
    // `getOauthAccountInfo()?.accountUuid ?? ''` fallback (the value is per-account
    // and never byte-matches claude-code regardless).
    let request_metadata = llm_runtime::RequestMetadata {
        user_id: llm_runtime::ApiService::build_api_metadata_user_id(
            &migrations::global_config::get_or_create_user_id(),
            "",
            &main_session_uuid,
            cfg.parent_session_id.as_deref(),
        ),
    };
    // Build the provider-neutral drive service (the retry/rate-limit/betas loop),
    // then wrap it in the thin `ProviderApiAdapter` that impls the orchestrator +
    // agent seams. The `with_*` builders live on `ApiService`.
    let session_composition = cfg.session_composition();
    let interactive_session = session_composition.is_interactive_session();
    let service_built = llm_runtime::ApiService::new_with_routing(
        llm_runtime,
        llm_transport,
        subscriber_state,
        UserAgentEnv::from_process_env(),
        env!("CARGO_PKG_VERSION"),
        Some(analytics_bus.clone()),
        cfg.fallback_model.clone(),
        Some(cost_estimator),
        fallback_overrides,
        settings_max_retries,
        settings_backoff_ms,
    )
    .with_subscription(subscription.clone())
    .with_interactive_session(interactive_session)
    .with_custom_cli_betas(cfg.custom_betas.clone())
    .with_request_metadata(request_metadata)
    // Boot SESSION thinking config, resolved host-side from MAX_THINKING_TOKENS
    // + --max-thinking-tokens + alwaysThinkingEnabled (claude-code `qIe()`+`wn`).
    // Default `Adaptive` keeps every existing session byte-identical; a fixed
    // env/flag budget pre-empts adaptive, `alwaysThinkingEnabled:false` disables.
    .with_thinking(cfg.session_thinking)
    // Surface API retry/backoff status to the UI (Claude Code's
    // `SystemAPIErrorMessage`): the retry loop reports each backoff and the
    // adapter forwards it to the session output stream (→ TUI).
    .with_retry_reporter(std::sync::Arc::new(OutputRetryReporter {
        output: output.clone(),
    }));
    // M2 (2.1.198): attach the AWS auth-refresh driver (`ZBd`/`t2d`) when an
    // `awsAuthRefresh` / `awsCredentialExport` command is configured. Resolves
    // the merged settings value + its Project provenance (binary `mqe`: a
    // project/local-sourced command is refused before workspace trust) and the
    // workspace-trust state (`yd()`: `hasTrustDialogAccepted` parent-walk in
    // the global config). With the driver attached, a Bedrock 401/403
    // (expired STS) runs the refresh script and retries instead of
    // dead-ending — bounded at Ygf=2 inside the drive loops.
    let service_built = match aws_auth_refresher(&cfg, &cwd, analytics_bus.clone()) {
        Some(refresher) => service_built.with_aws_auth(refresher),
        None => service_built,
    };
    // `--json-schema` structured output: FORCE the `StructuredOutput` tool so the
    // model returns its final result through it (1:1 with claude-code). Untouched
    // for every normal turn (`json_schema` is `None`).
    let service_built = if cfg.json_schema.is_some() {
        service_built.with_forced_tool_choice(llm_runtime::ToolChoice::Tool {
            name: orchestrator::structured_output::STRUCTURED_OUTPUT_TOOL_NAME.to_string(),
        })
    } else {
        service_built
    };
    // (M4 cc2.1.198) `--effort <level>` — the CLI-validated initial effort
    // rides the MAIN loop's requests as `output_config.effort` (binary session
    // state `thinkingConfig: SF(a.effort)`); `None` keeps bodies unchanged.
    // (/fast) One shared fast-mode flag cloned into BOTH the request-building
    // adapter (which reads it per-turn to send `speed:"fast"`) and the
    // orchestrator (whose `set_fast_mode` handle flips it). Same `Arc`, so a
    // live `/fast` toggle is seen by the adapter on the next turn. Defaults
    // `false`, so request bodies stay byte-identical until toggled.
    let fast_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Keep the concrete session service shared with compaction/recap. Their
    // forked summary call must use the same resolved provider route and live
    // credential as the parent turn (Claude Code's single API pipeline).
    let api_service = Arc::new(service_built);
    let provider_adapter = Arc::new(
        ProviderApiAdapter::new(api_service.clone())
            .with_initial_effort(cfg.initial_effort.clone().map(serde_json::Value::String))
            .with_fast_mode(fast_flag.clone()),
    );
    // WebSearch uses the resolved Anthropic key, while MCP large-result
    // confirmation reuses the fully routed/OAuth-aware main session provider.
    let tool_provider = Arc::new(
        AnthropicRequestBuilder::new(
            resolved_anthropic_api_key.clone().unwrap_or_default(),
            Some(cfg.api_base.clone()),
        )
        .with_mcp_token_counter(provider_adapter.clone()),
    );
    let provider_adapter_handle = provider_adapter.clone();
    let api_client: Arc<dyn OrchestratorApiClient> = provider_adapter.clone();
    // The SAME `ProviderApiAdapter` drives the streaming turn path: it impls both
    // `OrchestratorApiClient` (batched/non-stream) and `StreamingApiClient` (SSE),
    // and conversation.rs documents `self.api == self.streaming_api` in production.
    // Without this the orchestrator falls back to `NoStreamingApiClient` and every
    // streaming turn fails with "no streaming client configured".
    let streaming_api: Arc<dyn orchestrator::StreamingApiClient> = provider_adapter.clone();
    let subagent_api: Arc<dyn agent::SubagentApiClient> = provider_adapter;

    // (4) Orchestrator config from `cfg` (was `argv.model`).
    // The bridge persists the structured effort choice in the same user
    // settings file used by `/effort`. Seed it before the first turn; an
    // explicit CLI effort remains higher priority and is left untouched.
    let persisted_reasoning_selection = if cfg.initial_effort.is_none() {
        command_core::effort::load_reasoning_default_selection_at(
            &cfg.lingxi_home.join("settings.json"),
        )
    } else {
        None
    };
    let mut orch_cfg = OrchestratorConfig::default();
    orch_cfg.interactive_session = interactive_session;
    // Bridge hosts have a live permission surface even though their session
    // identity remains SDK. Treating them as headless denies Plan-mode questions.
    orch_cfg.interactive_permissions = session_composition.supports_interactive_permissions();
    // Resolve output style before query identity: Claude Code includes builtin
    // output-style names in `repl_main_thread:outputStyle:*`.
    let output_style = if cfg.restricted {
        effective_settings
            .as_ref()
            .and_then(|settings| settings.settings.output_style.clone())
    } else {
        load_merged_output_style(&cfg.cwd)
    };
    // Claude Code 2.1.245: CLI is `repl_main_thread`, SDK/bridge transport is
    // `sdk`, and print is the explicit headless print mode only.
    let (query_source, print) = session_composition
        .query_source_and_print(output_style.as_deref(), cfg.deny_unresolved_ask);
    orch_cfg.query_source = query_source;
    orch_cfg.print = print;
    orch_cfg.is_tty = cfg.is_tty;
    // TPM-C: use the bare id produced by parse_model_ref (strips a profile/ prefix
    // so a qualified default_model like "openai/gpt-4o" never reaches the wire).
    orch_cfg.model.clone_from(&default_model_id);
    // Opus-fallback hop: thread the (already print-mode-gated) fallback model
    // into `OrchestratorConfig.fallback_model`. `None` keeps the turn_loop's
    // 529-overload interception a strict no-op (`turn_loop.rs:496`).
    orch_cfg.fallback_model.clone_from(&cfg.fallback_model);
    // (2.1.212) CLI `--effort <level>` — the session's resolved reasoning-effort
    // level (already normalized to low/medium/high/xhigh/max). Threaded here so
    // every REAL assistant transcript line records it as a top-level `effort`
    // field (the SAME source the provider adapter uses for `output_config.effort`
    // via `with_initial_effort`). `None` (no `--effort`) omits the field, keeping
    // transcripts byte-identical.
    orch_cfg.effort.clone_from(&cfg.initial_effort);
    orch_cfg.workflow_keyword_trigger_enabled = if cfg.restricted {
        effective_settings
            .as_ref()
            .and_then(|settings| settings.settings.workflow_keyword_trigger_enabled)
            .unwrap_or(false)
    } else {
        load_merged_workflow_keyword_trigger_enabled(&cfg.cwd)
    };
    // CLI `--max-turns` / `--max-budget` caps. Unset leaves the OrchestratorConfig
    // defaults (unbounded turns / no cost cap). USD → nano-USD for the cost cap.
    if let Some(max_turns) = cfg.max_turns {
        orch_cfg.max_turns = max_turns;
    }
    // Structured output forces `tool_choice` to `StructuredOutput`, which compels
    // the model to call it on EVERY assistant turn — so cap the turn at a SINGLE
    // model call: the model calls the tool once (capturing the result), then the
    // cap ends the turn. The print path reads the captured slot regardless of the
    // resulting MaxTurns stop and drives its own validate/retry loop. (Overrides
    // any `--max-turns` here; structured output is inherently one-shot per turn.)
    if cfg.json_schema.is_some() {
        orch_cfg.max_turns = 1;
    }
    orch_cfg.max_budget_nano_usd = cfg.max_budget_usd.map(|usd| {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let nano = (usd.max(0.0) * 1_000_000_000.0) as u64;
        nano
    });
    // Subscription-flag hop (API.6): thread the resolved Claude.ai-subscriber flag
    // (computed in step 3.2 from the OAuth token scopes) into the orchestrator
    // config so the fallback-aware api-client seam resolves the consecutive-529
    // Opus-fallback gate and the 429-retry gate exactly as claude-code does.
    // (M13) `is_enterprise` is seeded from the tier PERSISTED in the stored
    // credential (claude-code reads `subscriptionType` synchronously from the
    // stored tokens) — no profile fetch in the build hot path, and no async
    // misclassification window before the background freshener lands.
    orch_cfg.is_subscriber = is_subscriber;
    orch_cfg.is_enterprise = subscriber_state.is_enterprise;
    // The API-key-disabled / credential-rejection copy names the specific
    // setting THIS user has to unset, so it needs the RESOLVED source — not
    // "is ANTHROPIC_API_KEY set", which is wrong whenever a higher-precedence
    // source wins.
    orch_cfg.credential_origin = credential_origin;
    orch_cfg.has_oauth_token = has_oauth_token;
    // OUTSTYLE.2: thread the merged `settings.outputStyle` (TS string) into the
    // orchestrator config so `build_system_prompt` injects the active style's
    // `# Output Style: <name>` section (Explanatory / Learning builtins). `None`
    // / "default" / unknown ⇒ no section (prompt byte-identical to before).
    orch_cfg.output_style = output_style;
    platform_api::session_flags::set_show_thinking_summaries(if cfg.restricted {
        effective_settings
            .as_ref()
            .and_then(|settings| settings.settings.show_thinking_summaries)
            .unwrap_or(false)
    } else {
        load_merged_show_thinking_summaries(&cfg.cwd)
    });
    platform_api::session_flags::set_agent_push_notif_enabled(if cfg.restricted {
        effective_settings
            .as_ref()
            .and_then(|settings| settings.settings.agent_push_notif_enabled)
            .unwrap_or(false)
    } else {
        load_merged_agent_push_notif_enabled(&cfg.cwd)
    });
    // `settings.taskOutputMaxChars` — the soft cap `TaskOutput` truncates a
    // task's model-facing output to, and the base its result budget is derived
    // from. Published RAW; `tool_task` applies the oracle's `see()` clamp.
    platform_api::session_flags::set_task_output_max_chars(if cfg.restricted {
        effective_settings
            .as_ref()
            .and_then(|settings| settings.settings.task_output_max_chars)
    } else {
        load_merged_task_output_max_chars(&cfg.cwd)
    });
    // `settings.bashOutputMaxChars` (2.1.261) — the same shape for Bash output.
    // Published RAW; `tool_shell` applies the `see()` clamp.
    platform_api::session_flags::set_bash_output_max_chars(if cfg.restricted {
        effective_settings
            .as_ref()
            .and_then(|settings| settings.settings.bash_output_max_chars)
    } else {
        load_merged_bash_output_max_chars(&cfg.cwd)
    });
    // `settings.attribution` / `settings.includeCoAuthoredBy` — the git
    // attribution trailers. Published at boot, not only from `/config`:
    // otherwise a user who has the setting on disk keeps emitting the trailer
    // until they happen to change it mid-session.
    let (attribution_commit, attribution_pr, include_co_authored_by) = if cfg.restricted {
        let attribution = effective_settings
            .as_ref()
            .and_then(|settings| settings.settings.attribution.clone());
        (
            attribution.as_ref().and_then(|a| a.commit.clone()),
            attribution.and_then(|a| a.pr),
            effective_settings
                .as_ref()
                .and_then(|settings| settings.settings.include_co_authored_by),
        )
    } else {
        load_merged_attribution(&cfg.cwd)
    };
    platform_api::session_flags::set_attribution(attribution_commit, attribution_pr);
    platform_api::session_flags::set_include_co_authored_by(include_co_authored_by);
    platform_api::session_flags::set_include_git_instructions(if cfg.restricted {
        effective_settings
            .as_ref()
            .and_then(|settings| settings.settings.include_git_instructions)
    } else {
        load_merged_settings(&cfg.cwd).and_then(|eff| eff.settings.include_git_instructions)
    });
    // OUTSTYLE.3: custom output-style search dirs — user (`~/.lingxi/output-styles`)
    // then project (`<cwd>/.lingxi/output-styles`), in increasing priority so a
    // project style overrides a user one and both override the builtins. A
    // `settings.outputStyle` naming a disk style now activates it
    // (`outputstyles::resolve_output_style`); absent dirs ⇒ builtin-only.
    orch_cfg.output_style_dirs = vec![
        cfg.lingxi_home.join("output-styles"),
        cfg.cwd.join(branding::DOT_DIR).join("output-styles"),
    ];
    // CLI `--system-prompt` / `--system-prompt-file`: override the assembled
    // system prompt for the session. `None` keeps the memory-hierarchy prompt
    // assembled from LINGXI.md files (byte-identical to the pre-field state).
    if let Some(override_prompt) = cfg.system_prompt_override.clone() {
        orch_cfg.system_prompt_override = Some(override_prompt);
    }
    // CLI `--plan-mode-instructions` (print-gated in init.rs): custom plan-mode
    // workflow body. `None` keeps the default 5-phase plan reminder.
    orch_cfg
        .plan_mode_instructions
        .clone_from(&cfg.plan_mode_instructions);
    // `settings.json` `plansDirectory` (206 `iT`): custom plan-file directory,
    // resolved against the project root with a within-root containment check by
    // the orchestrator. `None` keeps the default `<project-root>/.lingxi/plans/`
    // (project-local so the file tools' trusted-dir gate accepts the plan file).
    // 2.1.266 `Zl`/`ay`: the session's plan-file identity. Published into the
    // permission policy (so plan mode's one write carve-out fires — without it
    // the plan-mode reminder tells the model to write a file the gate then
    // prompts on) and read back by `ExitPlanMode` through the same policy, so
    // the file the model was allowed to write is exactly the file whose contents
    // are echoed on approval. The plans directory derivation is shared with the
    // orchestrator's reminder (`ConversationOrchestrator::plans_dir`) rather than
    // re-derived here.
    // 2.1.266 `getPlanSlug`: the plan file is named by a random three-word slug
    // (`brave-quiet-otter.md`), re-rolled on collision, NOT by the session id.
    // Upstream can seed it from the transcript (`planSlugSeed`); LingXi has no
    // seed source, so it takes the unseeded form.
    let plans_dir =
        orchestrator::ConversationOrchestrator::plans_dir(&cwd, cfg.plans_directory.as_deref());
    let plan_slug = platform_api::plan_slug::generate_slug(None, &|candidate| {
        platform_api::plan_slug::slug_taken_in(&plans_dir, candidate)
    });
    let plan_files = std::sync::Arc::new(permission::plan_files::PlanFileMatcher::with_identity(
        permission::plan_files::PlanFileIdentity {
            plans_dir,
            slug: plan_slug,
            // `ZUe()` — LingXi ships no workshop skill.
            workshop_enabled: false,
        },
    ));
    orch_cfg.plans_directory.clone_from(&cfg.plans_directory);
    // ONE plan-file identity: the reminder's path, the permission carve-out and
    // `ExitPlanMode`'s read-back all resolve through this object.
    orch_cfg.plan_files = Some(plan_files.clone());
    // CLI `--exclude-dynamic-system-prompt-sections`: move the per-machine env
    // block out of the (cacheable) system prompt into the first user message.
    orch_cfg.exclude_dynamic_system_prompt_sections = cfg.exclude_dynamic_system_prompt_sections;
    // (gap218 #43) The in-place (bridge/desktop) resume adopts a resumed agent's
    // frontmatter `model` ONLY when the user did NOT pass `--model` — the
    // hot-resume twin of the COLD-resume gate below (`!cfg.default_model_explicit`).
    // The root owns `--model`, so we resolve the gate here; the orchestrator then
    // resolves the alias → wire id and applies it. An explicit `--model` sets this
    // `false`, so it is never overridden by agent frontmatter.
    orch_cfg.apply_resumed_agent_model = !cfg.default_model_explicit;
    // CLI `--append-system-prompt` / `--append-system-prompt-file`: text to
    // append after the assembled system prompt (or after `system_prompt_override`
    // when both are set). Appended with a newline separator.
    if let Some(append) = cfg.append_system_prompt.clone() {
        let base = orch_cfg
            .system_prompt_override
            .get_or_insert_with(String::new);
        if !base.is_empty() {
            base.push('\n');
        }
        base.push_str(&append);
    }

    // Phase 2a T7: one pricing catalog backs both ordinary cost and Fusion.
    let pricing = Arc::new(pricing);

    // (4.5) Acquire the one process-wide writer claim and hydrate the mixed
    // session coordinator before constructing any paid tracker or Fusion
    // handler. When persistence is disabled this remains fully ephemeral and
    // no durable directories/locks are touched.
    // The migration path is resolved once from the same configured home that
    // owns the session ledger.  A TempDir/custom `lingxi_home` must not read
    // the developer's ambient global config; such hosts pass `None` until an
    // explicit legacy path is provided by their composition fixture.
    let legacy_config_path = if cfg.session_persistence {
        migrations::global_config::global_config_path().filter(|_| {
            migrations::global_config::lingxi_config_home()
                .is_some_and(|ambient_home| ambient_home == cfg.lingxi_home)
        })
    } else {
        None
    };
    let legacy_opening_balance =
        capture_legacy_opening_balance(legacy_config_path.as_deref(), &cfg.cwd);
    // `--no-session-persistence` means "leave no transcript behind", not
    // "spend without accounting". Fusion charges several models per run, so it
    // needs a ledger; giving the ephemeral host a disposable one is what lets
    // there be a single billing path instead of two. The directory lives under
    // the OS temp root and is removed at shutdown.
    let ephemeral_home = if cfg.session_persistence {
        None
    } else {
        Some(ephemeral_session_home()?)
    };
    let ledger_home = ephemeral_home
        .clone()
        .unwrap_or_else(|| cfg.lingxi_home.clone());
    let session_state_manager = {
        let legacy_shadow = legacy_opening_balance.map(|(legacy_session_id, amount)| {
            Arc::new(move |session_id| (session_id == legacy_session_id).then_some(amount))
                as Arc<dyn Fn(protocol::SessionId) -> Option<u64> + Send + Sync + 'static>
        });
        session_state::SessionStateManager::new_with_legacy_shadow(
            ledger_home.clone(),
            legacy_shadow,
        )
    };
    let (session_state, durable_hydration) = {
        let lease = if let Some(lease) = construction_writer_lease {
            lease
        } else {
            platform_api::live_sessions::LiveSessionDir::at_live(ledger_home.join("sessions"))
                .claim_session_id(&main_session_id.to_string(), std::process::id())
                .map_err(|error| BuildError::DurableSession(error.to_string()))?
                .into_shared()
        };
        let coordinator = session_state::SessionStateCoordinator::open(
            &ledger_home,
            main_session_id,
            lease.clone(),
        )
        .map_err(|error| BuildError::DurableSession(error.to_string()))?;
        let initialization: Result<_, cost::CostPersistError> = async {
            coordinator.start().await?;
            let _initial_hydration = coordinator.hydrate(main_session_id).await?;
            coordinator
                .import_legacy_opening_balance(
                    legacy_opening_balance
                        .filter(|(session_id, _)| *session_id == main_session_id)
                        .map(|(_, amount)| amount),
                )
                .await?;
            // The import marker/opening balance is itself a durable mutation,
            // so seed the tracker only from the post-import authoritative
            // projection.
            let hydration = coordinator.hydrate(main_session_id).await?;
            session_state_manager
                .register(main_session_id, coordinator.clone())
                .await?;
            Ok(hydration)
        }
        .await;
        let hydration = match initialization {
            Ok(hydration) => hydration,
            Err(error) => {
                let cleanup = coordinator.close_and_drain().await.err();
                let message = cleanup.map_or_else(
                    || error.to_string(),
                    |cleanup| format!("{error}; coordinator cleanup failed: {cleanup}"),
                );
                return Err(BuildError::DurableSession(message));
            }
        };
        (coordinator, hydration)
    };

    // Decorate the same writer that the orchestrator receives. Fusion delivery
    // resolves its active path under this writer's lock and the coordinator's
    // durable transaction, so ordinary append, outbox delivery, and `/cd`
    // retargeting share one authority.
    let main_jsonl_writer = if cfg.session_persistence {
        let coordinator = &session_state;
        let durable_writer = Arc::new(session::jsonl::DurableTranscriptWriter::from_pinned(
            coordinator.journal().root().to_path_buf(),
            coordinator.journal().root_identity(),
        ));
        let writer = Arc::new(main_jsonl_writer.with_durable_lock(durable_writer));
        writer
            .activate_session_target(
                main_session_id,
                main_transcript_path.clone(),
                cfg.cwd.clone(),
            )
            .map_err(|error| BuildError::DurableSession(error.to_string()))?;
        writer
    } else {
        Arc::new(main_jsonl_writer)
    };
    session_state_manager.set_transcript_writer(main_jsonl_writer.clone());
    let fusion_transcript_target =
        fusion_recorder::FusionTranscriptTarget::new(main_jsonl_writer.clone())
            .for_session(main_session_id);
    let fusion_recorder_factory_impl =
        Arc::new(fusion_recorder::DesktopFusionRecorderFactory::new(
            session_state_manager.clone(),
            fusion_transcript_target.clone(),
        ));
    // Resolve the boot recorder through the same factory retained for hot
    // sessions and shutdown recovery. This both shares its per-delivery lock
    // and ensures a boot outbox is included in `retry_pending_all()`.
    let fusion_recovery_recorder = fusion_recorder_factory_impl
        .recorder_for_session(main_session_id)
        .expect("boot durable session is registered before recorder wiring");
    let fusion_recorder =
        fusion_recovery_recorder.clone() as Arc<dyn platform_api::FusionRunRecorder>;
    let fusion_recorder_factory =
        fusion_recorder_factory_impl.clone() as Arc<dyn platform_api::FusionRunRecorderFactory>;

    // One CostTracker per process. The ephemeral path retains compatibility
    // with hosts that explicitly disabled session persistence; production
    // persistence uses the hydrated app-owned coordinator and its exact lease.
    let cost_tracker = {
        let coordinator_core = session_state_manager
            .coordinator_core(main_session_id)
            .ok_or_else(|| {
                BuildError::DurableSession("boot coordinator cache is missing".into())
            })?;
        Arc::new(
            cost::CostTracker::new(
                main_session_id,
                pricing.clone(),
                tokio::sync::mpsc::channel(1).0,
            )
            .try_with_durable_persistence(
                durable_hydration,
                coordinator_core.clone() as Arc<dyn cost::CostPersistence>,
                coordinator_core.writer_lease_core(),
                session_state.durability_gate(),
            )
            .map_err(|error| BuildError::DurableSession(error.to_string()))?,
        )
    };
    let subagent_usage_recorder = Arc::new(session_agents::DesktopSubagentUsageRecorder::new(
        cost_tracker.clone(),
    ));
    // Phase 2a T7: the CostTracker uses the SAME assembled pricing catalog the
    // estimator was built from (built-in reference tiers + non-Anthropic preset
    // rows + settings overrides), not a fresh `builtin_reference()`, so session
    // cost accounting matches per-response cost estimation. WP1 (F001/G003):
    // the SAME `Arc` also backs `desktop_fusion_executor`'s `FusionPriceBook`
    // adapter below, so Fusion's hard-budget quote/settlement prices against
    // the identical catalog the rest of the session bills from.
    // (4.6) Subagent spawner pool + budget enforcer for the `AgentTool` seam.
    //       `AgentTool::call` requires BOTH `subagent_spawner` and
    //       `budget_enforcer` to be `Some` — wiring the spawner alone is inert.
    //
    //       The pool is the production `StateMachinePool` driven by the posix
    //       `RuntimeSpawner`; the capacity mirrors Claude Code 2.1.217's
    //       `CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS` (default 20).
    //       `with_api_client(subagent_api)` hands the child runner the
    //       real model seam so spawned subagents drive the multi-turn
    //       `run_subagent_loop` (gated on `ctx.api_client.is_some()`) instead of
    //       the legacy stub completion.
    let subagent_pool = Arc::new(agent::StateMachinePool::new(
        Arc::new(PosixRuntime::new()),
        platform_api::subagent_spawn::max_concurrent_subagents(),
    ));
    // Clone the subagent model seam BEFORE it is moved into the spawner — the
    // M10 coordinator teammate handler (T13) hands the SAME seam to every
    // spawned `InProcessTeammate` so it drives the real multi-turn loop.
    let teammate_api = subagent_api.clone();
    // The LSP registry is composed later with the rest of the tool substrate.
    // Capture it through a set-once cell so both child factories can be wired
    // here without moving that large composition block. No child can spawn
    // before engine construction completes and the cell is filled below.
    let lsp_diagnostics_cell =
        Arc::new(OnceLock::<lsp::diagnostic_registry::LspDiagnosticRegistry>::new());
    let teammate_session_cwd_cell = Arc::new(OnceLock::<Arc<SessionCwd>>::new());
    // The spawner cannot receive the tool registry / agent catalog here: both
    // are built below, and the registry construction forms a cycle through
    // `BuiltinToolContext` (which consumes `subagent_spawner`). So we grab clones
    // of the spawner's set-once cells BEFORE boxing it, and fill them once the
    // registry + catalog exist (just after `desktop_tool_registry`, below).
    // `with_default_model` anchors `AgentModel::Inherit` + family-alias tiers to
    // the parent model so built-in subagent spawns resolve to a concrete wire id
    // instead of passing `"inherit"`/`"haiku"` raw (parity batch 22).
    // G4/SC-02: stamp the owning conversation id on child hook/checkpoint
    // context. The near-limit checkpoint uses this value as its per-session
    // dedupe/ref key, so a cosmetic random id would split it from the main
    // session's hard-429 checkpoint.
    let subagent_hook_session_id = main_session_id;
    let mut subagent_spawner_concrete = agent::PoolSubagentSpawner::new(subagent_pool)
        .with_refusal_fallback_chain(orch_cfg.refusal_chain())
        .with_session_interactive(interactive_session)
        .with_api_client(subagent_api)
        .with_usage_recorder(subagent_usage_recorder)
        // #15: the parent model handed to the spawner must be the RESOLVED
        // main-loop wire id (claude `getMainLoopModel()`), NOT the raw alias —
        // `orch_cfg.model` is `cfg.default_model` with only a `profile/` prefix
        // stripped (or the connected-provider fallback's rerouted id), so an
        // `opusplan`/`sonnet` install leaves it a bare alias. An
        // `AgentModel::Inherit` spawn in DEFAULT mode returns the parent verbatim,
        // which would be a bogus wire id that fails at the provider. Resolve it
        // here; the raw alias is still threaded via `with_model_setting` below for
        // the plan-mode `opusplan→Opus` swap.
        .with_default_model(agent::model_resolution::resolve_user_specified_model(
            &orch_cfg.model,
        ))
        // #15: thread the live permission mode + the RAW user model setting
        // (e.g. "opusplan" / "haiku" — claude-code's
        // `getUserSpecifiedModelSetting()`, the UN-resolved alias) into the
        // spawner so `resolve_agent_model`'s `getRuntimeMainLoopModel` branch
        // actually fires for an `AgentModel::Inherit` spawn: an `opusplan` install
        // in plan mode resolves the subagent to Opus (not the resolved Sonnet
        // main-loop model). Without these the Inherit branch returns the parent
        // model unchanged (default mode → byte-identical to before this seam).
        // `model_setting_for_spawns` = `cfg.default_model` unless the
        // connected-provider fallback rerouted the session (then the plan-mode
        // swap must not resurrect the disconnected anthropic route).
        .with_permission_mode(cfg.permission_mode)
        .with_model_setting(model_setting_for_spawns.clone())
        // (parity 2.1.207 H-BIN-08) Managed availableModels restriction: a
        // subagent whose explicitly-requested model is policy-barred inherits the
        // parent/runtime model (binary `Qly`), and the plan-mode `opusplan`→Opus /
        // `haiku`→Sonnet upgrade is gated to the newest permitted family model
        // (binary `RF`). `None` (default install) ⇒ unrestricted (legacy).
        .with_model_restriction_opt(session_model_restriction.clone())
        // (M10 cc2.1.198) Explore `GAe` firstParty gate, multi-provider half:
        // a non-Anthropic default profile behaves like the TS non-firstParty
        // branch (Explore → inherit, never the opus cap).
        .with_session_provider_first_party(session_provider_first_party)
        // Legacy callers without an explicit origin keep the boot context.
        // Normal and nested Agent calls carry their owning session, so a
        // session switch cannot redirect an older background child's files.
        .with_hook_context(
            subagent_hook_session_id,
            cwd.clone(),
            Some(main_subagents_dir.clone()),
        )
        .with_subagents_dir_for_session_provider(Arc::new({
            let lingxi_home = cfg.lingxi_home.clone();
            let project_cwd = cwd.to_string_lossy().into_owned();
            move |session_id: protocol::SessionId| {
                let dir = orchestrator::transcript_paths::subagents_dir(
                    &lingxi_home,
                    &project_cwd,
                    &session_id.as_uuid().to_string(),
                );
                std::fs::create_dir_all(&dir).map_err(|error| {
                    platform_api::subagent_spawn::SubagentSpawnError::Runtime(format!(
                        "cannot create subagent transcript directory {}: {error}",
                        dir.display(),
                    ))
                })?;
                Ok(dir)
            }
        }))
        // …and the writer that actually creates the file the line above names.
        // Without it `agent_transcript_path` pointed at nothing, and a
        // background agent's conversation existed only in memory.
        .with_transcript_fs(
            Arc::new(PosixFileSystem::new(cwd.clone())) as Arc<dyn platform_api::FileSystem>
        )
        // Every subagent gets its own passive-diagnostics cursor. Sharing the
        // main registry as a source would make diagnostics first-reader-wins
        // across the main loop and concurrent children.
        .with_new_diagnostics_source_factory(Arc::new({
            let diagnostics = lsp_diagnostics_cell.clone();
            move |child_cwd| {
                diagnostics
                    .get()
                    .expect("LSP diagnostics registry is initialized before subagent spawn")
                    .diagnostics_source(
                        child_cwd.map(std::path::PathBuf::from),
                        Some(std::time::Duration::from_millis(500)),
                    )
            }
        }))
        // 2.1.186: append the subagent `<env>` block (`tIm`) after the `Notes:`
        // trailer on every NON-fork spawn. The renderer probes the boot-stable
        // environment once (cwd/git/platform/shell/OS) via the orchestrator's own
        // helpers and fills in the spawn's resolved model id per call. Lives at the
        // composition root because the `agent` crate cannot reach
        // `orchestrator::prompt` (dep cycle).
        .with_subagent_env_renderer(std::sync::Arc::new(
            orchestrator::prompt::subagent_env::boot_renderer(cwd.clone()),
        ));
    if let Some(observer) = cfg.session_agent_observer.clone() {
        subagent_spawner_concrete = subagent_spawner_concrete.with_spawn_observer(observer);
    }
    let subagent_tool_registry_cell = subagent_spawner_concrete.tool_registry_handle();
    let subagent_agent_catalog_cell = subagent_spawner_concrete.agent_catalog_handle();
    // G4/G5: grab the set-once hook-executor + skill-loader cells BEFORE boxing,
    // to fill once the `HookExecutorImpl` (5.25) and shared command registry exist
    // (same cycle-break as the tool-registry / agent-catalog cells above).
    let subagent_hook_executor_cell = subagent_spawner_concrete.hook_executor_handle();
    let subagent_strict_plugin_hooks_cell =
        subagent_spawner_concrete.strict_plugin_only_hooks_handle();
    let subagent_skill_loader_cell = subagent_spawner_concrete.skill_loader_handle();
    // §24b: grab the set-once agent-MCP-tool-builder cell BEFORE boxing, to
    // fill once `mcp_registry` + `mcp_tool_ctx` exist (same cycle-break as the
    // hook/skill cells above — see `mcp_tool_builder`'s doc in `agent::handle`).
    let subagent_mcp_tool_builder_cell = subagent_spawner_concrete.mcp_tool_builder_handle();
    // FIX 1 (subagent pool): grab the set-once tool-wide-deny-names cell BEFORE
    // boxing, to fill once the permission policy is built (same cycle-break as
    // the registry/catalog/hook cells). Filled inside the enforcement branch
    // below from `policy.tool_wide_deny_names()`; left empty otherwise ⇒ the
    // subagent tool pool is unfiltered (byte-identical to before).
    let subagent_tool_wide_deny_cell = subagent_spawner_concrete.tool_wide_deny_names_handle();
    // (2.1.263 `bs(Rn)`) The spawn-time bypass clamps: an agent definition's
    // `permissionMode: bypassPermissions` must NOT raise a restrictive parent
    // session. Grabbed before boxing because `bypass_disabled` only exists once
    // the boot permission tiers load, far below. Filled in BOTH arms of the
    // enforcement branch so the cell is never left at its no-clamp default.
    let subagent_bypass_gates_cell = subagent_spawner_concrete.spawn_bypass_gates_handle();
    // Coordinator mode is constructed below because it owns the session
    // lifecycle. Capture the spawner's set-once seam now and fill it once the
    // live mode exists, so each spawn consults `is_enabled()` at spawn time.
    let subagent_coordinator_mode_cell = subagent_spawner_concrete.coordinator_mode_handle();
    // FIX (B-agent-model-inheritance): grab the set-once live-default-model cell
    // BEFORE boxing, to fill once the orchestrator (which owns the LIVE
    // `session.model`) exists — same cycle-break as the cells above. Once filled,
    // a spawn whose request carries no `parent_model_override` (the non-`AgentTool`
    // spawn paths) resolves `AgentModel::Inherit` against the LIVE session model
    // (updated by `/model` switches / resume) instead of the boot snapshot below.
    let subagent_default_model_selection_provider_cell =
        subagent_spawner_concrete.default_model_selection_provider_handle();
    let subagent_provider_first_party_resolver_cell =
        subagent_spawner_concrete.provider_first_party_resolver_handle();
    // Box ONCE as the concrete `Arc<PoolSubagentSpawner>` so it can serve as
    // BOTH the one-shot `SubagentSpawner` and the persistent/resume
    // `StreamingSubagentSpawner` (Phase-1 seam) — the LocalAgent handler needs
    // the streaming half to make a backgrounded agent "come to rest" + resume.
    let subagent_spawner_arc = Arc::new(subagent_spawner_concrete);
    let lifecycle_subagent_spawner = subagent_spawner_arc.clone();
    let subagent_spawner: Arc<dyn platform_api::subagent_spawn::SubagentSpawner> =
        subagent_spawner_arc.clone();
    let subagent_streaming_spawner: Arc<dyn agent::StreamingSubagentSpawner> =
        subagent_spawner_arc.clone();

    //       The budget enforcer shares both the process `CostTracker` and the
    //       CLI `--max-budget` ceiling with the main orchestrator. Claude Code
    //       2.1.217 stops background subagents when that ceiling is reached;
    //       `Halt` makes each child runner's turn-boundary budget check enforce
    //       the same limit. With no CLI ceiling this remains unlimited.
    let shared_budget_enforcer = Arc::new(cost::BudgetEnforcer::new(
        cost::BudgetConfig {
            max_session_nano_usd: orch_cfg.max_budget_nano_usd,
            max_turn_nano_usd: None,
            max_turn_tokens: None,
            warning_thresholds: Vec::new(),
            on_exceed: cost::BudgetExceedPolicy::Halt,
        },
        cost_tracker.clone(),
    ));
    // Main responses and workflows must publish to the same session/turn
    // book that will authorize physical Fusion attempts. Disabled-persistence
    // hosts retain the legacy counters, since these scopes require durability.
    let workflow_output_scopes = shared_budget_enforcer.workflow_output_scopes();
    {
        if let Err(error) = workflow_output_scopes
            .ensure_current(
                main_session_id,
                protocol::MessageId::new(),
                orch_cfg.token_budget,
            )
            .await
        {
            let cleanup = session_state.close_and_drain().await.err();
            let message = cleanup.map_or_else(
                || error.to_string(),
                |cleanup| format!("{error}; coordinator cleanup failed: {cleanup}"),
            );
            return Err(BuildError::DurableSession(message));
        }
    }
    session_state_manager.configure_retention(
        &cost_tracker,
        &shared_budget_enforcer,
        &fusion_recorder_factory_impl,
    );
    let fusion_attempts = desktop_fusion_attempts(
        api_service.clone(),
        shared_budget_enforcer.clone(),
        cost_tracker.clone(),
        pricing.clone(),
        workflow_output_scopes.clone(),
    );
    let budget_enforcer: Arc<dyn platform_api::budget::BudgetEnforcerHandle> =
        shared_budget_enforcer;

    // (5) Memory filler + the permission gate. The gate is the F2-01 branch
    //     point: the CLI opts into the always-allow `NoOpPermissionGate`; a
    //     transport binds the connection-scoped `AdapterPermissionGate` and
    //     keeps its handle to `resolve()` inbound approvals (F2-06).
    //
    //     M5-13: the hook executor is no longer the `noop_hook_executor()` stub
    //     — it is constructed below (5.25) once `hook_registry` exists, so the
    //     HTTP / Command hook arms run for real.
    //     Memory provider: the production host injects
    //     `cfg.memory_provider = Some(orchestrator::prompt::real_provider())`
    //     so the orchestrator loads the real `<cwd>/LINGXI.md` +
    //     `~/.lingxi/LINGXI.md` hierarchy into the system prompt (claude-code
    //     parity) and `fire_instructions_loaded()` fires over those files.
    //     `None` (the default + every test caller) falls back to the empty
    //     `StaticMemoryProvider`, so a default build loads NO memory and the
    //     boot tests stay deterministic (they never read the real filesystem).
    let memory: Arc<dyn orchestrator::prompt::MemoryHierarchyProvider> = cfg
        .memory_provider
        .clone()
        .unwrap_or_else(|| Arc::new(StaticMemoryProvider::empty()));

    let (perms, adapter_gate): (Arc<dyn PermissionGate>, Option<Arc<AdapterPermissionGate>>) =
        if let Some(injected) = cfg.injected_permission_gate.clone() {
            // INTERACTIVE prompt transport (the TUI's `TuiPermissionGate`): used
            // as the base gate, still wrapped by `PolicyPermissionGate` below when
            // enforcement is on, so an unresolved `Ask` surfaces as a dialog. No
            // `AdapterPermissionGate` handle (that is the bridge transport's gate).
            (injected, None)
        } else if cfg.use_noop_permission_gate {
            // HEADLESS deny-on-ask (`--print` parity): a non-interactive session
            // has no prompt to surface an unresolved `Ask`, so deny it instead of
            // allowing. `PolicyPermissionGate` (wrapped below when enforcement is
            // on — the CLI default) still resolves allow/deny rules + read-only
            // auto-allow BEFORE delegating here, so only an otherwise-unresolved
            // mutating ask is denied. Defaults off → the prior always-allow inner.
            if cfg.deny_unresolved_ask {
                (Arc::new(permission::DenyOnAskGate), None)
            } else {
                (Arc::new(NoOpPermissionGate), None)
            }
        } else {
            // (3c) Persist an `AllowAlways` choice to `<cwd>/.lingxi/settings.local.json`.
            let gate = Arc::new(AdapterPermissionGate::new(permission_sink).with_persist(
                permission::PermissionPaths {
                    lingxi_home: cfg.lingxi_home.clone(),
                    cwd: cwd.clone(),
                },
            ));
            (gate.clone() as Arc<dyn PermissionGate>, Some(gate))
        };

    // (5.1) Load `.mcp.json` (project preferred over user-global) and
    //       auto-connect every enabled server (Plan 13). `connect_all` seeds
    //       disabled servers as `Disconnected` so `/mcp` still lists them,
    //       connects the rest, and records per-server failures as loop-eligible
    //       `Disconnected { last_error }`. A background reconnect/backoff task
    //       then retries dropped remote servers. The precedence-ordered paths
    //       arrive via `cfg` (previously derived from `dirs::config_dir()` + cwd
    //       in the CLI).
    let project_mcp_path = cfg
        .mcp_paths
        .first()
        .cloned()
        .unwrap_or_else(|| std::path::PathBuf::from("/dev/null"));
    let global_mcp_path = cfg
        .mcp_paths
        .get(1)
        .cloned()
        .unwrap_or_else(|| std::path::PathBuf::from("/dev/null"));
    // All three claude-code MCP scopes (precedence local > project > user):
    // project `.mcp.json` (mcp_paths[0]), user + local both inside the global
    // config `~/.lingxi.json` (mcp_paths[1]); local is keyed by the canonical
    // project key for `cwd`.
    let strict_plugin_policy = Arc::new(plugin::StrictPluginOnlyPolicy::from_settings_tiers(
        managed_settings_for_strict.iter().map(String::as_str),
    ));
    let strict_plugin_only_mcp =
        strict_plugin_policy.is_locked(plugin::PluginComponent::McpServers);
    let strict_plugin_only_hooks = strict_plugin_policy.is_locked(plugin::PluginComponent::Hooks);
    let strict_plugin_only_agents = strict_plugin_policy.is_locked(plugin::PluginComponent::Agents);
    let strict_plugin_only_skills = strict_plugin_policy.is_locked(plugin::PluginComponent::Skills);
    let _ = subagent_strict_plugin_hooks_cell.set(strict_plugin_only_hooks);
    // A host (including Fusion evaluation) can enable safe mode after the CLI
    // resolved these paths. Enforce the discovery gate at consumption too;
    // explicit servers and managed policy are still folded below.
    let mut mcp_configs = if cfg.customization_gates.disables_mcp_discovery() {
        Vec::new()
    } else {
        mcp::load_mcp_servers(&project_mcp_path, &global_mcp_path, &cwd)
    };
    // CLI `--mcp-config` servers: highest precedence — override a discovered
    // server of the same name, else append. (With `--strict-mcp-config` the host
    // nulled the discovered paths above, so `mcp_configs` starts empty and these
    // become the only servers.)
    for c in &cfg.cli_mcp_servers {
        if let Some(existing) = mcp_configs.iter_mut().find(|x| x.name == c.name) {
            *existing = c.clone();
        } else {
            mcp_configs.push(c.clone());
        }
    }
    // Per-project MCP-server enable/disable gate (claude-code `eI()`/`rTo()`/`bX`,
    // 2.1.200+): the `~/.lingxi.json` `projects.<cwd_key>` keys `enabledMcpServers`
    // (allowlist, applies ONLY to the builtin `computer-use` server) and
    // `disabledMcpServers` (denylist, applies to every other server) mark a gated
    // server `disabled` so it is seeded as `Disconnected` and never auto-connected
    // (`if(eI(cn))return`) while `/mcp` still lists it as disabled. Applied AFTER
    // the `--mcp-config` merge so an explicitly-supplied server is gated too.
    mcp::apply_project_server_gate(&mut mcp_configs, &global_mcp_path, &cwd);
    // Enterprise MCP policy (claude-code `Qme`/`Ree`): when a managed
    // `managed-mcp.json` is active it takes EXCLUSIVE control — only its own
    // servers load; otherwise any server the managed allow/deny policy blocks
    // (`deniedMcpServers`/`allowedMcpServers`) is dropped before connect. Inert
    // (no server removed) when no managed config/policy is present, so a default
    // deployment is byte-identical.
    let mut ordinary_mcp_policy_sources = Vec::new();
    let (include_user_policy, include_project_policy) = cfg.setting_source_scope;
    for (path, included) in [
        (cfg.lingxi_home.join("settings.json"), include_user_policy),
        (
            cwd.join(branding::DOT_DIR).join("settings.json"),
            include_project_policy,
        ),
        (
            cwd.join(branding::DOT_DIR).join("settings.local.json"),
            include_project_policy,
        ),
    ] {
        if !included {
            continue;
        }
        if let Ok(raw) = tokio::fs::read_to_string(path).await {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) {
                ordinary_mcp_policy_sources.push(value);
            }
        }
    }
    if let Some(flag) = cfg
        .flag_settings
        .as_ref()
        .and_then(|settings| serde_json::to_value(settings).ok())
    {
        ordinary_mcp_policy_sources.push(flag);
    }
    let managed_mcp_policy_sources: Vec<serde_json::Value> = managed_settings_for_strict
        .iter()
        .filter_map(|raw| serde_json::from_str(raw).ok())
        .collect();
    let effective_mcp_policy = mcp::enterprise_policy::McpPolicy::from_effective_settings(
        &ordinary_mcp_policy_sources,
        &managed_mcp_policy_sources,
    );
    mcp::enterprise_policy::apply_enterprise_mcp_policy_with(
        &mut mcp_configs,
        &effective_mcp_policy,
    );
    if strict_plugin_only_mcp {
        // The strict slot accepts plugin and policy-controlled sources only.
        // Plugin servers materialize later through PluginManager; preserve
        // managed/enterprise candidates and reject ambient/manual scopes here.
        mcp_configs.retain(|cfg| {
            matches!(
                cfg.scope,
                mcp::ConfigScope::Enterprise
                    | mcp::ConfigScope::Settings(protocol::SettingsScope::Managed)
            )
        });
    }
    // MCP config-load diagnostics (claude-code `F7t`): surface per-entry config
    // problems (unknown type, url-without-type, invalid entry, reserved name,
    // missing env vars, `servers`-vs-`mcpServers`) to stderr at startup, the way
    // claude logs them. Silent when every config is clean, so a healthy setup
    // prints nothing.
    for w in mcp::config_diagnostics::collect_all_mcp_config_warnings(&cwd, Some(&global_mcp_path))
    {
        eprintln!("{}", w.to_stderr_line());
    }

    // (5.3) Agent catalog — load from project + user agents/. Project wins on
    //       collision (passed SECOND; later paths win). The user agents dir is
    //       `cfg.lingxi_home/agents` (was `dirs::home_dir()/.lingxi/agents`).
    //       HOISTED above the MCP registry build (M7 cc2.1.220): the
    //       agent-frontmatter MCP merge just below must see the wanted agent's
    //       markdown/flag definition BEFORE `connect_all` snapshots the config
    //       list. Nothing between here and the former position reads the
    //       catalog; plugin agents still land later via the plugin bootstrap's
    //       `plugin_agent_catalog` writes.
    let user_agents_dir = cfg.lingxi_home.join("agents");
    // claude `O5` walks UP from the cwd collecting `<dir>/<DOT_DIR>/agents` at
    // every level to the enclosing project root, so a monorepo's per-package
    // agents and its root agents are both in scope, with the definition closest
    // to the cwd winning. The port used to read the cwd's directory alone.
    // `HOME` is the ceiling (never itself collected); `project_root_of` is the
    // `kQr(cwd)` boundary — its absence just lets the walk run to the ceiling.
    // `--add-dir` roots contribute their own `<dir>/<DOT_DIR>/agents` between
    // the user tier and the project tier (`Rp()` in `wQr`, agents-only).
    let agent_dirs = agent::catalog::agent_dir_precedence(
        user_agents_dir,
        &cwd,
        std::env::var_os("HOME")
            .map_or_else(|| std::path::PathBuf::from("/"), std::path::PathBuf::from)
            .as_path(),
        permission::set_cwd::project_root_of(&cwd).as_deref(),
        &cfg.add_dir,
    );
    // (M3 cc2.1.198) `--safe-mode` / `--bare` disable custom agent definitions
    // (`V5d.agents:!0`, `K5d.agents:!1`) — skip the dir scan, empty catalog.
    let mut agents =
        if cfg.customization_gates.disables_custom_agents() || strict_plugin_only_agents {
            Vec::new()
        } else {
            agent::load_agents_from_dirs(&agent_dirs).await
        };
    // (M4 cc2.1.198) `--agents <json>` flag agents — see
    // [`merge_cli_flag_agents`].
    if !strict_plugin_only_agents {
        merge_cli_flag_agents(
            &mut agents,
            cfg.cli_agents_json.as_deref(),
            cfg.customization_gates.safe_mode,
        );
    }
    // `Z$`'s TOP tier: `[built-in, plugin, userSettings, projectSettings,
    // flagSettings, policySettings]` applied later-wins, so an org-provisioned
    // agent outranks every other source — `--agents` included. Hence AFTER the
    // flag merge. `wQr` gives this tier no `Fr(...)` / `ku("agents")` gate of
    // its own (unlike user and project), because org policy is not user
    // customization; the safe-mode / `--bare` arm above still suppresses the
    // whole disk catalog before this runs.
    if !cfg.customization_gates.disables_custom_agents() {
        let policy_agents = agent::load_agents_from_dirs(&[(
            agent::catalog::policy_agent_dir(
                &crate::desktop::settings_watch::managed_settings_dir(),
            ),
            agent::definition::AgentSource::Settings(protocol::SettingsScope::Managed),
        )])
        .await;
        agent::catalog::merge_agents_later_wins(&mut agents, policy_agents);
    }

    // (P2-02 cc2.1.207 / M7 cc2.1.220) The agent to apply to the MAIN loop: an
    // EXPLICIT `--agent` (fresh boot or re-passed on `--resume`) wins;
    // otherwise, on a resume with no `--agent`, the persisted `agentSetting`
    // (`rVe` restoration). `from_resume` selects the miss warning + suppresses
    // the re-persist (the record is already on disk). Computed HERE — before
    // the MCP registry connects — because claude merges the resolved
    // main-thread agent's frontmatter `mcpServers` into `dynamicMcpConfig`
    // (`FWt`) BEFORE the MCP clients connect. The APPLICATION to the
    // orchestrator seam still happens later, against the FINAL
    // (plugin-inclusive) catalog.
    let (wanted_agent, resumed_agent_snapshot, from_resume): (
        Option<String>,
        Option<serde_json::Value>,
        bool,
    ) = match cfg.cli_agent.clone() {
        Some(w) => (Some(w), None, false),
        None if cfg.session_id_override.is_some() => {
            let snapshot_fs =
                Arc::new(PosixFileSystem::new(cwd.clone())) as Arc<dyn platform_api::FileSystem>;
            let (persisted, snapshot) = session::jsonl::read_agent_resume_state(
                &main_transcript_path,
                snapshot_fs,
                &main_session_uuid,
            )
            .await;
            (persisted, snapshot, true)
        }
        None => (None, None, false),
    };
    // (M7 cc2.1.220) Resolve the definition the `FWt` merge consults. The
    // FINAL catalog does not exist yet (plugin agents land with the plugin
    // bootstrap), but plugin agents cannot carry `mcpServers` (LingXi's
    // parse-time privilege gate rejects them; claude strips the field with a
    // warning), so the markdown/flag set + the resume snapshot covers every
    // server-bearing definition. Miss handling (the "not found" warning) stays
    // with the application block below.
    let main_agent_def_for_mcp: Option<agent::AgentDefinition> =
        wanted_agent.as_ref().and_then(|wanted| {
            resumed_agent_snapshot
                .as_ref()
                .and_then(|v| serde_json::from_value::<agent::AgentDefinition>(v.clone()).ok())
                .filter(|a| &a.agent_type == wanted)
                .or_else(|| {
                    agents
                        .iter()
                        .find(|a| &a.agent_type == wanted)
                        .or_else(|| {
                            let suffix = format!(":{wanted}");
                            agents.iter().find(|a| a.agent_type.ends_with(&suffix))
                        })
                        .cloned()
                })
        });
    // Main-thread agent frontmatter `permissionMode` participates in the boot
    // mode precedence (`CLI / dangerous-skip > agent frontmatter > settings
    // defaultMode`). Resolve it once from the SAME selected agent / resume
    // snapshot that the main-loop application and frontmatter MCP merge use.
    let selected_main_agent_permission_mode = main_agent_def_for_mcp
        .as_ref()
        .and_then(|def| agent::permission_mode::definition_mode_override(def.permission_mode));
    // (M7 cc2.1.220) `FWt(existing, agentDef, opts)` — fold the agent's
    // frontmatter `mcpServers` into the to-connect list so they register,
    // connect and surface tools EXACTLY like `--mcp-config` servers. Applied
    // AFTER `apply_project_server_gate` + `apply_enterprise_mcp_policy`: agent
    // servers are never project-approval-gated (claude approval covers
    // `.mcp.json` servers) and the merge runs its OWN `Yee` enterprise filter +
    // `T3()` managed-exclusive skip below, mirroring claude's ordering (`FWt`
    // merges into `dynamicMcpConfig` after discovery filtering). The
    // `--mcp-config` names stand in for `dynamicMcpConfig`'s key set: `mcp_configs`
    // is flat here, and only those entries outrank an agent's server.
    let dynamic_mcp_names: Vec<String> =
        cfg.cli_mcp_servers.iter().map(|c| c.name.clone()).collect();
    let agent_mcp_blocked = merge_agent_frontmatter_mcp_servers(
        &mut mcp_configs,
        &dynamic_mcp_names,
        main_agent_def_for_mcp.as_ref(),
        AgentMcpMergeGates {
            safe_mode: cfg.customization_gates.safe_mode,
            strict_mcp_config: cfg.strict_mcp_config,
            enterprise_mcp_active: mcp::enterprise_policy::enterprise_mcp_active(),
            strict_plugin_only_mcp,
        },
        &mcp::enterprise_policy::read_managed_mcp_policy(),
    );
    if !agent_mcp_blocked.is_empty() {
        // claude's headless-start `onBlocked` (the only site that prints):
        // `Warning: agent frontmatter MCP ${Tt(len,"server")} blocked by
        // enterprise policy: ${names.join(", ")}` — `Tt` pluralizes WITHOUT a
        // count.
        eprintln!(
            "Warning: agent frontmatter MCP {} blocked by enterprise policy: {}",
            if agent_mcp_blocked.len() == 1 {
                "server"
            } else {
                "servers"
            },
            agent_mcp_blocked.join(", ")
        );
    }
    // Apply the immutable project policy snapshot once more after the agent
    // merge. The earlier pass protects discovered/CLI candidates; this final
    // pass is the security boundary that prevents a same-named agent
    // frontmatter server from resurrecting an entry in `disabledMcpServers`.
    // Project approval remains scope-aware, so agent servers are not
    // incorrectly treated as `.mcp.json` candidates.
    mcp::apply_project_server_gate(&mut mcp_configs, &global_mcp_path, &cwd);
    let agent_catalog = Arc::new(tokio::sync::RwLock::new(agents));

    // Build one concrete host MCP transport and hand the SAME `Arc` to the
    // registry as BOTH `McpTransport` (discovery) and `RawConnectionProvider`
    // (live-client bridge). Unix keeps `PosixMcpTransport`; Windows selects
    // `WindowsMcpTransport`, whose remote IDE connections expose the same
    // shared JSON-RPC handle to the registry.
    let mcp_transport = new_desktop_mcp_transport();
    // The registry is BUILT here but `connect_all` is deferred to (5.26),
    // after the real `hooks` executor exists: the elicitation hook dispatcher
    // (`OrchestratorHookDispatcher`) must be wired via `with_hook_dispatcher`
    // BEFORE any server connects, so an incoming `elicitation/create` consults
    // the `Elicitation` hook. The registry is not used by anything between here
    // and (5.26), so deferring the connect is behavior-neutral aside from the
    // dispatcher wiring.

    // (5.2) HookRegistry — read settings.json hooks from project
    //       (cwd/.lingxi/settings.json) then user (lingxi_home/settings.json),
    //       project last so it wins on identical command registration. The user
    //       root is `cfg.lingxi_home` (was `dirs::config_dir()/claude`).
    let mut hook_registry = hooks::HookRegistry::new();
    let project_settings_path = cwd.join(branding::DOT_DIR).join("settings.json");
    let local_settings_path = cwd.join(branding::DOT_DIR).join("settings.local.json");
    let user_settings_path = cfg.lingxi_home.join("settings.json");
    // `--setting-sources` scope (default `(true, true)` = all tiers): skip the
    // user tier when `!include_user` and the project tier when `!include_project`
    // so e.g. `--setting-sources project` does NOT register user-level hooks.
    let (incl_user_settings, incl_project_settings) = cfg.setting_source_scope;
    // (M3 cc2.1.198) `--safe-mode` / `--bare`: skip settings-file hooks. Bare
    // disables hooks outright (binary `V5d.hooks:!0`); safe mode collapses the
    // hooks-config merge to the POLICY tier only (`UQr()`: `if(e?.
    // allowManagedHooksOnly===!0||Ql())return e?.hooks??{}`) — lingxi loads no
    // policySettings hook tier, so both modes register zero settings hooks.
    let skip_settings_hooks = cfg.customization_gates.disables_settings_hooks();
    for (path, source, included) in [
        (
            user_settings_path,
            hooks::definition::HookSource::Settings(protocol::SettingsScope::User),
            incl_user_settings,
        ),
        (
            project_settings_path,
            hooks::definition::HookSource::Settings(protocol::SettingsScope::Project),
            incl_project_settings,
        ),
        (
            local_settings_path,
            hooks::definition::HookSource::Settings(protocol::SettingsScope::Local),
            incl_project_settings,
        ),
    ] {
        if !included || skip_settings_hooks || strict_plugin_only_hooks {
            continue;
        }
        if let Ok(raw) = tokio::fs::read_to_string(&path).await {
            match hooks::parse_hooks_from_settings_json(&raw, source) {
                Ok(hooks_vec) => {
                    for h in hooks_vec {
                        hook_registry.register(h);
                    }
                }
                Err(e) => tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    "skipping malformed settings hooks"
                ),
            }
        }
    }
    if !skip_settings_hooks && !strict_plugin_only_hooks {
        if let Some(raw) = cfg
            .flag_settings
            .as_ref()
            .and_then(|settings| serde_json::to_string(settings).ok())
        {
            match hooks::parse_hooks_from_settings_json(
                &raw,
                hooks::definition::HookSource::Session,
            ) {
                Ok(hooks_vec) => {
                    for hook in hooks_vec {
                        hook_registry.register(hook);
                    }
                }
                Err(error) => tracing::warn!(error = %error, "skipping malformed --settings hooks"),
            }
        }
    }
    // Policy hooks remain authoritative under strict-plugin-only and safe
    // mode. Bare mode disables hooks entirely.
    if !cfg.customization_gates.bare {
        for raw in &managed_settings_for_strict {
            match hooks::parse_hooks_from_settings_json(
                raw,
                hooks::definition::HookSource::Settings(protocol::SettingsScope::Managed),
            ) {
                Ok(hooks_vec) => {
                    for hook in hooks_vec {
                        hook_registry.register(hook);
                    }
                }
                Err(error) => tracing::warn!(
                    error = %error,
                    "skipping malformed managed settings hooks"
                ),
            }
        }
    }
    let hook_registry = Arc::new(tokio::sync::RwLock::new(hook_registry));

    // (5.2b) Permission enforcement (OPT-IN, parity phase 2). When
    //        LINGXI_ENFORCE_PERMISSIONS is set, load the settings permission
    //        rules into a PermissionPolicy and WRAP the gate selected above with
    //        PolicyPermissionGate: deny/allow rules are now enforced, an `Ask`
    //        delegates to the inner gate's prompt (or auto-allows a read-only
    //        tool to avoid an ask-storm). Unset (the DEFAULT) leaves the
    //        always-allow NoOp/Adapter gate untouched — NO behavior change.
    //        Reads the user/project/local settings files PLUS the managed
    //        (policySettings) tier — read last, so a managed `defaultMode`
    //        wins (parity 2.1.207 P1-10). File-glob content
    //        matching (3a) + subagent/teammate-path enforcement (3b) now land
    //        too; only Bash/WebFetch content matching (3a-bash) stays tool-wide.
    // (PERM.1) Enforce permissions BY DEFAULT on the CLI/desktop path (parity
    // §0.1 / §B). claude-code's default mode enforces deny/allow rules + the active
    // permission mode + sandbox-auto-allow; only the explicit
    // `--dangerously-skip-permissions` (BypassPermissions) opts out into allow-all.
    // We mirror that: wrap the base gate with `PolicyPermissionGate` unless either
    // (a) the env escape hatch `LINGXI_ENFORCE_PERMISSIONS` is explicitly set falsey
    // (`0|off|false|no|""`), or (b) the session is in BypassPermissions mode
    // (already root/Docker-guarded upstream by `enforce_bypass_safety`).
    //
    // Scope: when the env var is UNSET, default-on applies to BOTH inners — the
    // `NoOpPermissionGate` (CLI/desktop, allow-all) AND the connection-scoped
    // `AdapterPermissionGate` (transport hosts). claude-code enforces ONE core
    // policy on every host; wrapping the adapter gate with `PolicyPermissionGate`
    // makes local deny/allow rules + defaultMode bind on the bridge too, while the
    // adapter gate stays the Ask-delegation transport (an unresolved mutating Ask
    // still forwards to the remote client). An explicit env value still overrides.
    //
    // Inner-gate selection (the `(perms, adapter_gate)` match at :1836):
    // - INTERACTIVE TUI sessions inject `tui::permission_gate::TuiPermissionGate`
    //   via `cfg.injected_permission_gate` (the `if let Some(injected)` arm), so an
    //   unresolved mutating `Ask` (a `DenyByDefault` tool with no matching rule)
    //   surfaces the permission dialog instead of silently resolving — wired by
    //   `build_runtime_for_tui` → root permission pump.
    // - The HEADLESS `-p`/`--print` and `--no-tui` stdio REPL paths have no dialog
    //   to surface a prompt, so they keep the `NoOpPermissionGate` (always-allow) or
    //   `DenyOnAskGate` (deny-on-ask) inner per `use_noop_permission_gate` /
    //   `deny_unresolved_ask`. Either way deny rules + modes are enforced below.
    let enforce_permissions = cfg.restricted
        || should_enforce_permissions(
            std::env::var("LINGXI_ENFORCE_PERMISSIONS").ok().as_deref(),
            cfg.use_noop_permission_gate,
            cfg.permission_mode,
        );
    // Read(deny) → search-exclude globs (GrepTool.ts:417-427, glob.ts lLa()).
    // Populated inside the enforcement branch below from the boot policy and
    // threaded into the tool ctx so `Grep`/`Glob` skip denied/sensitive paths.
    // Empty (no enforcement / no Read-deny rule) ⇒ VCS-only behavior unchanged.
    let mut read_deny_exclude_globs: Vec<String> = Vec::new();
    // (#3 shell-expansion) Capture the boot `Arc<PermissionPolicy>` before it is
    // consumed by `PolicyPermissionGate::new` so `tool_ctx.permission_policy` can
    // share the SAME base policy the model-facing gate enforces. The prompt
    // shell-expansion provider reads it as the base for embedded `!`cmd`` bodies.
    // `None` only when enforcement is off (no boot policy is built) — the
    // `tool_ctx` literal then falls back to a Default-mode policy with roots.
    let mut boot_permission_policy: Option<Arc<permission::PermissionPolicy>> = None;
    // H-CHG-02: capture the enforcing gate's set-once LIVE-model cell (cycle-break)
    // so it can be filled once the orchestrator (owner of the live `session.model`)
    // exists — the live `set_permission_mode` auto gate then evaluates `dUe(wi())`
    // against the CURRENT model (mutated by `/model` switches / resume), mirroring
    // claude-code `Nle` reading `wi()`. `None` when enforcement is off (no
    // `PolicyPermissionGate` is built, so there is no live surface to gate).
    let mut loop_classifier_cell = None;
    let mut live_model_provider_cell: Option<
        Arc<std::sync::OnceLock<permission::LiveModelProvider>>,
    > = None;
    // The session's additional working directories (settings
    // `additionalDirectories` union CLI `--add-dir`), captured out of the
    // enforcement branch so BOTH the file-tool `trusted_dirs` (below) and the
    // MCP registry `roots/list` source see them. claude-code's file tools
    // (`FY(t)`) and `roots/list` (`r1d()`) BOTH advertise cwd +
    // additionalWorkingDirectories — file tools inside an `--add-dir` root are
    // allowed, not hard-blocked (parity 2.1.207 P1-08). Raw entries (`~`,
    // relative) are expanded when they land in `trusted_dirs`. Assigned in BOTH
    // arms below (the full union when enforcing; `--add-dir` only otherwise), so
    // it is always initialized before its later reads.
    let boot_additional_working_dirs: Vec<std::path::PathBuf>;
    let workspace_leases = permission::WorkspacePermissionLeaseRegistry::new();
    let perms: Arc<dyn PermissionGate> = if enforce_permissions {
        // Read the persistable rule tiers in ASCENDING priority — user →
        // project → local (3c: settings.local.json read after project so a
        // persisted `AllowAlways` is honored on the next enforced boot), then
        // the managed (policySettings) tier LAST/highest so enterprise
        // deny/ask/allow rules bind and managed `defaultMode` /
        // `disableBypassPermissionsMode` win (parity 2.1.207 P1-10). The
        // `--setting-sources` scope gates user/project+local but NOT managed
        // (claude-code `Xv()` force-includes `policySettings`); a managed
        // `allowManagedPermissionRulesOnly: true` drops every non-managed rule.
        // Full tier semantics on `load_boot_permission_tiers`.
        let BootPermissionTiers {
            mut rules,
            mut mode,
            mode_preference_allowed,
            bypass_disabled,
            auto_mode_disabled,
            classify_all_shell,
            mut additional_working_dirs,
            block_reads_outside_working_directories,
            raw_tiers,
            allow_managed_permission_rules_only,
        } = load_boot_permission_tiers_with_flag(
            &cfg.lingxi_home,
            &cwd,
            cfg.setting_source_scope,
            cfg.flag_settings.as_ref(),
        )
        .await;
        // (2.1.263 `bs(Rn)`) All three clamp inputs exist here: `ey()` is this
        // tier fold's `disableBypassPermissionsMode == "disable"`, `Rn.restricted`
        // is the `--restricted` bit, and `YYe()` is read ONCE at this edge — never
        // inside the clamp, because `CLAUDE_CODE_EVAL_CONFINED` is a process global
        // and an env-reading gate makes a parallel test suite flaky.
        let _ = subagent_bypass_gates_cell.set(agent::permission_mode::SpawnBypassGates {
            confined: platform_api::env::is_eval_confined_session(),
            bypass_disabled,
            restricted: cfg.restricted,
        });
        if cfg.restricted {
            additional_working_dirs = permission::working_dirs::AdditionalWorkingDirs::new();
        }
        append_mcp_permission_rules(
            &mut rules,
            &mcp_configs,
            allow_managed_permission_rules_only,
        );
        if cfg.restricted {
            append_restricted_builtin_denies(&mut rules, cfg.restricted_tools.as_deref());
        }
        // CLI `--add-dir <directories...>`: union the host-provided dirs into
        // the working-dir set, exactly like a settings-tier
        // `additionalDirectories` entry (claude-code "Additional directories
        // to allow tool access to").
        additional_working_dirs.extend_from_source(
            cfg.add_dir.iter().cloned(),
            permission::PermissionRuleSource::CliArg,
        );
        // Capture the union (settings additionalDirectories + --add-dir) for the
        // file-tool `trusted_dirs` and MCP `roots/list` source below. These
        // consumers want the FULL `rb` union, not the read block's narrower set.
        boot_additional_working_dirs = additional_working_dirs.paths();
        let rule_count = rules.len();
        // Phase 3a: supply the filesystem roots so file-path CONTENT rules
        // (`Edit(src/**)`, `Read(./secrets/**)`) match the input path. Roots
        // resolve per rule source — user settings against `lingxi_home`,
        // project/local against `cwd` — exactly as claude-code's
        // `rootPathForSource` does.
        let roots = permission::FsRoots {
            cwd: cwd.clone(),
            home: dirs::home_dir(),
            lingxi_home: cfg.lingxi_home.clone(),
        };
        // Phase 3a-bash: attach the sandbox-auto-allow config derived from
        // the SAME settings tiers, so a sandboxable bash command that
        // matched no explicit deny/ask rule is auto-allowed (the sandbox is
        // the safety boundary). Faithful to claude-code's
        // `bashToolHasPermission` sandbox branch; a no-op when sandboxing is
        // disabled in settings (`enabled = false`). OUTSIDE enforce mode this
        // whole block is skipped, so the layer stays a permanent no-op there.
        //
        // `raw_tiers` already ends with the managed (policySettings) tier —
        // `load_boot_permission_tiers` appends it LAST/highest, so the
        // sandbox-auto-allow fold (last write wins) lets a managed `sandbox.*`
        // override user/project/local (SETTING_SOURCES: …→localSettings→
        // flagSettings→policySettings) with ONE disk read shared between the
        // permission-rule and sandbox derivations (parity 2.1.207 P1-10).
        let raw_tier_refs: Vec<&str> = raw_tiers.iter().map(String::as_str).collect();
        let sandbox_auto_allow = sandbox_auto_allow_from_settings_tiers(&raw_tier_refs, &cwd);
        // Main-thread agent frontmatter `permissionMode` sits below explicit
        // CLI overrides but above settings `defaultMode`. An explicit CLI
        // `default` still suppresses the agent mode, so we must key off the
        // RAW request rather than the resolved `cfg.permission_mode` alone.
        let env_scrub_active = platform_api::env::is_env_truthy(
            std::env::var("LINGXI_SUBPROCESS_ENV_SCRUB").ok().as_deref(),
        );
        if mode_preference_allowed && !env_scrub_active && !cfg.restricted {
            if let Some(preference) = cfg.permission_mode_preference {
                if preference != permission::PermissionMode::BypassPermissions
                    || (cfg.allow_dangerously_skip_permissions && !bypass_disabled)
                {
                    mode = preference;
                }
            }
        }
        if cfg.permission_mode_cli_explicit {
            mode = cfg.permission_mode;
        } else if !env_scrub_active {
            if let Some(agent_mode) = selected_main_agent_permission_mode {
                // claude-code `Qu`: the `disableBypassPermissionsMode` killswitch
                // is only HALF the gate. Bypass must also have been EARNED —
                // the disclaimer accepted once, or a tier waiving the prompt —
                // or a single frontmatter line in a discovered agent file grants
                // full bypass at startup to a user who was never asked.
                let bypass_ok = permission::boot_agent_may_adopt_bypass(
                    bypass_disabled,
                    // `kM()` — truthy in ANY tier.
                    raw_tier_refs.iter().copied().any(
                        permission::loader::skip_dangerous_mode_permission_prompt_from_settings_json,
                    ),
                    migrations::global_config::global_config_path()
                        .and_then(|p| migrations::global_config::read_map(&p).ok())
                        .and_then(|m| {
                            m.get("bypassPermissionsModeAccepted")
                                .and_then(serde_json::Value::as_bool)
                        })
                        .unwrap_or(false),
                );
                if !(agent_mode == permission::PermissionMode::BypassPermissions && !bypass_ok) {
                    mode = agent_mode;
                }
            }
        }
        // Auto-mode availability gate — claude-code `xms` mode-load downgrade
        // (`if(t==="auto"&&!P0())return"default"`). When the resolved mode is
        // `auto` but auto mode is unavailable (the `disableAutoMode` settings
        // killswitch, or the boot model does not support it), silently downgrade
        // to `default` so the session never boots INTO an unavailable auto mode.
        // The local denial circuit-breaker is fresh at boot; Statsig
        // remote-disable is a documented omission. Model and provider are the
        // post-fallback route the session actually boots on.
        if mode == permission::PermissionMode::Auto {
            let (gated, _reason) = permission::apply_auto_mode_gate(
                mode,
                &permission::AutoGateInputs {
                    disabled_by_settings: auto_mode_disabled,
                    circuit_broken: false,
                    model: default_model_id.clone(),
                    provider: session_auto_mode_provider.clone(),
                },
            );
            mode = gated;
        }
        // Construct in `Default` and apply the resolved boot `mode` LAST (below),
        // so the auto-mode dangerous-rule strip runs AFTER
        // `with_classify_all_shell` is set and therefore honors the
        // `autoMode.classifyAllShell` escalation on a session that BOOTS directly
        // into auto mode. (The availability gate `rule_is_available_in_mode` also
        // enforces the escalation at authorize time, so this ordering only keeps
        // the strip stash faithful — but it costs nothing and removes the
        // stale-flag foot-gun.)
        let mut policy =
            permission::PermissionPolicy::from_rules(permission::PermissionMode::Default, rules)
                .with_roots(roots)
                .with_working_dirs(additional_working_dirs)
                .with_block_reads_outside_working_directories(
                    block_reads_outside_working_directories,
                )
                .with_workspace_leases(workspace_leases.clone())
                .with_sandbox_runtime(sandbox_auto_allow)
                .with_managed_permission_rules_only(allow_managed_permission_rules_only)
                .with_restricted(cfg.restricted)
                // `autoMode.classifyAllShell` escalation (`QOi()`): any tier enabling
                // it suspends every Bash/PowerShell allow rule in auto mode.
                .with_classify_all_shell(classify_all_shell)
                // TS `isBypassPermissionsModeAvailable` (2.1.211 permissionSetup):
                // `S = (n === "bypassPermissions" || o) && !g && !_` — available when
                // the session RESOLVED to bypass mode OR the explicit
                // `--allow-dangerously-skip-permissions` flag was passed, unless the
                // settings killswitch (`disableBypassPermissionsMode: "disable"`)
                // vetoes it. (`g`, the Statsig remote killswitch, is a documented
                // omission here like the other remote gates.)
                .with_bypass_available(
                    !cfg.restricted
                        && (mode == permission::PermissionMode::BypassPermissions
                            || cfg.allow_dangerously_skip_permissions)
                        && !bypass_disabled,
                )
                // Enable PowerShell path-containment via a real `pwsh` parse
                // (claude-code `validatePowerShellCommandPaths`). Inert on hosts
                // without PowerShell — `SystemPwshParser` returns passthrough when
                // `pwsh`/`powershell` is not on PATH, exactly like claude-code.
                .with_pwsh_parser(std::sync::Arc::new(
                    permission::powershell_parse::SystemPwshParser,
                ))
                .with_plan_files(plan_files.clone())
                .with_session_read_allowances(session_read_allowances_for_boot(
                    &cfg.lingxi_home,
                    &cwd,
                    &main_session_uuid,
                    session_kind_for_job_tmp().as_deref(),
                    job_dir_from_env().as_deref(),
                ))
                // `zj`'s `!Ae()` — plan mode counts as bypassPermissions only
                // in an interactive launch. `interactive_session` is the
                // `host.launchOptions.isInteractive()` twin resolved above.
                .with_interactive_session(interactive_session);
        policy.bypass_killswitch_active = bypass_disabled;
        // Auto-mode killswitch (`Bpa()`): the live `set_permission_mode` gate
        // refuses `auto` when any tier set `disableAutoMode: "disable"`.
        policy.auto_mode_disabled = auto_mode_disabled;
        // Apply the resolved boot mode now that every field (crucially
        // `classify_all_shell`) is set — this triggers the auto-mode
        // dangerous-rule strip with the escalation in effect. A no-op when `mode`
        // is `Default` (from == to).
        policy.set_mode(mode);
        // Resolve the active Read(deny) rules to search-exclude globs while
        // the policy is still in scope (before it moves into the gate).
        read_deny_exclude_globs = permission::read_deny_exclude_globs(&policy, &cwd);
        // FIX 1 (subagent pool): hand the policy's TOOL-WIDE deny names to the
        // subagent spawner so a blanket-denied tool is stripped from each
        // child's advertised pool too (claude-code `assembleToolPool` →
        // `filterToolsByDenyRules`). Set-once; only meaningful when there are
        // tool-wide deny rules (empty otherwise ⇒ no child-pool filtering).
        let _ = subagent_tool_wide_deny_cell.set(policy.tool_wide_deny_names());
        let policy = Arc::new(policy);
        // Share the boot policy into `tool_ctx` for the prompt shell-expansion
        // gate (clone the `Arc` BEFORE `policy` moves into the gate below).
        boot_permission_policy = Some(policy.clone());
        tracing::info!(
            rules = rule_count,
            mode = ?mode,
            "permission enforcement enabled (default on; disable with LINGXI_ENFORCE_PERMISSIONS=0)"
        );
        // Grab the LIVE-model cell BEFORE coercing to `Arc<dyn PermissionGate>`
        // (the concrete handle is only reachable pre-coercion); it is filled once
        // the orchestrator exists (below).
        let enforcing = permission::PolicyPermissionGate::new(policy, perms);
        live_model_provider_cell = Some(enforcing.live_model_provider_handle());
        loop_classifier_cell = Some(enforcing.loop_classifier_handle());
        Arc::new(enforcing)
    } else {
        // Enforcement off: the settings `additionalDirectories` tiers are not
        // loaded here, but the CLI `--add-dir` dirs still widen file-tool access
        // and the MCP roots (claude-code's `additionalWorkingDirectories` are
        // independent of permission mode).
        boot_additional_working_dirs = cfg.add_dir.clone();
        perms
    };
    // Enforcement-off fallback for the clamp inputs: the settings tiers were not
    // loaded, so `disableBypassPermissionsMode` is unknown (⇒ `false`). A no-op
    // when the enforcing arm above already filled the cell.
    let _ = subagent_bypass_gates_cell.set(agent::permission_mode::SpawnBypassGates {
        confined: platform_api::env::is_eval_confined_session(),
        bypass_disabled: false,
        restricted: cfg.restricted,
    });
    // Capture the enforcing gate for the interactive TUI's Shift+Tab live
    // permission-mode cycling (`set_permission_mode`), before `perms` is moved
    // into the tool context below.
    let enforcing_permission_gate: Option<Arc<dyn PermissionGate>> = Some(perms.clone());

    // (5.25) M5-13: build the real hook executor now that `hook_registry`
    //        exists. This replaces the `noop_hook_executor()` stub (which fed
    //        `UnusedHttp` + `UnusedRuntime` and a `(None, None)` Command guard):
    //        - `http.clone()` is the real `PosixHttp`, so the HTTP arm performs
    //          real (SSRF-guarded) requests.
    //        - `PosixRuntime` is the real `RuntimeSpawner`.
    //        - `with_process_runner(PosixProcess, PosixSandbox)` makes the
    //          Command arm spawn real child processes (the runner only accepts a
    //          `SandboxedCommand`, which the sandbox mints).
    //        The hooks Agent arm IS now wired via `.with_agent_spawner(..)`
    //        (see the builder chain below): an `agent`-type hook action spawns a
    //        subagent through the SAME pool spawner the tool-context
    //        `subagent_spawner` uses (4.6). They remain distinct injection points
    //        on `HookExecutorImpl` but share one spawner. The orchestrator's
    //        `hooks` param is the concrete `Arc<hooks::HookExecutorImpl>`, so no
    //        trait-object coercion is needed.
    //        The Prompt arm is wired via `with_prompt_runner`: the
    //        `ApiClientHookPromptRunner` reuses the SAME `api_client`
    //        (`OrchestratorApiClient::messages_create`) the orchestrator uses
    //        for its other one-shot LLM passes, so a `prompt` hook
    //        (`execPromptHook.ts`) runs an inline single-turn query through the
    //        shared provider/routing/telemetry plumbing. Decoupled: the hooks
    //        crate only sees the `HookPromptRunner` trait, never the api-client.
    // (5.255) B5 async hook registry. A matched hook with `blocking == false`
    //         (the config-`async` analog — claude-code `hooks.ts:995-1030`
    //         `executeInBackground`) is handed to this registry instead of being
    //         awaited inline, so the originating turn proceeds IMMEDIATELY rather
    //         than blocking on a slow non-blocking hook. The registry tracks each
    //         in-flight handle (so it can be cancelled/joined), races it against
    //         its `asyncTimeout` (default 15s), and publishes the eventual
    //         `(HookId, HookResult)` on `async_hook_completion_tx`. Without this
    //         wiring `HookExecutorImpl::background_hook` degrades to running the
    //         hook inline-and-discard — which still can't `Block`, but DOES block
    //         the turn — so attaching it here is what realizes the async behavior.
    //
    //         The SAME `Arc<PosixRuntime>` backs both the executor's
    //         `RuntimeSpawner` and the registry's spawner, so backgrounded hooks
    //         run on the one process runtime. The completion channel is drained by
    //         a best-effort background loop (below) — the full claude-code
    //         `getAsyncHookResponseAttachments` fold-back (re-injecting completed
    //         async-hook stdout as `async_hook_response` attachments into the next
    //         turn) is a separate, larger feature and is NOT part of this seam; the
    //         drain keeps the bounded channel from back-pressuring a fire-and-forget
    //         hook. When no hooks are configured nothing is ever backgrounded, so
    //         this wiring is a no-op for the common case (byte-identical).
    let hook_runtime = Arc::new(PosixRuntime::new());
    let (async_hook_completion_tx, mut async_hook_completion_rx) =
        tokio::sync::mpsc::channel::<(protocol::HookId, hooks::HookResult)>(64);
    let async_hook_registry = Arc::new(hooks::AsyncHookRegistry::new(
        hook_runtime.clone() as Arc<dyn platform_api::RuntimeSpawner>,
        async_hook_completion_tx,
    ));
    // B5 fold-back (claude-code `getAsyncHookResponseAttachments` +
    // `normalizeAttachmentForAPI` case `async_hook_response`, `messages.ts:4026`):
    // drain the completion channel and stash each completed background hook's
    // `systemMessage` AND `additionalContext` into the buffer, for the
    // orchestrator to re-inject as an `async_hook_response` reminder on the NEXT
    // turn. UNLIKE the synchronous PreToolUse/PostToolUse path (where ONLY
    // `additionalContext` is model-facing and `systemMessage` is suppressed,
    // `messages.ts:4258`), the `async_hook_response` attachment surfaces BOTH as
    // separate meta user messages that reach the model (`messages.ts:4030-4055`).
    // So we push both fields here, each on its own line. Hooks that returned
    // neither contribute nothing. Draining still keeps the bounded channel from
    // back-pressuring a fire-and-forget hook; when no hooks are configured
    // nothing is ever published, so this stays a no-op for the common case.
    let async_hook_response_buffer = AsyncHookResponseBuffer::default();
    let async_hook_drain_buffer = async_hook_response_buffer.clone();
    tokio::spawn(async move {
        while let Some((_id, result)) = async_hook_completion_rx.recv().await {
            let should_rewake = result
                .response
                .as_ref()
                .is_some_and(|response| response.async_rewake);
            if let Some(resp) = result.response.as_ref() {
                if let Some(text) = resp.system_message.clone() {
                    async_hook_drain_buffer.push(text);
                }
                if let Some(text) = resp.additional_context.clone() {
                    async_hook_drain_buffer.push(text);
                }
            }
            if should_rewake {
                async_hook_drain_buffer.rewake().await;
            }
        }
    });
    // H-BIN-12: source the CC 2.1.207 HTTP-hook security policy
    // (`allowedHttpHookUrls` / `httpHookAllowedEnvVars`) from the merged settings
    // so the HTTP hook executor gates outbound URLs + intersects the per-hook
    // env-var allowlist. `(None, None)` = no restriction (behavior-neutral).
    let (http_hook_urls, http_hook_env_vars) = if cfg.restricted {
        effective_settings
            .as_ref()
            .map(|settings| {
                (
                    settings.settings.allowed_http_hook_urls.clone(),
                    settings.settings.http_hook_allowed_env_vars.clone(),
                )
            })
            .unwrap_or((None, None))
    } else {
        load_merged_http_hook_policy(&cwd)
    };
    // Transcript sink for the per-hook-run `attachment` records claude-code
    // persists (one `hook_success` / `hook_non_blocking_error` /
    // `hook_cancelled` line per hook run). Created empty here because the hook
    // executor is built BEFORE the orchestrator that owns the JSONL writer;
    // `attach` fills the cell once `orch` exists (step 4.x below), the same
    // shape as `subagent_hook_executor_cell`.
    let hook_attachment_sink = Arc::new(orchestrator::JsonlHookAttachmentSink::new());
    // Same late-bound shape for the prompt-hook evaluator: it needs the
    // session's live model/profile (a non-Anthropic session evaluates on its
    // own model), and the session does not exist yet. `attach` below.
    let hook_prompt_runner = Arc::new(orchestrator::ApiClientHookPromptRunner::new(
        api_client.clone(),
    ));
    let hook_mcp_invoker = DesktopHookMcpInvoker::default();
    let hooks = Arc::new(
        hooks::HookExecutorImpl::new(
            hook_registry.clone(),
            http.clone(),
            hook_runtime as Arc<dyn platform_api::RuntimeSpawner>,
        )
        .with_policy_disable_all_hooks(if cfg.restricted {
            effective_settings
                .as_ref()
                .and_then(|settings| settings.settings.disable_all_hooks)
                .unwrap_or(false)
        } else {
            load_merged_disable_all_hooks(&cwd)
        })
        .with_http_hook_policy(http_hook_urls, http_hook_env_vars)
        .with_process_runner(
            Arc::new(PosixProcess::new()) as Arc<dyn platform_api::ProcessRunner>,
            Arc::new(PosixSandbox::new()) as Arc<dyn platform_api::Sandbox>,
        )
        .with_prompt_runner(hook_prompt_runner.clone() as Arc<dyn hooks::HookPromptRunner>)
        .with_async_registry(async_hook_registry)
        // Wire the Agent hook arm: an `agent`-type hook action spawns a subagent
        // through the SAME pool spawner the `AgentTool` uses (4.6 below). The arm
        // + `with_agent_spawner` builder already exist in the hooks crate; only
        // this production wiring was missing, so an `agent` hook now runs instead
        // of degrading to a no-op. Opt-in: byte-identical when no `agent` hook is
        // configured. (`subagent_spawner` is an `Arc`; cloned here, still moved
        // into the tool context below.)
        .with_agent_spawner(subagent_spawner.clone())
        .with_mcp_invoker(Arc::new(hook_mcp_invoker.clone()))
        // P4: wire the output stream as hook observer so --include-hook-events
        // and the SessionStart/Setup always-stream gate emit hook_started /
        // hook_response frames. Default no-op when the stream impl ignores them
        // (TUI / plain sink paths).
        .with_hook_observer(output.clone())
        // One transcript `attachment` line per hook run, matching claude-code.
        .with_attachment_sink(hook_attachment_sink.clone() as Arc<dyn hooks::HookAttachmentSink>),
    );

    // G4: fill the subagent spawner's hook-executor cell now that `hooks` exists,
    // so a child runner can fire `SubagentStart` (collecting + injecting the
    // hooks' `additionalContexts`) and register/clear the agent's frontmatter
    // hooks (Stop→SubagentStop) scoped to the child id. First fill wins.
    let _ = subagent_hook_executor_cell.set(hooks.clone());

    // (5.26) Build the MCP registry NOW (deferred from (5.1)) so it can carry
    //         the elicitation hook dispatcher, then auto-connect. The
    //         `OrchestratorHookDispatcher` shares the SAME `hooks` executor, so
    //         an inbound `elicitation/create` fires the `Elicitation` hook
    //         (claude-code `runElicitationHooks`): a hook may PROVIDE the answer
    //         or DENY it; with no hook it falls through to `{"action":"cancel"}`.
    //         `with_hook_dispatcher(Some(..))` is the only behavioral delta from
    //         the previous `with_raw_conn` wiring.
    let elicitation_dispatcher: Arc<dyn mcp::HookDispatcher> =
        Arc::new(orchestrator::OrchestratorHookDispatcher::new(
            hooks.clone(),
            cwd.clone(),
            main_transcript_path.clone(),
        ));
    // OAuth 2.1 + PKCE seam for OAuth-configured remote (SSE/HTTP) MCP servers.
    // Reuses the platform `http` / `clock` / `storage` already built in step (1);
    // `on_authorization_url` surfaces the consent URL to the user (logs it
    // prominently + best-effort detached OS browser open). When a server has no
    // `oauth` config this is entirely inert — static-token / no-oauth servers
    // take the unchanged path.
    let mcp_on_auth_url = mcp_on_authorization_url();
    // XAA IdP-login config layer. When an `xaaIdp` settings tier is present
    // (`{issuer, clientId, callbackPort}` — mirror of claude-code
    // `getXaaIdpSettings`), wire a concrete `XaaConfigProvider` so an
    // `oauth.xaa==Some(true)` server resolves its token via the Cross-App-Access
    // token-exchange chain. The provider supplies the IdP `id_token` (cached or a
    // one-time OIDC browser pop), the AS `client_secret`
    // (`mcpOAuthClientConfig[serverKey]`), and the IdP token endpoint
    // (`discoverOidc`). Reuses the SAME http/clock/storage/on_authorization_url
    // Arcs as OAuthDeps. Absent the settings, `xaa_config` stays `None` and an
    // XAA-flagged server keeps its actionable hard-fail (XAA stays opt-in).
    let xaa_config: Option<Arc<dyn mcp::registry::XaaConfigProvider>> = {
        let mut tiers: Vec<String> = Vec::new();
        for p in [
            cfg.lingxi_home.join("settings.json"),
            cwd.join(branding::DOT_DIR).join("settings.json"),
            cwd.join(branding::DOT_DIR).join("settings.local.json"),
        ] {
            if let Ok(raw) = tokio::fs::read_to_string(&p).await {
                tiers.push(raw);
            }
        }
        let refs: Vec<&str> = tiers.iter().map(String::as_str).collect();
        mcp::XaaIdpSettings::from_settings_tiers(&refs).map(|settings| {
            // Build the server→(AS client_id, server_key) lookup from the known
            // MCP configs so the provider can resolve the AS `client_secret`.
            let lookup = mcp::MapServerOAuthLookup::from_specs(
                mcp_configs.iter().map(|c| (c.name.as_str(), &c.spec)),
            );
            Arc::new(mcp::XaaIdpConfigProvider::new(
                http.clone() as Arc<dyn platform_api::HttpTransport>,
                clock.clone() as Arc<dyn platform_api::Clock>,
                mcp_oauth_storage.clone(),
                mcp_on_auth_url.clone(),
                settings,
                Arc::new(lookup) as Arc<dyn mcp::ServerOAuthLookup>,
            )) as Arc<dyn mcp::registry::XaaConfigProvider>
        })
    };
    let mcp_oauth_deps = mcp::registry::OAuthDeps {
        http: http.clone() as Arc<dyn platform_api::HttpTransport>,
        clock: clock.clone() as Arc<dyn platform_api::Clock>,
        storage: mcp_oauth_storage,
        on_authorization_url: mcp_on_auth_url,
        xaa_config,
    };
    // LIVE additional-roots cell shared between the MCP registry (seeds every
    // server's `roots/list`) and the runtime `/add-dir` effect: pushing into it
    // via `mcp_registry.add_root(...)` is seen by every connected server on its
    // next `roots/list` without a reconnect (parity 2.1.207 P1-08).
    //
    // Seed with the EXPANDED absolute paths (same `expand_trusted_dir` the
    // file-tool `trusted_dirs` set uses below), NOT the raw settings/`--add-dir`
    // entries. `RootsListHandler::roots_value` forwards each dir verbatim into
    // `format!("file://{dir}")`, so a raw `~/shared` / relative `data` would
    // emit a malformed `file://~/shared` (authority `~`, non-resolvable) instead
    // of claude-code's `pathToFileURL(resolved)` = `file:///home/user/shared`.
    // Expanding here also keeps the MCP-roots cell and the trusted-dir set in
    // lock-step, so a later runtime `/add-dir <same abs path>` dedupes
    // identically in both surfaces (review RV3).
    let mcp_roots_seed: Vec<std::path::PathBuf> = {
        let home = dirs::home_dir();
        boot_additional_working_dirs
            .iter()
            .map(|raw| expand_trusted_dir(raw, &cwd, home.as_deref()))
            .collect()
    };
    let mcp_additional_roots = mcp::new_shared_roots(mcp_roots_seed);
    let mcp_registry = Arc::new(
        mcp::McpRegistry::with_raw_conn(
            mcp_transport.clone() as Arc<dyn McpTransport>,
            mcp_transport as Arc<dyn mcp::RawConnectionProvider>,
        )
        .with_hook_dispatcher(Some(elicitation_dispatcher))
        .with_discovery_cache_store(mcp::DiscoveryCacheStore::new(
            cfg.lingxi_home.join("mcp-discovery-cache"),
        ))
        .with_oauth(mcp_oauth_deps)
        .with_headers_helper_cwd(cwd.clone())
        // Advertise the session's additional working dirs (settings
        // `additionalDirectories` + `--add-dir`) on every server's `roots/list`,
        // matching claude-code r1d() = [cwd, ...additionalWorkingDirectories].
        .with_additional_roots(mcp_additional_roots),
    );
    hook_mcp_invoker.bind(mcp_registry.clone());
    // Subscribe before connecting: a server is allowed to invalidate a catalog
    // immediately after initialization, before the shared ToolRegistry exists.
    // Tokio's broadcast receiver retains those early notifications until the
    // refresh driver below is installed.
    let mut mcp_catalog_changes = mcp_registry.subscribe_catalog_changes();
    mcp_registry.connect_all(mcp_configs).await;
    // Clone handles the runtime `/add-dir` live effect needs (the same registry
    // Arc is moved into the orchestrator builder below via `with_mcp_registry`).
    let runtime_mcp_registry = mcp_registry.clone();

    // (5.4) Real compaction. In-Loop Compaction Batch 6: back the autocompact
    //       layer with a REAL forked summary call (sharing the parent's prompt
    //       cache) instead of the deterministic-fallback summarizer. The same
    //       `cache_safe_slot` is handed to BOTH the summarizer (here) and the
    //       orchestrator (`with_cache_safe_slot` below), so the turn loop's
    //       per-call snapshot is what the summary call replays.
    //
    //       The autocompact threshold follows the session's CURRENT model
    //       (`with_model_derived_threshold`): the proactive pre-call trigger
    //       re-resolves it on every call, so a catalog model with a 1M-token
    //       window is not compacted at a fixed 200k-era constant, and a
    //       mid-session model switch moves the gate with it. This expression is
    //       only the seed for the model the session boots on.
    let cache_safe_slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
    let side_query_client: Arc<dyn sidequery::SideQueryClient> =
        Arc::new(sidequery::ProviderSideQueryClient::new(
            cfg.api_key.clone(),
            Some(cfg.api_base.clone()),
            http.clone() as Arc<dyn platform_api::HttpTransport>,
        ));
    let compaction_side_query: Arc<dyn sidequery::SideQueryClient> = Arc::new(
        sidequery::ProviderSideQueryClient::from_service(api_service.clone()),
    );
    let forked_runner = Arc::new(
        sidequery::ForkedAgentRunner::new()
            .with_side_query_client(compaction_side_query, orch_cfg.model.clone())
            // (M10 cc2.1.198) the compaction summary call INHERITS the session
            // extended-thinking config (binary: `thinkingConfig: mXt(r)` on the
            // summarizer `sEt` call @216945141). This is the SAME resolved
            // `cfg.session_thinking` the main-loop `ApiService` holds (boot
            // MAX_THINKING_TOKENS / --max-thinking-tokens / alwaysThinkingEnabled
            // resolution) — and the model predicates + `LINGXI_DISABLE_THINKING`
            // kill switches still apply per request inside `reasoning_for_request`.
            .with_session_thinking(cfg.session_thinking),
    );
    // `/recap` reuses the SAME single-turn forked runner the autocompact
    // summarizer uses — CLONE the `Arc` here BEFORE `forked_runner` moves into
    // the `Autocompactor` below, so recap replays the identical cache-safe
    // prefix (the same `cache_safe_slot` is already shared with both). Recap
    // reads the runner read-only; it never mutates history/slot.
    let recap_runner = forked_runner.clone();
    let autocompactor =
        compaction::Autocompactor::with_forked_runner(forked_runner, cache_safe_slot.clone());
    let compactor = Arc::new(
        compaction::CompactionOrchestrator::with_autocompactor(
            autocompactor,
            compaction::thresholds::auto_compact_threshold(&orch_cfg.model, &[]),
        )
        .with_model_derived_threshold(),
    );

    // (5.45) The real desktop `TaskRegistry`, wired into the tool context. Tasks
    //        materialize stdout/stderr under a SESSION-SCOPED project temp dir
    //        `<projectTempDir>/<sessionId>/tasks` (claude-code `getTaskOutputDir`,
    //        `diskOutput.ts:50-55`) instead of an in-repo `<cwd>/.lingxi/...`
    //        path: the session id keeps concurrent sessions in one project from
    //        clobbering each other's spools, and the temp root keeps task output
    //        out of the working tree / git status (T16). The spawner is the
    //        tokio-backed `PosixRuntime`. The same handle is returned for a
    //        transport/TUI poller to read live state.
    // Key the directory by THIS session's id, not a fresh one: claude-code's
    // `o1e()` derives it from `K()` (the live session id), so the task spools
    // sit beside the session's other per-session state instead of under a uuid
    // that exists nowhere else in the process.
    let task_output_dir = session_task_output_dir(&cwd, &main_session_uuid);
    // Eagerly create the dir (claude-code `ensureOutputDir`'s `mkdir(recursive)`)
    // so the very first spool `allocate` (exclusive create) finds its parent.
    if let Err(e) = std::fs::create_dir_all(&task_output_dir) {
        tracing::warn!(
            target: "harness_runtime::desktop::tasks",
            dir = %task_output_dir.display(),
            error = %e,
            "could not create the session task-output dir; task spools may fail to allocate"
        );
    }
    let mut task_registry_inner = tasks::registry::TaskRegistry::new(
        Arc::new(PosixRuntime::new()),
        Arc::new(PosixFileSystem::new(cwd.clone())),
        Arc::new(tasks::output_manager::TaskOutputManager::new(
            task_output_dir,
            Arc::new(PosixFileSystem::new(cwd.clone())),
        )),
    )
    // `tengu_agent_tool_terminated` (async twin): the registry is where
    // `killed_by` is known, so it is where the event can name its origin.
    .with_analytics_bus(analytics_bus.clone())
    // Fire the `TaskCompleted` hook (claude-code `executeTaskCompletedHooks`)
    // when a task reaches a terminal status. The firer wraps the SAME
    // `Arc<HookExecutorImpl>` the orchestrator fires its other hooks through, so
    // the `tasks` leaf reaches `orch.hooks` without a dependency cycle.
    .with_task_completed_firer(Arc::new(orchestrator::OrchestratorTaskCompletedFirer::new(
        hooks.clone(),
        cwd.clone(),
        main_transcript_path.clone(),
    )))
    // Fire the `TaskCreated` hook (claude-code `executeTaskCreatedHooks`) when a
    // task is created. Counterpart to the `TaskCompleted` firer above — wraps
    // the SAME `Arc<HookExecutorImpl>` so the `tasks` leaf reaches `orch.hooks`
    // without a dependency cycle.
    .with_task_created_firer(Arc::new(orchestrator::OrchestratorTaskCreatedFirer::new(
        hooks.clone(),
        cwd.clone(),
        main_transcript_path.clone(),
    )));
    // Register the M2 self-contained per-type handlers (LocalBash + MonitorMcp)
    // before the registry is shared. Both depend only on platform traits we
    // already build here; agent/teammate/workflow/remote/dream handlers register
    // once their production pools are wired (M9+).
    //
    // (M8 cc2.1.198 "Task panels: no stuck Running") The bash worker's
    // terminal status + exit code now write THROUGH to the registry via a
    // deferred `RegistryStatusSink` (bound at (5.46f) once the registry `Arc`
    // exists — the same cycle-break as the LocalAgent sink). Pre-fix the
    // handler defaulted to `NoopStatusSink`, so a finished background bash
    // task's stored status stayed `Running` forever.
    let bash_status_sink = Arc::new(tasks::registry_status_sink::RegistryStatusSink::new());
    tasks::registry::register_self_contained_handlers(
        &mut task_registry_inner,
        Arc::new(PosixProcess::new()),
        Arc::new(PosixSandbox::new()),
        mcp_registry.clone(),
        bash_status_sink.clone() as Arc<dyn tasks::handlers::TaskStatusSink>,
    );

    // (5.46) M10 (T13): construct the per-session coordinator subsystem — one
    //        `TeamRegistry` + one `CoordinatorMode` per `build()`. The registry
    //        is observable (the status feed / PHASE-2 command router read it)
    //        but stays empty unless a coordinator session spawns teammates. The
    //        mode is entered at BUILD time ONLY when the session was started as a
    //        coordinator (the registry is built-once-and-moved, so mode-exclusive
    //        tool selection must be decided here); a default session leaves it
    //        DISABLED so the build is byte-identical to the pre-M10 build.
    let coordinator_id = protocol::AgentId::new();
    let coordinator = Arc::new(
        coordinator::TeamRegistry::new(coordinator_id).with_config_home(cfg.lingxi_home.clone()),
    );
    coordinator.set_permission_gate(perms.clone()).await;
    // Named background agents can address main even without experimental teams.
    coordinator
        .mailbox_router
        .register(
            coordinator_id,
            Arc::new(coordinator::TeammateMailbox::new(coordinator_id)),
        )
        .await;
    coordinator
        .mailbox_router
        .register_alias("main", coordinator_id)
        .await;
    coordinator
        .mailbox_router
        .register_alias("team-lead", coordinator_id)
        .await;
    if let Some(team_name) = &cfg.initial_teammate_team_name {
        coordinator.set_team_name(Some(team_name.clone())).await;
    }
    let coordinator_mode = {
        let mut mode = coordinator::CoordinatorMode::new();
        mode.session_started_as_coordinator = cfg.session_started_as_coordinator;
        if cfg.session_started_as_coordinator {
            mode.enter();
        }
        Arc::new(mode)
    };
    let _ = subagent_coordinator_mode_cell
        .set(coordinator_mode.clone()
            as Arc<dyn platform_api::coordinator_mode::CoordinatorModeHandle>);

    // (5.46-prompt) D1 ITEM 4: coordinator-mode system prompt + user context.
    //        Mirrors TS `buildEffectiveSystemPrompt` (systemPrompt.ts:59-75):
    //        when coordinator mode is active AND nothing has already overridden
    //        the system prompt (the Rust analog of "no main-thread agent
    //        definition" + "no explicit overrideSystemPrompt"), swap in the
    //        coordinator system prompt. `system_prompt_override` being `None` is
    //        precisely that condition here — the desktop host does not set it for
    //        a normal session, and a CLI `--system-prompt` / agent override would
    //        have populated it (TS: `overrideSystemPrompt` wins first). The
    //        per-turn coordinator USER context (TS `getCoordinatorUserContext`,
    //        injected at QueryEngine.ts:304) has no per-turn user-context seam in
    //        this orchestrator yet, so it is appended to the coordinator system
    //        prompt as a trailing `<system-reminder>` block (the worker-tools
    //        allow-list + connected-MCP names; scratchpad is omitted — no
    //        scratchpad gate/path is wired on desktop). A true per-turn
    //        recomputation is DEFERRED until a per-turn user-context seam exists.
    if coordinator_mode.is_enabled() && orch_cfg.system_prompt_override.is_none() {
        let simple = coordinator::is_env_truthy(std::env::var("LINGXI_SIMPLE").ok().as_deref());
        let mut prompt = coordinator::coordinator_system_prompt(simple);
        // Connected MCP server names for the worker-tools user context.
        let mcp_names: Vec<String> = mcp_registry
            .snapshot()
            .await
            .into_iter()
            .map(|s| s.name)
            .collect();
        if let Some(user_ctx) = coordinator::coordinator_user_context(&mcp_names, None, simple) {
            // Wrap as a system-reminder, mirroring how claude-code injects
            // per-turn meta context (`wrapInSystemReminder`).
            prompt.push_str("\n\n<system-reminder>\n");
            prompt.push_str(&user_ctx);
            prompt.push_str("\n</system-reminder>");
        }
        orch_cfg.system_prompt_override = Some(prompt);
    }

    // (5.46a) M10 (T13): register the `InProcessTeammate` handler DIRECTLY (not
    //        via `register_agent_handlers`) so the coordinator's
    //        `CoordinatorStatusSink` is attached — that sink maps the teammate's
    //        `TaskStatus` transitions onto `WorkerStatus` AND pushes the live
    //        `active_workers` scalar to the orchestrator-facing `OutputStream`.
    //        Registration takes `&mut self`, so it MUST happen before the
    //        registry is `Arc`-wrapped below.
    //
    //        Teammates run in their OWN `StateMachinePool` (`TEAMMATE_POOL_CAP`),
    //        separate from the `AgentTool` `subagent_pool`: persistent teammates
    //        park on `wait_for_message` and never free their slot, so a shared
    //        pool would risk starving one-shot subagent spawns (T14 regression).
    //
    //        The handler's tool-dispatch seam is a `DeferredToolInvoker`: the
    //        teammate must inherit the parent's `Arc<ToolRegistry>` (recursion
    //        lock), but that registry is assembled AFTER this point (its
    //        `BuiltinToolContext` carries `task_registry.clone()`). The deferred
    //        invoker is injected now and bound to the real `RegistryToolInvoker`
    //        once `tools` exists (5.5a). Definition resolution maps the worker
    //        id back to its declared `agent_type`, then resolves the live
    //        catalog; unknown types retain the permissive fallback.
    let coordinator_sink = Arc::new(coordinator::CoordinatorStatusSink::new(
        coordinator.clone(),
        output.clone(),
    ));
    let teammate_registry_status_sink =
        Arc::new(tasks::registry_status_sink::RegistryStatusSink::new());
    let teammate_status_sink = Arc::new(TeammateStatusFanout {
        task_registry: teammate_registry_status_sink.clone(),
        coordinator: coordinator_sink,
    });
    let teammate_invoker = Arc::new(DeferredToolInvoker::new());
    let teammate_pool = Arc::new(agent::StateMachinePool::new(
        Arc::new(PosixRuntime::new()),
        TEAMMATE_POOL_CAP,
    ));
    let teammate_handler = tasks::handlers::InProcessTeammateHandler::new(
        teammate_pool,
        task_registry_inner.output_manager.clone(),
        teammate_api,
    )
    // Keep teammate auto-claim on the same host-owned task-store root as the
    // Task* tools and orchestrator reminders.
    .with_config_home(cfg.lingxi_home.clone())
    .with_definitions(Arc::new(CoordinatorTeammateDefinitionResolver {
        team: coordinator.clone(),
        catalog: agent_catalog.clone(),
    }))
    .with_tool_invoker(teammate_invoker.clone() as Arc<dyn platform_api::tool_invoker::ToolInvoker>)
    // Anchor the teammate's `AgentModel::Inherit` / family aliases to the parent
    // model — the same seam the `PoolSubagentSpawner` gets above. #15: resolve
    // the alias to the concrete main-loop wire id (claude `getMainLoopModel()`)
    // so an `Inherit` teammate in default mode runs against a real id, not the
    // raw `orch_cfg.model` alias (which would fail at the provider).
    .with_default_model(agent::model_resolution::resolve_user_specified_model(
        &orch_cfg.model,
    ))
    // #15: thread the live permission mode + the RAW user model setting (the
    // un-resolved alias, e.g. "opusplan") so the teammate's `Inherit` resolution
    // gets the same `getRuntimeMainLoopModel` plan-mode swap as the spawner above
    // (opusplan + plan → Opus). Default mode → byte-identical to before. Uses
    // the post-fallback `model_setting_for_spawns` (same reasoning as the
    // spawner seam).
    .with_permission_mode(cfg.permission_mode)
    .with_model_setting(model_setting_for_spawns.clone())
    .with_session_interactive(interactive_session)
    .with_new_diagnostics_source_factory(Arc::new({
        let diagnostics = lsp_diagnostics_cell.clone();
        let session_cwd = teammate_session_cwd_cell.clone();
        move || {
            diagnostics
                .get()
                .expect("LSP diagnostics registry is initialized before teammate spawn")
                .diagnostics_source(
                    Some(
                        session_cwd
                            .get()
                            .expect("session cwd is initialized before teammate spawn")
                            .cwd(),
                    ),
                    Some(std::time::Duration::from_millis(500)),
                )
        }
    }))
    .with_status_sink(teammate_status_sink as Arc<dyn tasks::handlers::TaskStatusSink>)
    // Fire the `TeammateIdle` hook (claude-code `executeTeammateIdleHooks`,
    // `stopHooks.ts:403`) each time a teammate finishes a turn-set and parks
    // awaiting the next message ("about to go idle"). The firer wraps the SAME
    // `Arc<HookExecutorImpl>` the orchestrator fires its other hooks through, so
    // the `tasks` leaf reaches `orch.hooks` without a dependency cycle —
    // mirroring the `TaskCompleted` / `TaskCreated` firers above.
    .with_teammate_idle_firer(Arc::new(orchestrator::OrchestratorTeammateIdleFirer::new(
        hooks.clone(),
        cwd.clone(),
        main_transcript_path.clone(),
    )))
    // Full teammate parity (P1): inherit the shared budget enforcer + fire
    // SubagentStart via the same `HookExecutorImpl` the orchestrator uses, and
    // stamp the SubagentStart `HookContext` (owning session id + cwd). `hooks` /
    // `budget_enforcer` already exist here (the teammate handler is built after
    // them), unlike the spawner's deferred cells. The advertised tool pool +
    // skills-preload registries are filled via handles below (they don't exist
    // yet). This makes a teammate a full team worker (tools + budget + hooks),
    // not a chat-only stub.
    .with_budget_enforcer(budget_enforcer.clone())
    .with_hook_executor(hooks.clone())
    .with_plan_approval_mailbox(coordinator.mailbox_router.clone())
    .with_plan_approval_gate(perms.clone())
    // The SAME identity the permission carve-out and the plan-mode reminder
    // resolve through, so a teammate's plan file is `ay(agentId)` inside the
    // session's plans directory rather than under the config home.
    .with_plan_files(plan_files.clone())
    .with_hook_context(subagent_hook_session_id, cwd.clone())
    .with_transcript(
        Arc::new(PosixFileSystem::new(cwd.clone())),
        main_subagents_dir.clone(),
    );
    // Grab the teammate handler's set-once cells BEFORE boxing, to fill once the
    // tool registry / skill loader exist (same deferred-fill the spawner uses).
    let teammate_tool_registry_cell = teammate_handler.tool_registry_handle();
    let teammate_skill_loader_cell = teammate_handler.skill_loader_handle();
    let teammate_tool_wide_deny_cell = teammate_handler.tool_wide_deny_names_handle();
    let teammate_strict_plugin_hooks_cell = teammate_handler.strict_plugin_only_hooks_handle();
    let teammate_system_prompt_renderer_cell = teammate_handler.system_prompt_renderer_handle();
    let _ = teammate_strict_plugin_hooks_cell.set(strict_plugin_only_hooks);
    task_registry_inner.register_handler(
        tasks::TaskType::InProcessTeammate,
        Arc::new(teammate_handler),
    );

    // (5.46c) Cron: register the `Dream` handler so cron-spawned `TaskType::Dream`
    //        tasks actually run. The `CronScheduler` (constructed + started below,
    //        after the registry is shared) creates `Dream` tasks; without a handler
    //        each fire would be an inert task-state row. Same deferred-invoker
    //        pattern as the teammate handler above: the real `RegistryToolInvoker`
    //        needs `tools` (built after this point), so a `DeferredToolInvoker` is
    //        injected now and bound to the real invoker at (5.5a) below.
    let dream_invoker = Arc::new(DeferredToolInvoker::new());
    let dream_status_sink = Arc::new(tasks::registry_status_sink::RegistryStatusSink::new());
    tasks::registry::register_dream_handler(
        &mut task_registry_inner,
        subagent_spawner.clone(),
        dream_invoker.clone() as Arc<dyn platform_api::tool_invoker::ToolInvoker>,
        budget_enforcer.clone(),
        dream_status_sink.clone() as Arc<dyn tasks::handlers::TaskStatusSink>,
    );

    // (5.46d) T15: register the `LocalAgent` handler so `TaskType::LocalAgent`
    //        tasks dispatch to a real one-shot subagent worker instead of failing
    //        with `UnknownType`. This closes the gap where `register_agent_handlers`
    //        was authored but never called from any composition root, leaving
    //        `LocalAgent`/`LocalWorkflow` with state variants but no handler.
    //
    //        We register the LocalAgent handler DIRECTLY rather than calling
    //        `register_agent_handlers` (which ALSO registers `InProcessTeammate`)
    //        because the teammate handler was already registered above (5.46a)
    //        with the coordinator `CoordinatorStatusSink` attached — calling the
    //        combined helper here would clobber that sink-bearing handler with a
    //        sink-less one.
    //
    //        Same deferred-invoker pattern as the teammate + dream handlers: the
    //        real `RegistryToolInvoker` needs `tools` (assembled after this
    //        point), so a `DeferredToolInvoker` is injected now and bound at
    //        (5.5a) below once `tools` exists.
    //
    //        DEFERRED (out of scope here): routing the BACKGROUNDED `AgentTool`
    //        spawn (claude-code `registerAsyncAgent`) through
    //        `TaskRegistry::spawn(TaskType::LocalAgent)` so background agents
    //        surface in TaskList/Get/Output. `AgentTool::call` always dispatches
    //        synchronously through the spawner today and exposes no clean
    //        backgrounded seam to re-route; that wiring lands with the async-agent
    //        work. Registering the handler here is the prerequisite for it.
    let local_agent_invoker = Arc::new(DeferredToolInvoker::new());
    // Bridge the LocalAgent worker's status (and rest signals) THROUGH to the
    // registry so list/get reflect reality and `take_pending_task_notifications`
    // actually fires (terminal completion + each "comes to rest"). Deferred: the
    // handler is registered before the registry `Arc` exists, so this is bound
    // at (5.46f) below once `task_registry` is built.
    let local_agent_status_sink = Arc::new(tasks::registry_status_sink::RegistryStatusSink::new());
    // ONE worktree manager shared by the AgentTool (which CREATES the isolation
    // worktree + judges it on the SYNC path) and the LocalAgent handler (which
    // judges it when a BACKGROUND agent reaches a terminal state — claude-code's
    // `getWorktreeResult` closure handed to the detached lifecycle).
    let worktree_manager: Arc<dyn platform_api::worktree::WorktreeManager> =
        Arc::new(PosixWorktreeManager::new(cwd.clone()));
    // The forked-skill resume gate. Its skill resolver is bound LATER (the
    // command registry does not exist yet — the same registration cycle the
    // status sink solves); until then it reports "not fork-capable", which
    // REFUSES rather than waving a forked skill through.
    let fork_capable_skills = Arc::new(fork_resume::RegistryForkCapableSkills::new());
    let fork_resume_gate = Arc::new(fork_resume::DesktopForkResumeGate {
        // Beside the agents' own transcripts (`agent-<id>.jsonl`), not in the
        // project session directory — that is where the writer puts them and
        // where anything keying off the real transcript will look.
        subagents_dir: main_subagents_dir.clone(),
        skills: fork_capable_skills.clone() as Arc<dyn fork_resume::ForkCapableSkills>,
    });
    task_registry_inner.register_handler(
        tasks::TaskType::LocalAgent,
        Arc::new(
            tasks::handlers::LocalAgentHandler::new(
                subagent_spawner.clone(),
                local_agent_invoker.clone() as Arc<dyn platform_api::tool_invoker::ToolInvoker>,
                budget_enforcer.clone(),
                task_registry_inner.output_manager.clone(),
            )
            // Wire the persistent/resume seam: a BACKGROUNDED LocalAgent now
            // parks ("comes to rest") after each turn-set and accepts
            // `send_message` to resume — claude-code's unified agent lifecycle
            // (`resumeAgentBackground` / `injectUserMessageToTeammate`).
            .with_streaming_spawner(subagent_streaming_spawner.clone())
            .with_status_sink(
                local_agent_status_sink.clone() as Arc<dyn tasks::handlers::TaskStatusSink>
            )
            // Terminal keep/cleanup of a background agent's isolation worktree.
            .with_worktree_manager(worktree_manager.clone())
            // Refuse to resume a forked skill whose permission scoping cannot
            // be re-established — resuming one unscoped would run it under the
            // parent's (strictly wider) permissions.
            .with_fork_resume_gate(
                fork_resume_gate.clone() as Arc<dyn platform_api::fork_resume_gate::ForkResumeGate>
            )
            // Record each parked agent so a LATER process can rebuild it; the
            // record is erased the moment it terminates.
            .with_parked_agent_store(Arc::new(agent_restore::DesktopParkedAgentStore {
                subagents_dir: main_subagents_dir.clone(),
            })
                as Arc<dyn platform_api::parked_agent_store::ParkedAgentStore>),
        ),
    );

    // (5.46e) Register the `LocalWorkflow` handler so the `Workflow` tool's
    //        `TaskRegistry::spawn(TaskType::LocalWorkflow)` dispatches to a real
    //        workflow worker (the embedded QuickJS runtime + agent()→subagent
    //        bridge) instead of failing with `UnknownType`. Same deferred-invoker
    //        pattern as the LocalAgent handler above (bound at (5.5a) once `tools`
    //        exists): a workflow's `agent()` calls inherit this invoker so their
    //        child runners dispatch tools through the parent registry.
    let local_workflow_invoker = Arc::new(DeferredToolInvoker::new());
    // Shared `budget.spent()` pool: published once the orchestrator exists
    // (built below) — the same `Arc<AtomicU64>` the main loop feeds per response,
    // so a workflow's `spent()` reads main loop + all workflows. Same deferred
    // pattern as `local_workflow_invoker` (handler registered before the orch).
    let local_workflow_output_pool: Arc<std::sync::OnceLock<Arc<std::sync::atomic::AtomicU64>>> =
        Arc::new(std::sync::OnceLock::new());
    // Turn-start output baseline (claude-code `xtr`) backing the workflow's
    // turn-relative `budget.spent()`; published from the orchestrator below.
    let local_workflow_turn_baseline: Arc<std::sync::OnceLock<Arc<std::sync::atomic::AtomicU64>>> =
        Arc::new(std::sync::OnceLock::new());
    // Deferred status sink (bound to the registry Arc below) so the workflow
    // worker's terminal `set_status(Completed/Failed)` actually reaches the
    // registry — WITHOUT this the handler keeps the default `NoopStatusSink` and
    // a finished workflow is stuck on `Running` forever in `/workflows`. Same
    // "no stuck Running" wiring bash + local_agent already have.
    let local_workflow_status_sink =
        Arc::new(tasks::registry_status_sink::RegistryStatusSink::new());
    // §14 — THE shared plugin-workflow registry for this session. Constructed
    // here, before its first consumer, because all FOUR of them must hold the
    // same `Arc`:
    //   - `plugin::PluginManager` (below, at the plugin bootstrap) — the sole
    //     WRITER: `enable`/`disable` seed and remove a plugin's entries.
    //   - `tasks::handlers::LocalWorkflowHandler` — the nested
    //     `workflow({name})` resolver.
    //   - `tool_workflow::WorkflowTool` — `validate_input`'s name resolution
    //     and its `Available:` listing.
    //   - `TaskRegistryWorkflowLauncher` — the launch-path `resolve_script_at`
    //     and the `tengu_workflow_launched` `workflow_source`.
    // Wiring a strict subset is worse than wiring none: the tool would accept
    // `acme:deploy` and the launcher would then report it "not found".
    // The manager fills it at `enable` time, long after the readers are built;
    // the registry is interior-mutable, so construction order does not matter.
    let plugin_workflow_registry = Arc::new(workflow::PluginWorkflowRegistry::new());
    let (workflow_event_tx, workflow_event_rx) =
        tokio::sync::mpsc::unbounded_channel::<DesktopWorkflowEvent>();
    let local_workflow_event_sink = Arc::new(DesktopWorkflowEventSink {
        registry: local_workflow_status_sink.clone(),
        tx: workflow_event_tx,
    });
    let fusion_executor: Arc<dyn platform_api::FusionExecutor> = desktop_fusion_executor(
        subagent_spawner.clone(),
        Arc::new(sidequery::ProviderSideQueryClient::from_service(
            api_service.clone(),
        )),
        &cfg,
        fusion_attempts.clone(),
        fusion_catalog_source.clone(),
        analytics_bus.clone(),
        pricing.clone(),
    );
    let local_workflow_handler = tasks::handlers::LocalWorkflowHandler::new(
        subagent_spawner.clone(),
        local_workflow_invoker.clone() as Arc<dyn platform_api::tool_invoker::ToolInvoker>,
        budget_enforcer.clone(),
        task_registry_inner.output_manager.clone(),
    )
    // Durable hosts share a captured session/turn account. Legacy hosts keep
    // the late-bound pool and baseline used by the existing budget reader.
    .with_token_budget(orch_cfg.token_budget)
    .with_output_pool_cell(local_workflow_output_pool.clone())
    .with_turn_baseline_cell(local_workflow_turn_baseline.clone())
    .with_workspace_permission_leases(workspace_leases.clone(), cwd.clone())
    .with_worktree_manager(worktree_manager.clone())
    .with_fusion(fusion_executor.clone())
    .with_terminal_recorder_opt(Some(fusion_recorder.clone()))
    .with_terminal_recorder_factory(fusion_recorder_factory.clone())
    .with_status_sink(local_workflow_event_sink.clone() as Arc<dyn tasks::handlers::TaskStatusSink>)
    .with_workflow_progress_sink(local_workflow_event_sink.clone()
        as Arc<dyn tasks::handlers::local_workflow::WorkflowProgressSink>)
    // Nested workflow names resolve against the same plugin registry.
    .with_plugin_workflows(plugin_workflow_registry.clone());
    let local_workflow_handler =
        local_workflow_handler.with_output_scopes(workflow_output_scopes.clone());
    task_registry_inner.register_handler(
        tasks::TaskType::LocalWorkflow,
        Arc::new(local_workflow_handler),
    );

    // Fusion `/fusion` + Agent `subagent_type: "fusion"` share one orchestrator.
    // Register the LocalFusion handler before the registry is Arc-wrapped. The
    // completion sink is bound after the orchestrator handle exists; the tool
    // invoker is bound at (5.5a) once `tools` exists. `/fusion` is explicit
    // per-run and works even when `fusion.enabled` is false.
    let fusion_completion_sink = Arc::new(fusion_command::DeferredFusionCompletionSink::new());
    let fusion_invoker = Arc::new(DeferredToolInvoker::new());
    let fusion_status_sink = Arc::new(tasks::registry_status_sink::RegistryStatusSink::new());
    tasks::registry::register_fusion_handler_with_recorder_factory(
        &mut task_registry_inner,
        fusion_executor.clone(),
        fusion_completion_sink.clone() as Arc<dyn platform_api::FusionCompletionSink>,
        fusion_invoker.clone() as Arc<dyn platform_api::tool_invoker::ToolInvoker>,
        budget_enforcer.clone(),
        fusion_status_sink.clone() as Arc<dyn tasks::handlers::TaskStatusSink>,
        Some(fusion_recorder.clone()),
        Some(fusion_recorder_factory.clone()),
    );

    let task_registry = Arc::new(task_registry_inner);
    // ONE observer pairing table per session. The registry files a pairing when
    // it spawns an observer; `ObserverReport` resolves against the same `Arc`
    // via the orchestrator below. Two tables would look wired and answer
    // "not armed" forever, so this is created once and shared, never cloned
    // from a second `ObserverPairings::new()`.
    let observer_pairings = Arc::new(platform_api::observer_pairing::ObserverPairings::new());
    task_registry.set_observer_pairings(observer_pairings.clone());
    subagent_spawner_arc.set_task_registry(task_registry.clone());
    teammate_registry_status_sink.bind(task_registry.clone());

    // (5.46f) Bind the deferred LocalAgent status sink now that the registry
    //         `Arc` exists: the persistent agent's `set_status` / `notify_rest`
    //         now reach `task_registry`, so terminal + per-rest notifications
    //         surface through `take_pending_task_notifications`.
    local_agent_status_sink.bind(task_registry.clone());
    dream_status_sink.bind(task_registry.clone());
    // (M8 cc2.1.198) Bind the deferred LocalBash sink too: the bash worker's
    // terminal `set_status` / `set_exit_code` now reach `task_registry`, so a
    // finished background command flips its panel row off `Running`.
    bash_status_sink.bind(task_registry.clone());
    // Bind the deferred LocalWorkflow sink: a finished workflow's terminal
    // `set_status(Completed/Failed)` now reaches `task_registry`, so `/workflows`
    // flips it off `Running` instead of showing it stuck forever.
    local_workflow_status_sink.bind(task_registry.clone());
    fusion_status_sink.bind(task_registry.clone());

    // (5.48) Cron: construct, load the single persisted tasks file, and start the
    //        live cron scheduler so jobs created by CronCreate actually fire —
    //        closing parity gap §0.3 / §B (the scheduler was never constructed, so
    //        persisted jobs never ran). 1:1 with claude-code `cronTasks.ts`: all
    //        durable jobs live in ONE project-relative file
    //        `<cwd>/.lingxi/scheduled_tasks.json` (the same project root the
    //        CronCreate/List/Delete tools key off via `BuiltinToolContext.workspace`,
    //        which is `cwd`). `load_persisted` reads `createdAt`/`lastFiredAt` in
    //        epoch ms; next-fire is COMPUTED at runtime from the cron string +
    //        `lastFiredAt ?? createdAt` (never persisted). Ticks every 60s on a
    //        posix RuntimeSpawner (D17). The tick task holds a self-clone; the
    //        session lifecycle therefore retains the scheduler and explicitly
    //        stops it before draining task producers on shutdown/remount.
    //        Gated by the `CLAUDE_CODE_DISABLE_CRON` local kill-switch
    //        (claude-code `prompt.ts:34/38` — the env override that wins over the
    //        GrowthBook fleet flag, which itself defaults on).
    let cron_scheduler = if !cron_native::is_child_runtime()
        && cron_scheduler_enabled(std::env::var("CLAUDE_CODE_DISABLE_CRON").ok().as_deref())
    {
        if cfg.enable_automation_scheduler && cfg.host_workspace_trusted == Some(true) {
            let migration_model = provider_adapter_handle
                .list_model_listings()
                .into_iter()
                .find(|listing| {
                    listing.request_model == default_model_id
                        && default_model_profile
                            .as_ref()
                            .is_none_or(|profile| &listing.provider_id == profile)
                })
                .map(|listing| {
                    platform_api::qualified_model_ref(
                        &listing.request_model,
                        Some(&listing.provider_id),
                    )
                });
            let migration_reasoning = cfg.initial_effort.as_ref().map_or_else(
                || serde_json::json!({"type":"automatic"}),
                |id| serde_json::json!({"type":"level", "id":id}),
            );
            cron_management::migrate_legacy(
                &PosixFileSystem::new(cwd.clone()),
                &cwd,
                migration_model,
                migration_reasoning,
            )
            .await
            .map_err(|error| {
                BuildError::DurableSession(format!("Scheduled task migration failed: {error}"))
            })?;
            if cron::scheduled_tasks_path(&cwd).exists() {
                cron::automation::recover_orphaned_automation_runs(
                    &PosixFileSystem::new(cwd.clone()),
                    &cwd,
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64,
                )
                .await
                .map_err(BuildError::DurableSession)?;
            }
        }
        let tasks_file = cron::tasks_file::scheduled_tasks_path(&cwd);
        let scheduler = Arc::new(
            cron::CronScheduler::new(
                task_registry.clone(),
                Arc::new(PosixFileSystem::new(cwd.clone())),
                clock.clone(),
                Arc::new(PosixRuntime::new()),
                tasks_file,
            )
            .with_session_id(main_session_uuid.clone())
            .with_session_cron(
                !(cfg.enable_automation_scheduler && cfg.host_workspace_trusted.is_some()),
            ),
        );
        // Load every durable job from the single tasks file (a recurring job
        // created days ago is aged correctly on load; its restored `lastFiredAt`
        // prevents a missed-run catch-up from re-firing an already-fired run).
        scheduler.load_persisted().await;
        if let Err(e) = scheduler.clone().start().await {
            tracing::error!("cron: failed to start scheduler: {e}");
        }
        Some(scheduler)
    } else {
        None
    };

    // Share task delivery and cancellation with the implicit team service.
    let spawn_seam: Arc<dyn platform_api::team_spawn::TeamSpawnSeam> =
        if platform_api::env::agent_swarms_enabled() {
            let executable = std::env::current_exe()
                .ok()
                .filter(|path| {
                    path.file_stem()
                        .is_some_and(|name| name == "lingxi-cli" || name == "lingxi")
                })
                .or_else(|| {
                    std::env::var_os("PATH").and_then(|path| {
                        std::env::split_paths(&path)
                            .map(|dir| dir.join("lingxi-cli"))
                            .find(|candidate| candidate.is_file())
                    })
                })
                .unwrap_or_else(|| std::path::PathBuf::from("lingxi-cli"));
            let seam = pane_teammate::PaneTeammateSpawner::new(
                task_registry.clone(),
                coordinator.clone(),
                Arc::new(PosixRuntime::new()),
                output.clone(),
                main_session_id,
                std::path::PathBuf::from("/tmp"),
                None,
                false,
                executable,
            )
            .with_backend_selector(teammate_backend_selector(
                cwd.clone(),
                cfg.flag_settings.as_ref().and_then(|s| s.teammate_mode),
                cfg.is_tty,
            ));
            Arc::new(seam)
        } else {
            task_registry.clone()
        };

    let departure_owner: Arc<dyn platform_api::team_spawn::TeammateDepartureCleanup> =
        coordinator.clone();
    task_registry
        .set_teammate_departure_cleanup(Arc::downgrade(&departure_owner))
        .await;

    if platform_api::env::agent_swarms_enabled() {
        task_registry
            .set_external_teammate_controller(Arc::downgrade(&spawn_seam))
            .await;
    }

    // (5.5) Assemble the desktop tool registry through the composition root.
    //       A coordinator session shares the team's `MailboxRouter` with the
    //       builtin `SendMessage` tool by casting it onto `tool_ctx.mailbox_router`
    //       (the trait impl lives on `MailboxRouter`); a default session leaves it
    //       `None` — byte-identical to the pre-M10 build.
    // Wire the shared `MailboxRouter` for EVERY session (was coordinator-only):
    // a backgrounded local_agent (`run_in_background`) registers its mailbox on
    // this router, and the `SendMessage` tool must be able to route to it even
    // outside a coordinator team. Faithful to claude-code, where `SendMessage`
    // always resolves a running async agent. Non-async default sessions are
    // unaffected — with no teammates/agents registered a send resolves to
    // `NotFound`, the same effective outcome as the prior `None`.
    let coordinator_mailbox: Option<Arc<dyn platform_api::mailbox::MailboxRouterHandle>> =
        Some(coordinator.mailbox_router.clone()
            as Arc<dyn platform_api::mailbox::MailboxRouterHandle>);
    // (SANDBOX.1) Make the bash sandbox path LIVE (parity §0.2 / §B). Previously
    // `sandbox_available` was hardcoded `false`, so bash NEVER sandboxed — even
    // when the user enabled it in settings — leaving the macOS SBPL / Linux bwrap /
    // sandbox-runtime stack as dead code. Resolve the runtime config from the
    // `sandbox` settings subsection (claude-code: opt-in via `sandbox.enabled`,
    // default OFF) and probe host deps (sandbox-exec on macOS / bwrap on Linux).
    // `should_use_sandbox` keys on `sandbox_available`, so it is
    // `settings-enabled AND deps-present`. Unset settings ⇒ off ⇒ byte-identical
    // to today; opt-in now actually sandboxes on a capable host.
    let sandbox_platform = if cfg!(target_os = "macos") {
        SandboxPlatform::Mac
    } else {
        SandboxPlatform::Linux
    };
    let sandbox_runtime_cfg = {
        let mut tiers: Vec<String> = Vec::new();
        let flag_settings_raw = cfg
            .flag_settings
            .as_ref()
            .and_then(|settings| serde_json::to_string(settings).ok());
        // Read the USER tier (lingxi_home/settings.json) separately so the
        // source-restricted `allowAppleEvents` resolution can consult it: CC honors
        // allowAppleEvents from user / managed / flag only, NOT project/local.
        let user_settings_raw = if cfg.restricted {
            None
        } else {
            tokio::fs::read_to_string(cfg.lingxi_home.join("settings.json"))
                .await
                .ok()
        };
        if let Some(raw) = &user_settings_raw {
            tiers.push(raw.clone());
        }
        if !cfg.restricted {
            for p in [
                cwd.join(branding::DOT_DIR).join("settings.json"),
                cwd.join(branding::DOT_DIR).join("settings.local.json"),
            ] {
                if let Ok(raw) = tokio::fs::read_to_string(&p).await {
                    tiers.push(raw);
                }
            }
        }
        // CLI `--settings` / `flagSettings` sits between localSettings and
        // policySettings in `SETTING_SOURCES`, so include its raw JSON before
        // the managed tiers in the ascending-priority fold.
        if let Some(raw) = &flag_settings_raw {
            tiers.push(raw.clone());
        }
        // Managed (policySettings) tier — HIGHEST priority (SETTING_SOURCES:
        // …→localSettings→flagSettings→policySettings). Appended LAST so the
        // ascending-priority fold lets a managed `sandbox.*` win over user /
        // project / local / flagSettings (faithful to
        // getInitialSettings()/loadSettingsFromDisk).
        let managed_tiers = crate::desktop::settings_watch::managed_settings_raw_tiers().await;
        tiers.extend(managed_tiers.iter().cloned());
        let refs: Vec<&str> = tiers.iter().map(String::as_str).collect();
        // allowManagedDomainsOnly / allowManagedReadPathsOnly: resolved from the
        // MANAGED tiers ONLY (per-source), then threaded onto the context so the
        // conversion overrides the merged allowlist when the flag is set.
        let (managed_allowed_domains, managed_read_paths) =
            managed_only_sandbox_overrides(&managed_tiers, &cwd);
        // allowAppleEvents: source-restricted to user / managed / flag (project &
        // local are IGNORED — CC parity @223928133). First-defined wins managed →
        // flag → user; `None` leaves the default `false`.
        let allow_apple_events_override = apple_events_override(
            &managed_tiers,
            flag_settings_raw.as_deref(),
            user_settings_raw.as_deref(),
        );
        // strictAllowlist: same source restriction (2.1.219).
        let strict_allowlist_override_v = strict_allowlist_override(
            &managed_tiers,
            flag_settings_raw.as_deref(),
            user_settings_raw.as_deref(),
        );
        let ripgrep_override_v = ripgrep_override(
            &managed_tiers,
            flag_settings_raw.as_deref(),
            user_settings_raw.as_deref(),
        );
        // Seed the `SandboxConvertContext` with the boot-resolvable hardening
        // paths so the settings/skills denyWrite defense actually fires
        // (sandbox-adapter.ts:225-299). Seeds with no boot analog
        // (cwd_settings_paths / worktree_main_repo_path / additional_md_dirs)
        // stay empty — see spec §5.
        let managed = crate::desktop::settings_watch::managed_settings_dir();
        // Preserve the lexical deny-write seeds in the session config. The
        // command path resolves them immediately before every sandboxed launch
        // (`BuiltinToolContext::effective_sandbox_runtime`), which covers both
        // symlinks present at boot and links created or retargeted later. If we
        // replaced a seed with its boot-time target here, a later retarget would
        // be impossible to observe because the original link path was lost.
        let deny_seed = |p: std::path::PathBuf| p.to_string_lossy().into_owned();
        let ctx = sandbox::policy_convert::SandboxConvertContext {
            lingxi_temp_dir: Some(lingxi_temp_dir()),
            task_output_dir: Some(
                session_task_output_dir(&cwd, &main_session_uuid)
                    .to_string_lossy()
                    .into_owned(),
            ),
            settings_file_paths: vec![
                deny_seed(cfg.lingxi_home.join("settings.json")),
                deny_seed(cwd.join(branding::DOT_DIR).join("settings.json")),
                deny_seed(cwd.join(branding::DOT_DIR).join("settings.local.json")),
                deny_seed(managed.join("managed-settings.json")),
            ],
            managed_drop_in_dir: Some(deny_seed(managed.join("managed-settings.d"))),
            skills_dirs: vec![deny_seed(cwd.join(branding::DOT_DIR).join("skills"))],
            managed_allowed_domains,
            managed_read_paths,
            allow_apple_events_override,
            strict_allowlist_override: strict_allowlist_override_v,
            ripgrep_override: ripgrep_override_v,
            ..Default::default()
        };
        sandbox_runtime_config_from_settings_tiers(&refs, &cwd, &ctx)
    };
    // Faithful to claude-code `isSandboxingEnabled()` (sandbox-adapter.ts:532):
    // supported-platform AND deps present AND in the `enabledPlatforms` list AND
    // the user opted in via `sandbox.enabled`. We fold the `enabledPlatforms`
    // gate (`isPlatformInEnabledList`) into BOTH `check_dependencies` (so a
    // missing dep is reported alongside an out-of-list platform) and
    // `sandbox_available`, replacing the old hardcoded `true`.
    // WSL-aware host detection: `platform_posix::sandbox::host_platform()` mirrors
    // claude-code's `getPlatform()` (returns `None`/refused on WSL1), so on WSL1
    // we do NOT report `Linux` and wrongly compute `sandbox_available == true`.
    // A coarse `cfg!(target_os = "linux") ⇒ Linux` would miss the WSL1 refusal.
    let current_platform = platform_posix::sandbox::host_platform();
    // `isPlatformInEnabledList` only makes sense for a supported platform; on an
    // unsupported host (WSL1 / non-POSIX) the platform can never be in the list.
    let in_enabled_list = current_platform.is_some_and(|p| {
        platform_in_enabled_list(sandbox_runtime_cfg.enabled_platforms.as_deref(), p)
    });
    // `check_dependencies(None, …)` yields the "platform not supported" error,
    // so `sandbox_available` correctly drops to false on WSL1 / non-POSIX.
    let sandbox_deps =
        sandbox::dependency_check::check_dependencies(current_platform, in_enabled_list);
    let sandbox_available =
        sandbox_runtime_cfg.enabled && in_enabled_list && sandbox_deps.errors.is_empty();

    // Startup reject/degrade, faithful to claude-code `isSandboxRequired()`
    // (sandbox-adapter.ts:479) + `getSandboxUnavailableReason()` (:562). When the
    // user explicitly enabled the sandbox but it cannot run here:
    //   - `failIfUnavailable: true`  ⇒ this is a HARD failure (their security
    //     posture is being silently ignored otherwise — issue #34044), so we
    //     refuse the build with `BuildError::SandboxUnavailable`;
    //   - otherwise ⇒ degrade to no-sandbox execution but WARN, so the operator
    //     knows commands run unsandboxed.
    let sandbox_required = sandbox_runtime_cfg.enabled && sandbox_runtime_cfg.fail_if_unavailable;
    if let Some(reason) =
        PosixSandbox::unavailable_reason_for(sandbox_runtime_cfg.enabled, in_enabled_list)
    {
        if sandbox_required {
            return Err(BuildError::SandboxUnavailable(reason));
        }
        tracing::warn!(%reason, "Sandbox disabled: commands will run WITHOUT sandboxing");
    }

    // Shared LSP registry: the SAME `Arc<LspRegistry>` is handed to the LSP
    // tool (via `tool_ctx.lsp_registry`) AND to the plugin manager below, so
    // plugin-supplied LSP servers (`.lsp.json`) are registered into the very
    // registry the `LSPTool` reads at runtime (the registry's
    // `register_plugin_servers` is the ONLY supported registration path).
    // Shared LSP diagnostics sink: the registry's `ensure_server_for_file`
    // spawns a passive subscriber per started server that drains
    // `publishDiagnostics` into it, and the orchestrator polls it each turn to
    // surface the `<new-diagnostics>` reminder to the model.
    let lsp_diagnostics = lsp::diagnostic_registry::LspDiagnosticRegistry::new();
    assert!(
        lsp_diagnostics_cell.set(lsp_diagnostics.clone()).is_ok(),
        "LSP diagnostics registry must be initialized exactly once"
    );
    let plugin_lsp_registry = Arc::new(
        lsp::LspRegistry::new(Arc::new(platform_posix::PosixLspTransport::new()))
            .with_diagnostics(lsp_diagnostics.clone()),
    );

    // (5.5b) Decorate the subagent spawner so AgentTool's `run_in_background`
    // path is LIVE: `spawn_async` spawns a PERSISTENT LocalAgent through the
    // registry, registers its mailbox on the shared router, and starts the
    // mailbox→runner pump (the `registerAsyncAgent` lifecycle). Built HERE —
    // after `task_registry` exists — so NO deferred cell is needed; the
    // one-shot / teammate / workflow handlers keep the raw spawner captured
    // earlier (they only use the sync `spawn`, which the decorator delegates).
    let teammate_spawner = if platform_api::env::agent_swarms_enabled() {
        let spawner = Arc::new(coordinator::ImplicitTeammateSpawner::new(
            coordinator.clone(),
            spawn_seam.clone(),
            Arc::new(PosixRuntime::new()),
            output.clone(),
            main_session_id.to_string(),
        ));
        spawner.initialize().await;
        Some(spawner)
    } else {
        None
    };
    let subagent_spawner: Arc<dyn platform_api::subagent_spawn::SubagentSpawner> =
        Arc::new(background_agent::BackgroundAgentSpawner {
            inner: subagent_spawner,
            teammate_spawner,
            registry: task_registry.clone(),
            mailbox_router: coordinator.mailbox_router.clone(),
            runtime: Arc::new(PosixRuntime::new()) as Arc<dyn platform_api::RuntimeSpawner>,
            // Where a forked skill's scoping sidecars land — beside the
            // background agent's own transcript in this session's
            // `subagents/` directory.
            subagents_dir: Some(main_subagents_dir.clone()),
        });

    // (/sandbox) One shared fast-toggle cell, seeded from the config's
    // `enabled` flag (read BEFORE `sandbox_runtime_cfg` is moved into the
    // literal below). It is cloned into BOTH the bash tool's
    // `sandbox_enabled_override` (read per-command via
    // `effective_sandbox_runtime`) AND the TUI's `/sandbox` handle, so a live
    // toggle flips sandboxing for the session's next command. Whether
    // sandboxing physically engages still rides platform support (unchanged),
    // exactly as the config `enabled` flag does today.
    let sandbox_toggle = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
        sandbox_runtime_cfg.enabled,
    ));
    // (`/sandbox` description fidelity) Capture the static config flags the TUI's
    // dynamic `/sandbox` description renders (claude-code `t`/`r`/`o`) BEFORE the
    // config is moved into `tool_ctx` below. `deps_ok` = claude-code
    // `checkDependencies().errors.length === 0` (the same `sandbox_deps` computed
    // above drives `sandbox_available`); `false` → the warning glyph. `managed`
    // (policy-lock) is still not modeled (default off), a documented residual.
    let sandbox_desc_auto_allow = sandbox_runtime_cfg.auto_allow_bash_if_sandboxed;
    let sandbox_desc_fallback = sandbox_runtime_cfg.are_unsandboxed_commands_allowed();
    let sandbox_desc_deps_ok = sandbox_deps.errors.is_empty();
    // File-tool trusted dirs = cwd FIRST, then every additional working dir
    // (settings `additionalDirectories` + `--add-dir`), expanded and deduped.
    // claude-code allows file tools (Read/Edit/Write/Glob/Grep/NotebookEdit)
    // inside `additionalWorkingDirectories`; without this they hard-error on any
    // `--add-dir` path (parity 2.1.207 P1-08).
    let trusted_dirs = {
        let home = dirs::home_dir();
        let mut dirs_vec = vec![cwd.clone()];
        for raw in &boot_additional_working_dirs {
            let expanded = expand_trusted_dir(raw, &cwd, home.as_deref());
            if !dirs_vec.contains(&expanded) {
                dirs_vec.push(expanded);
            }
        }
        dirs_vec
    };
    // Bound (not inlined) so the SAME `Arc<SessionCwd>` can also be handed to
    // the orchestrator below via `.with_session_cwd(...)` — Task 5 (worktree
    // 206 session-cwd plumbing): the system prompt's `Primary working
    // directory:` line and the conditional-rules memory cache must see the
    // SAME cwd cell the FS/Bash tools swap on `EnterWorktree`/`ExitWorktree`,
    // not an independent, never-swapped cell. Seeded with the P1-08
    // `trusted_dirs` set (cwd + `--add-dir`/`additionalDirectories`) so the
    // file tools' `ctx.trusted_dirs()` gate keeps allowing the additional dirs.
    let session_cwd = SessionCwd::new(cwd.clone(), trusted_dirs);
    let _ = teammate_session_cwd_cell.set(session_cwd.clone());
    // Local IDE endpoints are discovered lazily by the provider-neutral
    // controller. The same MCP registry owns their live JSON-RPC connection;
    // no Anthropic credential or cloud auth path participates here.
    let ide_handle: Arc<dyn platform_api::IdeHandle> = Arc::new(DesktopIdeHandle::new(
        cfg.lingxi_home.join("ide"),
        mcp_registry.clone(),
    ));
    // Handle the runtime `/add-dir` live effect needs to widen the file-tool
    // trusted set (the same `Arc<SessionCwd>` is moved into the orchestrator
    // builder below via `with_session_cwd`). P1-08.
    let runtime_session_cwd = session_cwd.clone();
    // (parity 2.1.212) Restore a persisted active worktree on --continue/--resume.
    // The `worktree_session` cell (written by EnterWorktree, cleared by
    // ExitWorktree) is in-memory only; on a cold resume it starts `None`, so
    // ExitWorktree would take its no-op path ("No-op: there is no active
    // EnterWorktree session to exit") even for a session that was inside a
    // worktree. Read the last `worktree-state` transcript record for this session
    // and, when it names a worktree still on disk, rehydrate the cell + move the
    // session into it (`session_cwd` swap) — the Rust analog of claude's
    // `restoreWorktreeSession`/`Z_t`. A missing worktree dir (removed since)
    // restores nothing, so the session stays out of the worktree and ExitWorktree
    // correctly no-ops — mirroring `Z_t`'s chdir-failure `gne(null)` guard.
    // (The upstream-reset optimization `[worktree] reset resumed worktree` and the
    // cross-project `tengu_resume_worktree_fallback` search are deferred
    // refinements — not needed to close the ExitWorktree-after-resume no-op.)
    let worktree_session_cell = tool_api::worktree_session::new_worktree_session_cell();
    if cfg.session_id_override.is_some() {
        let restore_fs: Arc<dyn platform_api::FileSystem> =
            Arc::new(PosixFileSystem::new(cwd.clone()));
        if let Some(payload) = session::jsonl::loader::read_worktree_state(
            &main_transcript_path,
            restore_fs,
            &main_session_uuid,
        )
        .await
        {
            if let Some(restored) = tool_api::WorktreeSession::from_persisted_json(&payload) {
                if restored.worktree_path.is_dir() {
                    // Move the session into the worktree exactly as EnterWorktree's
                    // own swap does (worktree path as the sole trusted dir).
                    session_cwd.swap(
                        restored.worktree_path.clone(),
                        vec![restored.worktree_path.clone()],
                    );
                    *worktree_session_cell
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(restored);
                }
            }
        }
    }
    // P1-06: ONE per-session read-file-state registry (claude-code's single
    // `readFileState` map on the `ToolUseContext`). Created here, cloned into
    // every file tool's `BuiltinToolContext` below, and the SAME `Arc` handed to
    // the orchestrator via `.with_read_state_map(...)` at the builder chain, so a
    // tool's `readFileState.set` feeds the orchestrator's post-compact restore
    // (and the staleness / `/files` consumers).
    let read_state_map = tool_api::read_file_state::new_read_file_state_map();
    let ask_user_question_timeout = load_ask_user_question_timeout(&cfg).await;
    // The device-audio capability, read ONCE here so the tool context below and
    // the `DesktopRuntime` returned at the end carry the very same `Arc`s (the
    // bridge's one `AudioBridge`), not two independent reads of `cfg` that a
    // later edit could let drift apart.
    let desktop_audio = cfg.audio.clone();
    let tool_ctx = BuiltinToolContext {
        // The live session, so tools that persist oversized output can write to
        // claude-code's session-scoped `<projects>/<session-id>/tool-results/`
        // instead of dropping artifacts inside the user's repository. Same
        // `main_session_id` the orchestrator gets via `.with_session_id`, so the
        // tool-results dir sits beside this session's transcript.
        session_id: Some(main_session_id),
        // FILE.B / P1-06: file tools share the ONE per-session read-state map
        // (staleness guard, Read-dedup) — the SAME `Arc` the orchestrator adopts
        // via `.with_read_state_map(read_state_map)` below.
        read_file_state: read_state_map.clone(),
        // Read(deny) → Grep/Glob search excludes (resolved from the boot policy
        // above; empty when enforcement is off or no Read-deny rule applies).
        read_deny_exclude_globs,
        fs: Arc::new(PosixFileSystem::new(cwd.clone())),
        bus: Arc::new(telemetry::AnalyticsBus::new()),
        // P1-08 trusted dirs now live inside `session_cwd` (built above from the
        // `trusted_dirs` set); `BuiltinToolContext` no longer has a standalone
        // `trusted_dirs` field — tools read `ctx.trusted_dirs()` off `session_cwd`.
        process: Arc::new(PosixProcess::new()),
        sandbox: Arc::new(PosixSandbox::new()),
        clock: clock.clone(),
        sandbox_runtime: sandbox_runtime_cfg,
        // (/sandbox) Live-toggle cell shared with the TUI (see above).
        sandbox_enabled_override: Some(sandbox_toggle.clone()),
        // (P2-14) `settings.skipWebFetchPreflight` → WebFetch skips the
        // domain-blocklist preflight (enterprise escape hatch). Read from the
        // merged settings via the same `Settings::load` seam as outputStyle.
        skip_web_fetch_preflight: if cfg.restricted {
            effective_settings
                .as_ref()
                .and_then(|settings| settings.settings.skip_web_fetch_preflight)
                .unwrap_or(false)
        } else {
            load_merged_skip_web_fetch_preflight(&cwd)
        },
        // (M-15) `settings.askUserQuestionTimeout` → the AskUserQuestion resolver's
        // idle window. Read from the merged settings via the same `Settings::load`
        // seam; parsed into `AskUserQuestionTimeout` at `tool_ui` registration.
        ask_user_question_timeout,
        // Inject the LIVE runner: the desktop session routes its sandboxed
        // bash/powershell/skill commands through `sandbox-runtime`'s
        // `SandboxManager` (forward proxies + Linux socat bridge + MITM/seccomp),
        // rather than the legacy sync `wrap_with_sandbox`. The manager is brought
        // up lazily on the first `wrap` and reused for the session.
        //
        // Teardown is Drop-based: harness-runtime::desktop has NO per-session teardown hook
        // (see the `fire_session_start` note below — `build` returns the runtime
        // and the host drops it on process exit; there is no hook-capable shutdown
        // seam, so `reset().await` cannot be called from here). The `Arc<dyn
        // SandboxRunner>` lives inside `tool_ctx` → the tool registry → the
        // runtime; when the last `Arc` ref drops, `SandboxRuntimeRunner` drops,
        // dropping its `SandboxManager` and the owned `RunningState`. That abort
        // the proxy accept-loop tasks (`JoinHandle` aborts on drop) and drops the
        // `LinuxBridge`, whose `Drop` SIGTERMs the `socat` bridge children
        // (`sandbox-runtime/src/linux.rs:404`). The only thing the explicit
        // `SandboxManager::reset()` does that Drop does not is remove the leftover
        // Unix socket files / dispose the ephemeral MITM-CA temp dir — cosmetic
        // temp-file cleanup, not a leaked process. When a host teardown seam is
        // added (the future-batch note on `fire_session_end`), call
        // `sandbox_runner.reset().await` there for the tidy socket/CA cleanup.
        sandbox_runner: new_live_sandbox_runner_with_permission_gate(perms.clone()),
        permission_mode: cfg.permission_mode,
        // (#3 shell-expansion) The base policy for embedded `!`cmd`` bodies in
        // prompt commands (`/commit` …). When enforcement is on this is the SAME
        // boot `Arc<PermissionPolicy>` the model-facing `PolicyPermissionGate`
        // enforces (captured above). When enforcement is off no boot policy was
        // built, so fall back to a Default-mode policy WITH roots (from `cwd`) so
        // read-only auto-allow AND per-command allowed-tools injection still
        // content-match — 1:1 with claude-code, which runs the orchestrator gate
        // regardless of any local enforcement toggle.
        permission_policy: boot_permission_policy.clone().unwrap_or_else(|| {
            Arc::new(
                permission::PermissionPolicy::new(cfg.permission_mode)
                    // Same TS formula as the enforced boot policy above; no
                    // settings were read on this path, so no killswitch term.
                    .with_bypass_available(
                        cfg.permission_mode == permission::PermissionMode::BypassPermissions
                            || cfg.allow_dangerously_skip_permissions,
                    )
                    // `zj`'s `!Ae()`; this fallback path has no settings read,
                    // so it takes the launch kind straight from the config.
                    .with_interactive_session(cfg.session_composition().is_interactive_session())
                    .with_roots(permission::FsRoots {
                        cwd: cwd.clone(),
                        home: dirs::home_dir(),
                        lingxi_home: cfg.lingxi_home.clone(),
                    })
                    .with_pwsh_parser(std::sync::Arc::new(
                        permission::powershell_parse::SystemPwshParser,
                    ))
                    .with_plan_files(plan_files.clone()),
            )
        }),
        sandbox_available,
        session_cwd: session_cwd.clone(),
        // Worktree 206 parity (Task 8): the session record cell — normally a
        // fresh `None` (inert until `EnterWorktree` populates it), but on a
        // `--continue`/`--resume` it may have just been rehydrated from the
        // persisted `worktree-state` transcript record above (parity 2.1.212), so
        // ExitWorktree operates on the resumed worktree instead of no-oping.
        worktree_session: worktree_session_cell,
        platform: sandbox_platform,
        http: http.clone(),
        provider: tool_provider,
        default_model: orch_cfg.model.clone(),
        web_search_config: Some(Arc::new(DesktopWebSearchConfigProvider {
            lingxi_home: cfg.lingxi_home.clone(),
            credentials: credentials.clone(),
        })),
        // The SAME manager the LocalAgent handler judges background-agent
        // worktrees with (created above) — one creation/judgment surface.
        worktree: worktree_manager.clone(),
        subagent_spawner: Some(subagent_spawner.clone()),
        task_registry: Some(
            task_registry.clone() as Arc<dyn platform_api::task_registry::TaskRegistryHandle>
        ),
        mailbox_router: coordinator_mailbox,
        budget_enforcer: Some(budget_enforcer.clone()),
        main_loop_model_profile_provider: Some(Arc::new({
            let model_providers = model_providers.clone();
            move |model: &str| {
                model_providers
                    .get(model)
                    .map(|(profile, _)| profile.clone())
                    .or_else(|| {
                        model.split_once('/').and_then(|(profile, bare)| {
                            let is_bare_claude_ref = profile.starts_with("claude-");
                            (!profile.is_empty() && !bare.is_empty() && !is_bare_claude_ref)
                                .then(|| profile.to_string())
                        })
                    })
            }
        })),
        coordinator_mode: Some(coordinator_mode.clone()
            as Arc<dyn platform_api::coordinator_mode::CoordinatorModeHandle>),
        // (3b) AgentTool threads this into the subagent's RegistryToolInvoker so
        // spawned subagents are gated by the same boot gate as the main loop.
        permission_gate: Some(perms.clone()),
        // (CLI-5) PRESENCE is the gate — `None` when `y0r` says off, which is
        // the unconfigured default (its auto-mode arm needs a rollout flag that
        // defaults false, upstream included).
        bash_edit_diff: resolve_bash_edit_diff(
            &cfg,
            effective_settings.as_ref(),
            &managed_settings_for_strict,
            &main_session_uuid,
        ),
        // G14: the AgentTool registers async-agent `name → agentId` in the
        // spawner's OWN internal registry (PoolSubagentSpawner::register_name),
        // so no separate ctx-level registry is wired here. A shared
        // `Some(Arc<dyn AgentNameRegistry>)` can be threaded once a SendMessage
        // resolver needs to read the same map outside the spawner.
        agent_name_registry: None,
        mcp_registry: Some(mcp_registry.clone()),
        lsp_registry: Some(plugin_lsp_registry.clone()),
        camera: None,
        // Device audio. Only bridge connections inject the one service; tools
        // and host-managed voice handles share it.
        audio: desktop_audio.as_ref().map(|audio| audio.service.clone()),
        audio_recording_handles: Arc::new(
            tokio::sync::Mutex::new(std::collections::HashMap::new()),
        ),
        share: None,
        notifications: None,
        clipboard: None,
        computer_control: platform_macos_computer_control::new_if_supported(),
        android_shell: None,
        android_git: None,
        android_git_secret: None,
        // BLOCKING TaskCreated/TaskCompleted hooks for the V2 Task* tool path
        // (claude-code `executeTaskCreatedHooks` / `executeTaskCompletedHooks`).
        // Wraps the SAME `Arc<HookExecutorImpl>` + cwd the registry firers use
        // (the fire-and-forget `OrchestratorTaskCreated/CompletedFirer` injected
        // into the `TaskRegistry` above), but REPORTS a Block decision so the
        // tool can roll back creation / refuse a completion. Separate seam — the
        // registry firers' observe-only contract is unchanged.
        task_lifecycle_hooks: Some(Arc::new(
            orchestrator::OrchestratorTaskLifecycleHookFirer::new(
                hooks.clone(),
                cwd.clone(),
                main_transcript_path.clone(),
            ),
        )),
    };
    // (worktree-tmux-launch plan, Task 3) `-w`/`--worktree [name]` boot
    // launch: create + enter a git worktree BEFORE anything downstream reads
    // `tool_ctx.session_cwd`/`tool_ctx.worktree_session` (the orchestrator's
    // `.with_session_cwd(session_cwd)` below shares the SAME `Arc`, and the
    // system prompt / conditional-rules cache re-derive from it per-turn, so
    // the exact position of this call relative to that wiring is immaterial
    // — only that it happens before any tool call, which it trivially does
    // here at boot). INERT INVARIANT: `cfg.worktree_launch == None` (every
    // caller except a CLI session with `-w`/`--worktree` set) is a no-op — no
    // create, no swap, `worktree_session` stays the fresh `None` set above —
    // so boot is byte-identical to before this field existed. A `--worktree`
    // that cannot be created is a HARD boot failure, not a silent degrade to
    // the plain cwd — the user explicitly asked for an isolated worktree.
    // (Task 4) `cfg.tmux_launch` additionally creates a detached tmux session
    // for that worktree — independently inert when `None` (see the function
    // doc); a tmux failure is logged, not a hard boot failure.
    apply_worktree_launch(&cfg.worktree_launch, &cfg.tmux_launch, &tool_ctx).await?;
    let coordinator_wiring = (platform_api::env::agent_swarms_enabled()
        || cfg.session_started_as_coordinator)
        .then(|| CoordinatorWiring {
            team: coordinator.clone(),
            spawn_seam: spawn_seam.clone(),
        });
    // (5.5b) MCP-invocation Batch 3: expose each Connected server's tools by
    //        their real `mcp__<server>__<tool>` FQN as individual wire entries
    //        (server `inputSchema` + truncated description), routed back to that
    //        connection's `McpClient::call_tool`. Mirrors claude-code's
    //        `fetchToolsForClient` per-tool `Tool` (services/mcp/client.ts:1766-1990).
    //
    //        The MCP partition supports live replacement after the registry is
    //        Arc-wrapped, so an inbound `notifications/tools/list_changed` can
    //        update the next model request without rebuilding the builtin pool.
    //        `tool_ctx` is consumed by `register_desktop_tools`, so the builder
    //        gets a clone taken first.
    let mcp_tool_ctx = tool_ctx.clone();
    // (`!` bash mode) Clone the session tool context for the TUI's sandboxed Bash
    // runner BEFORE `tool_ctx` is moved into `register_desktop_tools` below. The
    // runner builds a `tool_shell::BashTool` over this exact context, so a typed
    // `!command` runs through the SAME sandbox path as a model-issued Bash call.
    let bash_runner: Arc<dyn tool_api::bash_runner::BashRunner> = Arc::new(DesktopBashRunner {
        ctx: tool_ctx.clone(),
    });
    // (#3 shell-expansion) Build the shared prompt shell-expansion provider from
    // the SAME `tool_ctx` (carrying the base `permission_policy` + sandbox/process
    // seams) BEFORE `tool_ctx` is moved into `register_desktop_tools` below. One
    // `Arc<dyn ShellExpansionProvider>` is chained onto the dispatcher (so
    // `/commit` … expand their embedded `!`git …`` bodies) AND stashed on
    // `DesktopRuntime.shell_expansion` for the ratatui TUI's `run_core_command`.
    let shell_expansion_provider = tool_skill::build_prompt_shell_provider(&tool_ctx);
    let mut tools_inner = ToolRegistry::new();
    // `RemoteTrigger`'s in-process OAuth resolver, backed by the credential
    // store built at (3). Reads tokens at call-time so the refresh driver wired
    // at (3.1) is always reflected.
    let cron_auth: Arc<dyn tool_cron::ClaudeAiAuthProvider> =
        Arc::new(CredentialStoreAuthProvider {
            credentials: credentials.clone(),
            base_api_url: cfg.api_base.clone(),
        });
    // SKILLEXEC.2: break the Skill-tool ↔ CommandRegistry init cycle. The tool
    // registry (which holds the `Skill` tool) must exist before the orchestrator,
    // and the command registry needs the orchestrator handle — so build the
    // shared command-registry slot now (empty), hand a `CommandRegistry`-backed
    // loader to the `Skill` tool, then FILL the same `Arc` with the real registry
    // at (6) and reuse it for the slash dispatcher. No skill call can fire before
    // `build()` returns, so the loader never reads the empty registry.
    let shared_command_registry: Arc<RwLock<CommandRegistry>> =
        Arc::new(RwLock::new(CommandRegistry::new()));
    let repo_root_reloader = Arc::new(DesktopRepoRootReloader::new(
        shared_command_registry.clone(),
        cfg.cwd.clone(),
        cfg.lingxi_home.clone(),
        cfg.customization_gates.safe_mode,
    ));
    // Bind the forked-skill resume gate's skill resolver to the SAME `Arc` that
    // is filled with the real registry at (6). Binding the slot (not its
    // contents) is what makes the deferral safe: the gate reads through it at
    // resume time, long after it is populated.
    fork_capable_skills.bind(shared_command_registry.clone());
    let plugin_output_style_registry =
        Arc::new(RwLock::new(outputstyles::OutputStyleRegistry::new()));
    // SKILLEXEC: the per-session id stamped onto every resolved skill descriptor
    // so the `Skill` tool substitutes `${LINGXI_SESSION_ID}` in the body (TS
    // `getSessionId()`, a per-process session value). Generated once here at build
    // time; format mirrors the engine's `SessionId` Display (`sess:<uuid>`).
    let skill_session_id = protocol::SessionId::new().to_string();
    let monitor_runtime = repo_root_reloader.clone();
    let skill_invocation_observer: command_api::SkillInvocationObserver = Arc::new(move |skill| {
        let monitor_runtime = monitor_runtime.clone();
        Box::pin(async move {
            let runtime = monitor_runtime.plugin_runtime.read().await.clone();
            if let Some(runtime) = runtime {
                let _ = runtime.manager.activate_skill_monitors(&skill).await;
            }
        })
    });
    let skill_loader: Arc<dyn tool_skill::skill::SkillLoader> = Arc::new(
        skill_loader::CommandRegistrySkillLoader::with_session_id(
            shared_command_registry.clone(),
            skill_session_id.clone(),
        )
        .with_invocation_observer(skill_invocation_observer.clone()),
    );
    // G5: fill the subagent spawner's skill-loader cell with a
    // `platform_api::skill_loader::SkillLoader` over the SAME shared command registry,
    // so a child agent runner can preload its frontmatter `skills:` (claude
    // runAgent.ts:577-646). First fill wins; the registry is filled at (6) before
    // any spawn fires, so the loader never reads the empty registry.
    let skill_loader_arc: Arc<dyn platform_api::skill_loader::SkillLoader> = Arc::new(
        agent_skill_loader::AgentSkillLoader::new(
            shared_command_registry.clone(),
            Some(skill_session_id),
        )
        .with_prompt_cwd(tool_ctx.session_cwd.clone())
        .with_shell_expansion(tool_skill::build_prompt_shell_provider(&tool_ctx)),
    );
    let _ = subagent_skill_loader_cell.set(skill_loader_arc.clone());
    // Same skills-preload loader for the in-process teammate (full parity).
    let _ = teammate_skill_loader_cell.set(skill_loader_arc);
    // Fire the `CwdChanged` hook (claude-code `onCwdChangedForHooks`,
    // Shell.ts:409) when a `cd` inside a Bash call moves the persistent shell
    // cwd. The firer wraps the SAME `Arc<HookExecutorImpl>` the orchestrator
    // fires its other hooks through (mirrors the `TaskCreated` / `TaskCompleted`
    // firers), so the `tool-shell` leaf reaches `orch.hooks` without a dependency
    // cycle. Injected here (the desktop composition root) only — `harness-runtime::mobile`
    // never registers the shell tools, so the mobile path keeps the no-firer
    // BashTool.
    // Shared mutable-cwd cell: the firer writes the new dir on each Bash `cd`,
    // and the orchestrator (below, via `with_current_cwd`) reads it for hook
    // payloads — so a PreToolUse/PostToolUse/lifecycle hook sees the post-`cd`
    // directory, 1:1 with claude-code's single global `getCwd()`/`setCwdState`.
    let current_cwd_cell = std::sync::Arc::new(std::sync::Mutex::new(cwd.clone()));
    // Mirror worktree enter/exit swaps into this shared live-cwd cell so a
    // file/Glob/Grep read *between* a `session_cwd.swap(..)` and the next Bash
    // call (which re-points its own copy) sees the post-swap cwd, not the stale
    // pre-swap one. Bash still owns intra-turn `cd` updates to the same cell.
    session_cwd.link_live_cwd(current_cwd_cell.clone());
    // Clone for the Stop/SubagentStop hook snapshot provider (it locates the
    // project-root cron file via the live cwd); the original cell is moved into
    // `.with_current_cwd(...)` below.
    // Watcher-rebind half of claude-code's `onCwdChanged`: a late-bound rebinder
    // handed to the `CwdChanged` firer NOW, its inner cell filled after the
    // file-changed watcher spawns below (the firer is built before the watcher).
    // On a mid-session `cd` the firer signals this to re-resolve the `FileChanged`
    // matchers against the new cwd and restart. Stays a no-op when no watcher
    // spawns (no `FileChanged` hooks). The clone the firer holds shares the same
    // cell as `file_changed_watcher_rebinder`, so the later `set` reaches it.
    let file_changed_watcher_rebinder = file_changed_watch::DeferredWatcherRebinder::new();
    let cwd_changed_firer: hooks::OptionalCwdChangedFirer = Some(Arc::new(
        orchestrator::OrchestratorCwdChangedFirer::new(
            hooks.clone(),
            cwd.clone(),
            main_transcript_path.clone(),
            current_cwd_cell.clone(),
        )
        .with_watcher_rebinder(Arc::new(file_changed_watcher_rebinder.clone())),
    ));
    // (parity 2.1.212) The worktree-state persister: a JSONL writer at the SAME
    // `main_transcript_path` the orchestrator persists messages to, so an
    // `EnterWorktree`/`ExitWorktree` `worktree-state` record lands in the one
    // session `<uuid>.jsonl` the resume loader reads. Gated on
    // `session_persistence` (no writer ⇒ nothing to resume from), mirroring the
    // `main_jsonl_writer` wiring below.
    let worktree_state_persister: Option<Arc<dyn tool_api::WorktreeStatePersister>> = if cfg
        .session_persistence
    {
        Some(Arc::new(JsonlWorktreeStatePersister {
            writer: Arc::new(session::jsonl::writer::JsonlWriter::new(
                main_transcript_path.clone(),
                Arc::new(PosixFileSystem::new(cwd.clone())) as Arc<dyn platform_api::FileSystem>,
            )),
            session_uuid: main_session_uuid.clone(),
        }))
    } else {
        None
    };
    // Keep a slash-command façade over the SAME context + persister before the
    // tool registry consumes `tool_ctx`. The handler itself is registered only
    // in the desktop command registry below, leaving the locked upstream
    // command-api builtin table untouched.
    let worktree_command_handler: Arc<dyn BuiltinCommandHandler> = Arc::new(
        DesktopWorktreeCommandHandler::new(tool_ctx.clone(), worktree_state_persister.clone()),
    );
    let cron_command_handler: Arc<dyn BuiltinCommandHandler> = Arc::new(
        cron_command::DesktopCronCommandHandler::new(tool_ctx.clone()),
    );
    // The wakeup cell for the registered `ScheduleWakeup` tool — surfaced on
    // `DesktopRuntime` so the bridge composition root fills it once the
    // per-connection queue + spawner exist (`boot::assemble`).
    let ask_user_question_resolver = cfg.ask_user_question_tx.clone().map(|tx| {
        Arc::new(tool_ui::ask_user_question::TuiBridgeResolver::new(
            tool_ui::ask_user_question::AskUserQuestionTimeout::parse_or_default(
                tool_ctx.ask_user_question_timeout.as_deref(),
            ),
            tx,
        )) as Arc<dyn tool_ui::ask_user_question::AskUserQuestionResolver>
    });
    let advertise_ask_user_question = ask_user_question_resolver.is_some();
    let computer_access_resolver = cfg.computer_access_tx.clone().map(|tx| {
        Arc::new(tool_computer_use::TuiBridgeResolver::new(tx))
            as Arc<dyn tool_computer_use::ComputerAccessResolver>
    });
    let (wakeup_scheduler_cell, loop_wakeup_armed) = register_desktop_tools_with_fusion_recorder(
        &mut tools_inner,
        tool_ctx,
        coordinator_wiring,
        ask_user_question_resolver,
        advertise_ask_user_question,
        computer_access_resolver,
        Some(cron_auth),
        Some(skill_loader),
        cwd_changed_firer,
        Some(side_query_client.clone()),
        // (P2-08) The SAME shared live-cwd cell the `CwdChanged` firer and the
        // orchestrator (`.with_current_cwd`) hold, so the desktop `BashTool` is
        // the single writer of the live cwd Read/Glob/Grep + LSP read.
        Some(current_cwd_cell.clone()),
        worktree_state_persister,
        Some(fusion_executor.clone()),
        Some(fusion_recorder.clone()),
        Some(fusion_recorder_factory.clone()),
    );
    // Workflow tool (desktop-only — it fans out subagents). Registered here,
    // after `register_desktop_tools`, because its launcher needs `task_registry`
    // (constructed above): `Workflow.call` spawns a `LocalWorkflow` background
    // task through it and returns `{status:"async_launched", taskId, taskType}`.
    let dynamic_workflows_gate;
    let workflow_size_guideline_state;
    #[cfg(test)]
    let wired_workflow_tool: Option<Arc<tool_workflow::WorkflowTool>>;
    {
        let workflow_launcher: Arc<dyn tool_workflow::WorkflowLauncher> =
            Arc::new(TaskRegistryWorkflowLauncher {
                registry: task_registry.clone(),
                project_cwd: cwd.clone(),
                current_cwd: current_cwd_cell.clone(),
                lingxi_home: cfg.lingxi_home.clone(),
                session_uuid: main_session_uuid.clone(),
                default_model_selection_provider: subagent_default_model_selection_provider_cell
                    .clone(),
                plugin_workflows: plugin_workflow_registry.clone(),
            });
        // Resolve the workflow-size setting through the canonical settings
        // composition: default → user → project → local → flag → managed.
        // `workflowSizeGuideline` has no environment mapping, so managed is the
        // effective highest source. The default is medium.
        let managed_workflow_layers: Vec<lingxi_core::settings::SettingsJson> =
            crate::desktop::settings_watch::managed_settings_raw_tiers()
                .await
                .into_iter()
                .filter_map(|raw| serde_json::from_str(&raw).ok())
                .collect();
        let (workflow_size_guideline, managed_workflow, default_workflow) =
            resolve_workflow_size_guideline(&cfg, &cwd, &managed_workflow_layers);
        workflow_size_guideline_state =
            platform_api::session_flags::WorkflowSizeGuidelineState::new(
                workflow_size_guideline.as_wire(),
                managed_workflow,
                default_workflow,
            )
            .expect("desktop workflowSizeGuideline must be valid");
        let (workflow_session_enabled, workflow_session_managed) =
            resolve_workflow_session_enabled(&cfg, &cwd, &managed_workflow_layers);
        // `disableWorkflows` is an ORG policy, so it is read from MANAGED
        // settings only — a project or user file must not be able to turn the
        // tool off on the org's behalf, nor to turn it back on.
        let managed_disable_workflows = std::fs::read_to_string(
            crate::desktop::settings_watch::managed_settings_dir().join("managed-settings.json"),
        )
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| {
            v.get("disableWorkflows")
                .and_then(serde_json::Value::as_bool)
        })
        .unwrap_or(false);
        let workflow_policy_enabled = tool_workflow::workflows_enabled(managed_disable_workflows);
        dynamic_workflows_gate = platform_api::session_flags::DynamicWorkflowsGate::new(
            workflow_policy_enabled && workflow_session_enabled,
            workflow_session_managed || !workflow_policy_enabled,
        );
        let mut workflow_tool = tool_workflow::WorkflowTool::new(Some(workflow_launcher))
            .with_current_cwd(current_cwd_cell.clone())
            .with_size_guideline_state(workflow_size_guideline_state.clone())
            .with_size_guideline_source(workflow_size_guideline, managed_workflow, default_workflow)
            .with_disable_workflows(managed_disable_workflows)
            .with_dynamic_workflows_gate(dynamic_workflows_gate.clone())
            .with_session_enabled(workflow_session_enabled)
            // §14 — the SAME registry the launcher above holds, so
            // `validate_input` and `launch` agree on what resolves.
            .with_plugin_workflows(plugin_workflow_registry.clone())
            // Route the nested `Read` check through the same enforcing gate as
            // the main turn, including its interactive/headless behavior.
            .with_permission_gate(perms.clone());
        if let Some(policy) = boot_permission_policy.clone() {
            workflow_tool = workflow_tool.with_permission_policy(policy);
        }
        let workflow_tool = Arc::new(workflow_tool);
        #[cfg(test)]
        {
            wired_workflow_tool = Some(workflow_tool.clone());
        }
        tools_inner.register_builtin(workflow_tool);
    }
    let registered_mcp_tools =
        tool_mcp::build_registered_mcp_tools(&mcp_registry, mcp_tool_ctx.clone()).await;
    for (conn_id, mcp_tools) in registered_mcp_tools {
        tools_inner.register_mcp_tools(conn_id, mcp_tools);
    }
    // Structured output (`--json-schema`): register the forced `StructuredOutput`
    // tool whose `input_schema` IS the user schema; its `call` captures the model's
    // result into `structured_output_slot` for the print path to validate + retry.
    // `None` (no `--json-schema`) leaves the registry + the slot untouched.
    let structured_output_slot: Option<orchestrator::structured_output::StructuredOutputSlot> =
        cfg.json_schema.as_ref().map(|schema| {
            let slot: orchestrator::structured_output::StructuredOutputSlot =
                Arc::new(std::sync::Mutex::new(None));
            tools_inner.register_builtin(Arc::new(
                orchestrator::structured_output::StructuredOutputTool::new(
                    schema.clone(),
                    slot.clone(),
                ),
            ));
            slot
        });

    // EndConversation (2.1.206): register the tool + create the shared
    // end-request slot when the feature is enabled. claude-code
    // `isEndConversationToolEnabled` (`NWn`) = `modelMeetsEndConversationFloor`
    // (`fJc`, the `LJh` version table) AND the `tengu_umber_kestrel` GB flag.
    // The LingXi equivalent of the GB flag — like the memdir-prefetch gate above
    // — is a default-OFF env flag; the model floor is enforced faithfully via
    // `meets_end_conversation_floor`. Either half failing (default: env unset)
    // leaves the registry + turn loop byte-identical
    // (`end_conversation_slot.is_none()`).
    let end_conversation_enabled = is_env_truthy("LINGXI_END_CONVERSATION")
        && orchestrator::prompt::end_conversation::meets_end_conversation_floor(&orch_cfg.model);
    let end_conversation_slot: Option<orchestrator::end_conversation_tool::EndConversationSlot> =
        end_conversation_enabled.then(|| {
            let slot: orchestrator::end_conversation_tool::EndConversationSlot =
                Arc::new(std::sync::atomic::AtomicBool::new(false));
            tools_inner.register_builtin(Arc::new(
                orchestrator::end_conversation_tool::EndConversationTool::new(true, slot.clone()),
            ));
            slot
        });

    if cfg.restricted {
        tools_inner.set_restricted_builtin_filter(cfg.restricted_tools.as_deref());
    }

    // Tool Search (2.1.207): now that the registry is fully assembled (builtins
    // + workflow + MCP + structured-output + end-conversation), publish the
    // DEFERRED tool set to `ToolSearch`'s live view cell. When tool search is
    // disabled (the default) the deferred set is empty, so this leaves the view
    // empty — the correct behavior — and the wire stays byte-identical.
    tools_inner.refresh_tool_search_view();

    let tools = Arc::new(tools_inner);
    // Oracle `kq` — publish the read-auto-allow probe now that BOTH inputs
    // exist: the policy, and the FINAL tool list. Earlier means an unknown tool
    // list (which the probe answers `false` for); later means after the file
    // tools can already run. With no policy there is nothing to evaluate, so
    // the probe stays unpublished and `read_auto_allowed` keeps answering
    // `false` — the fail-safe answer.
    if let Some(policy) = boot_permission_policy.clone() {
        platform_api::read_auto_allow::set_read_auto_allow_probe(std::sync::Arc::new(
            permission::read_auto_allow::PolicyReadAutoAllow::new(policy, tools.all_names()),
        ));
    }
    // The SAME registry the orchestrator dispatches through, kept for
    // `DesktopRuntime::tools` (the orchestrator takes ownership below).
    let runtime_tools = tools.clone();

    // Start the perpetual reconnect owner only after every fallible build step
    // has completed. A detached reconnect loop spawned before a later `?` /
    // policy rejection would otherwise retain the half-built MCP/hooks/pool
    // graph with no `DesktopSessionLifecycle` available to stop it.
    let mcp_reconnect_task = tokio::spawn(Arc::clone(&mcp_registry).run_reconnect_loop());

    // MCP servers can mutate their tool/prompt/resource catalogs while the
    // session is running. Refresh the registry snapshot on every generation-
    // checked notification; only a successful tools/list replaces the live
    // ToolRegistry partition, so transient RPC failures keep the last-known
    // tools available (the Claude Code behavior).
    //
    // Capture the MCP registry weakly. A strong Arc here would keep its own
    // broadcast sender alive forever and prevent this task from terminating
    // when the desktop runtime is dropped.
    let mcp_catalog_refresh_task = {
        let mcp_registry_weak = Arc::downgrade(&mcp_registry);
        let live_tools = tools.clone();
        let live_mcp_tool_ctx = mcp_tool_ctx.clone();
        tokio::spawn(async move {
            let mut recovery = std::collections::VecDeque::new();
            loop {
                let change = if let Some(change) = recovery.pop_front() {
                    change
                } else {
                    match mcp_catalog_changes.recv().await {
                        Ok(change) => change,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            let Some(registry) = mcp_registry_weak.upgrade() else {
                                break;
                            };
                            tracing::warn!(
                                target: "lingxi_harness_runtime::desktop::mcp",
                                skipped,
                                "MCP catalog refresh receiver lagged; refreshing every connected catalog"
                            );
                            let refreshed = tool_mcp::build_registered_mcp_tools(
                                registry.as_ref(),
                                live_mcp_tool_ctx.clone(),
                            )
                            .await;
                            live_tools.replace_mcp_tools(refreshed);
                            recovery.extend(registry.catalog_refresh_snapshot().await);
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                };
                let Some(registry) = mcp_registry_weak.upgrade() else {
                    break;
                };

                if change.retired_connection_id.is_some() {
                    let refreshed = tool_mcp::build_registered_mcp_tools(
                        registry.as_ref(),
                        live_mcp_tool_ctx.clone(),
                    )
                    .await;
                    live_tools.replace_mcp_tools(refreshed);
                }

                tracing::debug!(
                    target: "lingxi_harness_runtime::desktop::mcp",
                    server = %change.server_name,
                    catalog = ?change.kind,
                    "Received MCP list_changed notification, refreshing catalog"
                );
                match registry.refresh_catalog(&change).await {
                    Ok(Some(_)) if change.kind == mcp::McpCatalogKind::Tools => {
                        let refreshed = tool_mcp::build_registered_mcp_tools(
                            registry.as_ref(),
                            live_mcp_tool_ctx.clone(),
                        )
                        .await;
                        live_tools.replace_mcp_tools(refreshed);
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::warn!(
                            target: "lingxi_harness_runtime::desktop::mcp",
                            server = %change.server_name,
                            catalog = ?change.kind,
                            %error,
                            "Failed to refresh MCP catalog; keeping the previous catalog"
                        );
                    }
                }
            }
        })
    };

    // (5.5a) M10 (T13): bind the teammate handler's `DeferredToolInvoker` to the
    //        real `RegistryToolInvoker` now that `tools` exists. The invoker
    //        stores the SAME `Arc<ToolRegistry>` the orchestrator owns, so a
    //        teammate's tool dispatch reuses the parent registry (recursion-lock
    //        invariant). No teammate can dispatch before `build()` returns, so
    //        the cell is always filled before first use.
    teammate_invoker.set(Arc::new(
        tool_api::tool_invoker_impl::RegistryToolInvoker::new(tools.clone())
            // (3b) Gate teammate tool dispatch with the same boot gate as the
            // main loop + subagents. Default-off `perms` is the no-op gate, so
            // this is behavior-neutral unless LINGXI_ENFORCE_PERMISSIONS is set.
            .with_gate(perms.clone()),
    ));

    // (5.5a-cron) Bind the cron `Dream` handler's `DeferredToolInvoker` to the
    //        real `RegistryToolInvoker` now that `tools` exists — same recursion-
    //        lock invariant and boot gate as the teammate invoker above.
    // Round-4 review finding [15]: a `Dream` (cron) task is never owned by
    // any interactive turn either — same reasoning as `local_workflow_invoker`
    // and `fusion_invoker` below — so a permission ask it raises must not be
    // wiped by Ctrl-C on an unrelated foreground turn.
    dream_invoker.set(Arc::new(
        tool_api::tool_invoker_impl::RegistryToolInvoker::new(tools.clone())
            .with_gate(perms.clone())
            .with_background_owned(true),
    ));

    // (5.5a-local-agent) T15: bind the `LocalAgent` handler's `DeferredToolInvoker`
    //        to the real `RegistryToolInvoker` now that `tools` exists — same
    //        recursion-lock invariant and boot gate as the teammate + dream
    //        invokers above. A `LocalAgent` task's child runner dispatches its
    //        tools through the parent registry.
    // Round-4 review finding [15]: a `LocalAgent` task (the backgrounded
    // async-agent worker) is never owned by any interactive turn either —
    // same reasoning as `local_workflow_invoker` and `fusion_invoker` below —
    // so a permission ask it raises must not be wiped by Ctrl-C on an
    // unrelated foreground turn.
    local_agent_invoker.set(Arc::new(
        tool_api::tool_invoker_impl::RegistryToolInvoker::new(tools.clone())
            .with_gate(perms.clone())
            .with_background_owned(true),
    ));

    // (5.5a-local-workflow) Bind the `LocalWorkflow` handler's `DeferredToolInvoker`
    //        to the real `RegistryToolInvoker` now that `tools` exists — so a
    //        workflow's `agent()` subagents dispatch their tools through the
    //        parent registry under the same recursion-lock + boot gate.
    //
    // Finding [23]: this is the ONE invoker every `LocalWorkflow` background
    // task dispatches through — its own script's direct tool calls, every
    // `agent()` subagent (`local_workflow.rs:3391 self.tool_invoker.clone()`,
    // optionally wrapped by `WorkspaceLeaseToolInvoker`, a transparent
    // delegate that does not touch this flag), and every workflow `fusion()`
    // panel (`local_workflow.rs:2658-2663`'s `SubagentInheritance {
    // tool_invoker: tool_invoker.clone(), .. }`, the SAME clone). No
    // interactive turn ever owns a `LocalWorkflow` task (it is spawned as a
    // background task, mirroring `fusion_invoker` below), and this cell has
    // no other reader (`local_workflow_invoker` referenced only at its
    // creation, at `LocalWorkflowHandler::new`, and here) — so the
    // `fusion_invoker` comment's "cannot mislabel a foreground direct tool
    // call" justification holds verbatim for it too. Without this, a
    // permission ask raised by a workflow's `fusion()` panel (or its
    // `agent()` subagents) is wiped by Ctrl-C on an UNRELATED foreground
    // turn, exactly the bug `with_background_owned` was introduced to close
    // for the `/fusion` slash entrypoint.
    local_workflow_invoker.set(Arc::new(
        tool_api::tool_invoker_impl::RegistryToolInvoker::new(tools.clone())
            .with_gate(perms.clone())
            .with_background_owned(true),
    ));

    // Finding 22: this is the ONE invoker `LocalFusionHandler` uses for every
    // `/fusion` background-task panel (`register_fusion_handler` above binds
    // this exact `fusion_invoker` cell, and `tasks::handlers::local_fusion`
    // clones it verbatim into `SubagentInheritance::tool_invoker` for each
    // panel spawn — no other caller reaches this `Arc`). A FOREGROUND
    // `Agent{subagent_type:"fusion"}` call (tools/agent/src/agent.rs
    // `call_fusion`) never touches this cell either — it builds its own
    // fresh `RegistryToolInvoker` per call. So marking this instance
    // background-owned cannot mislabel a foreground direct tool call.
    fusion_invoker.set(Arc::new(
        tool_api::tool_invoker_impl::RegistryToolInvoker::new(tools.clone())
            .with_gate(perms.clone())
            .with_background_owned(true),
    ));

    // Break the subagent construction cycle now that `tools` + `agent_catalog`
    // exist: fill the spawner's set-once cells (filled before `build()` returns,
    // so before any spawn). Each spawn then resolves its advertised tools +
    // dispatch allow-list from this registry at spawn time (parity batch 20) and
    // real user/project `AgentDefinition`s from the SAME catalog `Arc` the
    // orchestrator holds — `.with_agent_catalog` below shares the lock, not a
    // copy (parity batch 21). First fill wins.
    let _ = subagent_tool_registry_cell.set(tools.clone());
    let _ = subagent_agent_catalog_cell.set(agent_catalog.clone());
    // §24b: fill the agent-MCP-tool-builder now that `mcp_registry` (7935) +
    // `mcp_tool_ctx` (8967) both exist. The closure owns clones of both plus
    // the boot-resolved strict-MCP gates (the SAME values
    // `merge_agent_frontmatter_mcp_servers` used for the main-thread agent
    // above) so every subagent Task spawn's frontmatter `mcpServers` connects
    // + builds tools through the identical `PRn` conversion.
    {
        let mcp_registry_for_agents = mcp_registry.clone();
        let mcp_tool_ctx_for_agents = mcp_tool_ctx.clone();
        let _ = subagent_mcp_tool_builder_cell.set(Arc::new(move |agent_id, def, lease| {
            let mcp_registry = mcp_registry_for_agents.clone();
            let mcp_tool_ctx = mcp_tool_ctx_for_agents.clone();
            Box::pin(build_agent_mcp_tool_set(
                mcp_registry,
                mcp_tool_ctx,
                strict_plugin_only_mcp,
                cfg.strict_mcp_config,
                agent_id,
                def,
                lease,
            ))
                as std::pin::Pin<
                    Box<
                        dyn std::future::Future<Output = agent::agent_mcp_tools::AgentMcpToolSet>
                            + Send,
                    >,
                >
        }));
    }
    let profile_first_party_for_subagents = profile_first_party.clone();
    let _ = subagent_provider_first_party_resolver_cell.set(Arc::new(move |profile| {
        profile_first_party_for_subagents.get(profile).copied()
    }));
    // In-process teammate full parity (P1): advertise the SAME resolved tool pool
    // + apply the SAME tool-wide deny filter as the spawner, so a teammate can
    // actually use tools (not chat-only). The deny names are copied from the
    // spawner's already-filled cell (set in the enforcement branch above; empty /
    // unfilled ⇒ no filtering).
    let _ = teammate_tool_registry_cell.set(tools.clone());
    if let Some(deny) = subagent_tool_wide_deny_cell.get() {
        let _ = teammate_tool_wide_deny_cell.set(deny.clone());
    }
    // Capture the fully-wired restore inheritance before `tools` and `perms`
    // move into the orchestrator. The actual cold restore runs later, after the
    // live model/provider selection cell is published.
    let parked_agent_restore_inheritance = cfg.session_id_override.is_some().then(|| {
        platform_api::subagent_spawn::SubagentInheritance {
            tool_invoker: Arc::new(
                tool_api::tool_invoker_impl::RegistryToolInvoker::new(tools.clone())
                    .with_gate(perms.clone()),
            ),
            budget: budget_enforcer.clone(),
        }
    });

    // Clone `cwd` for the settings watcher before it is moved into the
    // orchestrator constructor below.
    let watch_cwd = cwd.clone();
    // Snapshot the FileChanged hook matchers under ONE registry read before
    // `hook_registry` is moved into the orchestrator. A `FileChanged` hook's group `matcher`
    // (`HookDefinition::matcher()`) is the pipe-separated filename list
    // claude-code's `resolveWatchPaths` reads (`fileChangedWatcher.ts:48-65`).
    let file_changed_matchers: Vec<String> = {
        let reg = hook_registry.read().await;
        let all = reg.all_hooks();
        all.iter()
            .filter(|h| {
                h.events
                    .contains(&hooks::events::HookEventType::FileChanged)
            })
            .filter_map(|h| h.matcher().map(ToString::to_string))
            .collect()
    };
    // Build the `FileChanged` firer over the SAME `Arc<HookExecutorImpl>` the
    // orchestrator is about to take ownership of (mirrors the `cwd_changed_firer`
    // built from `hooks.clone()` at (5.5)). Captured BEFORE `hooks` is moved into
    // the orchestrator constructor below so the watcher (spawned at (7.3), after
    // `hooks` is moved) reaches `orch.hooks` without a getter. Keep this firer
    // even for an initially empty matcher set so Desktop can install the first
    // FileChanged hook without restarting the process.
    let file_changed_firer: Arc<dyn hooks::FileChangedFirer> =
        Arc::new(orchestrator::OrchestratorFileChangedFirer::new(
            hooks.clone(),
            watch_cwd.clone(),
            main_transcript_path.clone(),
        ));
    // (6.5-pre) Clone the registry Arcs the plugin bootstrap (below, after the
    //           command registry is filled at (6)) writes through, BEFORE they
    //           are moved into the orchestrator constructor. `Arc<RwLock<…>>`
    //           shares state, so plugin hooks/agents registered after the move
    //           are still observed by the orchestrator's clone.
    let plugin_hook_registry = hook_registry.clone();
    let plugin_agent_catalog = agent_catalog.clone();
    let plugin_mcp_registry = mcp_registry.clone();
    // `cwd` is moved into the orchestrator below; the plugin bootstrap's
    // sandboxed `PosixFileSystem` (a Plan-16 dead-code field on `PluginManager`)
    // needs a workspace root, so snapshot it here.
    let cwd_for_plugins = watch_cwd.clone();
    // #39 UserPromptExpansion: capture the SAME `Arc<HookExecutorImpl>` BEFORE
    // it is moved into the orchestrator, so the slash-command dispatcher (built
    // at (6), below) can fire `UserPromptExpansion` through it at command
    // expansion. The dispatcher pairs it with a context provider that reads the
    // orchestrator's live session id (`orch.expansion_hook_context()`).
    let expansion_hook_executor = hooks.clone();
    // Gap #5: PERSIST the interactive session to JSONL so `--resume` / `-c` / the
    // resume screen (all backed by `session::jsonl::loader`, which scans
    // `<config_home>/projects/<sanitize(cwd)>/<uuid>.jsonl`) can find sessions
    // this desktop/CLI TUI itself created. Prior to this the production
    // composition root wired NO `JsonlWriter` (every `with_jsonl_writer` call
    // site was a test), so the projects dir stayed empty and resume never found
    // a TUI-created session. We point the writer at the SAME `main_transcript_path`
    // (`<cfg.claude_home>/projects/<sanitize(cwd)>/<main_session_uuid>.jsonl`)
    // already computed (FIX A) for the hook payloads' `transcript_path` and the
    // leaf firers, so the on-disk transcript, the hook `transcript_path`, and the
    // orchestrator's live session id are one consistent file end-to-end. The
    // writer creates the file (mode 0o600) + project dir (mode 0o700) lazily on
    // the first append; the orchestrator's existing per-block / per-message
    // persist machinery (`persist_assistant_per_block`,
    // `persist_message_to_jsonl_with_parent`) then appends user/assistant lines
    // the loader counts as a resumable session (title falls back to the first
    // user message). `PosixFileSystem` does not confine `append_file_with_mode`
    // to its workspace root, so rooting it at `watch_cwd` is fine for a path
    // under `claude_home`.
    // `main_jsonl_writer` was created before durable session setup so Fusion
    // delivery and ordinary persistence can share its lock/active path.
    // (P2-02 cc2.1.207) `main_jsonl_writer` MOVES into the orchestrator builder
    // below (when `session_persistence`); capture a clone so the `--agent` block
    // can persist the applied `agentType` as an `agent-setting` transcript record
    // (claude `{type:"agent-setting",agentSetting,sessionId}`) for `rVe` resume
    // restoration. Same shared-`Arc` file target, so the record lands in the SAME
    // `<uuid>.jsonl` the orchestrator appends messages to.
    let main_agent_setting_writer = main_jsonl_writer.clone();
    // Desktop hosts own their trust decision; CLI callers retain disk/session
    // trust resolution. This shared value gates cron, task messages, and /goal.
    let workspace_trusted = resolve_workspace_trust(
        cfg.host_workspace_trusted,
        &cwd,
        migrations::global_config::global_config_path().as_deref(),
    );
    let goal_hooks_restricted = if cfg.restricted {
        effective_settings
            .as_ref()
            .map(|settings| {
                settings.settings.disable_all_hooks.unwrap_or(false)
                    || settings.settings.allow_managed_hooks_only.unwrap_or(false)
            })
            .unwrap_or(false)
    } else {
        load_merged_hooks_restricted(&cwd)
    };
    // Keep a clone for the settings watcher before `perms` is moved into the
    // orchestrator. The watcher must remain live even without ConfigChange
    // hooks because managed `disableAutoMode` is a safety policy, not an
    // optional notification hook.
    let settings_permission_gate = perms.clone();
    let orch_builder = ConversationOrchestrator::new_with_streaming(
        orch_cfg,
        api_client,
        streaming_api,
        tools,
        hooks,
        perms,
        output,
        memory,
        cwd,
    )
    .with_dynamic_workflows_gate(dynamic_workflows_gate);
    let orch_builder = orch_builder.with_workflow_size_guideline(workflow_size_guideline_state);
    // Gap #5: wire the production JSONL writer (constructed just above) so the
    // session is persisted + discoverable by the resume loader.
    // (M3 cc2.1.198) `--no-session-persistence` ⟶ `cfg.session_persistence:
    // false`: leave the orchestrator's `jsonl_writer` slot `None` (its persist
    // paths are already `Option`-gated) so NO transcript is written under
    // `projects/` and the session cannot be resumed.
    let orch_builder = if cfg.session_persistence {
        orch_builder.with_jsonl_writer(main_jsonl_writer)
    } else {
        orch_builder
    };
    let orch_builder = orch_builder
        // FIX A: hand the orchestrator the resolved claude-home so its hook payloads
        // carry a deterministically-computed `transcript_path`
        // (`<config_home>/projects/<sanitize(cwd)>/<uuid>.jsonl`, claude-code
        // `getTranscriptPathForSession`). This is the SAME path the Gap #5
        // `JsonlWriter` (wired just above) persists to, so the hook payload path and
        // the on-disk transcript agree. Without this every PreToolUse /
        // PostToolBatch / lifecycle hook fired with an empty path.
        // (/fast) Share the same fast-mode flag the adapter reads, so the
        // `set_fast_mode` handle flips the value the next request-build sees.
        .with_fast_mode(fast_flag.clone())
        // (/rewind) Share the file-history store so the turn loop snapshots each
        // turn + the write tools back up pre-edit content.
        .with_file_history(file_history.clone())
        .with_vision_delegation(vision_delegation_enabled)
        .with_config_home(cfg.lingxi_home.clone())
        // Share the SAME mutable-cwd cell the `cwd_changed_firer` writes on a Bash
        // `cd`, so hook payloads read the post-`cd` directory (claude-code parity).
        .with_current_cwd(current_cwd_cell)
        // Task 5 (worktree 206 session-cwd plumbing): share the SAME
        // `Arc<SessionCwd>` the tool context swaps on `EnterWorktree`/
        // `ExitWorktree`, so the system prompt's `Primary working directory:`
        // line and the conditional-rules memory cache re-derive from the
        // post-swap worktree cwd instead of the frozen boot cwd.
        .with_session_cwd(session_cwd)
        // FIX A/B/C: adopt the boot-canonical session id so the orchestrator's LIVE
        // session matches the id baked into the leaf firers' `transcript_path` and the
        // subagent spawner's subagents dir — one consistent session id end-to-end.
        .with_session_id(main_session_id)
        .with_cost_tracker(cost_tracker.clone())
        .with_observer_pairings(observer_pairings.clone())
        .with_loop_usage_opt(
            cron_scheduler
                .clone()
                .map(|scheduler| scheduler as Arc<dyn platform_api::LoopUsageProvider>),
        )
        .with_cost_session_switcher_opt(Some(session_state_manager.clone()))
        .with_session_activation_observer(Arc::new(ProcessSessionActivationObserver))
        // (review #12) Wire the /goal trust + hooks-restricted gates (resolved
        // above) into the orchestrator, replacing the hardcoded trusted=true /
        // restricted=false defaults.
        .with_workspace_trusted(workspace_trusted)
        .with_hooks_restricted(goal_hooks_restricted)
        .with_analytics_bus(analytics_bus.clone())
        .with_mcp_registry(mcp_registry.clone())
        .with_ide_handle(ide_handle.clone())
        .with_hook_registry(hook_registry.clone())
        .with_agent_catalog(agent_catalog)
        .with_repo_root_reloader(repo_root_reloader.clone())
        .with_output_style_registry(plugin_output_style_registry.clone())
        .with_compaction(compactor)
        .with_cache_safe_slot(cache_safe_slot)
        // `/fork` engine seam: hand the orchestrator the background-agent spawner
        // (`BackgroundAgentSpawner`, built above) + the budget the spawned agent
        // inherits, so `fork_conversation` can dispatch a detached background agent.
        .with_fork_spawner(subagent_spawner.clone())
        .with_fork_budget(budget_enforcer.clone())
        // `/recap` engine seam: the SAME forked runner the summarizer uses (cloned
        // above), so recap replays the identical cache-safe prefix, read-only.
        .with_recap_runner(recap_runner)
        // Surface LSP `<new-diagnostics>` to the model each turn (the same sink the
        // LSP registry drains publishDiagnostics into).
        .with_new_diagnostics_source(
            Arc::new(lsp_diagnostics.clone()) as Arc<dyn platform_api::NewDiagnosticsSource>
        )
        // SKILLLIST.1: enumerate model-invocable skills each turn so the model
        // can discover them. Reads `shared_command_registry` lazily at turn time
        // (populated below at (6), before any turn fires).
        .with_skill_listing(registry_skill_listing_provider(
            shared_command_registry.clone(),
            read_state_map.clone(),
        ))
        // B5: fold completed background (`async`) hook responses back into the
        // next turn. Backed by the completion-channel drain buffer above.
        .with_async_hook_responses(Arc::new(async_hook_response_buffer.clone()))
        // T35: fold terminal background tasks (a backgrounded `local_bash` /
        // `local_agent` / MCP `monitor` …) back into the next turn as a
        // `<task-notification>` reminder so the model learns its async task
        // finished. Backed by the SAME `TaskRegistry` Arc wired into the tool
        // context above; the provider drains the registry's terminal-not-notified
        // tasks each turn (mark-notified + evict ⇒ each completion surfaces once).
        .with_task_notifications(Arc::new(orchestrator::RegistryTaskNotifications::new(
            task_registry.clone() as Arc<dyn platform_api::task_registry::TaskRegistryHandle>,
        )))
        // hook-bg-fields: populate the `Stop` / `SubagentStop` hook payload's
        // `background_tasks` (claude-code `Lic(taskRegistry.all())`) +
        // `session_crons` (claude-code `Mic()`) from the SAME live `TaskRegistry`
        // Arc wired above plus the project-root `.lingxi/scheduled_tasks.json` cron
        // file (located via the shared `current_cwd` cell). The orchestrator stamps
        // the snapshot onto the payload ONLY at its Stop / SubagentStop firings
        // (claude's tool-use-context `s` gate).
        .with_stop_hook_snapshot(Arc::new(RegistryStopHookSnapshot {
            registry: task_registry.clone()
                as Arc<dyn platform_api::task_registry::TaskRegistryHandle>,
            project_root: watch_cwd.clone(),
        }))
        // Finding #73: supply the V2 task list to the per-turn `task_reminder`
        // (the default variant when tasks are enabled). Reads the file-backed
        // `TodoStore` for the active list each turn, resolving the list id via the
        // same env/team precedence the `Task*` tools use. V1 (`todo_reminder`)
        // needs no provider; it reads `session.todos` directly.
        .with_todo_reminder_tasks(Arc::new(orchestrator::TodoStoreReminderTasks::new()))
        // P1-06: hand the orchestrator the SAME `readFileState` map the file tools'
        // `BuiltinToolContext` share (created just above), so a tool's
        // `readFileState.set` feeds the post-compact file restore + staleness /
        // `/files` consumers — 1:1 with claude-code's single per-session map.
        .with_read_state_map(read_state_map);

    // MEM-1 ACTIVATION. The composition-root presence of the prefetch IS the
    // gate (`memory_prefetch.is_some()`), and that presence is now decided by
    // `memory::auto_memory_enabled` — the port of claude-code `dLt()`, whose
    // last statement is `return!0`, i.e. **ON by default**.
    //
    // 🚨 This used to read `LINGXI_MEMDIR_PREFETCH` and cite `tengu_moth_copse`
    // as the upstream gate. That attribution was wrong: `tengu_moth_copse`
    // (`X$()`) guards `CLAUDE_MEMORY_STORES`, a different feature. So the port
    // shipped auto-memory OFF for everyone on a mis-mapped flag.
    //
    // Costs a Haiku-class side query per turn over `side_query_client`; turn it
    // off with `autoMemoryEnabled:false` or `*_DISABLE_AUTO_MEMORY` / `*_SIMPLE`.
    let (memory_prefetch_on, session_memory_on) = resolve_memory_feature_gates(
        memory::auto_memory_enabled(
            &memory::AutoMemoryEnv::from_process_env(),
            effective_settings
                .as_ref()
                .and_then(|s| s.settings.auto_memory_enabled),
        ),
        is_env_truthy("LINGXI_SESSION_MEMORY"),
    );
    let orch_builder = match (memory_prefetch_on, dirs::home_dir()) {
        (true, Some(home)) => {
            orch_builder.with_memory_prefetch(orchestrator::prompt::build_memdir_prefetch(
                side_query_client.clone(),
                Arc::new(PosixRuntime::new()) as Arc<dyn platform_api::RuntimeSpawner>,
                &home,
                &cfg.cwd,
            ))
        }
        _ => orch_builder,
    };

    // EndConversation: hand the orchestrator the SAME end-request slot the tool
    // holds, so the turn loop can terminate on a confirmed (2nd) call. `None`
    // (feature disabled, the default) leaves the turn loop byte-identical.
    // `/loop` dynamic mode: the turn loop ends a turn whose only tool call was
    // the `ScheduleWakeup` that armed a wakeup (binary's lone-wakeup arm). Same
    // `Arc` the tool raises.
    let orch_builder = orch_builder.with_loop_wakeup_armed_slot(loop_wakeup_armed);
    let orch_builder = orch_builder
        .with_coordinator_mode(coordinator_mode.clone()
            as Arc<dyn platform_api::coordinator_mode::CoordinatorModeHandle>);
    let orch_builder = match end_conversation_slot.clone() {
        Some(slot) => orch_builder.with_end_conversation_slot(slot),
        None => orch_builder,
    };

    // 2.1.212 `/fork` (`vAd`) background-session forker seam. When the host
    // (`apps/cli`) injects one, wire it so `fork_to_background_session` copies
    // the live conversation into a new background session. `None` (default /
    // non-CLI hosts) leaves that `/fork` variant failing with a clear
    // `ActionFailed`, byte-identical to before this seam existed.
    let orch_builder = match cfg.bg_session_forker.clone() {
        Some(forker) => {
            forker.set_task_registry(task_registry.clone());
            orch_builder.with_bg_session_forker(forker)
        }
        None => orch_builder,
    };

    // EXPERIMENTAL_SKILL_SEARCH skill-discovery prefetch ACTIVATION (gated,
    // default OFF). claude-code keeps this behind `feature('EXPERIMENTAL_SKILL_SEARCH')`
    // — DCE'd out of the shipping 2.1.195 binary (every skill-search literal = 0
    // hits), so default-OFF is the correct parity state. The composition-root
    // PRESENCE of the prefetch IS the gate (`skill_discovery_prefetch.is_some()`),
    // exactly like the memory prefetch above. Wired ONLY when the flag is ON via
    // `telemetry::flag_bool` (the GrowthBook-style sync reader; empty snapshot ⇒
    // returns the `false` default by default), OR one of the env overrides is
    // truthy (`bun`-bundle `envBool(..., false)` parity). The faithful local
    // backend is a `RegistryCandidateSource` over the desktop skill set (substring
    // trigger discovery — the binary's native lexical index; AKI/Haiku backends
    // are out of scope). Unset/false ⇒ no prefetch ⇒ everything inert and the
    // locked fixtures byte-identical.
    let skill_search_on = telemetry::flag_bool("EXPERIMENTAL_SKILL_SEARCH", false)
        || is_env_truthy("CLAUDE_CODE_EXPERIMENTAL_SKILL_SEARCH")
        || is_env_truthy("LINGXI_SKILL_SEARCH");
    let orch_builder = if skill_search_on {
        let source: Arc<dyn skill_api::SkillCandidateSource> = Arc::new(
            skill_api::RegistryCandidateSource::new(Arc::new(desktop_skill_registry())),
        );
        orch_builder.with_skill_discovery_prefetch(Arc::new(
            skill_api::SkillDiscoveryPrefetch::new(
                source,
                Arc::new(PosixRuntime::new()) as Arc<dyn platform_api::RuntimeSpawner>,
            ),
        ))
    } else {
        orch_builder
    };

    // P1 session-memory standalone trigger (§6.5, gated, default OFF). When
    // `LINGXI_SESSION_MEMORY` is truthy, wire the threshold-gated extractor
    // so durable notes are background-distilled (a Haiku-class fork) once the
    // Claude-compatible token/activity gates cross and written to
    // `<configHome>/agents/session-memory/<id>.md`, which the Session-tier memdir
    // scan re-loads next session. The builder's legacy tool-count parameters
    // stay zero so its 10k/5k/3 defaults apply. Unset/false ⇒ no handle ⇒ inert,
    // so the locked fixtures stay byte-identical.
    let orch_builder = match (session_memory_on, dirs::home_dir()) {
        (true, Some(home)) => {
            orch_builder.with_session_memory(orchestrator::prompt::build_session_memory_handle(
                side_query_client.clone(),
                "claude-haiku-4-5".to_string(),
                0,
                0,
                &home,
                Arc::new(PosixRuntime::new()) as Arc<dyn platform_api::RuntimeSpawner>,
            ))
        }
        _ => orch_builder,
    };
    let orch_builder = orch_builder.with_workflow_output_scopes(workflow_output_scopes);
    let orch = Arc::new(orch_builder);
    if let Some(selection) = persisted_reasoning_selection {
        orch.initialize_reasoning_selection_for_model(
            &default_model_id,
            default_model_profile.as_deref(),
            selection,
        );
    }
    orch.enable_goal_retries();
    orch.attach_owned_session_switches();
    async_hook_response_buffer.attach_rewake_target(&orch);

    // The task registry had to be completed before the orchestrator existed.
    // Bind the teammate's live default-prompt renderer now through a Weak so
    // the ownership graph remains acyclic.
    let _ =
        teammate_system_prompt_renderer_cell.set(Arc::new(OrchestratorTeammatePromptRenderer {
            orchestrator: Arc::downgrade(&orch),
        }));

    // Fill the hook-attachment sink's cell now that the orchestrator (and its
    // JSONL writer) exists, so every hook run from here on persists its one
    // transcript `attachment` line. The sink holds a `Weak`, so this does not
    // create an orchestrator↔hook-executor reference cycle.
    hook_attachment_sink.attach(&orch);
    hook_prompt_runner.attach(&orch);

    // Publish the orchestrator's shared output-token pool to the workflow
    // handler (registered above with a still-empty cell). From here, a launched
    // workflow's `budget.spent()` reads the same `Arc<AtomicU64>` the main loop
    // feeds per response — main loop + all workflows, claude-code's shared pool.
    let _ = local_workflow_output_pool.set(orch.output_token_pool());
    let _ = local_workflow_turn_baseline.set(orch.turn_start_output_baseline());

    // (6) Command registry through the desktop composition root.
    let handle: Arc<dyn OrchestratorHandle> = orch.clone();
    fusion_transcript_target.attach_orchestrator(&orch);
    let fusion_recovery_task = Some(tokio::spawn(async move {
        let _ = fusion_recovery_recorder.retry_pending().await;
    }));
    // Production terminal publication is owned by the durable Fusion recorder.
    // Keep the compatibility sink deliberately unbound here: binding it to an
    // orchestrator that owns this task registry would create
    // registry -> handler -> sink -> orchestrator -> registry, pinning the old
    // session after a drained remount. Standalone/legacy constructors may still
    // bind the deferred sink explicitly in their own compatibility tests.
    // FIX (B-agent-model-inheritance): now that the orchestrator exists, wire the
    // subagent spawner's LIVE default-model source to read the orchestrator's LIVE
    // `session.model` (the SAME source `build_prompt_context` / `get_status_snapshot`
    // read — updated by a mid-session `/model` switch or resume), superseding the
    // boot snapshot `orch_cfg.model` for spawns that carry no `parent_model_override`.
    // The read happens at spawn time. A last-known-good provider-qualified
    // selection covers a contended session lock; it must never fall back to the
    // boot model because that can silently change providers after `/model`.
    {
        let session = orch.session();
        let selection_model_providers = model_providers.clone();
        let selection_profile_auto_mode_provider = profile_auto_mode_provider.clone();
        let last_selection = std::sync::Arc::new(std::sync::Mutex::new(
            session
                .try_lock()
                .ok()
                .map(|state| agent::DefaultModelSelection {
                    model: state.model.clone(),
                    model_profile: state.model_profile.clone(),
                    provider_first_party: state
                        .model_profile
                        .as_ref()
                        .or_else(|| {
                            selection_model_providers
                                .get(&state.model)
                                .map(|(profile, _)| profile)
                        })
                        .and_then(|profile| selection_profile_auto_mode_provider.get(profile))
                        .map_or(true, |provider| provider == "firstParty"),
                }),
        ));
        let _ =
            subagent_default_model_selection_provider_cell.set(std::sync::Arc::new(move || {
                if let Ok(state) = session.try_lock() {
                    let selection = agent::DefaultModelSelection {
                        model: state.model.clone(),
                        model_profile: state.model_profile.clone(),
                        provider_first_party: state
                            .model_profile
                            .as_ref()
                            .or_else(|| {
                                selection_model_providers
                                    .get(&state.model)
                                    .map(|(profile, _)| profile)
                            })
                            .and_then(|profile| selection_profile_auto_mode_provider.get(profile))
                            .map_or(true, |provider| provider == "firstParty"),
                    };
                    if let Ok(mut cached) = last_selection.lock() {
                        *cached = Some(selection.clone());
                    }
                    return Some(selection);
                }
                last_selection.lock().ok().and_then(|cached| cached.clone())
            }));
    }

    // Cold resume: rebuild parked agents only after the live provider-qualified
    // session selection is published above. Transcript metadata pins the exact
    // model/profile used before restart; legacy transcripts fall back to the
    // parked row and resolve through this live selection instead of a boot-only
    // default. All inherited dependencies are live at this point as well.
    if let Some(restore_inheritance) = parked_agent_restore_inheritance {
        for (agent_id, outcome) in agent_restore::restore_parked_agents(
            &main_subagents_dir,
            subagent_spawner.as_ref(),
            fork_resume_gate.as_ref(),
            &restore_inheritance,
        )
        .await
        {
            match outcome {
                agent_restore::RestoreOutcome::Restored(restored_id) => {
                    tracing::info!(%agent_id, %restored_id, "restored parked background agent");
                }
                agent_restore::RestoreOutcome::Refused(reason) => {
                    tracing::warn!(%agent_id, %reason, "refused parked background agent restore");
                }
                agent_restore::RestoreOutcome::EmptyTranscript => {
                    tracing::warn!(%agent_id, "parked agent transcript is empty; restore skipped");
                }
                agent_restore::RestoreOutcome::Failed(error) => {
                    tracing::warn!(%agent_id, %error, "parked background agent restore failed");
                }
            }
        }
    }
    // H-CHG-02: wire the enforcing gate's live `set_permission_mode` auto gate to
    // the SAME live `session.model` source, so a runtime switch to `auto` after a
    // `/model` to an auto-unsupported model is rejected (`dUe(wi())` — claude-code
    // `Nle`) instead of silently accepted. Non-blocking read (`try_lock`); a
    // contended read returns `None` and the model check is skipped (fail-open).
    if let Some(cell) = loop_classifier_cell {
        let _ = cell.set(Arc::new(
            orchestrator::loop_permission_classifier::SessionLoopClassifier::new(
                &orch,
                api_service.clone(),
            ),
        ));
    }
    if let Some(cell) = live_model_provider_cell.as_ref() {
        let session = orch.session();
        let model_providers = model_providers.clone();
        let profile_auto_mode_provider = profile_auto_mode_provider.clone();
        let first_party_environment_provider = first_party_environment_provider.to_string();
        let _ = cell.set(std::sync::Arc::new(move || {
            session.try_lock().ok().map(|state| {
                let profile = state.model_profile.clone().or_else(|| {
                    model_providers
                        .get(&state.model)
                        .map(|(profile, _)| profile.clone())
                });
                let profile_provider = profile
                    .as_ref()
                    .and_then(|profile| profile_auto_mode_provider.get(profile))
                    .map_or("firstParty", String::as_str);
                let provider = if profile_provider == "firstParty" {
                    first_party_environment_provider.clone()
                } else {
                    profile_provider.to_string()
                };
                permission::LiveModelContext {
                    model: state.model.clone(),
                    provider,
                }
            })
        }));
    }
    // TPM-C (Task 5 step 2): seed the initial model_profile from a
    // profile-qualified default_model.  SessionState::empty starts model_profile
    // at None; this is a no-op when default_model is a bare id.
    if let Some(profile) = default_model_profile.as_deref() {
        orch.seed_initial_model_profile(&default_model_id, profile)
            .await;
    }
    orch.spawn_startup_responses_websocket_prewarm();
    // Plan 3c: `/connect` seams — Copilot device-flow over `PosixHttp`, and the
    // API-key writer over the host secure prompt (tui-supplied; headless no-op).
    // M8: also wire the ChatGPT OAuth seam (`/connect chatgpt`).
    // Round-4 review finding [8] / round-5 finding [15]: EVERY `/connect`
    // seam below is wrapped so a successful credential write refreshes
    // Fusion's catalog filter (`FusionCatalogRefresher`) — without this, a
    // provider connected here stayed invisible to `/fusion` for the rest of
    // the process even though this same write makes the ordinary turn loop
    // route it immediately. Round 4 wrapped only the API-key writer and the
    // Copilot device flow; the ChatGPT and unified-OAuth drivers are wrapped
    // here too, and the TUI key view (which bypasses all four and writes
    // straight through `secret::CredentialManager`) reaches the same
    // refresher through `refresh_fusion_catalog_after_credential_write`.
    let connect_copilot: Arc<dyn command_core::CopilotConnectDriver> =
        Arc::new(FusionCatalogRefreshingCopilotConnect {
            inner: Arc::new(crate::desktop::connect::EngineCopilotConnect::new(
                credentials.clone(),
            )),
            refresher: fusion_catalog_refresher.clone(),
        });
    let connect_writer: Arc<dyn command_core::ConnectCredentialWriter> =
        Arc::new(FusionCatalogRefreshingCredentialWriter {
            inner: Arc::new(crate::desktop::connect::EngineCredentialWriter::new(
                credentials.clone(),
                cfg.connect_prompt.clone().unwrap_or_else(|| {
                    Arc::new(crate::desktop::connect::NoopKeyPrompt)
                        as Arc<dyn crate::desktop::connect::SecureKeyPrompt>
                }),
            )),
            refresher: fusion_catalog_refresher.clone(),
        });
    let connect_chatgpt_inner: Arc<dyn command_core::ChatGptConnectDriver> =
        Arc::new(crate::desktop::connect::EngineChatGptConnect::new(
            openai_oauth_client,
            credentials.clone(),
        ));
    // Round-5 review finding [15] class sweep: EVERY `/connect` seam that
    // persists a credential refreshes Fusion's catalog, not just the two
    // round 4 wrapped. The OAuth driver below is built over the UNWRAPPED
    // ChatGPT driver so a ChatGPT sign-in through the picker refreshes once,
    // not twice.
    let connect_chatgpt: Arc<dyn command_core::ChatGptConnectDriver> =
        Arc::new(FusionCatalogRefreshingChatGptConnect {
            inner: connect_chatgpt_inner.clone(),
            refresher: fusion_catalog_refresher.clone(),
        });
    // Unified OAuth sign-in driver for the TUI `/connect` picker (Anthropic
    // Pro/Max + OpenAI ChatGPT browser flows). Reuses the same backends as
    // `/login` (the Anthropic `auth` handle) and `/connect chatgpt`
    // (`connect_chatgpt`); built here while both are still owned (the registry
    // call below moves `connect_chatgpt`).
    let oauth_connect_driver: Arc<dyn command_core::OAuthConnectDriver> =
        Arc::new(FusionCatalogRefreshingOAuthConnect {
            inner: Arc::new(crate::desktop::connect::EngineOAuthConnect::new(
                auth.clone(),
                connect_chatgpt_inner,
            )),
            refresher: fusion_catalog_refresher.clone(),
        });
    let mut reg = desktop_command_registry(
        handle.clone(),
        auth.clone(),
        &cfg.cwd,
        &cfg.lingxi_home,
        connect_writer,
        connect_copilot.clone(),
        connect_chatgpt,
        cfg.customization_gates,
        strict_plugin_only_skills,
        &cfg.add_dir,
        shared_command_registry.clone(),
    )
    .await;
    reg.register_builtin_handler(Arc::new(command_core::VersionHandler::with_build_info(
        cfg.build_info,
    )));
    // The TUI intercepts `/workflows` to open its interactive picker. Bind the
    // same registry-backed text projection for headless/bridge dispatch paths.
    reg.register_builtin_handler(Arc::new(command_core::WorkflowsHandler::with_registry(
        task_registry.clone() as Arc<dyn platform_api::task_registry::TaskRegistryHandle>,
    )));
    reg.register_builtin_handler(worktree_command_handler);
    if cron_scheduler_enabled(std::env::var("CLAUDE_CODE_DISABLE_CRON").ok().as_deref()) {
        // `/cron` is an explicit management action. Keep it out of the model
        // permission loop and invoke the same validated cron tools directly.
        reg.register_builtin_handler(cron_command_handler);
    }
    reg.register_builtin_handler(Arc::new(
        fusion_command::DesktopFusionCommandHandler::new(
            task_registry.clone(),
            fusion_executor.clone(),
            handle.clone(),
            model_providers
                .iter()
                .map(|(model, (profile, _))| (model.clone(), profile.clone()))
                .collect(),
        )
        .with_durable_publication_available(cfg.session_persistence)
        .with_publication_retrier(Some(fusion_recorder_factory_impl.clone())),
    ));

    // WIZARD-06: re-register `/auto-mode-setup` WITH its runners attached.
    // `register_all_builtin_commands` wires the handle-free shape (grammar,
    // `--help`, every rejection path); only the composition root can supply the
    // two branches that need real capabilities — a live `ApiService` for
    // `--propose` and the settings writer for `--apply-file`. Until this point
    // both branches report `unavailable_here` rather than pretending to work.
    {
        let listing = default_listings
            .iter()
            .find(|l| l.request_model == default_model_id || l.display_model == default_model_id);
        // The oracle derives the thinking flag from the MODEL (`IQt(r)`), not
        // from session config, and grants the no-thinking budget top-up when the
        // model carries no thinking config.
        let thinking = listing.is_some_and(|l| l.supports_reasoning);
        // `subscription_signal` reads the plan from the live snapshot; an
        // unauthenticated or still-fetching session yields `None`, which renders
        // as the "unknown" signal rather than a guessed plan.
        let plan = subscription
            .read()
            .ok()
            .and_then(|g| g.as_ref().and_then(|s| s.subscription_type.clone()));
        let transcript_dir =
            cfg.lingxi_home
                .join("projects")
                .join(session::jsonl::path::project_dir_name(
                    &cfg.cwd.to_string_lossy(),
                ));
        let propose = std::sync::Arc::new(auto_mode_propose::DesktopProposeRunner::new(
            api_service.clone(),
            default_model_id.clone(),
            default_model_profile.clone(),
            thinking,
            plan,
            cfg.cwd.clone(),
            cfg.lingxi_home.clone(),
            transcript_dir,
            task_registry.clone(),
        ));
        let apply = std::sync::Arc::new(auto_mode_propose::DesktopApplyRunner::new(
            command_core::auto_mode_setup::apply_file_roots(&cfg.lingxi_home),
            permission::PermissionPaths {
                lingxi_home: cfg.lingxi_home.clone(),
                cwd: cfg.cwd.clone(),
            },
        ));
        reg.register_builtin_handler(std::sync::Arc::new(
            command_core::AutoModeSetupHandler::new()
                .with_propose(propose)
                .with_apply(apply),
        ));
    }
    // SKILLEXEC.2: fill the shared command-registry slot the `Skill` tool's
    // loader holds, then hand the SAME `Arc` to the slash dispatcher so the tool
    // and the dispatcher observe one command set (plugin lifecycle mutations via
    // the dispatcher's write lock are visible to the loader too).
    // claude-code `getAllCommands` folds `mcp.commands` into the command list:
    // an MCP server's PROMPTS become `/<server>:<prompt>` slash commands. The
    // registry has always fetched them at connect; this is the read side, and
    // without it they existed on the wire and nowhere the user or model could
    // reach. Merged LAST so a same-named local command wins — a remote server
    // must not shadow one of the user's own.
    let mcp_prompt_commands =
        command_api::mcp_prompts::mcp_prompt_commands(&mcp_registry.connected_prompts().await);
    for cmd in mcp_prompt_commands.iter().cloned() {
        if reg.resolve(&cmd.name).is_none() {
            reg.register_command(cmd);
        }
    }
    *shared_command_registry.write().await = reg;

    // (6.5) Plugin bootstrap — discover installed plugins on disk and
    //       materialise their COMMANDS + HOOKS into the live registries, plus
    //       their AGENTS into the agent catalog. Mirrors claude-code's
    //       cache-only plugin load at startup (`main.tsx:282`
    //       `loadAllPluginsCacheOnly()` → `pluginLoader.ts:1887`
    //       `loadPluginsFromMarketplaces({cacheOnly})`; `setup.ts:318`
    //       `loadPluginHooks`). Plugins live under `getPluginsDirectory()` =
    //       `~/.lingxi/plugins` (`pluginDirectories.ts:53`), honoring the
    //       `LINGXI_PLUGIN_CACHE_DIR` override. Discovery is allowlist-
    //       driven (faithful): the `settings.enabledPlugins`
    //       (`plugin@marketplace` → enabled) entries resolve to versioned cache
    //       dirs `cache/{marketplace}/{plugin}/{version}/`, the layout
    //       `loadAllPluginsCacheOnly` consumes; a flat-walk fallback covers
    //       pre-fetched local plugin dirs. Each plugin command's BODY +
    //       frontmatter are loaded from its markdown file (not empty), and a
    //       plugin loads all-or-nothing (agent frontmatter is validated before
    //       any registry mutation). Best-effort: a malformed plugin logs a
    //       warning and is skipped — discovery never breaks boot (a fresh
    //       install with no `plugins/` dir yields zero plugins, an exact
    //       no-op). Plugin MCP servers live-connect through the same
    //       `connect_all` path as configured `.mcp.json` servers (the manager
    //       owns the same `mcp_registry` Arc and dials them at `enable()`).
    //       RESIDUAL: marketplace-catalog source
    //       resolution + enterprise allow/blocklist policy, and reading the
    //       exact installed version from `installed_plugins.json` (we probe the
    //       single-version cache dir instead). The manager is given a real but
    //       isolated LSP/skill/output-style/tool registry so `enable()` is
    //       non-panicking while only commands + hooks reach the engine's live
    //       registries.
    //       (M3 cc2.1.198) `--safe-mode` / `--bare` skip the AMBIENT bootstrap
    //       (`K5d.plugins:!1` / `V5d.plugins:!0`; safe-mode log "Skipping
    //       plugin hooks - safe mode disables plugins"). This also skips
    //       plugin LSP servers — lingxi's only LSP-server source — matching
    //       `Hc("lspServers")` gating `initializeLspServerManager`.
    //       (M4 cc2.1.198) `--plugin-dir` session-only plugins are an EXPLICIT
    //       request that survives `--bare` (its help text: "Explicitly provide
    //       context via: … --plugin-dir") but not safe mode; they load AFTER
    //       the marketplace-installed discovery through the SAME `pm.enable`
    //       materialisation path (binary `EBm` → the shared plugin merge).
    let ambient_plugins = !cfg.restricted && !cfg.customization_gates.disables_plugins();
    let inline_plugins = !cfg.cli_plugin_dirs.is_empty() && !cfg.customization_gates.safe_mode;
    // Restricted mode suppresses ambient user/project/local plugin settings,
    // but it must still materialise the trusted managed/flag settings tier.
    // Keep the existing safe/bare gates authoritative: those modes disable
    // plugins unless an explicit `--plugin-dir` survives via `inline_plugins`.
    let restricted_policy_plugins = cfg.restricted && !cfg.customization_gates.disables_plugins();
    // (`/reload-plugins`) The retained plugin subsystem — `None` when plugins are
    // entirely disabled (safe mode / `--bare` with no `--plugin-dir`), so the
    // interactive refresh reports "plugins disabled" rather than reloading.
    let mut plugin_runtime: Option<Arc<PluginRuntime>> = None;
    if ambient_plugins || inline_plugins || restricted_policy_plugins {
        let plugins_dir = std::env::var_os("LINGXI_PLUGIN_CACHE_DIR")
            .map_or_else(|| cfg.lingxi_home.join("plugins"), std::path::PathBuf::from);
        // Primary (faithful) path: resolve the `settings.enabledPlugins`
        // allowlist (`plugin@marketplace` → enabled) to versioned cache dirs
        // `cache/{marketplace}/{plugin}/{version}/`, exactly as
        // `loadAllPluginsCacheOnly` (`pluginLoader.ts:1888`) consumes a real
        // `~/.lingxi/plugins`; a flat-walk fallback covers pre-fetched local
        // dirs, and `--plugin-dir` session plugins append. The shared body is
        // `discover_plugin_set`, reused by [`PluginRuntime::refresh`].
        let discovered = discover_plugin_set(
            ambient_plugins,
            inline_plugins,
            &cfg.lingxi_home,
            &cwd_for_plugins,
            &plugins_dir,
            &cfg.cli_plugin_dirs,
            &[],
            cfg.restricted,
            cfg.flag_settings.as_ref(),
            &analytics_bus,
        )
        .await;
        // Build the manager UNCONDITIONALLY (even when zero plugins resolve on
        // disk) and RETAIN it in `plugin_runtime`, so a later `/reload-plugins`
        // can enable a plugin the user turns on mid-session. Live registries the
        // manager materialises components into:
        // - command  → `shared_command_registry` (drives `/`-completion + the
        //   per-turn skill listing).
        // - hooks    → `plugin_hook_registry` (the orchestrator's clone).
        // - MCP      → `plugin_mcp_registry` (== the orchestrator's
        //   `mcp_registry`; scoped configs live-connect via `connect_all`, the
        //   same path as configured `.mcp.json` servers, and the reconnect loop
        //   covers any that fail their initial dial).
        // - LSP      → `plugin_lsp_registry` (== the `LSPTool`'s registry).
        // - workflow → `plugin_workflow_registry` (§14; == the registry the
        //   `WorkflowTool`, its `TaskRegistryWorkflowLauncher`, and the
        //   `LocalWorkflowHandler` all read). This is its only WRITER.
        // The SKILL and OUTPUT-STYLE registries have no turn-loop consumer yet,
        // so they are local instances here (residual, as at startup).
        // Seed the persisted non-sensitive `userConfig` (settings `pluginConfigs`
        // scope) so the loader resolves `${user_config.*}` options from disk (not
        // just field defaults) and injects `LINGXI_PLUGIN_OPTION_*` into plugin
        // hooks. Sensitive values are NOT here — they resolve live from
        // `CredentialManager`.
        let plugin_configs =
            load_plugin_configs(&cfg.lingxi_home, cfg.restricted, cfg.flag_settings.as_ref()).await;
        let blocked_marketplaces = load_blocked_marketplaces().await;
        let managed_plugin_names = load_managed_plugin_names().await;
        let pm = Arc::new(
            plugin::PluginManager::new(
                plugins_dir.clone(),
                Arc::new(PosixFileSystem::new(cwd_for_plugins.clone())),
                http.clone(),
                Arc::new(PosixRuntime::new()),
                credentials.clone(),
                strict_plugin_policy.clone(),
                shared_command_registry.clone(),
                Arc::new(RwLock::new(SkillRegistry::new())),
                plugin_hook_registry.clone(),
                plugin_output_style_registry.clone(),
                plugin_mcp_registry.clone(),
                plugin_lsp_registry.clone(),
                Arc::new(RwLock::new(ToolRegistry::new())),
            )
            .with_agent_catalog(plugin_agent_catalog.clone())
            .with_analytics_bus(analytics_bus.clone())
            .with_plugin_configs(plugin_configs)
            .with_blocked_marketplaces(blocked_marketplaces)
            .with_managed_plugin_names(managed_plugin_names)
            .with_safe_mode(cfg.customization_gates.safe_mode)
            .with_plugin_workflows(plugin_workflow_registry.clone())
            .with_project_dir(cwd_for_plugins.clone())
            .with_task_registry(
                task_registry.clone() as Arc<dyn platform_api::task_registry::TaskRegistryHandle>
            ),
        );
        for (id, manifest, dir) in discovered {
            let plugin_name = manifest.name.clone();
            // Materialise COMMANDS + HOOKS + MCP + LSP + AGENTS. The plugin's
            // AGENTS are materialised into `plugin_agent_catalog` ONLY on
            // success — the dir-scan loader is ungated, so gating on enable
            // keeps a plugin that failed to load out of the live catalog.
            //
            // §19.1: an agent declaring `permissionMode` / `mcpServers` /
            // `hooks` is NOT a load failure. `enable` warns per privileged
            // field and strips all three from the `AgentDefinition` before it
            // reaches the catalog, so the agent lands live with the escalation
            // removed rather than taking the whole plugin down. Pinned by
            // `plugin_runtime_refresh_strips_agent_escalation_from_live_catalog`.
            match pm.enable(&id, manifest, dir).await {
                Ok(()) => {}
                Err(e) => tracing::warn!(
                    plugin = %plugin_name,
                    error = %e,
                    "skipping plugin that failed to load"
                ),
            }
        }
        plugin_runtime = Some(Arc::new(PluginRuntime {
            manager: pm,
            analytics_bus: analytics_bus.clone(),
            plugins_dir,
            home: cfg.lingxi_home.clone(),
            cwd: cwd_for_plugins.clone(),
            cli_plugin_dirs: cfg.cli_plugin_dirs.clone(),
            additional_project_roots: repo_root_reloader.registered_roots(),
            ambient: ambient_plugins,
            inline: inline_plugins,
            restricted: cfg.restricted,
            flag_settings: cfg.flag_settings.clone(),
            refresh_lock: Arc::new(tokio::sync::Mutex::new(())),
        }));
        if inline_plugins && plugin_dir_watch_enabled() {
            if let Some(runtime) = plugin_runtime.clone() {
                spawn_cli_plugin_dir_collection_watch(runtime);
            }
        }
    }
    // Inventory only configs that actually reached the live registry. Counting
    // discovered plugin manifests here over-reported blocked, colliding, or
    // failed-to-materialize plugin servers.
    let inventory_configs = {
        let connections = mcp_registry.connections.read().await;
        connections
            .values()
            .map(|state| state.config().clone())
            .collect::<Vec<_>>()
    };
    emit_mcp_servers_inventory(
        analytics_bus.as_ref(),
        &mcp_servers_inventory_payload(&inventory_configs),
    )
    .await;

    // Oracle `Pd` reports the completed discovery round once. Re-snapshot
    // after plugin materialization so enabled plugin MCP tools/prompts are not
    // omitted from the one startup event.
    let loaded_mcp_tools =
        tool_mcp::build_registered_mcp_tools(&mcp_registry, mcp_tool_ctx.clone()).await;
    let loaded_mcp_prompt_commands =
        command_api::mcp_prompts::mcp_prompt_commands(&mcp_registry.connected_prompts().await);
    emit_mcp_tools_commands_loaded(
        analytics_bus.as_ref(),
        &mcp_tools_commands_loaded_payload(
            registered_mcp_tool_count(&loaded_mcp_tools),
            &loaded_mcp_prompt_commands,
        ),
    )
    .await;
    repo_root_reloader
        .set_plugin_runtime(plugin_runtime.clone())
        .await;

    // (M4 cc2.1.198) `--agent <agent>` — resolve the session agent against the
    // FINAL catalog (dir + `--agents` flag + plugin agents), the binary's `dts`
    // lookup: exact `agentType` match, else FQN `…:{name}` suffix; a miss logs
    // `Warning: agent "X" not found. Available agents: …. Using default
    // behavior.` and the session proceeds with default behavior.
    //
    // (P2-02 cc2.1.207) On a HIT the binary APPLIES the agent to the MAIN loop
    // via `bde(h?.agentType)` + `mainThreadAgentDefinition`. We adopt the
    // model-visible pieces here:
    //   • `agentType` — rides every main-thread lifecycle hook payload (claude
    //     `wf`/`MVe` `?? MB()`);
    //   • system prompt — becomes the main-loop system prompt on every query via
    //     `nre` (`--system-prompt` still winning);
    //   • `tools:` + `disallowedTools` frontmatter — narrows the advertised tool
    //     pool (claude `HJ(us,to,!1,!0).resolvedTools`, `n=true` ⇒ NO subagent
    //     always-disallowed strip);
    //   • `model` — replaces the main-loop model (claude `jb(Zo(y.model))`),
    //     gated exactly like the binary: only when the user did NOT pass
    //     `--model` (`!cfg.default_model_explicit` ≙ `!userSpecifiedModel`) AND
    //     the agent declares an explicit model (`AgentModel != Inherit`).
    // This runs BEFORE the `SessionStart` firing below so that hook carries the
    // `agentType`, and AFTER the default-model seed above so the override wins.
    //   • frontmatter `hooks` — registered as `mainThreadAgentHooks` (claude
    //     `Rft`→`o_n`), gated by [`agent_source_is_trusted`] (`g9e`), BELOW.
    //   • RESUME restoration (`rVe`) — when NO `--agent` is passed on a `--resume`
    //     (`cfg.session_id_override` set), the applied `agentType` persisted at
    //     the ORIGINAL boot is read back from this session's transcript
    //     (`agentSettings.get(sessionId)`) and re-adopted through the SAME block
    //     (prompt + tools + model + hooks). A miss emits the byte-exact
    //     `Resumed session had agent "X" but it is no longer available. Using
    //     default behavior.` warning (claude `rVe`) and falls back to default. A
    //     re-passed `--agent` wins (claude `rVe`'s `if(t)return`) — it is applied
    //     via the explicit arm below and the resume read is skipped.
    //   • frontmatter `mcpServers` (scope `"agent"`) — CLOSED (M7 cc2.1.220):
    //     merged into the to-connect config list by the `FWt` pre-pass in
    //     (5.1)/(5.3) above, BEFORE the MCP registry `connect_all` and the
    //     `Arc<ToolRegistry>` snapshot — the servers register, connect and
    //     surface tools exactly like `--mcp-config` servers. The
    //     `(wanted_agent, resumed_agent_snapshot, from_resume)` triple this
    //     block consumes is computed THERE (one transcript read serves both the
    //     merge and this application).
    // (Built-in agent defs live in the subagent spawner, not this catalog, so
    // their names are absent from the miss warning's "Available agents" list —
    // residual.)
    if let Some(wanted) = wanted_agent {
        // Resolve against the FINAL catalog, extracting what the main thread
        // applies (agentType + system prompt + tool policy + model) so the
        // catalog read lock is released before we mutate the orchestrator seam.
        let applied = {
            let cat = plugin_agent_catalog.read().await;
            let snapshot_def = resumed_agent_snapshot
                .as_ref()
                .and_then(|v| serde_json::from_value::<agent::AgentDefinition>(v.clone()).ok())
                .filter(|a| a.agent_type == wanted);
            let hit = snapshot_def.as_ref().or_else(|| {
                cat.iter().find(|a| a.agent_type == wanted).or_else(|| {
                    let suffix = format!(":{wanted}");
                    cat.iter().find(|a| a.agent_type.ends_with(&suffix))
                })
            });
            match hit {
                Some(a) => {
                    // claude `if(!userSpecifiedModel&&y.model&&y.model!=="inherit")
                    // {jb(Zo(y.model))}`. `Zo` = `resolve_user_specified_model`
                    // (alias→wire id). Frontmatter never yields `Explicit`, but
                    // handle both alias/explicit arms for completeness. This is
                    // ALSO the resume model reset (`rVe` applies the same `jb`).
                    let model_override = if cfg.default_model_explicit {
                        None
                    } else {
                        match &a.model {
                            agent::AgentModel::Alias(spec) | agent::AgentModel::Explicit(spec) => {
                                Some(agent::model_resolution::resolve_user_specified_model(spec))
                            }
                            agent::AgentModel::Inherit => None,
                        }
                    };
                    Some((
                        a.agent_type.clone(),
                        a.system_prompt.clone(),
                        a.tools.clone(),
                        a.disallowed_tools.clone(),
                        model_override,
                        // (P2-02 cc2.1.207) keep the frontmatter `hooks` + `source`
                        // so `Rft` can register them as `mainThreadAgentHooks`
                        // below (the source drives the `g9e` trusted-source gate).
                        a.frontmatter_hooks.clone(),
                        a.source,
                        a.clone(),
                    ))
                }
                None => {
                    if from_resume {
                        // claude `rVe`: the persisted agent is gone from the final
                        // catalog → byte-exact warn, then fall back to default.
                        tracing::warn!(
                            "Resumed session had agent \"{wanted}\" but it is no longer available. Using default behavior."
                        );
                    } else {
                        tracing::warn!(
                            "Warning: agent \"{wanted}\" not found. Available agents: {}. Using default behavior.",
                            cat.iter()
                                .map(|a| a.agent_type.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                    }
                    None
                }
            }
        };
        if let Some((
            agent_type,
            system_prompt,
            tools,
            disallowed_tools,
            model_override,
            frontmatter_hooks,
            source,
            resolved_definition,
        )) = applied
        {
            tracing::debug!(agent = %agent_type, from_resume, "--agent applied to main thread");
            // (P2-02 cc2.1.207) Persist the applied `agentType` as an
            // `agent-setting` transcript record (claude
            // `{type:"agent-setting",agentSetting:currentSessionAgentSetting,
            // sessionId}`) so a later `--resume` with no `--agent` re-adopts it via
            // `rVe`. Only on the EXPLICIT path (`!from_resume`) — the resume replay
            // already read this record — and only when the session is persisted
            // (no writer ⇒ nothing to resume from). Borrows `agent_type` before it
            // moves into `set_main_thread_agent`.
            if !from_resume && cfg.session_persistence {
                if let Err(e) = main_agent_setting_writer
                    .append_agent_setting_snapshot(
                        &main_session_uuid,
                        &agent_type,
                        &serde_json::to_value(&resolved_definition)
                            .expect("resolved AgentDefinition must serialize"),
                    )
                    .await
                {
                    tracing::warn!(error = %e, "failed to persist --agent agent-setting record");
                }
            }
            orch.set_main_thread_agent(
                agent_type,
                system_prompt,
                tools,
                disallowed_tools,
                model_override,
            )
            .await;

            // (P2-02 cc2.1.207) `Rft` — register the agent's frontmatter `hooks`
            // as `mainThreadAgentHooks` (`o_n(e.hooks)`). The binary gate is
            //   `if(e?.hooks && (!uA("hooks") || g9e(e.source))) o_n(e.hooks)`.
            // `uA("hooks")` is the `strictPluginOnlyCustomization` policy (NOT
            // `disableAllHooks` — that gate lives at hook DISPATCH, on the
            // executor's `policy_disable_all_hooks`). The immutable managed
            // policy snapshot was resolved beside the MCP gate above; when the
            // hooks slot is locked, only a trusted/plugin-owned source may
            // install command-capable frontmatter hooks. `is_agent=false` keeps
            // `Stop` as `Stop` (this is the MAIN thread, not a subagent).
            // Registered BEFORE the `fire_session_start("startup")` call below so
            // a SessionStart frontmatter hook fires with the agent applied. The
            // orchestrator owns the bucket identity so an in-place resume can
            // replace it without leaking hooks from the previously mounted
            // session.
            //
            // (cc 2.1.218 `QEt`) The binary now ALSO gates on `mvo(e)` — the
            // definition's folder must be trusted before its `hooks:` become
            // live main-thread hooks:
            //   `let t=!VR("hooks")||J0e(e.source), r=mvo(e);
            //    if(t&&r){b1r(e.hooks);return} if(t&&!r)hvo(e,"mainThread"); b1r(void 0)`
            // The same immutable gate is also threaded into subagent contexts
            // below so the main-thread and child registration paths agree.
            if !frontmatter_hooks.is_empty()
                && (!strict_plugin_only_hooks || agent_source_is_trusted(source))
            {
                if agent::hooks_trust::agent_hooks_origin_trusted(&resolved_definition, &cfg.cwd) {
                    orch.replace_main_thread_agent_hooks(&frontmatter_hooks)
                        .await;
                } else {
                    agent::hooks_trust::report_untrusted_hooks(
                        &resolved_definition,
                        &cfg.cwd,
                        agent::hooks_trust::HooksTrustSurface::MainThread,
                        false,
                    );
                    // `QEt`'s untrusted arm ends in `b1r(void 0)` (clear the
                    // bucket). A no-op at boot (the bucket starts empty), kept
                    // for parity with the resume site's clear.
                    orch.replace_main_thread_agent_hooks(&[]).await;
                }
            }
        }
    }

    // #39 UserPromptExpansion: wire the dispatcher to fire `UserPromptExpansion`
    // (claude-code `WFa`→`b$t`) the moment it expands a markdown / MCP-prompt
    // slash command. The context provider reads THIS conversation's live
    // session id + cwd from the orchestrator (`expansion_hook_context`), matching
    // the base hook input the orchestrator's own lifecycle hooks build. A strict
    // no-op unless a `UserPromptExpansion` hook is registered.
    // `--disable-slash-commands` (claude-code "Disable all skills"): after ALL
    // builtin + plugin + skill registration, replace the shared command registry
    // with an empty one so the dispatcher AND the `Skill` tool's loader (which
    // share this `Arc`) observe zero commands/skills. Plugin MCP servers / hooks
    // / tools live in other registries and are intentionally unaffected (claude's
    // flag disables skills/commands only).
    if cfg.disable_slash_commands {
        *shared_command_registry.write().await = CommandRegistry::new();
    }
    let expansion_ctx_orch = orch.clone();
    let background_command_orch = orch.clone();
    let mcp_prompt_registry = mcp_registry.clone();
    let prompt_paths_orch = orch.clone();
    let mut dispatcher = RegistrySlashDispatcher::new(shared_command_registry.clone())
        .with_prompt_paths(Arc::new(move || {
            (
                prompt_paths_orch.project_root(),
                prompt_paths_orch.current_cwd(),
            )
        }))
        .with_skill_invocation_observer(skill_invocation_observer)
        .with_skill_usage_home(cfg.lingxi_home.clone())
        .with_mcp_prompt_resolver(Arc::new(move |connection_id, prompt_name, arguments| {
            let registry = mcp_prompt_registry.clone();
            Box::pin(async move {
                registry
                    .get_prompt(connection_id, &prompt_name, arguments)
                    .await
                    .map_err(|error| error.to_string())
            })
        }))
        .with_background_prompt_launcher(Arc::new(move |prompt| {
            let orch = background_command_orch.clone();
            Box::pin(async move {
                orch.fork_conversation(&prompt)
                    .await
                    .map(|outcome| {
                        let tail = &outcome.agent_id[outcome.agent_id.len().saturating_sub(4)..];
                        format!(
                            "\u{2442} started code-review in background as {} ({tail})",
                            outcome.name
                        )
                    })
                    .map_err(|error| error.to_string())
            })
        }))
        .with_expansion_hooks(
            expansion_hook_executor,
            std::sync::Arc::new(move || {
                let orch = expansion_ctx_orch.clone();
                Box::pin(async move { orch.expansion_hook_context().await })
            }),
        )
        // (#3) Real embedded-shell expansion for markdown/plugin `!`cmd`` bodies
        // AND the builtin `InjectMessage` prompts (`/commit` …). Non-MCP only.
        .with_shell_expansion(shell_expansion_provider.clone());
    // MP-1: hand the dispatcher the enforcing gate so each input's frontmatter
    // `disallowed-tools` reaches `alwaysDenyRules.command` (upstream `Tbt`).
    // Without this the field is parsed and then ignored for every skill that
    // runs INLINE — only `context: fork` skills were ever scoped by it.
    if let Some(gate) = enforcing_permission_gate.clone() {
        dispatcher = dispatcher.with_permission_gate(gate);
    }

    // (7) Session lifecycle: fire the `SessionStart` hooks now that the
    //     orchestrator + hook registry are fully wired. claude-code fires the
    //     `SessionStart` hook event at session startup (`utils/hooks.ts:3876-3881`,
    //     the SessionStart path) with `source` = one of
    //     `startup` / `resume` / `clear` / `compact`. The desktop composition root
    //     OWNS the session lifecycle (it constructs the orchestrator), and `build`
    //     assembles exactly one fresh session per call, so the byte-faithful
    //     `source` here is `"startup"`. Best-effort: `fire_session_start` discards
    //     the hook aggregate, so a failing or malformed `SessionStart` hook never
    //     breaks boot, and it is a strict no-op when no `SessionStart` hook is
    //     registered (the common case). NOTE: there is no harness-runtime::desktop-local
    //     teardown seam — `build` returns the runtime and the host (`apps/cli` /
    //     the bridge-server) drops it on process exit with no hook-capable
    //     shutdown path — so the matching `SessionEnd` is NOT fired here. The
    //     `ConversationOrchestrator::fire_session_end` helper exists for a future
    //     batch that adds an explicit host teardown seam.
    let session_start = orch.fire_session_start("startup").await;
    if session_start.reload_skills {
        let home = dirs::home_dir().unwrap_or_else(|| cfg.lingxi_home.clone());
        let managed_dir = crate::desktop::settings_watch::managed_settings_dir();
        let handler = command_core::reload_skills::ReloadSkillsHandler::with_all_roots(
            shared_command_registry.clone(),
            cfg.cwd.clone(),
            cfg.lingxi_home.clone(),
            Some(managed_dir),
            home,
            Vec::new(),
            cfg.customization_gates.safe_mode,
        );
        if let Some(parsed) = parse_slash_command("/reload-skills") {
            let _ = handler.handle(&parsed).await;
        }
    }

    // (7.1) Instruction-load lifecycle: fire the `InstructionsLoaded` hooks now
    //       that memory + the hook registry are wired. claude-code fires this
    //       fire-and-forget hook once per LINGXI.md / `LINGXI.local.md` spliced
    //       into context by the eager session-start `getMemoryFiles` pass
    //       (`utils/claudemd.ts:1054-1071`, `utils/hooks.ts:4335-4369`), each
    //       carrying the file's `file_path` / `memory_type` / `load_reason`
    //       (`session_start` for top-level files). The orchestrator owns the
    //       memory provider, so it loads memory once and fires from that single
    //       point. Best-effort: `fire_instructions_loaded` discards each hook
    //       aggregate, so a failing/malformed `InstructionsLoaded` hook never
    //       breaks boot, and it is a strict no-op when none is registered (the
    //       common case) or when no instruction files are present.
    orch.fire_instructions_loaded().await;

    // (7.2) ConfigChange lifecycle: start the settings watcher now that the
    //       orchestrator + hook registry are wired. claude-code watches the
    //       user / project / local / policy settings files and, on every
    //       detected change, fires the `ConfigChange` hook with the layer
    //       `source` + changed `file_path` BEFORE applying the change
    //       (`changeDetector.ts:285-297` → `executeConfigChangeHooks`,
    //       `utils/hooks.ts:4214`). The Rust port had no watcher; this wires it
    //       at the composition root via the in-tree `notify`-backed
    //       `FileSystem::watch` primitive (`platform-posix`'s `watch_helper`).
    //       The watcher fires `fire_config_change` BEFORE applying the narrow
    //       managed `disableAutoMode` update. Hook execution is best-effort,
    //       while the safety callback is required even when no ConfigChange
    //       hook is registered. Ordinary user/project/local settings are not
    //       reloaded here. The handle is returned on the runtime so it lives
    //       for the session; dropping the runtime aborts the watch tasks
    //       (RAII), releasing the OS handles cleanly.
    //
    //       The full `platform-posix` `FileSystem` is used here (NOT the
    //       `posix-minimal` one wired into the engine) because only it has the
    //       real `notify`-backed `watch`; `posix-minimal::watch` is an
    //       empty-stream stub, so wiring it would observe no events.
    //
    //       GATED (decided above, before `hook_registry` moved into the
    //       orchestrator): only spawn the watcher when at least one
    //       `ConfigChange` hook is registered. The fire is a strict no-op
    //       otherwise, so the background `notify` watcher (and its blocking pump
    //       thread) would be pure overhead in the common no-hook case — gating
    //       keeps boot cheap and avoids holding an OS watch handle nobody
    //       consumes.
    #[cfg(not(test))]
    let watch_fs: Arc<dyn platform_api::FileSystem> =
        Arc::new(PosixFileSystem::new(watch_cwd.clone()));
    // Unit tests exercise the real watcher lifecycle with cancellable streams,
    // without depending on the host FSEvents daemon's blocking startup/stop RPCs.
    #[cfg(test)]
    let watch_fs: Arc<dyn platform_api::FileSystem> =
        Arc::new(watcher_test_support::WatchFs::new(watch_cwd.clone()));
    let firer: Arc<dyn settings_watch::ConfigChangeFirer> = orch.clone();
    let settings_watcher =
        settings_watch::SettingsWatcher::new(&cfg.lingxi_home, &watch_cwd, firer)
            .with_permission_gate(settings_permission_gate)
            .spawn(watch_fs)
            .await;

    // (7.3) FileChanged lifecycle: start the file-changed watcher now that the
    //       orchestrator + hook registry are wired. claude-code resolves a set
    //       of watch paths from the user's `FileChanged` hook config (each
    //       hook's `matcher` is a pipe-separated filename list,
    //       `fileChangedWatcher.ts:48-65`), watches them, and on every debounced
    //       `change` / `add` / `unlink` fires the `FileChanged` hook with the
    //       path + chokidar event name (`handleFileEvent` →
    //       `executeFileChangedHooks`, `utils/hooks.ts:4278`). The Rust port had
    //       no watcher; this wires it at the composition root via the in-tree
    //       `notify`-backed `FileSystem::watch` primitive (`platform-posix`'s
    //       `watch_helper`), exactly as the settings watcher (7.2) does.
    //
    //       The firer is the `OrchestratorFileChangedFirer` over the SAME
    //       `Arc<HookExecutorImpl>` the orchestrator fires its other hooks
    //       through (mirrors the `CwdChanged` / task firers), so the watcher
    //       reaches `orch.hooks` without a dependency cycle. Best-effort: a
    //       failing/blocking `FileChanged` hook never breaks the watch loop.
    //
    //       An empty matcher set starts an idle supervisor with zero OS watch
    //       handles; its control channel remains available for hot reload.
    let matcher_refs: Vec<&str> = file_changed_matchers.iter().map(String::as_str).collect();
    let watcher =
        file_changed_watch::FileChangedWatcher::new(&matcher_refs, &watch_cwd, file_changed_firer);
    #[cfg(not(test))]
    let watch_fs: Arc<dyn platform_api::FileSystem> =
        Arc::new(PosixFileSystem::new(watch_cwd.clone()));
    #[cfg(test)]
    let watch_fs: Arc<dyn platform_api::FileSystem> =
        Arc::new(watcher_test_support::WatchFs::new(watch_cwd.clone()));
    let file_changed_watcher = watcher.spawn(watch_fs).await;
    // Fill the `CwdChanged` firer's deferred rebinder cell now that the watcher
    // exists (it spawns AFTER the firer is built). On a mid-session `cd` the
    // firer rebinds this watcher — the watcher-rebind half of `onCwdChanged`.
    // The idle supervisor also yields a rebinder, so future live-installed
    // matchers follow cwd changes without a restart.
    if let Some(rebinder) = file_changed_watcher.rebinder() {
        file_changed_watcher_rebinder.set(rebinder);
    }

    // Phase 2a §6.2: `provider_availability` is computed EARLY in build() (the
    // connected-provider default-model fallback consults it before the
    // orchestrator exists) and reused verbatim here for the `/model` picker's
    // Connect badge — nothing between the two points mutates credentials.

    // T2a: per-profile login-method tag derived from the builtin catalog auth
    // strategy.  AuthStrategy::None providers are not connectable → skipped.
    let mut provider_auth_methods: std::collections::BTreeMap<String, String> =
        llm_runtime::builtin_presets()
            .providers
            .iter()
            .filter_map(|p| {
                use llm_runtime::AuthStrategy::*;
                let tag = match p.auth {
                    ApiKey | Bearer => "api_key",
                    CopilotBearer => "copilot_device",
                    ChatGptOAuth | OAuthBearer | AwsSigV4 | GcpToken | AzureToken => "oauth",
                    None => return Option::None,
                };
                Some((p.profile_name.clone(), tag.to_string()))
            })
            .collect();
    // Anthropic is not in the builtin catalog presets (it is the native auth
    // path), but the /connect picker still needs it represented with its auth
    // method tag ("api_key") — mirror the provider_availability approach above.
    provider_auth_methods
        .entry("anthropic".to_string())
        .or_insert_with(|| "api_key".to_string());

    if let (Some(scheduler), Some((config, permissions))) = (&cron_scheduler, native_cron_seed) {
        if orch.workspace_trusted().await && cron::scheduled_tasks_path(&config.cwd).exists() {
            cron::automation::recover_orphaned_automation_runs(
                &PosixFileSystem::new(config.cwd.clone()),
                &config.cwd,
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
            )
            .await
            .map_err(BuildError::DurableSession)?;
        }
        scheduler
            .set_automation_firer(Arc::new(cron_native::NativeCronFirer::new(
                config,
                permissions,
                Arc::downgrade(&orch),
            )))
            .await;
    }
    let session_lifecycle = Arc::new(DesktopSessionLifecycle {
        ephemeral_home: ephemeral_home.clone(),
        settings_watcher: settings_watcher.clone(),
        file_changed_watcher: file_changed_watcher.clone(),
        cron_scheduler,
        orchestrator: orch.clone(),
        mcp_reconnect_task: tokio::sync::Mutex::new(Some(mcp_reconnect_task)),
        mcp_catalog_refresh_task: tokio::sync::Mutex::new(Some(mcp_catalog_refresh_task)),
        task_registry: task_registry.clone(),
        command_registry: shared_command_registry.clone(),
        subagent_spawner: lifecycle_subagent_spawner,
        fusion_api_service: api_service,
        cost_tracker,
        session_state_manager,
        fusion_recorder_factory: fusion_recorder_factory_impl.clone(),
        fusion_recovery_task: tokio::sync::Mutex::new(fusion_recovery_task),
    });

    Ok(DesktopRuntime {
        catalog_registry,
        provider_region,
        orchestrator: orch,
        session_state,
        fusion_recorder,
        fusion_recorder_factory: fusion_recorder_factory_impl,
        session_lifecycle,
        analytics_bus,
        shared_command_registry,
        dispatcher,
        auth,
        task_registry,
        #[cfg(test)]
        wired_workflow_tool: wired_workflow_tool.expect("desktop Workflow tool is registered"),
        coordinator,
        coordinator_mode,
        permission_gate: adapter_gate,
        enforcing_permission_gate,
        settings_watcher,
        file_changed_watcher,
        repo_root_reloader: repo_root_reloader.clone(),
        subscription,
        sandbox_toggle,
        sandbox_desc_auto_allow,
        sandbox_desc_fallback,
        sandbox_desc_deps_ok,
        file_history,
        plugin_runtime,
        hook_registry: hook_registry.clone(),
        provider_availability,
        default_model_fallback,
        model_provenance,
        provider_auth_methods,
        model_providers,
        provider_adapter: provider_adapter_handle,
        credentials,
        http: http.clone() as Arc<dyn platform_api::HttpTransport>,
        structured_output_slot,
        wakeup_scheduler_cell,
        runtime_spawner: Arc::new(PosixRuntime::new()) as Arc<dyn platform_api::RuntimeSpawner>,
        bash_runner,
        shell_expansion: shell_expansion_provider,
        connect_copilot,
        oauth_connect_driver,
        // P1-08 runtime `/add-dir` live-effect handles (captured before the
        // orchestrator builder consumed the originals).
        session_cwd: runtime_session_cwd,
        mcp_registry: runtime_mcp_registry,
        ide_handle,
        workflow_events: Some(workflow_event_rx),
        tools: runtime_tools,
        audio: desktop_audio,
    })
}

pub(super) fn resolve_workspace_trust(
    host_trusted: Option<bool>,
    cwd: &std::path::Path,
    global_config_path: Option<&std::path::Path>,
) -> bool {
    host_trusted.unwrap_or_else(|| {
        global_config_path
            .map(|path| migrations::global_config::check_has_trust_dialog_accepted(path, cwd))
            .unwrap_or(false)
    })
}
