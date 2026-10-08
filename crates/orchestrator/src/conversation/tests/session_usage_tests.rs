use super::*;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::OrchestratorConfig;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tool_api::registry::ToolRegistry;

fn bare_orchestrator(api: Arc<MockApiClient>, cwd: &std::path::Path) -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api,
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        cwd.to_path_buf(),
    )
}

#[tokio::test]
async fn session_usage_api_returns_live_snapshot_and_validates_native_arguments() {
    let root = tempfile::tempdir().unwrap();
    let module = root.path().join("session-usage.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
          on('tool.call', async ($) => ({ result: {
            noArgs: await $.session.usage(),
            columnsOnly: await $.session.usage({ columns: 64 }),
            extraField: await $.session.usage({ extra: true }).then(
              () => 'unexpected success', error => error.message),
            badBreakdown: await $.session.usage({ breakdown: 'detail' }).then(
              () => 'unexpected success', error => error.message),
            badColumns: await $.session.usage({ columns: 0 }).then(
              () => 'unexpected success', error => error.message),
            requestedBreakdown: await $.session.usage({ breakdown: 'summary' }).then(
              () => 'unexpected success', error => error.message)
          } }));
        }"#,
    )
    .unwrap();

    let now_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let active_reset = now_seconds + 3_600;
    let api = Arc::new(MockApiClient::new(vec![]));
    api.set_raw_utilization(Some(llm_runtime::model::rate_limit::RawUtilization {
        five_hour: Some(llm_runtime::model::rate_limit::RawWindow {
            utilization: 0.42,
            resets_at: active_reset,
        }),
        seven_day: Some(llm_runtime::model::rate_limit::RawWindow {
            utilization: 0.91,
            resets_at: now_seconds.saturating_sub(1),
        }),
    }));

    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load("usage-mod", root.path(), &module, serde_json::json!({}))
        .await
        .unwrap();
    let orchestrator = bare_orchestrator(api.clone(), root.path());
    orchestrator
        .compaction_runtime
        .last_response_input_tokens
        .store(123, std::sync::atomic::Ordering::Relaxed);

    let result = host
        .dispatch_with_log_at_session(
            "tool.call",
            serde_json::json!({"tool":"UsageProbe"}),
            &orchestrator,
            |_| async { panic!("Mod answers tool.call") },
            |_, _| async {},
        )
        .await
        .unwrap();
    let probe = &result["result"];
    let usage = &probe["noArgs"];
    let columns_only = &probe["columnsOnly"];

    let model = orchestrator.session.lock().await.model.clone();
    let expected_window =
        llm_runtime::model::context_window::context_window_for_model(&model, &api.active_betas());
    let expected_percent = ((123.0 / expected_window as f64) * 100.0)
        .round()
        .clamp(0.0, 100.0);
    assert_eq!(usage["context"]["window"], expected_window);
    assert_eq!(usage["context"]["tokens"], 123);
    assert_eq!(usage["context"]["percent"].as_f64(), Some(expected_percent));
    assert!(usage["context"].get("breakdown").is_none());
    assert_eq!(usage["rateLimits"].as_array().unwrap().len(), 1);
    assert_eq!(usage["rateLimits"][0]["kind"], "five_hour");
    assert_eq!(usage["rateLimits"][0]["percentUsed"].as_f64(), Some(42.0));
    let expected_reset =
        chrono::DateTime::<chrono::Utc>::from_timestamp(i64::try_from(active_reset).unwrap(), 0)
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    assert_eq!(usage["rateLimits"][0]["resetsAt"], expected_reset);
    assert_eq!(usage["cost"]["usd"].as_f64(), Some(0.0));
    let started_at = usage["startedAt"].as_str().expect("ISO startedAt");
    chrono::DateTime::parse_from_rfc3339(started_at).unwrap();

    // `columns` has no effect unless `breakdown` requests contextData.
    assert_eq!(columns_only["context"], usage["context"]);
    assert_eq!(columns_only["rateLimits"], usage["rateLimits"]);
    assert_eq!(columns_only["cost"], usage["cost"]);
    assert!(columns_only["startedAt"].is_string());

    assert_eq!(
        probe["extraField"],
        "usage-mod: $.session.usage takes { breakdown, columns } or nothing (not extra)"
    );
    assert_eq!(
        probe["badBreakdown"],
        "usage-mod: $.session.usage takes breakdown \"summary\" or \"full\" (got detail)"
    );
    assert_eq!(
        probe["badColumns"],
        "usage-mod: $.session.usage takes columns, a positive whole number (got 0)"
    );
    assert!(probe["requestedBreakdown"]
        .as_str()
        .unwrap()
        .contains("contextData builder"));
}

#[tokio::test]
async fn session_usage_default_snapshot_keeps_window_cost_and_empty_limits() {
    let root = tempfile::tempdir().unwrap();
    let api = Arc::new(MockApiClient::new(vec![]));
    let orchestrator = bare_orchestrator(api.clone(), root.path());
    let snapshot = orchestrator
        .mod_session_usage_snapshot(None, None)
        .await
        .unwrap();

    assert!(snapshot["startedAt"].is_string());
    assert!(snapshot["context"]["window"].as_u64().unwrap() > 0);
    assert!(snapshot["context"].get("tokens").is_none());
    assert!(snapshot["context"].get("percent").is_none());
    assert_eq!(snapshot["rateLimits"], serde_json::json!([]));
    assert_eq!(snapshot["cost"]["usd"].as_f64(), Some(0.0));
}
