use crate::mobile::{
    local_apps_host::{LocalAppsAgentExecutor, LocalAppsHostBroker},
    local_apps_llm::{ApiServiceModel, LocalAppsLlm},
    local_apps_mcp::{LocalAppsMcpTransport, LOCAL_APPS_REGISTRY_KEY},
    local_apps_profile::profile_apps,
    mcp_transport::MobileMcpTransport,
    mobile_command_registry, register_android_ui_automation,
    turn_durability::DurableTurnStore,
};
use client::adapter::{
    AdapterOutputStream, AdapterPermissionGate, ClientEventListener, ListenerSink,
    PermissionRequestSink,
};
use client::protocol::events::ClientEvent;
use command_api::model::BuiltinCommandHandler;
use command_api::parse_slash_command;
use command_api::RegistrySlashDispatcher;
use lingxi_core::host::{AuthHandle, OrchestratorHandle, OutputStream, Platform};
use lingxi_llm_client::auth::oauth::anthropic::ClaudeAiOAuthConfig;
use lingxi_llm_client::auth::oauth::openai::OpenAiOAuthConfig;
use llm_runtime::auth::anthropic::{OAuthCredentialProvider, OAuthHandle, RefreshDriver};
use llm_runtime::auth::openai as openai_oauth;
use llm_runtime::{CredentialProvider, ModelRuntime, Transport};
use mcp::registry::OAuthDeps;
use mcp::{ConfigScope as McpConfigScope, McpRegistry, McpServerConfig, RawConnectionProvider};
use orchestrator::model::user_agent::UserAgentEnv;
use orchestrator::provider_adapter::SubscriberState;
use orchestrator::test_support::StaticMemoryProvider;
use orchestrator::{
    ConversationOrchestrator, OrchestratorApiClient, OrchestratorConfig, ProviderApiAdapter,
    StreamingApiClient,
};
use permission::gate::PermissionGate;
use permission::PermissionMode;
use sandbox::runtime_config::{Platform as SandboxPlatform, SandboxRuntimeConfig};
use secret::CredentialManager;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::{Mutex, RwLock};
use tool_api::SessionCwd;
use tool_api::{BuiltinToolContext, ToolRegistry};

use super::{
    anthropic_models, apply_mobile_profile_allowlist, boot_backfill_sweep_should_run,
    build_mobile_runtime_environment, build_mobile_subagent_env_renderer, fast_mode_preference,
    gate_mobile_git_ctx, gate_mobile_shell_ctx, mint_app_init_session, mobile_apps_data_root,
    mobile_builtin_plugin_enabled_from_settings, mobile_launch_is_interactive,
    mobile_local_app_scope_id, mobile_mcp_oauth_authorization_callback, mobile_mcp_preflight,
    mobile_mcp_record_reload_intent, mobile_mcp_run_reload_job, mobile_provider_settings,
    mobile_reload_skills_handler, mobile_settings_write_lock, mobile_skill_listing_provider,
    mobile_typescript_lsp_mode, mobile_typescript_lsp_ready, model_listings, model_preference,
    model_visible_mobile_cwd, permission_preference, provider_model_catalog_from_listings,
    resolve_default_model_ref, run_app_boot_backfill_sweep, settings_commands,
    settle_mobile_loop_turn, ActiveTurn, MobileAppAgentExecutor, MobileBuildError, MobileConfig,
    MobileEngineError, MobileEngineHandle, MobileMcpReloadJob, MobileMsgQueueInput,
    MobileOAuthManager, MobileRuntime, MobileSessionAgentObserver, MobileWakeupDelivery,
    TurnLifecycleListener, LOCAL_APPS_MCP_TIMEOUT_MS, MOBILE_CRON_HANDLES,
};

/// Build a fully-wired mobile [`MobileRuntime`] from a deterministic
/// [`MobileConfig`] + an `Arc<dyn Platform>` (plan F3-03 — the mobile sibling of
/// `harness_runtime::desktop::build`).
///
/// Off-device-deterministic: no `std::env` / argv reads. The OS handles
/// (filesystem / http / clock / process / sandbox / worktree) and the device
/// capabilities (camera / audio / share) are read from `platform`; everything
/// else arrives via `cfg`. The `listener` becomes the adapter's
/// [`client::adapter::ClientEventSink`] (wrapped in a [`ListenerSink`]) so every
/// translated [`client::protocol::events::ClientEvent`] is delivered to the
/// foreign host; `permission_sink` is where the [`AdapterPermissionGate`]'s
/// outbound permission requests go.
///
/// Engine behavior is preserved verbatim (spec §1 non-goal): only the *source*
/// of each input moved from env to `cfg`/`platform`, and the output / permission
/// sinks become connection-scoped parameters — mirroring the F2-01 desktop lift.
///
/// # Errors
///
/// Returns [`MobileBuildError`] if the api-client or orchestrator cannot be
/// constructed (effectively infallible in the current wiring).
pub async fn build_mobile(
    cfg: MobileConfig,
    platform: Arc<dyn Platform>,
    listener: Arc<dyn ClientEventListener>,
    permission_sink: Arc<dyn PermissionRequestSink>,
) -> Result<MobileRuntime, MobileBuildError> {
    build_mobile_inner(cfg, platform, listener, permission_sink, None).await
}

/// As [`build_mobile`], but allows a test to substitute the streaming client.
///
/// Production callers use [`build_mobile`] (`streaming_override == None`), which
/// wires the same router-backed [`ProviderApiAdapter`] for BOTH the batched and
/// the streaming paths — a mobile client always streams its turns. The off-device
/// walking-skeleton test (plan F3-06) passes a scripted
/// [`orchestrator::test_support_stream::MockStreamingApiClient`] here so the turn
/// is deterministic without a network — exactly the bridge-server e2e pattern,
/// which builds the orchestrator with the same mock. Engine behavior is unchanged
/// either way: only the *source* of the stream's bytes differs.
#[doc(hidden)]
// A cohesive composition root: the 8 numbered build steps below read as one
// linear sequence; splitting it only to satisfy the line cap would scatter them.
#[allow(clippy::too_many_lines)]
pub async fn build_mobile_inner(
    cfg: MobileConfig,
    platform: Arc<dyn Platform>,
    listener: Arc<dyn ClientEventListener>,
    permission_sink: Arc<dyn PermissionRequestSink>,
    streaming_override: Option<Arc<dyn StreamingApiClient>>,
) -> Result<MobileRuntime, MobileBuildError> {
    build_mobile_inner_with_ask(
        cfg,
        platform,
        listener,
        permission_sink,
        streaming_override,
        None,
    )
    .await
}

#[allow(clippy::too_many_lines)]
pub(super) async fn build_mobile_inner_with_ask(
    cfg: MobileConfig,
    platform: Arc<dyn Platform>,
    listener: Arc<dyn ClientEventListener>,
    permission_sink: Arc<dyn PermissionRequestSink>,
    streaming_override: Option<Arc<dyn StreamingApiClient>>,
    ask_user_question_tx: Option<tokio::sync::mpsc::Sender<tool_ui::AskUserQuestionExchange>>,
) -> Result<MobileRuntime, MobileBuildError> {
    let cwd = cfg.cwd.clone();
    settings_commands::prepare_mobile_mcp_storage(&cfg.lingxi_home).map_err(|error| {
        MobileBuildError::Orchestrator(format!("MCP settings migration: {error}"))
    })?;
    // Resolve all platform handles before constructing or connecting the MCP
    // registry. This is intentionally a preflight boundary: an OAuth config
    // must be rejected before any remote dial (or plaintext credential access)
    // when the host did not provide encrypted storage.
    let http = platform.http();
    let clock = platform.clock();
    let fs = platform.filesystem();
    let storage: Arc<dyn lingxi_core::host::SecureStorage> = platform
        .secure_storage()
        .unwrap_or_else(|| Arc::new(platform_posix_minimal::PlainTextSecureStorage::new()));
    let oauth_supported = lingxi_core::host::SecureStorage::is_encrypted(storage.as_ref());

    let local_apps_mcp = Arc::new(LocalAppsMcpTransport::new(mobile_apps_data_root(&cfg)));
    let _ = local_apps_mcp.attach_lingxi_home(cfg.lingxi_home.clone());
    let remote_mcp = Arc::new(platform_common::RemoteMcpTransport::new());
    let mobile_mcp = Arc::new(MobileMcpTransport::new(local_apps_mcp.clone(), remote_mcp));
    let mcp_auth_url = Arc::new(StdMutex::new(None::<String>));
    let mcp_auth_callback =
        mobile_mcp_oauth_authorization_callback(mcp_auth_url.clone(), platform.deep_link());
    let mut mcp_registry = McpRegistry::with_raw_conn(
        mobile_mcp.clone() as Arc<dyn lingxi_core::host::McpTransport>,
        mobile_mcp.clone() as Arc<dyn RawConnectionProvider>,
    )
    .with_headers_helper_cwd(cwd.clone())
    .with_discovery_cache_store(mcp::DiscoveryCacheStore::new(
        cfg.lingxi_home.join("mcp-discovery-cache"),
    ));
    if oauth_supported {
        mcp_registry = mcp_registry.with_oauth(OAuthDeps {
            http: http.clone(),
            clock: clock.clone(),
            storage: storage.clone(),
            on_authorization_url: mcp_auth_callback,
            xaa_config: None,
        });
    }
    let mcp_registry = Arc::new(mcp_registry);
    local_apps_mcp
        .attach_registry(Arc::downgrade(&mcp_registry))
        .map_err(|_| {
            MobileBuildError::Orchestrator(
                "local apps MCP registry was already attached during bootstrap".into(),
            )
        })?;
    // Subscribe before connecting so initialization-time catalog notifications
    // are retained until the shared ToolRegistry is ready below.
    let mut mcp_catalog_changes = mcp_registry.subscribe_catalog_changes();
    mcp_registry
        .connect(McpServerConfig {
            name: LOCAL_APPS_REGISTRY_KEY.into(),
            spec: lingxi_core::host::McpTransportSpec::InProcess {
                registry_key: LOCAL_APPS_REGISTRY_KEY.into(),
            },
            scope: McpConfigScope::Settings(lingxi_core::types::SettingsScope::Managed),
            disabled: false,
            timeout_ms: Some(LOCAL_APPS_MCP_TIMEOUT_MS),
            always_load: true,
            discovery_cache: None,
            tools: Vec::new(),
            tool_permissions: std::collections::BTreeMap::new(),
            config_error: None,
            metadata: Default::default(),
        })
        .await
        .map_err(|error| {
            MobileBuildError::Orchestrator(format!("local apps MCP bootstrap failed: {error}"))
        })?;
    // Keep iOS/Android MCP discovery on the same parser and precedence rules
    // as desktop. The app-private settings file is the mobile equivalent of
    // the user global config; `.mcp.json` remains project-scoped.
    let configured_mcp = mobile_mcp_preflight(
        mcp::load_mcp_servers(
            &cwd.join(".mcp.json"),
            &cfg.lingxi_home.join("mcp-config.json"),
            &cwd,
        ),
        oauth_supported,
    );
    // Keep the config-side policy declarations while startup connections are
    // driven through the guarded, nonblocking mobile reconciliation path.
    let configured_mcp_policy_rules: Vec<permission::PermissionRule> = configured_mcp
        .iter()
        .flat_map(|server| {
            permission::permission_rules_from_mcp_tool_policies(&server.name, &server.tools)
        })
        .collect();
    let mcp_reload_generations = Arc::new(StdMutex::new(HashMap::new()));
    let startup_jobs: Vec<MobileMcpReloadJob> = configured_mcp
        .into_iter()
        .map(|config| {
            let name = config.name.clone();
            let (generation, _) =
                mobile_mcp_record_reload_intent(&mcp_reload_generations, &name, Some(&config));
            MobileMcpReloadJob::Connect {
                name,
                generation,
                desired: config,
                previous: None,
            }
        })
        .collect();
    // Remote startup, especially an interactive OAuth flow, may legitimately
    // wait for the user for several minutes. Never hold the mobile engine
    // constructor open for that interaction: Local Apps is ready synchronously
    // above, while configured MCP connections continue on the owned runtime and
    // publish their catalog through the subscription below.
    let configured_mcp_registry = mcp_registry.clone();
    let startup_generations = mcp_reload_generations.clone();
    tokio::spawn(async move {
        futures_util::future::join_all(startup_jobs.into_iter().map(|job| {
            mobile_mcp_run_reload_job(
                job,
                configured_mcp_registry.clone(),
                startup_generations.clone(),
            )
        }))
        .await;
    });

    // (1) OS handles from the aggregate `Platform` (NOT a concrete posix type —
    //     the device supplies these; the host test supplies a portable shim).
    let main_session_id = lingxi_core::types::SessionId::new();
    let main_session_uuid = main_session_id.as_uuid().to_string();
    // v3 Phase 3 (MCP create 收权): the LIVE current-session uuid, updated on
    // every New/Resume/Clear retarget. The local-apps MCP `create` stamps an
    // app's origin `conversation_id` from THIS cell — model input is never
    // trusted for it.
    let active_session_uuid = Arc::new(std::sync::Mutex::new(main_session_uuid.clone()));
    // The plans directory derivation is shared with the orchestrator's plan-mode
    // reminder rather than re-derived here, so the path the model is told to
    // write, the path the permission carve-out allows, and the path
    // `ExitPlanMode` reads back cannot diverge. Mobile sets no `plansDirectory`,
    // so the directory is the project-local default (`<cwd>/.lingxi/plans/`).
    // 2.1.266 `getPlanSlug`: the plan file is named by a random three-word slug
    // (`brave-quiet-otter.md`), re-rolled on collision, NOT by the session id.
    // Upstream can seed it from the transcript (`planSlugSeed`); LingXi has no
    // seed source, so it takes the unseeded form.
    let plans_dir = orchestrator::ConversationOrchestrator::plans_dir(&cwd, None);
    let plan_files = Arc::new(permission::plan_files::PlanFileMatcher::with_identity(
        permission::plan_files::PlanFileIdentity {
            slug: lingxi_core::host::plan_slug::generate_slug(None, &|candidate| {
                lingxi_core::host::plan_slug::slug_taken_in(&plans_dir, candidate)
            }),
            plans_dir: plans_dir.clone(),
            // `ZUe()` — LingXi ships no workshop skill.
            workshop_enabled: false,
        },
    ));
    {
        let cell = active_session_uuid.clone();
        let _ = local_apps_mcp.attach_session_provider(Arc::new(move || {
            cell.lock().ok().map(|guard| guard.clone())
        }));
    }
    // ── Plan-approval record ───────────────────────────────────────────────
    // `LocalAppPrepare` may only land a template for a plan the USER approved,
    // and the engine's only writer of "approved" is `ExitPlanMode`'s success
    // branch. That branch's structured result reaches this listener as a
    // `ToolUseResult`, so the observation rides the same connection-scoped
    // listener the adapter sinks already use — no permission-gate decorator, no
    // second session read. See `crate::mobile::plan_approval`.
    let plan_approval_log = Arc::new(crate::mobile::plan_approval::PlanApprovalLog::default());
    let observed_listener: Arc<dyn ClientEventListener> =
        Arc::new(crate::mobile::plan_approval::PlanApprovalWatcher::new(
            listener.clone(),
            plan_approval_log.clone(),
            active_session_uuid.clone(),
        ));
    {
        let log = plan_approval_log.clone();
        let _ = local_apps_mcp.attach_plan_approval_log(log);
    }
    // v3 Phase 4: the connection-scoped init-session minter — forks the
    // origin chat (this connection's cwd catalog) into the new app's
    // workspace catalog, or anchors an empty session.
    let _ = local_apps_mcp.attach_origin_cwd(cwd.to_string_lossy().to_string());
    {
        let minter_home = cfg.lingxi_home.clone();
        let minter_source_cwd = cwd.to_string_lossy().to_string();
        let minter_data_root = mobile_apps_data_root(&cfg);
        let minter_fs = fs.clone();
        let _ = local_apps_mcp.attach_init_session_minter(Arc::new(move |record| {
            let lingxi_home = minter_home.clone();
            let source_cwd = minter_source_cwd.clone();
            let data_root = minter_data_root.clone();
            let fs = minter_fs.clone();
            Box::pin(async move {
                mint_app_init_session(&lingxi_home, &source_cwd, &data_root, fs, &record).await
            })
        }));
    }
    let session_writer = Arc::new(session::jsonl::writer::JsonlWriter::new(
        orchestrator::transcript_paths::main_transcript_path(
            &cfg.lingxi_home,
            &cwd.to_string_lossy(),
            &main_session_uuid,
        ),
        fs.clone(),
    ));
    let mobile_linux = platform.mobile_linux();
    let mobile_linux_capability = match mobile_linux.as_ref() {
        Some(runtime) => Some(runtime.probe_capability().await),
        None => None,
    };
    let process = platform.process();
    let sandbox = platform.sandbox();
    let worktree = platform.worktree();
    // Build the shared credential manager and provider-specific OAuth handles
    // before assembling the client. This lets a native Keychain session restore
    // into the live provider graph on every engine boot.
    let credentials = Arc::new(CredentialManager::new(storage, clock.clone(), http.clone()));
    let llm_transport: Arc<dyn Transport> = Arc::new(
        platform_common::provider_transport()
            .map_err(|e| MobileBuildError::ApiBase(e.to_string()))?,
    );
    let anthropic_oauth_config = ClaudeAiOAuthConfig::default_with_port(0);
    let anthropic_oauth_handle = Arc::new(OAuthHandle::new(
        anthropic_oauth_config.clone(),
        llm_transport.clone(),
        credentials.clone(),
        clock.clone(),
    ));
    let openai_oauth_config = OpenAiOAuthConfig::default();
    let openai_oauth_handle = Arc::new(
        openai_oauth::OpenAiOAuthHandle::new(
            openai_oauth_config.clone(),
            llm_transport.clone(),
            credentials.clone(),
        )
        .with_clock(clock.clone()),
    );
    let anthropic_refresh_spawner: Arc<dyn lingxi_core::host::RuntimeSpawner> =
        Arc::new(platform_posix_minimal::PosixRuntime::new());
    let openai_refresh_spawner: Arc<dyn lingxi_core::host::RuntimeSpawner> =
        Arc::new(platform_posix_minimal::PosixRuntime::new());

    let anthropic_oauth_state = match credentials.get_oauth_tokens().await {
        Ok(Some(tokens)) => {
            match llm_runtime::auth::anthropic::login::init_refresh_driver(
                anthropic_oauth_config,
                tokens.access_token,
                tokens.refresh_token,
                tokens.expires_at,
                llm_transport.clone(),
                clock.clone(),
                None,
                Some(credentials.clone()),
                anthropic_refresh_spawner.clone(),
            )
            .await
            {
                Ok(state) => Some(state),
                Err(error) => {
                    tracing::warn!(%error, "failed to restore Anthropic OAuth session");
                    None
                }
            }
        }
        Ok(None) => None,
        Err(error) => {
            tracing::warn!(%error, "could not read Anthropic OAuth session");
            None
        }
    };
    let openai_oauth_state = match credentials.get_openai_oauth_tokens().await {
        Ok(Some(tokens)) => match openai_oauth::login::init_refresh_driver(
            openai_oauth_config,
            tokens.access_token,
            tokens.refresh_token,
            tokens.expires_at,
            tokens.account_id,
            tokens.fedramp,
            tokens.email,
            llm_transport.clone(),
            clock.clone(),
            None,
            Some(credentials.clone()),
            openai_refresh_spawner.clone(),
        )
        .await
        {
            Ok(state) => Some(state),
            Err(error) => {
                tracing::warn!(%error, "failed to restore OpenAI ChatGPT OAuth session");
                None
            }
        },
        Ok(None) => None,
        Err(error) => {
            tracing::warn!(%error, "could not read OpenAI ChatGPT OAuth session");
            None
        }
    };
    // Audit fix (telemetry parity): ONE shared AnalyticsBus drives the whole
    // pipeline — the `ApiService` (so `tengu_api_*` events are not dropped), the
    // tool context (`tool_ctx.bus`), and the orchestrator (`.with_analytics_bus`)
    // — instead of the prior split where a private bus served only the tools and
    // the ApiService got `None`. Mirrors the desktop root's single logEvent sink.
    let analytics_bus = Arc::new(telemetry::AnalyticsBus::new());

    // Model networking is owned by the shared SDK.
    //
    //      Phase 2a-mobile: assemble the FULL multi-provider client config
    //      (Anthropic + builtin catalog presets + settings `providers`) + chains
    //      + credential sources + pricing catalog via `provider_config::assemble`,
    //      mirroring `harness_runtime::desktop::build`. Restored OAuth sessions are wired
    //      through the provider credential ids below. Anthropic's API-key flag
    //      intentionally remains true when both credentials exist because the
    //      shared assembler gives API Key precedence over OAuth.
    let stored_anthropic_key = credentials.get_anthropic_api_key().await.ok().flatten();
    let has_api_key = !cfg.api_key.trim().is_empty() || stored_anthropic_key.is_some();
    let has_anthropic_oauth = anthropic_oauth_state.is_some();
    let provider_settings = mobile_provider_settings(&cfg)
        .map_err(|error| MobileBuildError::Orchestrator(format!("provider settings: {error}")))?;
    let vision_delegation_enabled = provider_settings
        .vision_delegation_enabled
        .unwrap_or(cfg.vision_delegation_enabled);
    let provider_region = match provider_settings.provider_region.unwrap_or_default() {
        lingxi_core::settings::ProviderRegion::ChinaMainland => llm_runtime::Region::ChinaMainland,
        lingxi_core::settings::ProviderRegion::International => llm_runtime::Region::International,
    };
    let mut assembled = provider_config::assemble_for_region(
        provider_config::AssembleInputs {
            anthropic_api_base: cfg.api_base.clone(),
            anthropic_models: anthropic_models(&cfg.default_model),
            anthropic_has_api_key: has_api_key,
            anthropic_has_oauth: has_anthropic_oauth,
            user_providers: provider_settings.providers.unwrap_or_default(),
            routing: provider_settings.routing.clone(),
        },
        provider_region,
    );
    let full_provider_model_catalog =
        provider_model_catalog_from_listings(&model_listings(&assembled.client_config.providers));
    apply_mobile_profile_allowlist(&mut assembled, provider_settings.routing.as_ref());
    for w in &assembled.warnings {
        tracing::warn!(warning = %w, "provider-config assembly (mobile)");
    }

    // TPM-C (mobile): resolve an optional `profile/model` qualifier in the
    // configured default_model so a shared id routes deterministically on the
    // first turn (mirror of harness-runtime::desktop). Must run while
    // `assembled.client_config.providers` is still owned (before `from_config`
    // moves it). `display_model`/`provider_label` are immaterial to parsing, so
    // we reuse `request_model` / the profile name for both fields.
    let default_listings = model_listings(&assembled.client_config.providers);
    let interactive_launch = mobile_launch_is_interactive(cfg.host_environment.as_ref());
    let saved_model = interactive_launch
        .then(|| model_preference::load(&cfg.lingxi_home))
        .flatten();
    let saved_fast_mode = interactive_launch
        .then(|| fast_mode_preference::load(&cfg.lingxi_home))
        .flatten();
    let (default_model_id, default_model_profile) = saved_model
        .as_deref()
        .and_then(|model| model_preference::resolve(model, &default_listings))
        .unwrap_or_else(|| resolve_default_model_ref(&cfg.default_model, &default_listings));
    let profile_auto_mode_provider: std::collections::BTreeMap<String, String> = assembled
        .client_config
        .providers
        .iter()
        .map(|profile| {
            let provider = match &profile.provider_id {
                llm_runtime::ProviderId::AnthropicFirstParty => "firstParty",
                llm_runtime::ProviderId::BedrockClaude => "anthropicAws",
                llm_runtime::ProviderId::VertexClaude => "vertex",
                llm_runtime::ProviderId::FoundryClaude => "foundry",
                _ => "other",
            };
            (profile.profile_name.clone(), provider.to_string())
        })
        .collect();
    let model_provider_profiles: std::collections::BTreeMap<String, String> = assembled
        .client_config
        .providers
        .iter()
        .flat_map(|profile| {
            let profile_name = profile.profile_name.clone();
            profile
                .models
                .iter()
                .map(move |model| (model.request_model.clone(), profile_name.clone()))
        })
        .collect();
    let boot_auto_mode_provider = default_model_profile
        .as_ref()
        .or_else(|| model_provider_profiles.get(&default_model_id))
        .and_then(|profile| profile_auto_mode_provider.get(profile))
        .cloned()
        .unwrap_or_else(|| "firstParty".to_string());

    let mut client = ModelRuntime::from_config(assembled.client_config)
        .map_err(|e| MobileBuildError::ApiBase(format!("llm-runtime config: {e}")))?;
    // §6.1: ONE composite credential slot for ALL providers. OAuth delegates
    // serve `anthropic-oauth` and `openai-chatgpt` without exposing tokens to
    // Swift; API-key profiles retain the existing keychain → env fallback.
    let mut oauth_delegates: std::collections::BTreeMap<String, Arc<dyn CredentialProvider>> =
        std::collections::BTreeMap::new();
    let anthropic_refresh = anthropic_oauth_state
        .clone()
        .map(|state| Arc::new(RefreshDriver::new(state)));
    let openai_refresh = openai_oauth_state
        .clone()
        .map(|state| Arc::new(openai_oauth::RefreshDriver::new(state)));
    let oauth = Arc::new(MobileOAuthManager::new(
        anthropic_oauth_handle.clone(),
        openai_oauth_handle.clone(),
        anthropic_refresh.clone(),
        openai_refresh.clone(),
        anthropic_oauth_state
            .as_ref()
            .map(|_| anthropic_refresh_spawner.clone()),
        openai_oauth_state
            .as_ref()
            .map(|_| openai_refresh_spawner.clone()),
        http.clone(),
    ));
    if !has_api_key {
        if let Some(driver) = anthropic_refresh {
            oauth_delegates.insert(
                "anthropic-oauth".to_string(),
                Arc::new(OAuthCredentialProvider::new(driver)) as Arc<dyn CredentialProvider>,
            );
        }
    }
    if let Some(driver) = openai_refresh {
        oauth_delegates.insert(
            "openai-chatgpt".to_string(),
            Arc::new(openai_oauth::OpenAiOAuthCredentialProvider::new(driver))
                as Arc<dyn CredentialProvider>,
        );
    }
    let composite = provider_config::MultiCredentialProvider::new(
        credentials.clone(),
        assembled.credential_sources.clone(),
        if !cfg.api_key.trim().is_empty() {
            Some(cfg.api_key.clone())
        } else {
            None
        },
        None,
        oauth_delegates,
    );
    client = client.with_credential_provider(Arc::new(composite));
    let llm_runtime = Arc::new(client);

    // No live subscription slot on mobile (no OAuth profile fetch) — static state stands.
    let subscriber_state = SubscriberState {
        is_subscriber: false,
        is_enterprise: false,
    };

    // 3c-T3: build the cost estimator from the assembled pricing catalog so
    // LlmResponse.cost is populated on every successful decode. The catalog
    // already carries the built-in reference tiers + non-Anthropic preset rows +
    // any settings per-profile pricing overrides folded in by `assemble`. Unpriced
    // / unknown models leave cost = None (never an error).
    let cost_estimator = {
        use llm_runtime::{CostEstimator, PricingPolicy};
        use orchestrator::cost_wiring::llm_catalog_from_cost;
        let llm_cat = llm_catalog_from_cost(&assembled.pricing);
        Arc::new(CostEstimator::new(llm_cat, PricingPolicy::MarkUnestimated))
    };

    // Audit #15: session CostTracker (desktop parity). The `cost_estimator` above
    // populates per-response `LlmResponse.cost`; the CostTracker accumulates the
    // running SESSION total the orchestrator records each turn. The persist
    // channel is DRAINED by a spawned recv-loop that discards each `CostState` —
    // byte-for-byte mirroring harness-runtime::desktop (which also just drains it): mobile
    // has no on-disk cost persistence / `/cost` UI consumer yet, but wiring the
    // tracker keeps the accounting path 1:1 with desktop. Bind it to the
    // same boot session id as the orchestrator; clear/resume then switch the
    // active projection while late Fusion views retain their origin.
    let cost_tracker = {
        let (cost_persist_tx, mut cost_persist_rx) = tokio::sync::mpsc::channel(64);
        tokio::spawn(async move { while cost_persist_rx.recv().await.is_some() {} });
        Arc::new(cost::CostTracker::new(
            main_session_id,
            Arc::new(assembled.pricing),
            cost_persist_tx,
        ))
    };
    let api_calls_recorded = Arc::new(std::sync::atomic::AtomicU32::new(0));

    // Phase 2a-mobile CHAINS BRIDGE: translate the assembled `ChainConfig` into
    // main's richer adapter's `fallback_overrides` shape (same as harness-runtime::desktop —
    // we reuse main's `ProviderApiAdapter::new_with_routing`, NOT parity's leaner
    // `new`). `assemble` keys each chain by the request/display model id with an
    // ordered list of `ChainEntry`; main's adapter routes by model-id through the
    // multi-provider registry, so the informational `ChainEntry.provider_id` is
    // dropped here — the per-entry `model` ids are the fallback chain. Cross-
    // provider routing still resolves because every provider's models are
    // registered in the assembled `ClientConfig`. The adapter's own alias map is
    // rebuilt from the client's `available_models()` (whose aliases `assemble`
    // already populated from `chains.aliases`), so no separate alias pass here.
    let fallback_overrides: std::collections::BTreeMap<String, Vec<String>> = assembled
        .chains
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
    let settings_max_retries = assembled.chains.retry.as_ref().map(|r| r.max_attempts);
    let settings_backoff_ms = assembled.chains.retry.as_ref().map(|r| r.backoff_ms);

    // ONE adapter implements BOTH `OrchestratorApiClient` (batched) and
    // `StreamingApiClient` (the streaming turn path the mobile transport always
    // drives). Production wires it for both paths; a test may substitute the
    // streaming side via `streaming_override` (plan F3-06). Mobile is NOT a
    // subscriber (`SubscriberState::default()` — api-key-only inference), and
    // binds no live subscription slot / availability map / CostTracker (out of
    // scope; mobile parity did not).
    let api_service = Arc::new(
        llm_runtime::ApiService::new_with_routing(
            llm_runtime,
            llm_transport,
            subscriber_state,
            UserAgentEnv::from_process_env(),
            env!("CARGO_PKG_VERSION"),
            Some(analytics_bus.clone()), // audit fix: API events share the one bus
            None,
            Some(cost_estimator),
            fallback_overrides,
            settings_max_retries,
            settings_backoff_ms,
        )
        .with_interactive_session(interactive_launch),
    );
    // Task 9: the local-app generator's three LLM calls (author/plan/write
    // source) ride the SAME `api_service` — routing, auth, retry — as the
    // main conversation, via `ApiService::messages_create_side_query`
    // (the same forced-tool-call mechanism `sidequery::ProviderSideQueryClient`
    // uses below). `default_model_id`/`default_model_profile` are the bare
    // model id and provider profile `orch_cfg.model` itself is set from a few
    // lines down — the local-app generator has no separate model selection of
    // its own.
    let local_apps_llm = Arc::new(LocalAppsLlm::new(Arc::new(
        ApiServiceModel::new(
            api_service.clone(),
            default_model_id.clone(),
            default_model_profile.clone(),
            vision_delegation_enabled,
        )
        .with_cost_tracking(cost_tracker.clone(), api_calls_recorded.clone()),
    )));
    let fast_flag = Arc::new(AtomicBool::new(saved_fast_mode.unwrap_or(false)));
    let provider_adapter =
        Arc::new(ProviderApiAdapter::new(api_service.clone()).with_fast_mode(fast_flag.clone()));
    let api_client: Arc<dyn OrchestratorApiClient> = provider_adapter.clone();
    let streaming_api: Arc<dyn StreamingApiClient> =
        streaming_override.unwrap_or(provider_adapter.clone() as Arc<dyn StreamingApiClient>);
    // `/login` remains the Anthropic trait-shaped command. Provider settings
    // use `oauth` above so ChatGPT's account-shaped identity stays separate.
    let auth: Arc<dyn AuthHandle> = anthropic_oauth_handle.clone();

    // (4) Orchestrator config from `cfg` (was a host env/arg read).
    let persisted_reasoning_selection =
        command_api::builtins::effort::load_reasoning_default_selection_at(
            &cfg.lingxi_home.join("settings.json"),
        );
    let mut orch_cfg = OrchestratorConfig::default();
    orch_cfg.output_style = provider_settings.output_style.clone();
    orch_cfg.output_style_dirs = vec![
        cfg.lingxi_home.join("output-styles"),
        cfg.cwd.join(branding::DOT_DIR).join("output-styles"),
    ];
    lingxi_core::host::session_flags::set_show_thinking_summaries(
        provider_settings.show_thinking_summaries.unwrap_or(false),
    );
    lingxi_core::host::session_flags::set_task_output_max_chars(
        provider_settings.task_output_max_chars,
    );
    lingxi_core::host::session_flags::set_bash_output_max_chars(
        provider_settings.bash_output_max_chars,
    );
    // `settings.attribution` / `settings.includeCoAuthoredBy` — the git
    // attribution trailers, published at boot beside the output caps.
    lingxi_core::host::session_flags::set_attribution(
        provider_settings
            .attribution
            .as_ref()
            .and_then(|a| a.commit.clone()),
        provider_settings
            .attribution
            .as_ref()
            .and_then(|a| a.pr.clone()),
    );
    lingxi_core::host::session_flags::set_include_co_authored_by(
        provider_settings.include_co_authored_by,
    );
    lingxi_core::host::session_flags::set_include_git_instructions(
        provider_settings.include_git_instructions,
    );
    // Mobile is a transport host, not the CLI REPL. Keep main-query telemetry
    // on Claude Code's SDK source and never mark it as `--print`.
    orch_cfg.query_source = orchestrator::QUERY_SOURCE_SDK.to_string();
    orch_cfg.print = false;
    orch_cfg.is_tty = false;
    // TPM-C: use the bare id produced by parse_model_ref (strips a profile/
    // prefix when present, passes through unchanged for bare ids).
    orch_cfg.model.clone_from(&default_model_id);
    // Foreground mobile hosts have a live chat UI, so interactive launches
    // follow interactive semantics on BOTH axes the orchestrator distinguishes:
    // - `interactive_permissions` feeds the main loop's per-tool-call
    //   `is_non_interactive_session` (turn_loop's ToolUseContext options) —
    //   the gate `AskUserQuestion` checks before forwarding to the mounted
    //   questionnaire card. The default (`false`) made the tool refuse with
    //   "no live prompt UI" even though the card was wired. Permission asks
    //   themselves already forward through the adapter gate, which is exactly
    //   what this flag asserts a host can do.
    // - `interactive_session` is published by `ConversationOrchestrator::new`
    //   to the PROCESS-global session flag prompt builders read. This one
    //   constructor also serves the cron-fired throwaway runtime, so it must
    //   retain the mobile process's interactive value. The typed runtime
    //   environment independently suppresses interactive prompt guidance for
    //   scheduled-headless sessions without poisoning the live conversation.
    // A session without a prompt transport stays safe regardless of both
    // flags: its registry has no ask resolver (`ask_user_question_tx: None`
    // ⇒ `DefaultTimeoutResolver` refuses) and cron's permission sink
    // auto-denies.
    orch_cfg.interactive_session = true;
    orch_cfg.interactive_permissions = interactive_launch;

    // (5) Connection-scoped sinks — the mobile transport's analog of the
    //     bridge-server's WS writer:
    //     - the `listener` becomes the `ClientEventSink` (via `ListenerSink`)
    //       the `AdapterOutputStream` pushes turn events to;
    //     - the `permission_sink` receives the gate's outbound requests.
    //     Mobile binds the `AdapterPermissionGate` (no always-allow mode), then
    //     wraps it with a local `PolicyPermissionGate` so the core policy binds.
    let event_sink = ListenerSink::arc(observed_listener.clone());
    let message_output = AdapterOutputStream::new(event_sink.clone());
    let output: Arc<dyn OutputStream> = Arc::new(message_output.clone());

    // (3c) No `.with_persist` on mobile: a device session has no project
    // `.lingxi/settings.local.json` convention to write back to, so AllowAlways
    // stays session-only here (the desktop transport gate persists; this does not).
    let adapter_gate =
        Arc::new(AdapterPermissionGate::new(permission_sink).with_event_sink(event_sink.clone()));
    adapter_gate.set_session_id(Some(main_session_uuid.clone()));
    // Wrap the adapter gate with a local `PolicyPermissionGate` so the CORE
    // allow/deny/ask/defaultMode semantics bind on mobile too — claude-code
    // enforces ONE core policy on every host, not "the remote client is the
    // enforcement". The adapter gate stays the Ask-delegation transport: an
    // unresolved mutating Ask still forwards to the remote client, but local deny/
    // allow rules + defaultMode are honored regardless of what the client
    // replicates. Rules are loaded from the SAME project + user settings.json the
    // hook loader reads below (dedup-aware: on mobile `lingxi_home` can equal
    // `<cwd>/.claude`, so a colliding path is read once to avoid doubling rules).
    // Read(deny) → Grep/Glob search-exclude globs, resolved from the policy below
    // (empty when no Read-deny rule ⇒ unchanged default).
    let read_deny_exclude_globs: Vec<String>;
    // (P2-14) `settings.skipWebFetchPreflight` → WebFetch skips the domain-blocklist
    // preflight. Mobile has no `lingxi_core::settings::Settings::load` seam (no `engine`
    // dep), so it reads the key directly from the SAME settings.json tiers the perms
    // loop below reads, scalar-override (later tier wins). `false` by default.
    let mut skip_web_fetch_preflight = false;
    // (M-15) `settings.askUserQuestionTimeout` (`60s`/`5m`/`10m`/`never`) → the
    // AskUserQuestion resolver idle window. Read from the SAME settings.json tiers
    // as the perms loop below, scalar-override (later tier wins). `None` by default
    // (⇒ `never`, block on the user). Parsed into `AskUserQuestionTimeout` at
    // `tool_ui` registration.
    let mut ask_user_question_timeout: Option<String> = None;
    // (M-03) `settings.disableAgentView` → the agent-view fork/subtask surface is
    // disabled exactly like `CLAUDE_CODE_DISABLE_AGENT_VIEW=1` (binary `I2i()`).
    // Read from the SAME settings.json tiers as the perms loop below,
    // scalar-override (later tier wins). `false` by default (agent view enabled;
    // the env half still applies independently). Threaded into
    // `register_core_batch_8` via `lingxi_core::host::agent_view::is_enabled_with_setting`.
    let mut disable_agent_view = false;
    // `agentPushNotifEnabled` scalar override (user → project → local). The
    // feature flag is checked independently by the cron/tool consumers.
    let mut agent_push_notif_enabled = false;
    // `taskOutputMaxChars` scalar override (user → project → local). Absent
    // leaves `TASK_MAX_OUTPUT_LENGTH` in charge; `tool_task` applies the
    // oracle's 4_000..=128_000 clamp when it reads this.
    let mut task_output_max_chars: Option<u32> = None;
    // `workflowSizeGuideline` scalar override (user → project → local). Absent
    // stays at Claude Code's built-in medium default; an explicit "medium"
    // remains explicit (is_default = false).
    let mut workflow_size_guideline = tool_workflow::WorkflowSizeGuideline::Medium;
    let mut workflow_size_guideline_is_default = true;
    // `enableWorkflows` scalar override (user → project → local). Absent stays
    // enabled, matching Claude Code's default-on session gate when no launch /
    // experiment policy disables it.
    let mut workflow_session_enabled = true;
    // (#3 shell-expansion) Capture the boot `Arc<PermissionPolicy>` before it is
    // consumed by `PolicyPermissionGate::new`, so `tool_ctx.permission_policy`
    // shares the SAME base policy the model-facing gate enforces (the prompt
    // shell-expansion provider reads it as the base for embedded `!`cmd`` bodies).
    let boot_permission_policy: Option<Arc<permission::PermissionPolicy>>;
    // H-CHG-02: capture the enforcing gate's set-once LIVE-model cell (cycle-break),
    // filled once the orchestrator (owner of the live `session.model`) exists, so
    // the live `set_permission_mode` auto gate evaluates `dUe(wi())` against the
    // CURRENT model — mirrors the desktop composition root.
    let loop_classifier_cell;
    let live_model_provider_cell: Option<Arc<std::sync::OnceLock<permission::LiveModelProvider>>>;
    // Published after settings + the auto availability gate resolve.  The
    // value is reused by subagent/workflow composition and the tool context
    // so every surface reports the same effective mode.
    let resolved_permission_mode;
    // (2.1.263 `bs(Rn)`) Spawn-time bypass clamps for subagent definitions,
    // published from the settings fold below like `resolved_permission_mode`.
    // `restricted` is always false on mobile: there is no `--restricted` flag.
    // `confined` is read ONCE here rather than inside the clamp — a gate that
    // reads `CLAUDE_CODE_EVAL_CONFINED` itself makes a parallel suite flaky.
    let mut subagent_bypass_gates = agent::permission_mode::SpawnBypassGates {
        confined: lingxi_core::host::env::is_eval_confined_session(),
        bypass_disabled: false,
        restricted: false,
    };
    let mut requested_permission_mode;
    let workspace_leases = permission::WorkspacePermissionLeaseRegistry::new();
    // ONE derivation of the (host, guest) workspace pairing. `model_cwd` below
    // is rebuilt from THIS binding rather than re-scanning the mount table —
    // two derivations of one root is exactly how the guest/host split forked
    // in the first place.
    let mobile_linux_mounts = mobile_linux
        .as_ref()
        .map(|runtime| runtime.current_mounts())
        .unwrap_or_default();
    // GUEST-COORD: the permission roots must anchor on the workspace the model
    // actually writes into. Uniqueness is a SECURITY precondition once the
    // permission root rides on this — the iOS bridge appends requested mounts
    // with their own `purpose` verbatim, so a second Workspace mount is
    // reachable and a bare `.find()` would silently take table order. With 0 or
    // >1 we keep the engine cwd, i.e. today's behavior.
    let workspace_mounts: Vec<_> = mobile_linux_mounts
        .iter()
        .filter(|m| matches!(m.purpose, mobile_linux_api::MountPurpose::Workspace))
        .collect();
    // THE workspace mount, chosen once. `model_cwd` below reuses this instead
    // of running its own `.find()`: a bare `.find()` takes table order, so with
    // two Workspace mounts the permission root and the model-visible cwd could
    // bind to different ones and the prompt-per-write bug would return
    // silently. Two derivations of one root is how the guest/host split forked.
    let workspace_mount = match workspace_mounts.as_slice() {
        [mount] => Some(*mount),
        _ => None,
    };
    let permission_cwd = match workspace_mounts.as_slice() {
        [mount] => {
            // BOTH sides must be canonicalized or the containment tests below
            // compare different spellings of the same directory. `cfg.cwd`
            // arrives from Swift without `resolvingSymlinksInPath()`, so on
            // device it can read `/var/mobile/...` while the mount canonicalizes
            // to `/private/var/mobile/...`. `Path::starts_with` is
            // component-wise, so the very first component already differs, all
            // three tests go false, and a NESTED pair takes the disjoint arm —
            // re-rooting the permission root upward (the case the comment below
            // says must be refused) and turning
            // `is_local_app_workspace_root(&roots.cwd)` false, which disables
            // `denies_host_owned_for_workspace` and `escapes_local_app_workspace`
            // outright.
            // Canonical forms are used ONLY to decide. BOTH returned values are
            // raw, and that is load-bearing: `translate_model_path` resolves a
            // guest path to `mount.host_path.join(rest)` with NO
            // canonicalization (`mobile_linux_api::find_guest_mount` is
            // pure path math). Returning the canonicalized spelling here would
            // leave `FsRoots.cwd` as `/private/var/...` while every translated
            // path arrives as `/var/...`; `path_matches_rule_pattern`
            // relativizes component-wise, so the first component would differ
            // and EVERY rule would miss — the guest/host fix would be inert in
            // exactly the disjoint case it exists for.
            let host_canon =
                std::fs::canonicalize(&mount.host_path).unwrap_or_else(|_| mount.host_path.clone());
            let cwd_canon = std::fs::canonicalize(&cwd).unwrap_or_else(|_| cwd.clone());
            // Which root the rules resolve against.
            //
            // A WIDER root is not "more deny coverage": both local-app write
            // guards are all-or-nothing on `is_local_app_workspace_root(
            // &roots.cwd)`, and that is false for any directory that is not
            // itself `.../apps/<id>/workspace`. Widening turns them OFF.
            if cwd_canon == host_canon {
                // Identical — `.project` / `.localApp`, where cwd already IS
                // the workspace. No-op.
                cwd.clone()
            } else if cwd_canon.starts_with(&host_canon) {
                // cwd is INSIDE the mount. Re-rooting would move the rule root
                // UPWARD, widening every `./**` pattern. Keep the narrower cwd.
                cwd.clone()
            } else {
                // Either disjoint (the `.global` sibling case the on-device
                // diagnostic printed) or cwd is an ANCESTOR of the mount. Both
                // re-root onto the workspace: it is the directory the model
                // actually writes into, and it is the only spelling under which
                // the local-app guards engage at all.
                mount.host_path.clone()
            }
        }
        _ => cwd.clone(),
    };

    let (perms, permission_policy_gate): (
        Arc<dyn PermissionGate>,
        Arc<permission::PolicyPermissionGate>,
    ) = {
        let mut rules = Vec::new();
        rules.extend(configured_mcp_policy_rules.iter().cloned());
        // Auto is the built-in default.  `apply_auto_mode_gate` below retains
        // the existing safety downgrade for unsupported models/providers or
        // disabled auto mode.
        let mut mode = PermissionMode::Auto;
        // Audit fix (#1): the project/enterprise bypassPermissions KILLSWITCH
        // (`disableBypassPermissionsMode`), sticky across tiers — mirrors desktop
        // (harness-runtime::desktop sets `policy.bypass_killswitch_active`). Without it a
        // settings `defaultMode:bypassPermissions` becomes an unguarded allow-all
        // on mobile, which has no interactive bypass-safety guard either.
        let mut bypass_disabled = false;
        // Auto-mode killswitch (`Bpa()`), sticky across tiers — mirrors desktop
        // (`policy.auto_mode_disabled`). Feeds both the boot mode-load downgrade
        // and the live `set_permission_mode` auto rejection.
        let mut auto_mode_disabled = false;
        // Audit fix (#12): `permissions.additionalDirectories`, unioned across
        // tiers, so an AcceptEdits write under a settings-declared extra dir
        // auto-allows (mirrors desktop's `.with_working_dirs`); empty ⇒ unchanged.
        let mut additional_working_dirs = permission::working_dirs::AdditionalWorkingDirs::new();
        // Sticky OR of `permissions.blockReadsOutsideWorkingDirectories` across tiers.
        let mut block_reads_outside_working_directories = false;
        let proj = cwd.join(branding::DOT_DIR).join("settings.json");
        let user = cfg.lingxi_home.join("settings.json");
        // Audit fix (#6): also read the LocalSettings tier (`settings.local.json`),
        // LAST so its rules/defaultMode win — mirrors desktop. Mobile does not
        // PERSIST to it (no `.with_persist`), but a synced/checked-in
        // settings.local.json's deny/allow rules + defaultMode are now honored.
        let local = cwd.join(branding::DOT_DIR).join("settings.local.json");
        let mut sources: Vec<(std::path::PathBuf, permission::PermissionRuleSource)> = Vec::new();
        if user != proj {
            sources.push((
                user,
                permission::PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User),
            ));
        }
        sources.push((
            proj,
            permission::PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project),
        ));
        // `settings.local.json` is a distinct filename from both `settings.json`
        // paths, so it never collides with the dedup above — always read it last.
        sources.push((
            local,
            permission::PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Local),
        ));
        for (path, source) in sources {
            if let Ok(raw) = tokio::fs::read_to_string(&path).await {
                match permission::permission_rules_from_settings_json(&raw, source) {
                    Ok(mut r) => rules.append(&mut r),
                    Err(e) => tracing::warn!(
                        error = %e,
                        path = %path.display(),
                        "engine-mobile: skipping malformed settings permissions"
                    ),
                }
                if let Some(m) = permission::default_mode_from_settings_json(&raw) {
                    // Repo-controlled project/local settings may select a
                    // restrictive mode, but cannot promote a session into
                    // classifier-driven auto mode.  User settings are the
                    // trusted mobile tier for that promotion.
                    if m != PermissionMode::Auto
                        || source
                            == permission::PermissionRuleSource::Settings(
                                lingxi_core::types::SettingsScope::User,
                            )
                    {
                        mode = m; // local settings read last → scalar modes win
                    } else {
                        tracing::warn!(
                            source = ?source,
                            "settings defaultMode \"auto\" ignored in an untrusted project/local tier"
                        );
                    }
                }
                if permission::bypass_permissions_disabled_from_settings_json(&raw) {
                    bypass_disabled = true; // sticky: any tier disabling wins
                }
                if permission::auto_mode_disabled_from_settings_json(&raw) {
                    auto_mode_disabled = true; // sticky: any tier disabling wins (Bpa)
                }
                // (P2-14) Scalar-override: a tier that declares the key overrides
                // (sources are ordered user → project → local, so local wins).
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                    if let Some(b) = v
                        .get("skipWebFetchPreflight")
                        .and_then(serde_json::Value::as_bool)
                    {
                        skip_web_fetch_preflight = b;
                    }
                    // (M-15) Scalar-override: a tier that declares the key overrides
                    // (user → project → local, so local wins).
                    if let Some(s) = v
                        .get("askUserQuestionTimeout")
                        .and_then(serde_json::Value::as_str)
                    {
                        ask_user_question_timeout = Some(s.to_string());
                    }
                    // (M-03) Scalar-override: a tier that declares the key
                    // overrides (user → project → local, so local wins).
                    if let Some(b) = v
                        .get("disableAgentView")
                        .and_then(serde_json::Value::as_bool)
                    {
                        disable_agent_view = b;
                    }
                    if let Some(b) = v
                        .get("agentPushNotifEnabled")
                        .and_then(serde_json::Value::as_bool)
                    {
                        agent_push_notif_enabled = b;
                    }
                    if let Some(chars) = v
                        .get("taskOutputMaxChars")
                        .and_then(serde_json::Value::as_u64)
                        .and_then(|n| u32::try_from(n).ok())
                    {
                        task_output_max_chars = Some(chars);
                    }
                    if let Some(size) = v
                        .get("workflowSizeGuideline")
                        .and_then(serde_json::Value::as_str)
                    {
                        if tool_workflow::WorkflowSizeGuideline::ALL_WIRE.contains(&size) {
                            workflow_size_guideline =
                                tool_workflow::WorkflowSizeGuideline::from_wire(size);
                            workflow_size_guideline_is_default = false;
                        }
                    }
                    if let Some(b) = v
                        .get("enableWorkflows")
                        .and_then(serde_json::Value::as_bool)
                    {
                        workflow_session_enabled = b;
                    }
                }
                additional_working_dirs.extend_from_source(
                    permission::additional_directories_from_settings_json(&raw),
                    source,
                );
                if permission::block_reads_outside_working_directories_from_settings_json(&raw) {
                    block_reads_outside_working_directories = true; // any tier arming wins
                }
            }
        }
        lingxi_core::host::session_flags::set_agent_push_notif_enabled(agent_push_notif_enabled);
        lingxi_core::host::session_flags::set_task_output_max_chars(task_output_max_chars);
        // Filesystem roots so file-path CONTENT rules (`Edit(src/**)`,
        // `Read(./secrets/**)`) match the call's path. `dirs` is not a mobile dep,
        // so HOME comes from the env (absent on a sandboxed device ⇒ `None`).
        let roots = permission::FsRoots {
            cwd: permission_cwd.clone(),
            home: std::env::var_os("HOME").map(std::path::PathBuf::from),
            lingxi_home: cfg.lingxi_home.clone(),
        };
        requested_permission_mode = mode.wire_str().to_string();
        // Auto-mode availability gate — claude-code `xms` mode-load downgrade:
        // a resolved `auto` mode downgrades to `default` when unavailable (the
        // `disableAutoMode` killswitch or an auto-unsupported boot model). Local
        // breaker fresh at boot; provider `"firstParty"` (multi-provider mapping
        // deferred — see `permission::auto_gate`).
        if mode == PermissionMode::Auto {
            let (gated, _reason) = permission::apply_auto_mode_gate(
                mode,
                &permission::AutoGateInputs {
                    disabled_by_settings: auto_mode_disabled,
                    circuit_broken: false,
                    model: default_model_id.clone(),
                    provider: boot_auto_mode_provider.clone(),
                },
            );
            mode = gated;
        }
        resolved_permission_mode = mode;
        let mut policy = permission::PermissionPolicy::from_rules(mode, rules)
            // `zj`'s `!Ae()` — plan mode counts as bypassPermissions only in an
            // interactive launch. A mobile foreground host is one; a cron-fired
            // throwaway runtime is not.
            .with_interactive_session(interactive_launch)
            .with_roots(roots)
            .with_working_dirs(additional_working_dirs)
            .with_block_reads_outside_working_directories(block_reads_outside_working_directories)
            .with_workspace_leases(workspace_leases.clone())
            .with_plan_files(plan_files.clone());
        // Audit fix (#1): honor the bypassPermissions killswitch resolved above.
        policy.bypass_killswitch_active = bypass_disabled;
        subagent_bypass_gates.bypass_disabled = bypass_disabled;
        // Auto-mode killswitch (`Bpa()`): the live `set_permission_mode` gate
        // refuses `auto` when any tier set `disableAutoMode: "disable"`.
        policy.auto_mode_disabled = auto_mode_disabled;
        let policy = Arc::new(policy);
        // Resolve active Read(deny) rules to search-exclude globs before the
        // policy moves into the gate (same as the desktop composition root).
        read_deny_exclude_globs = permission::read_deny_exclude_globs(&policy, &permission_cwd);
        // Share the boot policy into `tool_ctx` for the prompt shell-expansion
        // gate (clone the `Arc` BEFORE `policy` moves into the gate below).
        boot_permission_policy = Some(policy.clone());
        // Grab the LIVE-model cell BEFORE coercing to `Arc<dyn PermissionGate>`;
        // filled once the orchestrator exists (below).
        // GUEST-COORD: give the gate the SAME `translate_model_path` seam the
        // file tools use, so the permission check and the tool can never
        // disagree about which mount a path belongs to. `fs` only translates
        // when a mobile-linux runtime is selected; otherwise the trait default
        // returns `Ok(None)` and the gate is byte-identical to desktop.
        let mut gate = permission::PolicyPermissionGate::new(policy, adapter_gate.clone());
        if mobile_linux.is_some() {
            gate = gate
                .with_path_translator(Arc::new(permission::FileSystemPathTranslator(fs.clone())));
        }
        let enforcing = Arc::new(gate);
        live_model_provider_cell = Some(enforcing.live_model_provider_handle());
        loop_classifier_cell = Some(enforcing.loop_classifier_handle());
        (enforcing.clone(), enforcing)
    };

    // (6) Hook executor + memory provider.
    //
    // P0.2: the hook executor is no longer the `noop_hook_executor()` stub — it
    // is the REAL `HookExecutorImpl` (mobile sibling of `harness_runtime::desktop::build`
    // §5.2 / §5.25), built from the settings hooks below so the Command / Prompt
    // hook arms run for real and the session-start `SessionStart` /
    // `InstructionsLoaded` lifecycle fires (step (9) below) dispatch against the
    // loaded hooks.
    //
    // (6a) HookRegistry — read settings.json hooks from project
    //      (`<cwd>/.lingxi/settings.json`) then user
    //      (`<lingxi_home>/settings.json`), project last so it wins on identical
    //      command registration (same precedence as desktop). The files are read
    //      via `tokio::fs` (NOT the workspace-constrained `FileSystem::read_file`)
    //      because `lingxi_home` may sit outside the orchestrator's cwd, exactly
    //      as desktop reads them. A missing or malformed file is skipped, never an
    //      error — the common (no-hooks) case registers nothing and stays a no-op.
    let mut hook_registry = hooks::HookRegistry::new();
    let project_settings_path = cwd.join(branding::DOT_DIR).join("settings.json");
    let user_settings_path = cfg.lingxi_home.join("settings.json");
    // On mobile `lingxi_home` is commonly `<cwd>/.claude`, so the user- and
    // project-settings paths can resolve to the SAME file. Desktop never
    // collides (lingxi_home = `~/.claude` ≠ cwd) and so has no dedup. Reading a
    // colliding path twice would `register()` every declared hook twice, so it
    // would fire twice per event — a parity divergence. De-dup to read each
    // distinct path ONCE; when they collide keep the Project tag (project is
    // read last so it wins precedence on differing paths, matching desktop).
    let mut settings_sources: Vec<(std::path::PathBuf, hooks::definition::HookSource)> = Vec::new();
    if user_settings_path != project_settings_path {
        settings_sources.push((
            user_settings_path,
            hooks::definition::HookSource::Settings(lingxi_core::types::SettingsScope::User),
        ));
    }
    settings_sources.push((
        project_settings_path,
        hooks::definition::HookSource::Settings(lingxi_core::types::SettingsScope::Project),
    ));
    // (H-BIN-12) Accumulate the CC 2.1.207 HTTP-hook security allowlists across
    // the SAME settings tiers, concat-deduped (CC merges these arrays across
    // sources). Stay `None` until a tier declares the key (⇒ no restriction); an
    // explicit `[]` sets `Some(empty)` (⇒ block ALL HTTP hooks for
    // allowedHttpHookUrls). Mobile has no `lingxi_core::settings::Settings::load`
    // seam, so read the keys directly like the `skipWebFetchPreflight` path.
    let mut allowed_http_hook_urls: Option<Vec<String>> = None;
    let mut http_hook_allowed_env_vars: Option<Vec<String>> = None;
    let mut disable_all_hooks = false;
    let mut hooks_restricted = false;
    let concat_dedup_str_array =
        |acc: &mut Option<Vec<String>>, val: Option<&serde_json::Value>| {
            let Some(arr) = val.and_then(serde_json::Value::as_array) else {
                return;
            };
            let out = acc.get_or_insert_with(Vec::new);
            for s in arr.iter().filter_map(|x| x.as_str().map(String::from)) {
                if !out.contains(&s) {
                    out.push(s);
                }
            }
        };
    for (path, source) in settings_sources {
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
                    "engine-mobile: skipping malformed settings hooks"
                ),
            }
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                if let Some(value) = v
                    .get("disableAllHooks")
                    .and_then(serde_json::Value::as_bool)
                {
                    disable_all_hooks = value;
                }
                if let Some(value) = v
                    .get("allowManagedHooksOnly")
                    .and_then(serde_json::Value::as_bool)
                {
                    hooks_restricted = value;
                }
                concat_dedup_str_array(&mut allowed_http_hook_urls, v.get("allowedHttpHookUrls"));
                concat_dedup_str_array(
                    &mut http_hook_allowed_env_vars,
                    v.get("httpHookAllowedEnvVars"),
                );
            }
        }
    }
    let hook_registry = Arc::new(RwLock::new(hook_registry));

    // (6b) Build the real executor. Per the mobile runner-sourcing decision:
    //      - `with_process_runner(process, sandbox)` makes the Command arm run
    //        child processes through the SAME platform-sourced runner + sandbox
    //        the tools use (a device-jailed runner on Android, the host runner on
    //        iOS/CI). Both are required — the runner only accepts a
    //        `lingxi_core::host::SandboxedCommand`, which only the sandbox can mint.
    //      - `with_prompt_runner(ApiClientHookPromptRunner)` evaluates inline
    //        single-turn `prompt` hooks over the SAME `api_client` the
    //        orchestrator drives (shared provider routing / auth / telemetry).
    //      The `with_async_registry` + `with_agent_spawner` builders are
    //      DELIBERATELY OMITTED: mobile has no subagent spawner, and with no
    //      async/agent hook configured this is behavior-neutral (a non-blocking
    //      hook falls back to running synchronously; an `agent` hook returns a
    //      structured "not wired" error rather than spawning). The
    //      `RuntimeSpawner` is the posix-minimal `PosixRuntime` (only the omitted
    //      Agent/async arms consult it; the Command arm uses `process`).
    // Transcript sink for the per-hook-run `attachment` records claude-code
    // persists (one line per hook run). Created empty because the hook
    // executor is built BEFORE the orchestrator that owns the JSONL writer;
    // `attach` fills the cell once `orch` exists (step 8 below).
    let hook_attachment_sink = Arc::new(orchestrator::JsonlHookAttachmentSink::new());
    // Same late-bound shape for the prompt-hook evaluator: it needs the
    // session's live model/profile (a non-Anthropic session evaluates on its
    // own model), and the session does not exist yet. `attach`ed in step 8.
    let hook_prompt_runner = Arc::new(orchestrator::ApiClientHookPromptRunner::new(
        api_client.clone(),
    ));
    let hooks: Arc<hooks::HookExecutorImpl> = Arc::new(
        hooks::HookExecutorImpl::new(
            hook_registry.clone(),
            http.clone(),
            Arc::new(platform_posix_minimal::PosixRuntime::new())
                as Arc<dyn lingxi_core::host::RuntimeSpawner>,
        )
        .with_policy_disable_all_hooks(disable_all_hooks)
        .with_process_runner(process.clone(), sandbox.clone())
        .with_prompt_runner(hook_prompt_runner.clone() as Arc<dyn hooks::HookPromptRunner>)
        // (H-BIN-12) Gate outbound HTTP-hook URLs + intersect the per-hook env
        // allowlist from the merged settings; `(None, None)` = no restriction.
        .with_http_hook_policy(allowed_http_hook_urls, http_hook_allowed_env_vars)
        // One transcript `attachment` line per hook run, matching claude-code.
        .with_attachment_sink(hook_attachment_sink.clone() as Arc<dyn hooks::HookAttachmentSink>),
    );

    // P0.2: the production FFI entry points inject
    // `cfg.memory_provider = Some(orchestrator::prompt::real_provider())` so the
    // orchestrator loads the real `<cwd>/LINGXI.md` + `<lingxi_home>/LINGXI.md`
    // hierarchy into its system prompt (claude-code parity) and step (9)'s
    // `fire_instructions_loaded()` fires over those files. `None` (the default +
    // every host test) falls back to the empty `StaticMemoryProvider`, so a
    // default build loads NO memory and the host tests stay deterministic.
    let memory: Arc<dyn orchestrator::prompt::MemoryHierarchyProvider> = cfg
        .memory_provider
        .clone()
        .unwrap_or_else(|| Arc::new(StaticMemoryProvider::empty()));

    // (7) Assemble the mobile tool registry through the composition root. The
    //     device capabilities (camera / audio / share) come from `platform`;
    //     desktop-only seams (subagent / mcp / lsp / team / worktree-tool) are
    //     absent because `harness-runtime::mobile` does not link those tool crates.
    // P1-06: ONE per-session read-file-state registry (see harness-runtime::desktop
    // note) — cloned into the file tools' `BuiltinToolContext` and the SAME
    // `Arc` handed to the orchestrator via `.with_read_state_map(...)` below.
    let read_state_map = tool_api::read_file_state::new_read_file_state_map();
    // Bound (not inlined) so the SAME `Arc<SessionCwd>` can also be handed to
    // the orchestrator below via `.with_session_cwd(...)` (Task 5 — worktree
    // 206 session-cwd plumbing). Mobile never registers the worktree tool
    // (see above), so this cell never actually swaps today; wiring it keeps
    // the orchestrator's cwd source consistent with desktop and future-proofs
    // a mobile worktree tool without a second staleness bug to fix later.
    // PathAtlas S2: guest paths translate onto the mount table's HOST roots
    // (workspace under `Application Support/workspaces/<id>`, persistent
    // `/root` under the managed rootfs dir) — SIBLINGS of the app-sandbox
    // cwd, not children. Without trusting them, every translated path would
    // pass translation and then die at `canonicalize_and_validate`
    // containment. Legacy/unavailable runtimes serve an empty table, so this
    // adds nothing off mobile-linux.
    let mut trusted_dirs = vec![cwd.clone()];
    trusted_dirs.extend(mobile_linux_mounts.iter().map(|m| m.host_path.clone()));
    // PathAtlas S3: the MODEL-VISIBLE cwd is the guest workspace when one is
    // mounted — the same coordinate the shell already uses, so file tools and
    // shell commands name the same files. Relative tool paths resolve against
    // it and come back through translate_model_path onto the host twin. The
    // engine-internal cwd (`cwd` — transcripts, .lingxi, memory files) stays
    // host.
    let model_cwd = workspace_mount
        .map(|mount| std::path::PathBuf::from(&mount.guest_path))
        .unwrap_or_else(|| cwd.clone());
    // DIAGNOSTIC (permission coordinate audit): the model names files in
    // `model_cwd` (GUEST) while `PermissionPolicy`'s `FsRoots.cwd` was built
    // from `cwd` (HOST) above. When these diverge, every file-path CONTENT
    // rule (`Edit(./**)`, `Read(src/**)`) is tested by relativizing a guest
    // path against a host root — which escapes upward and matches nothing, so
    // allow rules silently never fire and every write falls through to a
    // prompt. Print BOTH values unconditionally: which one the device actually
    // has is not decidable by reading the source (the `unwrap_or_else`
    // fallback collapses them whenever no Workspace mount is linked).
    //
    // `eprintln!` and not `tracing!`: no `tracing-subscriber` is installed on
    // mobile (it is not a dependency of harness-runtime::mobile or ios-framework, and no
    // log-init seam is exposed to Swift), so every `tracing::warn!` in this
    // crate is dropped on the floor on device. stderr reaches the Xcode/device
    // console.
    //
    // Deliberately NOT `#[cfg(debug_assertions)]`: `build-xcframework.sh` pins
    // `PROFILE="release"` and the workspace declares no `[profile.release]`
    // override, so `debug_assertions` is OFF in every shipped iOS engine — a
    // debug-gated diagnostic is compiled out of the only build that can
    // exhibit the bug. (The `[turn-diagnostic]` prints later in this file are
    // debug-gated and therefore already dead on device.)
    //
    // Env-gated rather than unconditional so it stays reachable on a release
    // device build without becoming permanent production output: this line
    // carries the host container path, and nothing would ever have removed it.
    if std::env::var_os("LINGXI_PERMISSION_ROOTS_DIAGNOSTIC").is_some() {
        eprintln!(
            "[permission-roots-diagnostic] host_cwd={} model_cwd={} permission_cwd={} diverged={}",
            cwd.display(),
            model_cwd.display(),
            permission_cwd.display(),
            model_cwd != cwd
        );
    }
    let session_cwd = SessionCwd::new(model_cwd, trusted_dirs);
    let gated_shell_ctx = gate_mobile_shell_ctx(
        cfg.mobile_shell().cloned(),
        mobile_linux_capability.as_ref(),
    );
    let gated_git_ctx =
        gate_mobile_git_ctx(cfg.mobile_git().cloned(), mobile_linux_capability.as_ref());
    let mobile_runtime_environment = build_mobile_runtime_environment(
        cfg.host_environment.as_ref(),
        gated_shell_ctx.as_ref(),
        mobile_linux_capability.as_ref(),
        &session_cwd,
    );
    orch_cfg.exclude_dynamic_system_prompt_sections = cfg.host_environment.is_some();
    let mobile_workspace_cwd_provider = {
        let session_cwd = session_cwd.clone();
        let mobile_linux = mobile_linux.clone();
        let has_mobile_linux_guest =
            mobile_runtime_environment
                .as_ref()
                .is_some_and(|environment| {
                    matches!(
                        environment.tool_runtime,
                        lingxi_core::host::MobileToolRuntime::MobileLinuxGuest
                    )
                });
        Arc::new(move |override_cwd: Option<&std::path::Path>| {
            if !has_mobile_linux_guest {
                return None;
            }
            let cwd = override_cwd
                .map(std::path::Path::to_path_buf)
                .unwrap_or_else(|| session_cwd.cwd());
            let mounts = mobile_linux
                .as_ref()
                .map(|runtime| runtime.current_mounts())
                .unwrap_or_default();
            model_visible_mobile_cwd(&cwd, &mounts, has_mobile_linux_guest)
        }) as Arc<dyn Fn(Option<&std::path::Path>) -> Option<String> + Send + Sync>
    };

    // ── v3 Phase 1: workflow-on-mobile stack ─────────────────────────────
    // (a) Task output spool + registry (mirror of the desktop composition,
    // harness-runtime::desktop lib.rs (5.46)). The spool lives under the app-private
    // lingxi home, keyed by the boot session so concurrent processes never
    // share a spool dir.
    let task_output_dir = cfg.lingxi_home.join("task-output").join(&main_session_uuid);
    if let Err(e) = std::fs::create_dir_all(&task_output_dir) {
        tracing::warn!(
            dir = %task_output_dir.display(),
            error = %e,
            "could not create the session task-output dir; task spools may fail to allocate"
        );
    }
    let mut task_registry_inner = tasks::registry::TaskRegistry::new(
        Arc::new(platform_posix_minimal::PosixRuntime::new()),
        fs.clone(),
        Arc::new(tasks::output_manager::TaskOutputManager::new(
            task_output_dir,
            fs.clone(),
        )),
    )
    // `tengu_agent_tool_terminated` (async twin): the registry is where
    // `killed_by` is known, so it is where the event can name its origin.
    .with_analytics_bus(analytics_bus.clone())
    // Same blocking TaskCreated/TaskCompleted hook contract as desktop; the
    // transcript path is unavailable before the session mounts (matches the
    // `task_lifecycle_hooks` wiring below).
    .with_task_completed_firer(Arc::new(orchestrator::OrchestratorTaskCompletedFirer::new(
        hooks.clone(),
        cwd.clone(),
        std::path::PathBuf::new(),
    )))
    .with_task_created_firer(Arc::new(orchestrator::OrchestratorTaskCreatedFirer::new(
        hooks.clone(),
        cwd.clone(),
        std::path::PathBuf::new(),
    )));
    task_registry_inner.set_workflow_session_filter(Some(main_session_uuid.clone()));
    let lsp_diagnostics = lsp::LspDiagnosticRegistry::new();
    let typescript_lsp_mode =
        match mobile_typescript_lsp_mode(&cfg.lingxi_home.join("settings.json")) {
            Ok(mode) => mode,
            Err(error) => {
                tracing::warn!(%error, "invalid TypeScript LSP setting; using auto");
                lsp::LspActivationMode::Auto
            }
        };
    let mobile_lsp_ready = match mobile_linux.as_ref() {
        Some(runtime) => {
            mobile_typescript_lsp_ready(
                runtime,
                mobile_linux_capability.as_ref(),
                cfg.host_environment.as_ref(),
            )
            .await
        }
        None => false,
    };
    let app_data_root = mobile_apps_data_root(&cfg);
    let auto_workspace_root = app_data_root.clone();
    let auto_workspace_predicate: Arc<dyn Fn(&std::path::Path) -> bool + Send + Sync> =
        Arc::new(move |workspace| {
            crate::mobile::mobile_lsp::is_managed_local_app_workspace(
                &auto_workspace_root,
                workspace,
            )
        });
    let (plugin_lsp_registry, mobile_lsp_path_mapper) =
        match (mobile_linux.clone(), mobile_lsp_ready) {
            (Some(runtime), true) => {
                let mapper = Arc::new(
                    crate::mobile::mobile_lsp::MobileLinuxGuestLspPathMapper::new(runtime.clone()),
                );
                (
                    Arc::new(
                        lsp::LspRegistry::new(Arc::new(
                            crate::mobile::mobile_lsp::MobileLinuxLspTransport::new(
                                runtime,
                                mapper.clone(),
                                std::env::temp_dir().join("lingxi-lsp"),
                            ),
                        ))
                        .with_path_mapper(mapper.clone())
                        .with_workspace_activation_predicate(auto_workspace_predicate.clone())
                        .with_diagnostics(lsp_diagnostics.clone()),
                    ),
                    Some(mapper),
                )
            }
            _ => (
                Arc::new(
                    lsp::LspRegistry::new(Arc::new(
                        crate::mobile::mobile_lsp::MobileLinuxLspTransport::unavailable(),
                    ))
                    .with_workspace_activation_predicate(auto_workspace_predicate)
                    .with_diagnostics(lsp_diagnostics.clone()),
                ),
                None,
            ),
        };
    plugin_lsp_registry.set_activation_mode(typescript_lsp_mode);
    plugin_lsp_registry
        .register_plugin_servers(
            crate::mobile::mobile_lsp::global_typescript_lsp_plugin_id(),
            vec![crate::mobile::mobile_lsp::global_typescript_lsp_config()],
        )
        .await;
    let mobile_lsp_workspace_leases = Arc::new(
        crate::mobile::mobile_lsp::MobileWorkspaceLspLeaseManager::new(
            plugin_lsp_registry.clone(),
            lsp_diagnostics.clone(),
            mobile_lsp_path_mapper,
        ),
    );

    // (b) The subagent pool + spawner (adapted from harness-runtime::desktop; no
    // worktree/coordinator seams on mobile). The spawner's set-once cells
    // (tool registry / agent catalog / hook executor / skill loader) are
    // grabbed BEFORE boxing and filled once the tool registry exists below —
    // the same construction-cycle break as desktop. Subagents keep upstream
    // interactivity semantics: a one-shot spawn is `is_async=false`, so
    // `AskUserQuestion` inside a workflow agent reaches the client through
    // the SAME shared registry + `TuiBridgeResolver` channel as the main
    // session.
    let subagent_pool = Arc::new(agent::StateMachinePool::new(
        Arc::new(platform_posix_minimal::PosixRuntime::new())
            as Arc<dyn lingxi_core::host::RuntimeSpawner>,
        lingxi_core::host::subagent_spawn::max_concurrent_subagents(),
    ));
    // Use the owning boot session for hook/checkpoint identity. Hot-resume
    // rebinding still requires the deferred dynamic session-context substrate,
    // but a fresh mobile session must not checkpoint under an unrelated id.
    let subagent_hook_session_id = main_session_id;
    let main_subagents_dir = orchestrator::transcript_paths::subagents_dir(
        &cfg.lingxi_home,
        &cwd.to_string_lossy(),
        &main_session_uuid,
    );

    let workflow_checkpoints = Arc::new(
        crate::mobile::workflow_support::MobileWorkflowCheckpointStore::new(
            cfg.lingxi_home.clone(),
            cwd.clone(),
        ),
    );
    let subagent_transcript_home = cfg.lingxi_home.clone();
    let subagent_transcript_cwd = cwd.to_string_lossy().into_owned();
    let subagent_active_session = active_session_uuid.clone();
    let subagents_dir_provider = Arc::new(move || {
        let session_uuid = subagent_active_session.lock().ok()?.clone();
        Some(orchestrator::transcript_paths::subagents_dir(
            &subagent_transcript_home,
            &subagent_transcript_cwd,
            &session_uuid,
        ))
    });
    let subagent_env_renderer = build_mobile_subagent_env_renderer(
        cwd.clone(),
        mobile_runtime_environment.as_ref(),
        mobile_workspace_cwd_provider.clone(),
    );
    let session_agent_observer = Arc::new(MobileSessionAgentObserver::new(
        event_sink.clone(),
        active_session_uuid.clone(),
    ));
    let mut subagent_spawner_concrete = agent::PoolSubagentSpawner::new(subagent_pool)
        .with_refusal_fallback_chain(orch_cfg.refusal_chain())
        .with_api_client(provider_adapter.clone() as Arc<dyn agent::SubagentApiClient>)
        .with_session_interactive(interactive_launch)
        .with_default_model(agent::model_resolution::resolve_user_specified_model(
            &orch_cfg.model,
        ))
        .with_permission_mode(resolved_permission_mode)
        .with_spawn_bypass_gates(subagent_bypass_gates)
        .with_model_setting(orch_cfg.model.clone())
        .with_hook_context(
            subagent_hook_session_id,
            cwd.clone(),
            Some(main_subagents_dir.clone()),
        )
        .with_subagents_dir_provider(subagents_dir_provider)
        .with_transcript_fs(fs.clone())
        .with_spawn_observer(session_agent_observer)
        .with_new_diagnostics_source_factory(Arc::new({
            let workspace_leases = mobile_lsp_workspace_leases.clone();
            move |cwd| {
                let host_root = cwd
                    .map(std::path::PathBuf::from)
                    .map(|path| std::fs::canonicalize(&path).unwrap_or(path));
                workspace_leases
                    .diagnostics_source(host_root, Some(std::time::Duration::from_millis(500)))
            }
        }))
        .with_subagent_env_renderer(subagent_env_renderer);
    if let Some(environment) = mobile_runtime_environment.clone() {
        subagent_spawner_concrete = subagent_spawner_concrete
            .with_mobile_runtime_environment(environment)
            .with_mobile_workspace_cwd_provider(mobile_workspace_cwd_provider);
    }
    let subagent_tool_registry_cell = subagent_spawner_concrete.tool_registry_handle();
    let subagent_agent_catalog_cell = subagent_spawner_concrete.agent_catalog_handle();
    let subagent_hook_executor_cell = subagent_spawner_concrete.hook_executor_handle();
    let subagent_skill_loader_cell = subagent_spawner_concrete.skill_loader_handle();
    let subagent_default_model_selection_provider_cell =
        subagent_spawner_concrete.default_model_selection_provider_handle();
    let subagent_provider_first_party_resolver_cell =
        subagent_spawner_concrete.provider_first_party_resolver_handle();
    let subagent_spawner_arc = Arc::new(subagent_spawner_concrete);
    let subagent_spawner: Arc<dyn lingxi_core::host::subagent_spawn::SubagentSpawner> =
        subagent_spawner_arc.clone();

    // (c) Budget enforcer over the session CostTracker (desktop parity —
    // background subagents halt at the same session ceiling as the main loop;
    // with no configured ceiling this stays unlimited).
    let budget_enforcer: Arc<dyn lingxi_core::host::budget::BudgetEnforcerHandle> =
        Arc::new(cost::BudgetEnforcer::new(
            cost::BudgetConfig {
                max_session_nano_usd: orch_cfg.max_budget_nano_usd,
                max_turn_nano_usd: None,
                max_turn_tokens: None,
                warning_thresholds: Vec::new(),
                on_exceed: cost::BudgetExceedPolicy::Halt,
            },
            cost_tracker.clone(),
        ));

    // (d) The LocalWorkflow handler. Its tool-dispatch seam is a deferred
    // invoker (filled with the real `RegistryToolInvoker` once `tools`
    // exists) and its terminal status writes through a deferred
    // mobile status sink (its registry delegate is bound once the registry
    // `Arc` exists) — without it a finished workflow is stuck `Running`
    // forever and the client never sees its terminal state. The output-pool
    // cells are published after the orchestrator is built.
    let plugin_workflow_registry = Arc::new(workflow::PluginWorkflowRegistry::new());
    // Pre-create the shared command-registry slot before the tool registry and
    // the orchestrator so the Skill tool, slash dispatcher, and per-turn skill
    // listing all observe one live command set.
    let shared_command_registry: Arc<RwLock<command_api::CommandRegistry>> =
        Arc::new(RwLock::new(command_api::CommandRegistry::new()));
    let local_workflow_invoker =
        Arc::new(crate::mobile::workflow_support::DeferredToolInvoker::new());
    let local_workflow_status_sink = Arc::new(
        crate::mobile::workflow_support::MobileWorkflowStatusSink::new(
            listener.clone(),
            workflow_checkpoints.clone(),
            active_session_uuid.clone(),
        ),
    );
    let local_workflow_output_pool: Arc<std::sync::OnceLock<Arc<std::sync::atomic::AtomicU64>>> =
        Arc::new(std::sync::OnceLock::new());
    let local_workflow_turn_baseline: Arc<std::sync::OnceLock<Arc<std::sync::atomic::AtomicU64>>> =
        Arc::new(std::sync::OnceLock::new());
    let local_workflow_handler = Arc::new(
        tasks::handlers::LocalWorkflowHandler::new(
            subagent_spawner.clone(),
            local_workflow_invoker.clone() as Arc<dyn lingxi_core::host::tool_invoker::ToolInvoker>,
            budget_enforcer.clone(),
            task_registry_inner.output_manager.clone(),
        )
        .with_token_budget(orch_cfg.token_budget)
        .with_workflow_progress_sink(local_workflow_status_sink.clone()
            as Arc<dyn tasks::handlers::local_workflow::WorkflowProgressSink>)
        .with_output_pool_cell(local_workflow_output_pool.clone())
        .with_turn_baseline_cell(local_workflow_turn_baseline.clone())
        .with_workspace_permission_leases(workspace_leases.clone(), mobile_apps_data_root(&cfg))
        .with_plugin_workflows(plugin_workflow_registry.clone())
        .with_status_sink(
            local_workflow_status_sink.clone() as Arc<dyn tasks::handlers::TaskStatusSink>
        ),
    );
    task_registry_inner.register_handler(
        tasks::TaskType::LocalWorkflow,
        local_workflow_handler.clone(),
    );
    let local_agent_status_sink = Arc::new(tasks::registry_status_sink::RegistryStatusSink::new());
    let agent_resume_gate = Arc::new(crate::mobile::agent_resume::MobileForkResumeGate {
        spawner: subagent_spawner.clone(),
        commands: shared_command_registry.clone(),
    });
    task_registry_inner.register_handler(
        tasks::TaskType::LocalAgent,
        Arc::new(
            tasks::handlers::LocalAgentHandler::new(
                subagent_spawner.clone(),
                local_workflow_invoker.clone()
                    as Arc<dyn lingxi_core::host::tool_invoker::ToolInvoker>,
                budget_enforcer.clone(),
                task_registry_inner.output_manager.clone(),
            )
            .with_streaming_spawner(subagent_spawner_arc.clone())
            .with_status_sink(local_agent_status_sink.clone())
            .with_worktree_manager(worktree.clone())
            .with_fork_resume_gate(agent_resume_gate),
        ),
    );
    let task_registry = Arc::new(task_registry_inner);
    // ONE observer pairing table per session, shared with the orchestrator
    // below (desktop parity). The registry files a pairing when it spawns an
    // observer; `ObserverReport` resolves against this same `Arc`. A second
    // `ObserverPairings::new()` anywhere would compile, look wired, and answer
    // "not armed" forever.
    let observer_pairings = Arc::new(lingxi_core::host::observer_pairing::ObserverPairings::new());
    task_registry.set_observer_pairings(observer_pairings.clone());
    subagent_spawner_arc.set_task_registry(task_registry.clone());
    local_agent_status_sink.bind(task_registry.clone());
    let tool_ctx = BuiltinToolContext {
        // No session: this context never persists tool output.
        session_id: None,
        // FILE.B / P1-06: file tools share the ONE per-session read-state map
        // (see harness-runtime::desktop note).
        read_file_state: read_state_map.clone(),
        // Read(deny) → Grep/Glob search excludes, resolved from the local
        // `PermissionPolicy` built above (empty when no Read-deny rule ⇒
        // unchanged default).
        read_deny_exclude_globs,
        // P1.8: kept a clone rather than a move — `build_mobile_inner` needs
        // `fs` again below to compose the mobile `PluginManager` with the
        // SAME filesystem handle the rest of the boot path uses.
        fs: fs.clone(),
        bus: analytics_bus.clone(),
        process,
        sandbox,
        clock: clock.clone(),
        sandbox_runtime: SandboxRuntimeConfig::default(),
        // Mobile has no interactive `/sandbox` toggle (no live TUI); the frozen
        // `sandbox_runtime` above governs — full Android/iOS sandboxing intact.
        sandbox_enabled_override: None,
        // (P2-14) `settings.skipWebFetchPreflight`, read from the settings.json
        // tiers in the perms loop above (scalar-override, local wins).
        skip_web_fetch_preflight,
        // (M-15) `settings.askUserQuestionTimeout`, read from the settings.json
        // tiers in the perms loop above (scalar-override, local wins).
        ask_user_question_timeout,
        // RUNNER ↔ AVAILABILITY COUPLING (#5): the live `SandboxRuntimeRunner`
        // (domain/proxy/policy enforcement) requires host forward proxies +
        // bwrap/seatbelt — desktop-OS primitives a phone (iOS/Android,
        // `platform-posix-minimal`) does NOT have. So mobile keeps the legacy
        // wrap AND reports `sandbox_available: false`, which makes
        // `should_use_sandbox` short-circuit to `NoSandbox`
        // (`sandbox/decision.rs:64`) BEFORE the runner is ever consulted — the
        // legacy wrap is therefore inert here, not an under-enforcement gap. The
        // `debug_assert!` below pins the invariant: if a future capable host flips
        // `sandbox_available` to `true`, it MUST also inject a live runner (the
        // legacy wrap can only express `--unshare-net`/`--share-net`, never the
        // domain/proxy enforcement the desktop runtime provides).
        sandbox_runner: tool_api::default_sandbox_runner(),
        permission_mode: resolved_permission_mode,
        // (#3 shell-expansion) Share the SAME boot policy the model-facing gate
        // enforces as the base for embedded `!`cmd`` bodies in prompt commands.
        // Always `Some` here (the `perms` block above is unconditional).
        permission_policy: boot_permission_policy
            .clone()
            .expect("boot permission policy is built unconditionally above"),
        sandbox_available: false,
        session_cwd: session_cwd.clone(),
        // Worktree 206 parity (Task 8): a fresh, empty (`None`) session
        // record. Mobile never registers the worktree tool (see the
        // `session_cwd` note above), so this cell stays inert in production —
        // wired for shape-consistency with desktop.
        worktree_session: tool_api::worktree_session::new_worktree_session_cell(),
        platform: if cfg!(target_os = "macos") {
            SandboxPlatform::Mac
        } else {
            SandboxPlatform::Linux
        },
        http: http.clone(),
        hosted_search: Some(provider_adapter.clone()),
        mcp_token_counter: Some(provider_adapter.clone()),
        default_model: orch_cfg.model.clone(),
        // Mobile has no settings.json-backed WebSearch config provider (desktop
        // injects `DesktopWebSearchConfigProvider`); WebSearch falls back to its
        // built-in defaults here. `None` matches the tool-api test-support host.
        web_search_config: None,
        worktree,
        // v3 Phase 1 (workflow-on-mobile): the real subagent spawner + task
        // registry + budget enforcer built above — the `Workflow` tool and
        // the Task command family run for real now.
        subagent_spawner: Some(subagent_spawner.clone()),
        agent_name_registry: None,
        task_registry: Some(
            task_registry.clone() as Arc<dyn lingxi_core::host::task_registry::TaskRegistryHandle>
        ),
        mailbox_router: None,
        budget_enforcer: Some(budget_enforcer.clone()),
        main_loop_model_profile_provider: None,
        // Mobile has no coordinator runtime; fork-subagent gate sees non-coordinator.
        coordinator_mode: None,
        // (3b) Share the enforcing mobile `PolicyPermissionGate` with tools
        // that own a permission round-trip (notably `ExitPlanMode`). This is
        // the same `perms` passed to the orchestrator, backed by the
        // connection-scoped `AdapterPermissionGate`; never leave a tool with
        // an approval seam unbound on mobile.
        permission_gate: Some(perms.clone()),
        // (CLI-5) Mobile never registers the shell tools at all, so there is
        // nothing to gate — and no git binary to snapshot with.
        bash_edit_diff: None,
        mcp_registry: Some(mcp_registry.clone()),
        lsp_registry: Some(plugin_lsp_registry.clone()),
        camera: platform.camera(),
        audio: platform.audio_service(),
        audio_recording_handles: Arc::new(
            tokio::sync::Mutex::new(std::collections::HashMap::new()),
        ),
        share: platform.share(),
        notifications: platform.notifications(),
        clipboard: platform.clipboard(),
        location: platform.location(),
        device_status: platform.device_status(),
        haptics: platform.haptics(),
        deep_link: platform.deep_link(),
        calendar: platform.calendar(),
        contacts: platform.contacts(),
        computer_control: platform.computer_control(),
        // Mobile shell/git registration is fail-closed when the host selected
        // mobile-linux but the runtime is blocked or unlinked. In that state the
        // tools stay ABSENT rather than silently falling back to the Android
        // legacy path.
        android_shell: gated_shell_ctx.clone(),
        android_git: gated_git_ctx.clone(),
        // Secret carrier follows the same public-gate decision: if the public
        // git tool is gated off, keep the secret seam absent too.
        android_git_secret: gated_git_ctx
            .as_ref()
            .and_then(|_| cfg.mobile_git_secret().cloned()),
        // Mobile uses the same blocking TaskCreated/TaskCompleted hook contract
        // as desktop. The transcript path is unavailable before the session is
        // mounted, so it remains empty; cwd and policy are still enforced.
        task_lifecycle_hooks: Some(Arc::new(
            orchestrator::OrchestratorTaskLifecycleHookFirer::new(
                hooks.clone(),
                cwd.clone(),
                std::path::PathBuf::new(),
            ),
        )),
    };
    // #5 invariant: mobile has no live sandbox runtime, so sandboxing must stay
    // unavailable — otherwise `should_use_sandbox` would route commands through
    // the under-enforcing legacy wrap. Enabling sandboxing on a future capable
    // host REQUIRES injecting a live runner alongside flipping this flag.
    debug_assert!(
        !tool_ctx.sandbox_available,
        "mobile sets sandbox_available=false because it has no live SandboxRuntimeRunner; \
         enabling sandboxing requires injecting one (see the sandbox_runner coupling note)"
    );
    // P1.8 (§19.2): compose the mobile `PluginManager` — P1.6 registered the
    // one compiled-in plugin through `register_verified_builtin`, but nothing
    // called that composition from `build_mobile_inner` yet, and the manager
    // was never handed the SAME live registries the rest of this function
    // wires for listing/dispatch. Handing it a registry of its own here would
    // leave a future plugin-declared command/skill invisible to the model's
    // listing (or a plugin-declared agent unspawnable) while every existing
    // test — none of which exercised `PluginManager` at all — stayed green.
    // So every registry below is the EXACT live object this function already
    // threads through the dispatcher / listing provider / subagent spawner,
    // not a fresh stand-in:
    //   - `command_registry` is `shared_command_registry` itself, the one
    //     `CommandRegistry` the slash dispatcher, `wired_skill_listing_provider`,
    //     and the Skill tool's loader all read below;
    //   - `hook_registry` / `mcp_registry` are the real live hook + MCP
    //     registries this connection already runs;
    //   - `skill_registry` / `output_style_registry` / `tool_registry` still
    //     stay fresh and inert by design, while `lsp_registry` is now the
    //     same live registry shared by plugin loading, file-write sync,
    //     diagnostics, and the builtin `LSP` tool.
    // P1.10 (§19.2): the compiled-in plugin's manifest USED to declare zero
    // components (`lib.rs`'s `mobile_builtin_plugin_manifest`), which made
    // registering it below a no-op over live state — a manifest with nothing
    // behind it, indistinguishable from "registration failed" from outside
    // this crate. `register_mobile_builtin_plugins_materialized` below
    // instead first materializes the packer's compiled-in bundle to a
    // verified, digest-checked on-disk root (`builtin_bundle::
    // materialize_compiled_in_plugin_bundle`) and builds the manifest's
    // `components` from THAT root's own resolved inventory, so the plugin
    // this boot registers actually contributes its real agents/skills/
    // workflows into the live registries wired above. The plugin-manager
    // tests below additionally materialize a SEPARATE fixture plugin through
    // `wired_plugin_manager.enable(..)` to prove the registries are shared by
    // identity, not merely seeded with equal content — that property does not
    // depend on which plugin is registered.
    // Share Desktop's canonical layer merge and userConfig/secure-secret resolver.
    // File-backed plugin changes take effect when the native client reconnects.
    let plugin_settings = mobile_provider_settings(&cfg)
        .map_err(|error| MobileBuildError::Orchestrator(format!("plugin settings: {error}")))?;
    let plugin_settings_value = serde_json::to_value(&plugin_settings)
        .map_err(|error| MobileBuildError::Orchestrator(format!("plugin settings: {error}")))?;
    let plugin_configs = plugin_settings_value
        .as_object()
        .map(plugin::PluginUserConfig::from_settings_map)
        .unwrap_or_default();
    let enabled_plugins: std::collections::BTreeMap<String, bool> = plugin_settings
        .enabled_plugins
        .as_ref()
        .into_iter()
        .flat_map(|values| values.iter())
        .filter_map(|(name, value)| value.as_bool().map(|enabled| (name.clone(), enabled)))
        .collect();
    let plugin_agent_catalog: Arc<tokio::sync::RwLock<Vec<agent::AgentDefinition>>> = Arc::new(
        tokio::sync::RwLock::new(agent::builtins::builtin_agent_definitions()),
    );
    let plugin_manager = Arc::new(
        plugin::PluginManager::new(
            cfg.lingxi_home.join("plugins"),
            fs.clone(),
            http.clone(),
            Arc::new(platform_posix_minimal::PosixRuntime::new())
                as Arc<dyn lingxi_core::host::RuntimeSpawner>,
            credentials.clone(),
            Arc::new(plugin::StrictPluginOnlyPolicy::empty()),
            shared_command_registry.clone(),
            Arc::new(RwLock::new(skill_api::SkillRegistry::new())),
            hook_registry.clone(),
            Arc::new(RwLock::new(outputstyles::OutputStyleRegistry::new())),
            mcp_registry.clone(),
            plugin_lsp_registry.clone(),
            Arc::new(RwLock::new(ToolRegistry::new())),
        )
        .with_agent_catalog(plugin_agent_catalog.clone())
        .with_plugin_configs(plugin_configs)
        .with_blocked_marketplaces(
            plugin_settings
                .blocked_marketplaces
                .clone()
                .unwrap_or_default(),
        )
        .with_project_dir(cwd.clone())
        .with_task_registry(
            task_registry.clone() as Arc<dyn lingxi_core::host::task_registry::TaskRegistryHandle>
        )
        .with_plugin_workflows(plugin_workflow_registry.clone()),
    );
    // Audit fix (#14): wire the mobile Skill tool to the SAME live registry the
    // slash dispatcher and listing provider use. The registry is filled below
    // once the orchestrator handle is available, and later `/reload-skills`
    // mutations stay visible to all three surfaces.
    let live_skill_loader = Arc::new(
        crate::mobile::skill_loader::MobileDiskSkillLoader::for_mode(
            shared_command_registry.clone(),
            cfg.session_mode,
        )
        .with_prompt_cwd(session_cwd.clone()),
    );
    let skill_loader: Arc<dyn tool_skill::skill::SkillLoader> = live_skill_loader.clone();
    let agent_skill_loader: Arc<dyn lingxi_core::host::skill_loader::SkillLoader> =
        live_skill_loader;
    // D1 (P-1.5 review): bind the per-turn skill-listing provider HERE, in the
    // same breath as the Skill loader above, and retain both handles on the
    // returned `MobileRuntime`. There is then exactly ONE construction site per
    // surface, and `mobile_listing_dispatcher_and_skill_tool_share_one_live_registry`
    // asserts against these objects rather than against replacements it builds
    // itself — so handing either surface a registry other than
    // `shared_command_registry` fails that test instead of silently emptying
    // the model's skill listing on device.
    // One Host-bounded scope decision drives both Local App model surfaces.
    // `permission::local_app_id_for_root` intentionally also understands
    // guest/legacy spellings for isolated runtimes, but using that broader
    // detector directly at this main-session composition root would let an
    // ordinary project whose path merely ends in `apps/<id>/workspace` inherit
    // the full app authoring surface.
    let local_app_scope_id = mobile_local_app_scope_id(&cwd, &mobile_apps_data_root(&cfg));
    let wired_skill_listing_provider = mobile_skill_listing_provider(
        shared_command_registry.clone(),
        cfg.session_mode,
        local_app_scope_id.is_some(),
        Some(read_state_map.clone()),
    );
    #[cfg(test)]
    let wired_skill_loader = skill_loader.clone();
    // (#3 shell-expansion) Build the shared prompt shell-expansion provider from
    // `tool_ctx` (carrying the base `permission_policy` + process/sandbox seams)
    // BEFORE `tool_ctx` is moved into the tool registry below, then chain it onto
    // the dispatcher so mobile `/commit` … expand their embedded `!`git …``
    // bodies identically to desktop. Mobile reports `sandbox_available:false`, so
    // `should_use_sandbox` short-circuits to `NoSandbox` and the expansion runs
    // via the plain `ProcessRunner` — consistent with mobile's own Bash tool.
    let shell_expansion_provider = tool_skill::build_prompt_shell_provider(&tool_ctx);
    let ask_resolver = ask_user_question_tx.map(|tx| {
        let timeout = tool_ui::ask_user_question::AskUserQuestionTimeout::parse_or_default(
            tool_ctx.ask_user_question_timeout.as_deref(),
        );
        Arc::new(tool_ui::ask_user_question::TuiBridgeResolver::new(
            timeout, tx,
        )) as Arc<dyn tool_ui::ask_user_question::AskUserQuestionResolver>
    });
    let (mut tools, wakeup_scheduler, loop_wakeup_armed) =
        crate::mobile::mobile_tool_registry_with_wakeup(
            tool_ctx.clone(),
            cfg.lingxi_home.clone(),
            skill_loader,
            ask_resolver,
        );
    register_android_ui_automation(&mut tools, platform.android_ui_automation());
    // v3 Phase 1: register the Workflow tool (mirror of the desktop
    // registration — after the base registry, because the launcher needs the
    // sealed `task_registry` Arc). Mobile has no managed-settings tier, so
    // `disableWorkflows` policy is absent (false); `LINGXI_DISABLE_WORKFLOWS`
    // still works via `workflows_enabled`.
    let workflow_cwd = Arc::new(std::sync::Mutex::new(session_cwd.cwd()));
    session_cwd.link_live_cwd(workflow_cwd.clone());
    let workflow_launcher = Arc::new(crate::mobile::workflow_support::MobileWorkflowLauncher {
        registry: task_registry.clone(),
        project_cwd: cwd.clone(),
        app_data_root: mobile_apps_data_root(&cfg),
        current_cwd: workflow_cwd.clone(),
        lingxi_home: cfg.lingxi_home.clone(),
        session_uuid: active_session_uuid.clone(),
        default_model_selection_provider: subagent_default_model_selection_provider_cell.clone(),
        checkpoints: workflow_checkpoints.clone(),
        status_sink: local_workflow_status_sink.clone(),
        plugin_workflows: plugin_workflow_registry.clone(),
    });
    let workflow_policy_enabled = tool_workflow::workflows_enabled(false);
    let workflow_size_guideline_state =
        lingxi_core::host::session_flags::WorkflowSizeGuidelineState::new(
            workflow_size_guideline.as_wire(),
            false,
            workflow_size_guideline_is_default,
        )
        .expect("mobile workflowSizeGuideline must be valid");
    let dynamic_workflows_gate = lingxi_core::host::session_flags::DynamicWorkflowsGate::new(
        workflow_policy_enabled && workflow_session_enabled,
        !workflow_policy_enabled,
    );
    let workflow_tool = Arc::new(
        tool_workflow::WorkflowTool::new(Some(
            workflow_launcher.clone() as Arc<dyn tool_workflow::WorkflowLauncher>
        ))
        .with_current_cwd(workflow_cwd)
        .with_size_guideline_state(workflow_size_guideline_state.clone())
        .with_size_guideline_source(
            workflow_size_guideline,
            false,
            workflow_size_guideline_is_default,
        )
        .with_dynamic_workflows_gate(dynamic_workflows_gate.clone())
        .with_session_enabled(workflow_session_enabled)
        .with_plugin_workflows(plugin_workflow_registry.clone())
        .with_permission_gate(perms.clone())
        .with_permission_policy(
            boot_permission_policy
                .clone()
                .expect("mobile workflow permission policy is always wired"),
        ),
    );
    tools.register_builtin(workflow_tool.clone());
    // First-party local-app host operations as ORDINARY builtins. Registered
    // here, while `tools` is still `&mut` — `register_builtin` cannot run once
    // the registry is `Arc`-wrapped below. The DYNAMIC per-app tools stay on
    // the MCP transport (their namespace binds `app_id` host-side, and they
    // must be added at runtime, which only `register_mcp_tools(&self, …)` does).
    for tool in crate::mobile::local_apps_tools::local_app_builtin_tools(
        &local_apps_mcp,
        local_app_scope_id.clone(),
    ) {
        tools.register_builtin(tool);
    }
    crate::mobile::apply_mobile_session_tool_policy(&mut tools, cfg.session_mode);
    let device_skill_tools = tools
        .available_tools(&tool_api::tool_trait::ToolStaticContext::default())
        .into_iter()
        .map(|tool| tool.name().to_owned())
        .collect::<Vec<_>>();
    let live_mcp_tool_ctx = tool_ctx.clone();
    let initial_mcp_tool_ctx = live_mcp_tool_ctx.clone();
    let app_agent_mcp_tool_context = live_mcp_tool_ctx.clone();
    if cfg.session_mode == session::jsonl::SessionMode::Code {
        for (connection_id, mcp_tools) in
            tool_mcp::build_registered_mcp_tools(&mcp_registry, tool_ctx).await
        {
            tools.register_mcp_tools(connection_id, mcp_tools);
        }
    }
    let tools = Arc::new(tools);
    // Oracle `kq` — publish the read-auto-allow probe now that BOTH inputs
    // exist: the policy, and the FINAL tool list. Earlier means an unknown tool
    // list (which the probe answers `false` for); later means after the file
    // tools can already run. With no policy there is nothing to evaluate, so
    // the probe stays unpublished and `read_auto_allowed` keeps answering
    // `false` — the fail-safe answer.
    if let Some(policy) = boot_permission_policy.clone() {
        lingxi_core::host::read_auto_allow::set_read_auto_allow_probe(Arc::new(
            permission::read_auto_allow::PolicyReadAutoAllow::new(policy, tools.all_names()),
        ));
    }

    // MCP servers may change their tool catalog after initialization. Keep the
    // mobile ToolRegistry in sync with the same generation-checked refresh path
    // used by desktop; otherwise settings changes and list_changed events only
    // update the MCP registry while the model continues seeing stale tools.
    if cfg.session_mode == session::jsonl::SessionMode::Code {
        let mcp_registry_weak = Arc::downgrade(&mcp_registry);
        let live_tools = tools.clone();
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
                                target: "lingxi_harness_runtime::mobile::mcp",
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
                    target: "lingxi_harness_runtime::mobile::mcp",
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
                            target: "lingxi_harness_runtime::mobile::mcp",
                            server = %change.server_name,
                            catalog = ?change.kind,
                            %error,
                            "Failed to refresh MCP catalog; keeping the previous catalog"
                        );
                    }
                }
            }
        });
    }

    // v3 Phase 1: fill the spawner's set-once cells now that the registry
    // exists (desktop (5.46f) mirror): subagents dispatch through the SAME
    // `Arc<ToolRegistry>` as the main loop, gated by the SAME permission
    // gate; the agent catalog serves the builtin definitions (incl.
    // `workflow-subagent`); the deferred workflow invoker + status sink bind
    // to their real targets.
    //
    // P1.8: this MUST be `plugin_agent_catalog` itself, not a fresh
    // `Vec`-seeded catalog of equal starting content — `plugin_manager` above
    // was built `.with_agent_catalog(plugin_agent_catalog.clone())`, so a
    // plugin-declared agent lands in whichever catalog this cell is filled
    // with. A second, separately-allocated catalog here would make the
    // subagent spawner (the actual invocation path) permanently blind to
    // anything `plugin_manager` ever registers, even though both catalogs
    // start out holding the identical builtin definitions.
    let _ = subagent_tool_registry_cell.set(tools.clone());
    let _ = subagent_agent_catalog_cell.set(plugin_agent_catalog.clone());
    let _ = subagent_hook_executor_cell.set(hooks.clone());
    let _ = subagent_skill_loader_cell.set(agent_skill_loader);
    let profile_first_party = profile_auto_mode_provider
        .iter()
        .map(|(profile, provider)| (profile.clone(), provider == "firstParty"))
        .collect::<std::collections::BTreeMap<_, _>>();
    let _ = subagent_provider_first_party_resolver_cell.set(Arc::new(move |profile| {
        profile_first_party.get(profile).copied()
    }));
    // Phase 2 Plugin agents declare frontmatter `skills:`. Their preload cell
    // therefore reads the same live registry as the Skill tool and listing;
    // bare agent entries resolve through the agent's plugin namespace.
    local_workflow_invoker.set(Arc::new(
        tool_api::RegistryToolInvoker::new(tools.clone()).with_gate(perms.clone()),
    ));
    local_workflow_status_sink.bind(task_registry.clone());

    // MEM-1 ACTIVATION on mobile — the same gate as desktop,
    // `memory::auto_memory_enabled`, which claude-code defaults ON (`dLt()` ends
    // `return!0`). When active, wire the memdir-backed memory selector so
    // relevant project memdir entries surface each turn via a Haiku-class side
    // query (a `ProviderSideQueryClient` over the device HTTP transport +
    // `cfg.api_key`, independent of the multi-provider turn client).
    //
    // Disable with `autoMemoryEnabled:false` or the `*_DISABLE_AUTO_MEMORY` /
    // `*_SIMPLE` env killswitches. A missing/unusable key makes the side query
    // fail → empty surfaced set (never breaks a turn).
    let memdir_prefetch = if memory::auto_memory_enabled(
        &memory::AutoMemoryEnv::from_process_env(),
        provider_settings.auto_memory_enabled,
    ) {
        // `cfg.lingxi_home` is the device `.claude` dir; the helper re-appends
        // the config-home segment, so pass its PARENT as `home`.
        let home = cfg
            .lingxi_home
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| cwd.clone());
        Some(orchestrator::prompt::build_memdir_prefetch_from_anthropic(
            cfg.api_key.clone(),
            Some(cfg.api_base.clone()),
            api_service.transport(),
            Arc::new(platform_posix_minimal::runtime::PosixRuntime::new())
                as Arc<dyn lingxi_core::host::RuntimeSpawner>,
            &home,
            &cwd,
        ))
    } else {
        None
    };

    // Audit fix (#3): autocompaction parity with desktop. Build the real
    // CompactionOrchestrator backed by a forked summary side-query over the SAME
    // device HTTP transport + cfg.api_key the memdir-prefetch uses; nothing here
    // needs a desktop-only primitive. Without this the orchestrator's compaction
    // stays None, the proactive `maybe_compact_before_call` is a strict no-op, and
    // a long mobile session fails at the wire on context-window overflow with no
    // summary recovery. The same `cache_safe_slot` is handed to BOTH the forked
    // summarizer and the orchestrator so the turn loop's per-call snapshot is what
    // the summary call replays. (This is the ordinary M3 autocompaction layer, NOT
    // the flag-gated CONTEXT_COLLAPSE/REACTIVE_COMPACT path.) Built before
    // `orch_cfg` is moved into the orchestrator so it can read `orch_cfg.model`.
    //
    // The autocompact threshold follows the session's CURRENT model
    // (`with_model_derived_threshold`); this expression is only the seed for the
    // model the session boots on — the proactive pre-call trigger re-resolves it
    // per call.
    let cache_safe_slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
    let compaction_side_query: Arc<dyn sidequery::SideQueryClient> = Arc::new(
        sidequery::ProviderSideQueryClient::from_service(api_service.clone()),
    );
    let forked_runner = Arc::new(
        sidequery::ForkedAgentRunner::new()
            .with_side_query_client(compaction_side_query, orch_cfg.model.clone()),
    );
    let compactor = Arc::new(
        compaction::CompactionOrchestrator::with_autocompactor(
            compaction::Autocompactor::with_forked_runner(forked_runner, cache_safe_slot.clone()),
            compaction::thresholds::auto_compact_threshold(&orch_cfg.model, &[]),
        )
        .with_model_derived_threshold(),
    );
    let app_agent_executor: Arc<dyn LocalAppsAgentExecutor> =
        Arc::new(MobileAppAgentExecutor::new(
            orch_cfg.clone(),
            api_client.clone(),
            streaming_api.clone(),
            hooks.clone(),
            perms.clone(),
            cfg.lingxi_home.clone(),
            mobile_apps_data_root(&cfg),
            local_apps_mcp.clone(),
            app_agent_mcp_tool_context,
        ));

    let mut orch_inner = ConversationOrchestrator::new_with_streaming(
        orch_cfg,
        api_client,
        streaming_api,
        tools.clone(),
        hooks,
        perms,
        output,
        memory,
        // `cwd` is reused below by the batch-8 registration, so clone here.
        cwd.clone(),
    )
    .with_dynamic_workflows_gate(dynamic_workflows_gate)
    .with_workflow_size_guideline(workflow_size_guideline_state)
    .with_session_id(main_session_id)
    .with_jsonl_writer(session_writer.clone())
    // P0.2: attach the SAME `HookRegistry` the executor reads so `list_hooks`
    // reports the loaded settings hooks (the executor fires against it; this
    // exposes it for inspection — mobile sibling of desktop's
    // `.with_hook_registry(hook_registry)`).
    .with_hook_registry(hook_registry)
    .with_vision_delegation(vision_delegation_enabled)
    // FIX A: hand the orchestrator the resolved claude-home so its hook payloads
    // carry a deterministically-computed `transcript_path` (claude-code
    // `getTranscriptPathForSession`) even though no `JsonlWriter` is wired —
    // mobile sibling of desktop's `.with_config_home(cfg.lingxi_home.clone())`.
    .with_config_home(cfg.lingxi_home.clone())
    .with_workspace_trusted(cfg.workspace_trusted)
    .with_hooks_restricted(hooks_restricted || disable_all_hooks)
    // Audit fix (#15): the orchestrator shares the ONE AnalyticsBus (so its
    // events ride the same sink as the ApiService + tools) + the session
    // CostTracker (desktop parity; accumulates the running session cost total).
    .with_analytics_bus(analytics_bus)
    .with_observer_pairings(observer_pairings.clone())
    .with_cost_tracker(cost_tracker)
    .with_api_calls_counter(api_calls_recorded)
    .with_new_diagnostics_source(lsp_diagnostics.diagnostics_source(
        Some(std::fs::canonicalize(&cwd).unwrap_or_else(|_| cwd.clone())),
        None,
    ))
    // Audit fix (#3): attach the compactor + the shared cache-safe slot so the
    // turn loop autocompacts before context-window overflow (desktop parity).
    .with_compaction(compactor)
    .with_cache_safe_slot(cache_safe_slot)
    // Audit fix (#13): per-turn V2 `<task-reminder>` over the file-backed
    // TodoStore (tool_task IS registered on mobile) — mirror of desktop.
    .with_todo_reminder_tasks(Arc::new(
        orchestrator::TodoStoreReminderTasks::with_config_home(cfg.lingxi_home.clone()),
    ))
    // v3 Phase 1: drain terminal-not-notified background tasks (workflows)
    // into the per-turn `<task-notification>` reminder — the model learns a
    // launched workflow finished on the next turn (desktop mirror).
    .with_task_notifications(Arc::new(orchestrator::RegistryTaskNotifications::new(
        task_registry.clone() as Arc<dyn lingxi_core::host::task_registry::TaskRegistryHandle>,
    )))
    // SKILLLIST.1: enumerate model-invocable skills each turn so the model
    // can discover bundled and user skills. Reads the shared registry lazily;
    // the registry is populated after the orchestrator handle is available.
    .with_skill_listing(wired_skill_listing_provider.clone())
    // P1-06: share the ONE `readFileState` map with the file tools (created
    // above) so post-compact file restore + staleness consumers see a tool's
    // `readFileState.set` — mirror of desktop.
    .with_read_state_map(read_state_map)
    // Task 5 (worktree 206 session-cwd plumbing): share the SAME
    // `Arc<SessionCwd>` the tool context reads, so the system prompt's env
    // block and the conditional-rules memory cache stay consistent with the
    // tool-facing cwd source (mobile mirror of desktop's
    // `.with_session_cwd(session_cwd)`; inert today — see the binding note
    // above).
    .with_session_cwd(session_cwd);
    // Keep the mobile request adapter and orchestrator on the same persisted
    // device preference. The adapter reads this flag when building the next
    // provider request; the handle exposes it to native controls.
    orch_inner = orch_inner.with_fast_mode(fast_flag);
    if let Some(selection) = persisted_reasoning_selection {
        orch_inner.initialize_reasoning_selection_for_model(
            &default_model_id,
            default_model_profile.as_deref(),
            selection,
        );
    }
    // PathAtlas S3: prompt probes (memory hierarchy, git status, file tree)
    // must read the HOST directory backing the guest session cwd while the
    // env block displays the guest path itself. Live table: external mounts
    // added later still resolve.
    orch_inner = if let Some(runtime) = mobile_linux.clone() {
        orch_inner.with_prompt_probe_cwd_resolver(std::sync::Arc::new(move |path| {
            mobile_linux_api::map_guest_path_to_host(
                &path.to_string_lossy(),
                &runtime.current_mounts(),
            )
            .unwrap_or_else(|| path.to_path_buf())
        }))
    } else {
        orch_inner
    };
    if let Some(environment) = mobile_runtime_environment.clone() {
        let runtime = mobile_linux.clone();
        let has_mobile_linux_guest = matches!(
            environment.tool_runtime,
            lingxi_core::host::MobileToolRuntime::MobileLinuxGuest
        );
        let resolver = Arc::new(move |path: &std::path::Path| {
            let mounts = runtime
                .as_ref()
                .map(|runtime| runtime.current_mounts())
                .unwrap_or_default();
            model_visible_mobile_cwd(path, &mounts, has_mobile_linux_guest)
        });
        orch_inner = orch_inner
            .with_mobile_runtime_environment(environment)
            .with_mobile_workspace_cwd_resolver(resolver);
    }
    orch_inner = orch_inner.with_mcp_registry(mcp_registry.clone());
    // P0.1 (gated): attach the memdir prefetch when enabled above.
    if let Some(prefetch) = memdir_prefetch {
        orch_inner = orch_inner.with_memory_prefetch(prefetch);
    }
    let orch = Arc::new(orch_inner.with_loop_wakeup_armed_slot(loop_wakeup_armed));
    orch.enable_goal_retries();

    // v3 Phase 1: publish the shared output-token pool + turn baseline to the
    // LocalWorkflow handler's cells now that the orchestrator exists — the
    // workflow script's `budget.spent()` reads the SAME pool as the main loop
    // (desktop (9014) mirror).
    let _ = local_workflow_output_pool.set(orch.output_token_pool());
    let _ = local_workflow_turn_baseline.set(orch.turn_start_output_baseline());
    {
        let session = orch.session();
        let selection_model_provider_profiles = model_provider_profiles.clone();
        let selection_profile_auto_mode_provider = profile_auto_mode_provider.clone();
        let last_selection = Arc::new(std::sync::Mutex::new(session.try_lock().ok().map(
            |state| {
                agent::DefaultModelSelection {
                    model: state.model.clone(),
                    model_profile: state.model_profile.clone(),
                    provider_first_party: state
                        .model_profile
                        .as_ref()
                        .or_else(|| selection_model_provider_profiles.get(&state.model))
                        .and_then(|profile| selection_profile_auto_mode_provider.get(profile))
                        .map_or(true, |provider| provider == "firstParty"),
                }
            },
        )));
        let _ = subagent_default_model_selection_provider_cell.set(Arc::new(move || {
            if let Ok(state) = session.try_lock() {
                let selection = agent::DefaultModelSelection {
                    model: state.model.clone(),
                    model_profile: state.model_profile.clone(),
                    provider_first_party: state
                        .model_profile
                        .as_ref()
                        .or_else(|| selection_model_provider_profiles.get(&state.model))
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

    // Fill the hook-attachment sink's cell now that the orchestrator (and its
    // JSONL writer) exists. The sink holds a `Weak`, so this does not create an
    // orchestrator↔hook-executor reference cycle.
    hook_attachment_sink.attach(&orch);
    hook_prompt_runner.attach(&orch);

    // H-CHG-02: wire the enforcing gate's live `set_permission_mode` auto gate to
    // the LIVE `session.model` (mutated by `/model` switches / resume), so a
    // runtime switch to `auto` on an auto-unsupported model is rejected
    // (`dUe(wi())` — claude-code `Nle`). Non-blocking `try_lock`; a contended read
    // returns `None` and the model check is skipped (fail-open). Desktop mirror.
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
        let model_provider_profiles = model_provider_profiles.clone();
        let profile_auto_mode_provider = profile_auto_mode_provider.clone();
        let _ = cell.set(std::sync::Arc::new(move || {
            session.try_lock().ok().map(|state| {
                let profile = state
                    .model_profile
                    .as_ref()
                    .or_else(|| model_provider_profiles.get(&state.model));
                let provider = profile
                    .and_then(|profile| profile_auto_mode_provider.get(profile))
                    .cloned()
                    .unwrap_or_else(|| "firstParty".to_string());
                permission::LiveModelContext {
                    model: state.model.clone(),
                    provider,
                }
            })
        }));
    }

    // (8) Command registry through the mobile composition root.
    let handle: Arc<dyn OrchestratorHandle> = orch.clone();
    // TPM-C (mobile step 2): seed the initial model_profile from a
    // profile-qualified default_model.  SessionState::empty starts model_profile
    // at None; this is a no-op when default_model is a bare id.
    if let Some(profile) = default_model_profile.as_deref() {
        orch.seed_initial_model_profile(&default_model_id, profile)
            .await;
    }
    orch.spawn_startup_responses_websocket_prewarm();
    // Fill the shared registry slot so batch-8, the slash dispatcher, the
    // per-turn skill listing, and the Skill tool all observe ONE command set.
    let mut reg = mobile_command_registry(handle.clone(), auth.clone());
    reg.register_builtin_handler(Arc::new(
        command_api::builtins::VersionHandler::with_build_info(cfg.build_info),
    ));
    crate::mobile::skill_loader::load_mobile_disk_commands_into_registry(
        &mut reg,
        &cwd,
        &cfg.lingxi_home,
        &cwd,
    )
    .await;
    crate::mobile::register_mobile_bundled_prompt_commands(&mut reg);
    crate::mobile::device_skills::register_mobile_device_skills(&mut reg, &device_skill_tools);
    // `/workflows`: mobile cannot open the TUI picker, so bind the shared
    // command handler to the same live registry that powers workflow tools and
    // return the picker's snapshot as a structured command-output result.
    reg.register_builtin_handler(Arc::new(
        command_api::builtins::WorkflowsHandler::with_registry(
            task_registry.clone() as Arc<dyn lingxi_core::host::task_registry::TaskRegistryHandle>
        ),
    ));
    // Batch 8 (`/fork`, `/goal`, `/recap`, `/reload-skills`, `/skill-doctor`,
    // `/stop`): wired here in the uniffi composition root because it needs the
    // shared `Arc<tokio::sync::RwLock<CommandRegistry>>` (tokio is uniffi-only in
    // this crate's default lib build). Mobile has no on-disk custom-skill
    // discovery layer, so no managed dir / no additional dirs / safe-mode off.
    command_api::builtins::register_core_batch_8(
        &mut reg,
        handle.clone(),
        shared_command_registry.clone(),
        cwd.clone(),
        cfg.lingxi_home.clone(),
        None,
        cwd.clone(),
        Vec::new(),
        false,
        disable_agent_view,
    );
    reg.register_builtin_handler(Arc::new(mobile_reload_skills_handler(
        shared_command_registry.clone(),
        cwd.clone(),
        cfg.lingxi_home.clone(),
        cwd.clone(),
        device_skill_tools.clone(),
    )));
    *shared_command_registry.write().await = reg;
    // P1.10 (§19.2): read the activation bit BEFORE materializing the
    // compiled-in bundle. A disabled boot keeps its inventory/status available
    // from compiled metadata but performs no bundle filesystem work; enabling
    // later takes the existing verified materialization + registration path.
    let enabled = mobile_builtin_plugin_enabled_from_settings(
        &plugin_settings,
        crate::mobile::MOBILE_BUILTIN_PLUGIN_DEFAULT_ENABLED,
    );
    if enabled {
        // The plugin manager writes into this shared Arc; registering earlier
        // would be overwritten by the composition-root assignment above and
        // silently drop the plugin's skills/commands.
        let builtin_plugin_bundle_root = cfg.lingxi_home.join("builtin-plugin-bundle");
        if let Err(error) = crate::mobile::register_mobile_builtin_plugins_materialized(
            &plugin_manager,
            &builtin_plugin_bundle_root,
            None,
        )
        .await
        {
            tracing::warn!(
                %error,
                "failed to materialize/register the compiled-in mobile plugin; any \
                 commands/skills/agents it would have contributed are unavailable this boot"
            );
        }
    } else {
        tracing::debug!("mobile builtin plugin disabled; deferring bundle materialization");
    }
    // Recorded installs are the same canonical manifests used by Desktop.
    // Register after the command registry assignment so materialized commands
    // and skills remain visible to the live dispatcher. Discovery resolves
    // dependency order and manifest defaultEnabled, while explicit layer values win.
    for (id, manifest, install_dir) in
        plugin::discover_effective_plugins(&cfg.lingxi_home.join("plugins"), &enabled_plugins).await
    {
        if manifest.name == crate::mobile::MOBILE_BUILTIN_PLUGIN_NAME {
            continue;
        }
        if let Err(error) = plugin_manager.enable(&id, manifest, install_dir).await {
            tracing::warn!(%error,"installed mobile plugin could not be loaded");
        }
    }
    // r2-critic-1 (coverage half): the agent-facing `LocalAppCreate` MCP tool
    // is a SECOND live create entry point — it never enters
    // `handle_create_app`, so that handler's plugin gate does not cover it, and
    // the local-apps transport itself stays connected while the plugin is
    // disabled (`disabled: false, always_load: true` above;
    // `set_builtin_plugin_enabled` only calls `PluginManager::disable`). Hand
    // the transport THIS manager — the same one `handle_create_app` and
    // `PluginCommand::SetEnabled` read and mutate — so both create paths answer
    // one question from one source of truth. Attached here, in the function
    // that owns both halves, so every runtime this composition root builds has
    // it; the probe is fail-closed, so a future root that drops this call
    // refuses creates loudly instead of reopening the hole.
    {
        let manager = plugin_manager.clone();
        let _ = local_apps_mcp.attach_plugin_availability(Arc::new(move || {
            let manager = manager.clone();
            Box::pin(async move {
                matches!(
                    manager
                        .plugin_state(&crate::mobile::mobile_builtin_plugin_id())
                        .await,
                    Some(plugin::PluginState::Loaded { .. })
                )
            })
        }));
    }
    let prompt_paths_orch = orch.clone();
    let dispatcher = RegistrySlashDispatcher::new(shared_command_registry.clone())
        .with_prompt_paths(Arc::new(move || {
            (
                prompt_paths_orch.project_root(),
                prompt_paths_orch.current_cwd(),
            )
        }))
        .with_skill_usage_home(cfg.lingxi_home.clone());
    let dispatcher = if cfg.session_mode == session::jsonl::SessionMode::Code {
        let background_command_handle = handle.clone();
        dispatcher
            .with_background_prompt_launcher(Arc::new(move |prompt| {
                let handle = background_command_handle.clone();
                Box::pin(async move {
                    handle
                        .fork_conversation(&prompt)
                        .await
                        .map(|outcome| {
                            let tail =
                                &outcome.agent_id[outcome.agent_id.len().saturating_sub(4)..];
                            format!(
                                "\u{2442} started code-review in background as {} ({tail})",
                                outcome.name
                            )
                        })
                        .map_err(|error| error.to_string())
                })
            }))
            // (#3) Real embedded-shell expansion for markdown/plugin + builtin
            // `InjectMessage` prompts. Non-MCP only. Chat deliberately leaves
            // both execution providers unwired as a second enforcement layer.
            .with_shell_expansion(shell_expansion_provider)
    } else {
        dispatcher
    };
    // MP-1 (mobile sibling of `harness_runtime::desktop::build`): hand the dispatcher the
    // enforcing gate so each input's frontmatter `disallowed-tools` reaches
    // `alwaysDenyRules.command` (upstream `Tbt`). Wiring only one root would
    // leave the field silently inert on the other.
    let dispatcher = dispatcher.with_permission_gate(permission_policy_gate.clone());

    // (9) Session lifecycle fires (P0.2 — mobile sibling of `harness_runtime::desktop::build`
    //     §7 / §7.1). Fire `SessionStart` then `InstructionsLoaded` now that the
    //     orchestrator + the real hook registry are fully wired:
    //     - `fire_session_start("startup")`: `build_mobile` assembles exactly one
    //       fresh session per call, so the byte-faithful `source` is `"startup"`
    //       (claude-code `utils/hooks.ts` SessionStart path).
    //     - `fire_instructions_loaded()`: fires once per eager LINGXI.md /
    //       `LINGXI.local.md` the memory provider yields (load_reason
    //       `session_start`), exactly as desktop. With the default empty provider
    //       this is a no-op over zero files; with the injected `real_provider()`
    //       it fires over the real hierarchy.
    //     Both are best-effort — each discards the hook aggregate, so a failing /
    //     malformed lifecycle hook never breaks boot, and each is a strict no-op
    //     when no matching hook is registered (the common case). No matching
    //     `SessionEnd` is fired here: like desktop, `build_mobile` returns the
    //     runtime and the FFI host drops it with no hook-capable teardown seam.
    let session_start = orch.fire_session_start("startup").await;
    if session_start.reload_skills {
        let handler = mobile_reload_skills_handler(
            shared_command_registry.clone(),
            cwd.clone(),
            cfg.lingxi_home.clone(),
            cwd.clone(),
            device_skill_tools.clone(),
        );
        if let Some(parsed) = parse_slash_command("/reload-skills") {
            let _ = handler.handle(&parsed).await;
        }
    }
    orch.fire_instructions_loaded().await;
    if interactive_launch {
        if let Some(preference) = permission_preference::load(&cfg.lingxi_home) {
            match permission_policy_gate
                .restore_session_permission_mode(preference.wire_str())
                .await
            {
                Ok(()) => {
                    let _ = lingxi_core::host::OrchestratorHandle::set_plan_mode(
                        orch.as_ref(),
                        preference == PermissionMode::Plan,
                    )
                    .await;
                    requested_permission_mode = preference.wire_str().to_string();
                }
                Err(error) => {
                    tracing::warn!(%error, "saved mobile permission mode rejected by current policy")
                }
            }
        }
    }
    // Publish the effective boot mode as an authoritative event. Auto may be
    // downgraded to Default by the model/provider/killswitch gate, so clients
    // must not infer the effective value from their persisted preference.
    let initial_permission_mode = orch
        .permission_mode()
        .unwrap_or_else(|| PermissionMode::Auto.wire_str().to_string());
    event_sink
        .emit(ClientEvent::PermissionModeChanged {
            mode: initial_permission_mode.clone(),
        })
        .await;

    Ok(MobileRuntime {
        provider_region,
        interactive_launch,
        orchestrator: orch,
        wakeup_scheduler,
        dispatcher,
        slash_registry: shared_command_registry,
        #[cfg(test)]
        wired_skill_listing_provider,
        #[cfg(test)]
        wired_skill_loader,
        #[cfg(test)]
        wired_plugin_manager: plugin_manager.clone(),
        #[cfg(test)]
        wired_plugin_workflow_registry: plugin_workflow_registry,
        #[cfg(test)]
        wired_local_workflow_handler: local_workflow_handler,
        #[cfg(test)]
        wired_workflow_tool: workflow_tool,
        plugin_manager,
        #[cfg(test)]
        wired_agent_catalog: plugin_agent_catalog,
        #[cfg(test)]
        wired_subagent_agent_catalog_cell: subagent_agent_catalog_cell,
        #[cfg(test)]
        wired_subagent_skill_loader_cell: subagent_skill_loader_cell,
        auth,
        oauth,
        permission_gate: adapter_gate,
        permission_policy_gate,
        requested_permission_mode: Arc::new(StdMutex::new(requested_permission_mode)),
        session_mode: cfg.session_mode,
        session_default_permission_mode: initial_permission_mode,
        listener,
        event_sink,
        message_output,
        session_writer,
        oauth_supported,
        credentials,
        mobile_linux,
        mcp_registry,
        mcp_tool_registry: tools,
        mcp_tool_context: initial_mcp_tool_ctx,
        lsp_registry: plugin_lsp_registry,
        typescript_lsp_runtime_available: mobile_lsp_ready,
        mcp_reload_generations,
        mcp_oauth_authorization_url: mcp_auth_url,
        routable_listings: default_listings.clone(),
        provider_model_catalog: full_provider_model_catalog,
        local_apps_mcp,
        local_apps_llm,
        task_registry,
        workspace_leases,
        workflow_checkpoints,
        workflow_status_sink: local_workflow_status_sink,
        workflow_launcher,
        active_session_uuid,
        plan_files,
        app_agent_executor,
    })
}

/// Build the shared mobile session host (plan F3-04): construct the
/// handle-owned tokio runtime, build the [`MobileRuntime`] on it, and return the
/// opaque [`MobileEngineHandle`] both FFI crates re-export.
///
/// The FFI packager crates (`ios-framework` / `android-aar`) call THIS after
/// constructing the device `Platform` from the foreign callbacks — so the
/// runtime / adapter / listener wiring lives in exactly one place and iOS /
/// Android cannot drift. `listener` is the foreign [`ClientEventListener`] the
/// host registers; it is stored on the runtime and fed by the adapter.
/// `permission_sink` is where the gate's outbound permission requests go (on
/// mobile, also the listener's transport).
///
/// Off-device-deterministic: the heavy lifting is [`build_mobile`], which reads
/// nothing from `std::env`. The owned runtime is a fresh `rt-multi-thread`
/// runtime; `build_mobile` (async) is driven to completion on it via
/// `block_on`, after which it owns the orchestrator's spawned work.
///
/// # Errors
///
/// Returns [`MobileEngineError::Internal`] if the tokio runtime cannot be built
/// or [`build_mobile`] fails (effectively infallible in the current wiring).
pub fn build_mobile_engine(
    cfg: MobileConfig,
    platform: Arc<dyn Platform>,
    listener: Arc<dyn ClientEventListener>,
    permission_sink: Arc<dyn PermissionRequestSink>,
) -> Result<Arc<MobileEngineHandle>, MobileEngineError> {
    build_mobile_engine_inner(cfg, platform, listener, permission_sink, None)
}

#[doc(hidden)]
pub fn build_mobile_engine_inner(
    cfg: MobileConfig,
    platform: Arc<dyn Platform>,
    listener: Arc<dyn ClientEventListener>,
    permission_sink: Arc<dyn PermissionRequestSink>,
    streaming_override: Option<Arc<dyn StreamingApiClient>>,
) -> Result<Arc<MobileEngineHandle>, MobileEngineError> {
    // The handle OWNS its runtime (§0.5). A multi-thread runtime so a streaming
    // turn spawned by F3-05 runs concurrently with the FFI read path.
    //
    // The worker stack is set explicitly. A turn driven through
    // `submit(SendPrompt)` builds a deep async state machine, and on tokio's
    // 2 MiB default it overflows and aborts the process with SIGABRT — measured
    // on `host::tests::submit_resume_session_mid_turn_is_rejected`, which
    // reproduces at 2 MiB and passes at 4 MiB. It took the whole harness-runtime::mobile
    // test binary down with it, so every test ordered after it silently never
    // ran. 8 MiB is 2x the measured debug requirement; release frames are
    // smaller, but the margin at the default was clearly not there. The size is
    // reserved address space, not resident memory.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(8 * 1024 * 1024)
        .build()
        .map_err(|e| MobileEngineError::Internal(format!("tokio runtime build failed: {e}")))?;

    // SESSIONS/HISTORY: capture the session-enumerator inputs BEFORE `cfg` /
    // `platform` are moved into `build_mobile_inner`. `submit(ListSessions)`
    // reads the on-disk catalog with these (the SAME `fs` the tools use).
    let lingxi_home = cfg.lingxi_home.clone();
    let session_cwd = cfg.cwd.to_string_lossy().into_owned();
    let fs = platform.filesystem();
    // Capture the build recipe + platform BEFORE they move into
    // `build_mobile_inner`, so the cron firing path can rebuild a fresh throwaway
    // runtime per fired job (the same pattern as `lingxi_home`/`session_cwd`/`fs`).
    let firer_cfg = cfg.clone();
    let firer_platform = platform.clone();
    // Construct turn ownership before the runtime so every adapter-originated
    // event is filtered/reclassified by the same connection-scoped lifecycle
    // listener used by the eventual handle.
    let active_cancel: Arc<Mutex<Option<Arc<ActiveTurn>>>> = Arc::new(Mutex::new(None));
    let durable_turns = Arc::new(DurableTurnStore::new(lingxi_home.join("mobile-turns")));
    // Taken before the wrap: the gate below is what a connection-scoped notice
    // has to get past, so it cannot be reached through the wrapped listener.
    let connection_sink = ListenerSink::arc(listener.clone());
    let lifecycle_listener: Arc<dyn ClientEventListener> = Arc::new(
        TurnLifecycleListener::new_durable(listener, active_cancel.clone(), durable_turns.clone()),
    );

    // `build_mobile` is async; drive it on the owned runtime so any spawned work
    // it does is owned by this handle's runtime, not an ambient one.
    let (ask_user_question_tx, ask_user_question_rx) =
        tokio::sync::mpsc::channel::<tool_ui::AskUserQuestionExchange>(8);
    let inner = runtime
        .block_on(build_mobile_inner_with_ask(
            cfg,
            platform,
            lifecycle_listener,
            permission_sink,
            streaming_override,
            Some(ask_user_question_tx.clone()),
        ))
        .map_err(|e| MobileEngineError::Internal(e.to_string()))?;
    let initial_session_key = runtime.block_on(async {
        inner
            .orchestrator
            .current_session_id()
            .await
            .as_uuid()
            .to_string()
    });
    let (session_lifecycle_tx, _) = tokio::sync::watch::channel(initial_session_key.clone());

    let skill_count = crate::mobile::mobile_skill_registry().len();
    let message_queue = Arc::new(msgqueue::MessageQueueManager::new());
    let loop_transition = Arc::new(Mutex::new(()));
    let loop_state = Arc::new(tool_cron::LoopRuntime::default());
    let loop_delivery = Arc::new(MobileWakeupDelivery {
        transition: loop_transition.clone(),
        queue: message_queue.clone(),
        orchestrator: Arc::downgrade(&inner.orchestrator),
        events: connection_sink.clone(),
        state: loop_state.clone(),
    });
    let loop_scheduler: Arc<dyn tool_cron::WakeupScheduler> =
        Arc::new(tool_cron::RuntimeWakeupScheduler::new(
            Arc::new(platform_posix_minimal::runtime::PosixRuntime::new()),
            loop_state.clone(),
            loop_delivery.clone(),
        ));
    let session_cron = if tool_cron::cron_tools_enabled() {
        let scheduler = Arc::new(
            cron::CronScheduler::new(
                inner.task_registry.clone(),
                fs.clone(),
                firer_platform.clock(),
                Arc::new(platform_posix_minimal::runtime::PosixRuntime::new()),
                cron::tasks_file::session_scheduled_tasks_path(&firer_cfg.cwd),
            )
            .with_session_id(initial_session_key.clone()),
        );
        runtime
            .block_on(async {
                scheduler.set_session_delivery(loop_delivery).await;
                scheduler.clone().start().await
            })
            .map_err(|error| MobileEngineError::Internal(error.to_string()))?;
        Some(scheduler)
    } else {
        None
    };
    let _ = inner.wakeup_scheduler.set(loop_scheduler.clone());
    inner
        .orchestrator
        .set_mid_turn_input(Arc::new(MobileMsgQueueInput {
            queue: message_queue.clone(),
            loop_state: loop_state.clone(),
        }));
    let cancel_reason = orchestrator::prompt::mid_turn_input::CancelReasonFlag::new();
    inner.orchestrator.set_cancel_reason(cancel_reason.clone());
    runtime.block_on({
        let message_queue = message_queue.clone();
        let cancel_reason = cancel_reason.clone();
        async move {
            message_queue
                .set_now_abort_hook(Arc::new(move || {
                    cancel_reason
                        .set(orchestrator::prompt::mid_turn_input::CancelReason::QueueNowCommand);
                }))
                .await;
        }
    });

    let event_sink = inner.event_sink.clone();
    let ask_user_question_broker = Arc::new(client::adapter::AskUserQuestionBroker::new(
        event_sink.clone(),
    ));
    {
        let broker = ask_user_question_broker.clone();
        runtime.spawn(async move { broker.run(ask_user_question_rx).await });
    }

    // LOCAL-APPS: one process-wide service per profile root. Conversation or
    // provider source changes only add/remove event subscribers; they do not
    // open a second SQLite/Git/generation owner for the same application data.
    let app_emissions = crate::mobile::local_apps_bridge::AppEmissionQueue::spawn(
        runtime.handle(),
        event_sink.clone(),
        mobile_apps_data_root(&firer_cfg),
    );
    let loaded_profile = runtime.block_on(profile_apps(
        mobile_apps_data_root(&firer_cfg),
        firer_platform.clock(),
        inner.mobile_linux.clone(),
        firer_cfg.local_apps_full_runtime,
        firer_cfg.local_apps_runtime_root.clone(),
        firer_cfg.physical_memory_bytes,
        inner.local_apps_llm.clone(),
        crate::mobile::local_apps_device::DeviceCapabilities {
            camera: firer_platform.camera(),
            audio: firer_platform.audio_service(),
            location: firer_platform.location(),
            notifications: firer_platform.notifications(),
            clipboard: firer_platform.clipboard(),
            share: firer_platform.share(),
            device_status: firer_platform.device_status(),
            haptics: firer_platform.haptics(),
            deep_link: firer_platform.deep_link(),
            calendar: firer_platform.calendar(),
            contacts: firer_platform.contacts(),
        },
    ));
    let (
        local_apps,
        local_apps_host,
        retained_profile,
        app_client_subscription,
        app_domain_subscription,
        app_domain_observer,
    ) = match loaded_profile {
        Ok(profile) => {
            let client_subscription = profile.client_events.subscribe(event_sink.clone());
            let observer = Arc::new(crate::mobile::local_apps_bridge::SinkAppEventObserver::new(
                app_emissions.clone(),
            ));
            let domain_subscription = profile.domain_events.subscribe(observer.clone());
            (
                Ok(profile.service.clone()),
                profile.host.clone(),
                Some(profile),
                Some(client_subscription),
                Some(domain_subscription),
                Some(observer),
            )
        }
        Err(error) => {
            // Preserve the established failure contract: a corrupt store does
            // not brick the conversation engine; every app command returns the
            // typed load error. This unattached fallback can only report that
            // same unavailable state and never mutate data.
            let host = LocalAppsHostBroker::new_with_physical_memory(
                mobile_apps_data_root(&firer_cfg),
                event_sink.clone(),
                inner.mobile_linux.clone(),
                firer_cfg.local_apps_full_runtime,
                firer_cfg.local_apps_runtime_root.clone(),
                firer_cfg.physical_memory_bytes,
            );
            (Err(error), host, None, None, None, None)
        }
    };
    if inner
        .local_apps_mcp
        .attach_host(local_apps_host.clone())
        .is_err()
    {
        tracing::warn!("local-apps MCP host was already attached");
    }
    // Terminal Local App build/use-test outcomes are revalidated by the
    // Host-owned QA boundary before the workflow sink publishes completion.
    // Keep this as a weak, one-time composition attachment: the sink must not
    // retain the profile broker or create a broker↔workflow ownership cycle.
    if inner
        .workflow_status_sink
        .attach_local_apps_host(Arc::downgrade(&local_apps_host))
        .is_err()
    {
        tracing::warn!("local-apps workflow status sink was already attached");
    }
    if local_apps_host
        .attach_mcp_registry(Arc::downgrade(&inner.mcp_registry))
        .is_err()
    {
        tracing::warn!("local-apps MCP registry was already attached");
    }
    if local_apps_host
        .attach_lsp_registry(Arc::downgrade(&inner.lsp_registry))
        .is_err()
    {
        tracing::warn!("local-apps LSP registry was already attached");
    }
    if local_apps_host
        .attach_agent_executor(inner.app_agent_executor.clone())
        .is_err()
    {
        tracing::warn!("local-apps Agent executor was already attached");
    }
    // The same host facts that render the mobile runtime reminder. A local
    // app's device context is derived from these, never declared by the
    // agent — the reminder's `Device class: phone` is not an iOS form factor,
    // so an agent reading it could only produce a rejected pair.
    if let Some(host_environment) = firer_cfg.host_environment.clone() {
        if local_apps_host
            .attach_host_environment(host_environment)
            .is_err()
        {
            tracing::warn!("local-apps host environment was already attached");
        }
    }
    // Where an app's pinned init session lives, so `LocalAppScaffold` can
    // rename it out of the shell placeholder the moment the app is formed.
    // The broker already knows the apps data root; `lingxi_home` and the
    // filesystem are the composition root's to hand over.
    if local_apps_host
        .attach_session_catalog(crate::mobile::local_apps_host::SessionCatalog {
            lingxi_home: firer_cfg.lingxi_home.clone(),
            fs: fs.clone(),
        })
        .is_err()
    {
        tracing::warn!("local-apps session catalog was already attached");
    }
    match &local_apps {
        Ok(service) => {
            if inner
                .local_apps_mcp
                .attach_service(service.clone())
                .is_err()
            {
                tracing::warn!("local-apps MCP service was already attached");
            }
            // A freshly-built handle has not passed through
            // `retarget_session_writer`, which is the normal New/Resume/Clear
            // activation boundary. Restore the INITIAL app conversation here
            // so an already-published MCP is usable immediately after a cold
            // boot. This is deliberately bounded to the current Host-owned app
            // cwd: global/project startup performs no all-app publication
            // sweep, and the Local App authoring plugin's enabled bit does not
            // suppress an independently enabled published app MCP.
            let apps_data_root = mobile_apps_data_root(&firer_cfg);
            if let Some(app_id) = mobile_local_app_scope_id(&firer_cfg.cwd, &apps_data_root) {
                match runtime.block_on(local_apps_host.expose_managed_mcp_for_conversation(
                    &initial_session_key,
                    &app_id,
                    false,
                )) {
                    Ok(true) => {
                        // The registry listener is asynchronous. Rebuild the
                        // already-connected partitions here as well so the
                        // freshly returned engine cannot race its first turn
                        // against delivery of the connect notification.
                        let refreshed = runtime.block_on(tool_mcp::build_registered_mcp_tools(
                            &inner.mcp_registry,
                            inner.mcp_tool_context.clone(),
                        ));
                        inner.mcp_tool_registry.replace_mcp_tools(refreshed);
                    }
                    Ok(false) => {}
                    Err(error) => tracing::warn!(
                        session_id = %initial_session_key,
                        %app_id,
                        %error,
                        "failed to expose initial managed Local App MCP tools"
                    ),
                }
            }
            // v3 Phase 4: repair init-session pins, drifted catalogs and
            // placeholder titles for every app. Runs as a background sweep on
            // the shared worker runtime (this builder is sync); see
            // `run_app_boot_backfill_sweep` for what it repairs and why each
            // repair is retried rather than rolled back.
            //
            // r1-backlog-engine-create-11: `boot_backfill_sweep_should_run`
            // keeps this call matching the function's own "once per launch"
            // doc — this builder re-runs on every scope switch/reconnect
            // within the same process, and without the guard the sweep would
            // re-walk every app record on each one.
            if boot_backfill_sweep_should_run(&apps_data_root) {
                crate::mobile::local_apps_profile::worker_runtime().spawn(
                    run_app_boot_backfill_sweep(
                        firer_cfg.lingxi_home.clone(),
                        firer_cfg.cwd.to_string_lossy().to_string(),
                        apps_data_root,
                        fs.clone(),
                        service.clone(),
                        local_apps_host.clone(),
                    ),
                );
            }
        }
        Err(error) => {
            tracing::warn!(
                error = %error,
                "local-apps store failed to load; app commands will report the failure"
            );
        }
    }

    let settings_write_lock = mobile_settings_write_lock(&lingxi_home.join("settings.json"));
    // Use the native turn slot and permission owner, exactly like SendPrompt.
    // Capture runtime components, never the handle itself: retaining/upgrading
    // the FFI Arc on a runtime worker can make that worker drop its own Runtime.
    let task_notification_watcher = {
        let registry = inner.task_registry.clone();
        let orch = inner.orchestrator.clone();
        let permission_gate = inner.permission_gate.clone();
        let session_uuid = inner.active_session_uuid.clone();
        let message_output = inner.message_output.clone();
        let sink = event_sink.clone();
        let active_cancel = active_cancel.clone();
        let message_queue = message_queue.clone();
        let cancel_reason = cancel_reason.clone();
        let loop_scheduler = loop_scheduler.clone();
        let loop_transition = loop_transition.clone();
        runtime
            .spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    let turn_busy = active_cancel.lock().await.is_some();
                    if turn_busy
                        && !message_queue
                            .get_by_max_priority(msgqueue::QueuePriority::Next, |command| {
                                command.is_main_thread()
                            })
                            .await
                            .is_empty()
                        && !lingxi_core::host::env::background_tasks_disabled()
                    {
                        registry
                            .background_all_tasks_with_reason(
                                lingxi_core::host::task_registry::TaskBackgroundReason::DeliverMessage,
                            )
                            .await;
                        continue;
                    }
                    let notifications = registry.has_pending_task_notifications_for(None).await;
                    let queued = message_queue
                        .get_by_max_priority(msgqueue::QueuePriority::Later, |command| {
                            command.is_main_thread()
                                && (command.uuid.starts_with("goal-retry-")
                                    || matches!(
                                        command.source,
                                        msgqueue::QueueSource::Cron
                                            | msgqueue::QueueSource::PromptInput
                                    ))
                        })
                        .await;
                    if !notifications && queued.is_empty() {
                        continue;
                    }
                    let _transition = loop_transition.lock().await;
                    let mut active = active_cancel.lock().await;
                    if active.is_some() {
                        continue;
                    }
                    let scheduled = if notifications {
                        None
                    } else {
                        message_queue
                            .dequeue_filtered(|command| {
                                command.is_main_thread()
                                    && (command.uuid.starts_with("goal-retry-")
                                        || matches!(
                                            command.source,
                                            msgqueue::QueueSource::Cron
                                                | msgqueue::QueueSource::PromptInput
                                        ))
                            })
                            .await
                    };
                    if !notifications && scheduled.is_none() {
                        continue;
                    }
                    let session_id = session_uuid.lock().map(|id| id.clone()).unwrap_or_default();
                    let permission_owner_id =
                        permission_gate.begin_main_turn(Some(session_id.clone()), None);
                    let turn =
                        Arc::new(ActiveTurn::new_owned(None, session_id, permission_owner_id));
                    *active = Some(turn.clone());
                    drop(active);
                    cancel_reason.reset();
                    message_queue
                        .register_active_turn(turn.cancel.clone())
                        .await;
                    message_output.reset_message_buffer().await;
                    orch.turn_span().reset();
                    let scheduled_task_id = scheduled
                        .as_ref()
                        .and_then(|command| command.scheduled_task_id.clone());
                    let scheduled_fire_id = scheduled
                        .as_ref()
                        .and_then(|command| command.scheduled_fire_id.clone());
                    let goal_retry_id = scheduled
                        .as_ref()
                        .filter(|c| c.uuid.starts_with("goal-retry-"))
                        .map(|c| c.uuid.clone());
                    let is_scheduled = scheduled
                        .as_ref()
                        .is_some_and(|command| command.source == msgqueue::QueueSource::Cron);
                    let human = scheduled.as_ref().is_some_and(|command| {
                        command.source == msgqueue::QueueSource::PromptInput && !command.is_meta
                    });
                    let prompt = scheduled
                        .as_ref()
                        .and_then(|command| {
                            command.text().map(|raw| {
                                if command.source == msgqueue::QueueSource::Cron {
                                    if let Some(state) = loop_scheduler.loop_runtime() {
                                        state
                                            .try_resolve_loop_default_fire(
                                                raw,
                                                &orch.project_root(),
                                                &orch.current_cwd(),
                                            )
                                            .inspect(|_prompt| {
                                                if command.uuid.starts_with("loop-wakeup-") {
                                                    state.begin_tick(raw.into());
                                                } else {
                                                    state.take_in_flight_prompt();
                                                    state.invalidate_noop_streak();
                                                }
                                            })
                                    } else {
                                        Ok(raw.to_string())
                                    }
                                } else {
                                    if let Some(state) = loop_scheduler.loop_runtime() {
                                        state.take_in_flight_prompt();
                                        state.invalidate_noop_streak();
                                    }
                                    Ok(raw.to_string())
                                }
                            })
                        })
                        .transpose();
                    if matches!(&prompt, Ok(None)) {
                        if let Some(state) = loop_scheduler.loop_runtime() {
                            state.take_in_flight_prompt();
                        }
                    }
                    // The orchestrator's notification entry emits TurnStarted; no
                    // empty prompt or synthetic durable user checkpoint is created.
                    let (
                        orch,
                        registry,
                        active_cancel,
                        permission_gate,
                        message_queue,
                        message_output,
                        sink,
                        task_turn,
                    ) = (
                        orch.clone(),
                        registry.clone(),
                        active_cancel.clone(),
                        permission_gate.clone(),
                        message_queue.clone(),
                        message_output.clone(),
                        sink.clone(),
                        turn.clone(),
                    );
                    let scheduler = loop_scheduler.clone();
                    let reason = cancel_reason.clone();
                    let task = tokio::spawn(async move {
                        let result = match prompt {
                            Err(error) => {
                                Err(orchestrator::OrchestratorError::Internal(error.to_string()))
                            }
                            Ok(Some(prompt)) if is_scheduled || goal_retry_id.is_some() => {
                                orch.run_queued_prompt_batch(
                                    vec![orchestrator::QueuedPromptInput {
                                        goal_retry_id,
                                        text: prompt,
                                        is_meta: true,
                                        message_id: None,
                                        queue_priority: Some("later".into()),
                                        scheduled_task_id,
                                        scheduled_fire_id,
                                    }],
                                    task_turn.cancel.clone(),
                                )
                                .await
                            }
                            Ok(Some(prompt)) => {
                                orch.run_turn_streaming_with_origin(
                                    &prompt,
                                    Vec::new(),
                                    task_turn.cancel.clone(),
                                    None,
                                    human,
                                )
                                .await
                            }
                            Ok(None) => {
                                orch.run_task_notification_rewake(
                                    registry.as_ref(),
                                    task_turn.cancel.clone(),
                                )
                                .await
                            }
                        };
                        if let Err(error) = result {
                            message_output.reset_message_buffer().await;
                            sink.emit(client::adapter::map_orchestrator_error(&error))
                                .await;
                        }
                        settle_mobile_loop_turn(
                            &orch,
                            &scheduler,
                            &task_turn.cancel,
                            &reason,
                            human,
                        )
                        .await;
                        let mut active = active_cancel.lock().await;
                        if active
                            .as_ref()
                            .is_some_and(|owner| Arc::ptr_eq(owner, &task_turn))
                        {
                            *active = None;
                        }
                        drop(active);
                        if let Some(owner_id) = task_turn.permission_owner_id {
                            permission_gate.end_main_turn(owner_id);
                        }
                        message_queue.clear_active_turn().await;
                        task_turn.mark_completed();
                    });
                    turn.set_task_handle(task);
                }
            })
            .abort_handle()
    };
    let settings_paths = ::configuration_admin::settings_bridge::SettingsPaths {
        lingxi_home: lingxi_home.clone(),
        project_dir: std::path::PathBuf::from(&session_cwd),
    };
    let managed = std::collections::BTreeMap::new();
    let mut active =
        ::configuration_admin::settings_bridge::active_settings_baseline(&settings_paths, &managed);
    active.insert(
        "providerRegion".into(),
        serde_json::to_value(inner.provider_region).expect("region serializes"),
    );
    let settings = Some(::configuration_admin::settings_bridge::SettingsContext {
        paths: settings_paths,
        active: Arc::new(std::sync::RwLock::new(active)),
        managed,
    });
    let handle = Arc::new(MobileEngineHandle {
        session_cron,
        scheduled_reload: std::sync::atomic::AtomicBool::new(false),
        settings,
        task_notification_watcher,
        runtime,
        inner,
        event_sink,
        connection_sink,
        active_cancel,
        message_queue,
        loop_transition,
        cancel_reason,
        ask_user_question_broker,
        durable_turns,
        ask_user_question_tx,
        skill_count,
        lingxi_home,
        settings_write_lock,
        session_cwd,
        session_lifecycle_tx,
        fs,
        firer_cfg,
        firer_platform,
        local_apps,
        app_emissions,
        local_apps_host,
        profile_apps: retained_profile,
        app_client_subscription,
        app_domain_subscription,
        app_domain_observer,
    });
    {
        // This runs on every scope switch / reconnect within one process, not
        // just at process start, so drop the handles whose engine is already
        // gone before appending. Without this the vector grows for the app's
        // lifetime and every targeted scheduled run walks all of it.
        let mut handles = MOBILE_CRON_HANDLES
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        handles.retain(|existing| existing.strong_count() > 0);
        handles.push(Arc::downgrade(&handle));
    }
    handle.runtime.block_on(handle.emit_controls_snapshot());
    handle.runtime.block_on(handle.emit_typescript_lsp_mode());
    Ok(handle)
}
