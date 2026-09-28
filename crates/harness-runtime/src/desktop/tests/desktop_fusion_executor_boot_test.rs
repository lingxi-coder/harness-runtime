use super::*;

/// Config preflight never reaches a provider, but every executor now
/// carries an attempt registrar, so build a real one over an unreachable
/// service rather than reintroducing an unregistered executor shape.
struct UnreachableTransport;
impl llm_runtime::Transport for UnreachableTransport {
    fn execute<'a>(
        &'a self,
        _: &'a llm_runtime::ProviderRequest,
    ) -> llm_runtime::transport::BoxFuture<
        'a,
        Result<llm_runtime::ProviderResponse, llm_runtime::LlmError>,
    > {
        Box::pin(async { panic!("preflight_error() must not send") })
    }
    fn open_stream<'a>(
        &'a self,
        _: &'a llm_runtime::ProviderRequest,
    ) -> llm_runtime::transport::BoxFuture<
        'a,
        Result<llm_runtime::transport::StreamingResponse, llm_runtime::LlmError>,
    > {
        Box::pin(async { panic!("preflight_error() must not stream") })
    }
}

fn unreachable_attempts() -> Arc<fusion_attempts::DesktopFusionAttempts> {
    let pricing = Arc::new(cost::PricingCatalog::builtin_reference());
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let tracker = Arc::new(cost::CostTracker::new(
        protocol::SessionId::new(),
        pricing.clone(),
        tx,
    ));
    let budget = Arc::new(cost::BudgetEnforcer::new(
        cost::BudgetConfig {
            max_session_nano_usd: None,
            max_turn_nano_usd: None,
            max_turn_tokens: None,
            warning_thresholds: Vec::new(),
            on_exceed: cost::BudgetExceedPolicy::Halt,
        },
        tracker.clone(),
    ));
    let outputs = budget.workflow_output_scopes();
    desktop_fusion_attempts(
        Arc::new(llm_runtime::ApiService::new(
            Arc::new(llm_runtime::DefaultLlmClient::from_config(Default::default()).unwrap()),
            Arc::new(UnreachableTransport),
            Default::default(),
            Default::default(),
            "test",
            None,
            None,
        )),
        budget,
        tracker,
        pricing,
        outputs,
    )
}

/// Never actually called: `preflight_error()` re-validates settings
/// directly and must not spawn a panel or issue a side query.
struct UnreachableSpawner;
#[async_trait::async_trait]
impl platform_api::subagent_spawn::SubagentSpawner for UnreachableSpawner {
    async fn spawn(
        &self,
        _request: platform_api::subagent_spawn::SubagentSpawnRequest,
        _inherit: platform_api::subagent_spawn::SubagentInheritance,
    ) -> Result<
        platform_api::subagent_spawn::SubagentResult,
        platform_api::subagent_spawn::SubagentSpawnError,
    > {
        panic!("preflight_error() must not spawn a panel");
    }
}

struct UnreachableSideQuery;
#[async_trait::async_trait]
impl sidequery::SideQueryClient for UnreachableSideQuery {
    async fn query(
        &self,
        _request: sidequery::SideQueryRequest,
    ) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
        panic!("preflight_error() must not issue a side query");
    }
}

/// Finding [14]: a `fusion.*` value that is invalid only on the MERGED
/// view (passes per-file validation, fails `FusionRuntimeConfig::from_settings`)
/// pins the boot-time `InvalidConfiguration` — before this fix, in a
/// frozen `RejectedFusionExecutor` for the executor's whole lifetime.
/// This test proves the fix: after the SAME executor is constructed
/// once, correcting the settings file and calling `preflight_error()`
/// again (no restart, no re-construction) must recover to `None`,
/// exactly as `DesktopFusionExecutor`'s doc comment now promises.
#[test]
fn preflight_error_recovers_after_a_fix_then_save_without_restart() {
    // `desktop_fusion_runtime_config` reads the managed tier at the call
    // boundary, and a managed tier outranks the user settings this test
    // fixes. Siblings in this binary set `MANAGED_DIR_ENV` process-wide, so
    // without the lock a neighbour's managed directory can be live here and
    // the "fixed" config stays invalid — the exact failure this asserts
    // against, arriving from somewhere else.
    let _guard = crate::desktop::tests::MANAGED_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let cwd = tmp.path().join("project");
    let lingxi_home = tmp.path().join("home");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&lingxi_home).unwrap();
    // Passes FusionSettingsJson::validate() per-file (no stage field is
    // present alongside totalTimeoutMs in this same file), but fails the
    // MERGED-view stage-sum check in FusionRuntimeConfig::from_settings:
    // the default stage sum is 600_000 + 120_000*2 + 180_000 = 1_020_000
    // > 500_000.
    std::fs::write(
        lingxi_home.join("settings.json"),
        r#"{"fusion":{"totalTimeoutMs":500000}}"#,
    )
    .unwrap();
    let cfg = DesktopConfig {
        cwd: cwd.clone(),
        lingxi_home,
        ..DesktopConfig::default()
    };

    let executor = desktop_fusion_executor(
        Arc::new(UnreachableSpawner),
        Arc::new(UnreachableSideQuery),
        &cfg,
        unreachable_attempts(),
        Arc::new(Vec::<fusion::CatalogModel>::new()),
        Arc::new(telemetry::AnalyticsBus::new()),
        Arc::new(cost::PricingCatalog::builtin_reference()),
    );

    let boot_error = executor
        .preflight_error()
        .expect("the invalid merged config must surface as a preflight error");
    let platform_api::FusionError::InvalidConfiguration(msg) = boot_error else {
        panic!("expected InvalidConfiguration, got {boot_error:?}");
    };
    assert!(
        msg.contains("must not exceed fusion.totalTimeoutMs"),
        "got: {msg}"
    );

    // Fix-then-save: raise totalTimeoutMs above the stage sum. Same
    // `executor` instance — no restart, no re-construction.
    std::fs::write(
        cfg.lingxi_home.join("settings.json"),
        r#"{"fusion":{"totalTimeoutMs":1500000}}"#,
    )
    .unwrap();

    assert_eq!(
        executor.preflight_error(),
        None,
        "a fixed-then-saved settings file must recover within the same \
         session, not only after a process restart"
    );
}

#[test]
fn preflight_reads_the_latest_managed_policy_without_reconstruction() {
    let _env_guard = crate::desktop::tests::MANAGED_ENV_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().expect("tempdir");
    let managed = tmp.path().join("managed-settings.json");
    std::fs::write(&managed, r#"{"fusion":{"totalTimeoutMs":500000}}"#)
        .expect("write managed settings");
    std::env::set_var(super::settings_watch::MANAGED_DIR_ENV, tmp.path());

    let cfg = DesktopConfig {
        cwd: tmp.path().join("project"),
        lingxi_home: tmp.path().join("home"),
        ..DesktopConfig::default()
    };
    let executor = desktop_fusion_executor(
        Arc::new(UnreachableSpawner),
        Arc::new(UnreachableSideQuery),
        &cfg,
        unreachable_attempts(),
        Arc::new(Vec::<fusion::CatalogModel>::new()),
        Arc::new(telemetry::AnalyticsBus::new()),
        Arc::new(cost::PricingCatalog::builtin_reference()),
    );
    assert!(executor.preflight_error().is_some());

    std::fs::write(&managed, r#"{"fusion":{"totalTimeoutMs":1500000}}"#)
        .expect("replace managed settings");
    assert_eq!(
        executor.preflight_error(),
        None,
        "preflight must use the same fresh managed snapshot as config load"
    );
    std::env::remove_var(super::settings_watch::MANAGED_DIR_ENV);
}
