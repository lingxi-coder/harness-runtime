use super::*;
use crate::test_support::{
    mock_message_response, noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use crate::OrchestratorConfig;
use std::sync::Arc;

struct MeasureSession;

#[async_trait::async_trait]
impl hooks::mods::ModSessionContext for MeasureSession {
    fn cwd(&self) -> std::path::PathBuf {
        std::env::current_dir().unwrap()
    }

    fn root(&self) -> std::path::PathBuf {
        std::env::current_dir().unwrap()
    }

    async fn model(&self) -> String {
        "claude-opus-4-7".to_owned()
    }

    async fn id(&self) -> String {
        "session-measure-test".to_owned()
    }

    async fn turns(&self) -> u64 {
        0
    }
}

async fn build_orchestrator(
    api: Arc<MockApiClient>,
    output: Arc<MockOutputStream>,
    root: &std::path::Path,
) -> (
    Arc<ConversationOrchestrator>,
    Arc<MeasureSession>,
    Arc<hooks::mods::ModHost>,
) {
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    let session = Arc::new(MeasureSession);
    let session_trait: Arc<dyn hooks::mods::ModSessionContext> = session.clone();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host.clone());
    registry.attach_mod_background_context(Arc::downgrade(&session_trait));
    let orchestrator = Arc::new(
        ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            api,
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            output,
            Arc::new(StaticMemoryProvider::empty()),
            root.to_path_buf(),
        )
        .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry))),
    );
    (orchestrator, session, host)
}

async fn logs(output: &MockOutputStream) -> Vec<String> {
    output
        .snapshot()
        .await
        .into_iter()
        .filter_map(|event| match event {
            lingxi_core::host::OutputEvent::ModLog { plugin, text } if plugin == "measure-mod" => {
                Some(text)
            }
            _ => None,
        })
        .collect()
}

async fn wait_for_log_count(output: &MockOutputStream, prefix: &str, count: usize) -> Vec<String> {
    tokio::time::timeout(std::time::Duration::from_secs(4), async {
        loop {
            let logs = logs(output).await;
            if logs.iter().filter(|line| line.starts_with(prefix)).count() >= count {
                return logs;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("expected Mod event log")
}

fn measure_entry(percent: f64) -> llm_runtime::model::rate_limit::RawUtilization {
    llm_runtime::model::rate_limit::RawUtilization {
        five_hour: Some(llm_runtime::model::rate_limit::RawWindow {
            utilization: percent / 100.0,
            resets_at: 2_000_000_000,
        }),
        seven_day: None,
    }
}

#[tokio::test]
async fn session_measure_runs_after_turn_complete_and_only_on_changed_units() {
    let root = tempfile::tempdir().unwrap();
    let module = root.path().join("session-measure.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
          on('turn.complete', ($, e, next) => {
            $.ui.log(`complete:${e.turnId}`, { to: 'transcript' });
            return next(e);
          });
          on('session.measure', async ($, e, next) => {
            const result = await next(e);
            $.ui.log(`measure:${JSON.stringify(e)}|${JSON.stringify(result)}`, { to: 'transcript' });
            return result;
          });
        }"#,
    )
    .unwrap();

    let mut responses = Vec::new();
    for _ in 0..3 {
        let mut response = mock_message_response(
            vec![llm_runtime::ContentBlock::Text {
                text: "answer".to_owned(),
                cache_control: None, citations: None,
            }],
            Some("end_turn"),
        );
        response.usage.counts_mut().input_tokens = 123;
        response.usage.counts_mut().output_tokens = 7;
        responses.push(response);
    }
    let api = Arc::new(MockApiClient::new(responses));
    api.set_raw_utilization(Some(measure_entry(25.0)));
    api.set_rate_limit_full(Some(crate::model::rate_limit::RateLimitInfo {
        status: Some("allowed".to_owned()),
        ..Default::default()
    }));
    let output = Arc::new(MockOutputStream::new());
    let (orchestrator, _session, host) =
        build_orchestrator(api.clone(), output.clone(), root.path()).await;

    // Seed the observed API snapshots before the Mod is registered. The first
    // turn then exercises only the queued `turn` reason; the separate idle
    // change below covers the native statusChanged/`limits` path.
    orchestrator.emit_rate_limit_if_changed().await;
    orchestrator.emit_raw_utilization_if_changed().await;
    host.load("measure-mod", root.path(), &module, serde_json::json!({}))
        .await
        .unwrap();

    orchestrator.run_turn("first").await.unwrap();
    wait_for_log_count(&output, "complete:", 1).await;
    let first = wait_for_log_count(&output, "measure:", 1).await;
    let first_measure = first
        .iter()
        .find(|line| line.starts_with("measure:"))
        .unwrap();
    let (first_input, first_result) = first_measure
        .strip_prefix("measure:")
        .unwrap()
        .split_once('|')
        .unwrap();
    let first_input: serde_json::Value = serde_json::from_str(first_input).unwrap();
    let first_result: serde_json::Value = serde_json::from_str(first_result).unwrap();
    let model = orchestrator.session.lock().await.model.clone();
    let expected_window =
        llm_runtime::model::context_window::context_window_for_model(&model, &api.active_betas());
    assert_eq!(first_input["context"]["window"], expected_window);
    if expected_window > 0 {
        let expected_percent = ((123.0 / expected_window as f64) * 100.0)
            .round()
            .clamp(0.0, 100.0);
        assert_eq!(first_input["context"]["tokens"], 123);
        assert_eq!(
            first_input["context"]["percent"].as_f64(),
            Some(expected_percent),
        );
    } else {
        assert!(first_input["context"].get("tokens").is_none());
        assert!(first_input["context"].get("percent").is_none());
    }
    assert_eq!(first_input["rateLimits"].as_array().unwrap().len(), 1);
    let first_limit = &first_input["rateLimits"][0];
    assert_eq!(first_limit["kind"], "five_hour");
    assert_eq!(first_limit["percentUsed"].as_f64(), Some(25.0));
    assert_eq!(first_limit["resetsAt"], "2033-05-18T03:33:20.000Z");
    assert!(first_input.get("cost").is_none());
    assert_eq!(
        first_input["changed"],
        serde_json::json!(["context", "rateLimits"])
    );
    assert_eq!(
        first_result,
        serde_json::json!({"changed":["context", "rateLimits"]})
    );
    assert_eq!(
        first
            .iter()
            .filter(|line| line.starts_with("complete:") || line.starts_with("measure:"))
            .count(),
        2,
        "first turn ModLog snapshot: {first:?}"
    );
    assert!(first[0].starts_with("complete:"));
    assert!(first[1].starts_with("measure:"));

    // A half-point shift is retained against the last emitted baseline; it
    // must not fire `session.measure` by itself.
    api.set_raw_utilization(Some(measure_entry(25.5)));
    orchestrator.run_turn("second").await.unwrap();
    wait_for_log_count(&output, "complete:", 2).await;
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let second = logs(&output).await;
    assert_eq!(
        second
            .iter()
            .filter(|line| line.starts_with("measure:"))
            .count(),
        1
    );

    // The provider's raw utilization is rounded to one decimal, and a full
    // percentage point of movement raises an idle rate-limit measurement.
    api.set_raw_utilization(Some(measure_entry(26.0)));
    orchestrator.emit_raw_utilization_if_changed().await;
    let third = wait_for_log_count(&output, "measure:", 2).await;
    let measure_logs = third
        .iter()
        .filter(|line| line.starts_with("measure:"))
        .collect::<Vec<_>>();
    let (third_input, third_result) = measure_logs[1]
        .strip_prefix("measure:")
        .unwrap()
        .split_once('|')
        .unwrap();
    let third_input: serde_json::Value = serde_json::from_str(third_input).unwrap();
    let third_result: serde_json::Value = serde_json::from_str(third_result).unwrap();
    assert_eq!(third_input["context"]["tokens"], 123);
    assert_eq!(
        third_input["rateLimits"][0]["percentUsed"].as_f64(),
        Some(26.0)
    );
    assert_eq!(third_input["changed"], serde_json::json!(["rateLimits"]));
    assert_eq!(third_result, serde_json::json!({"changed":["rateLimits"]}));

    let event_logs = third
        .iter()
        .filter(|line| line.starts_with("complete:") || line.starts_with("measure:"))
        .collect::<Vec<_>>();
    assert_eq!(event_logs.len(), 4);
    assert!(event_logs[0].starts_with("complete:"));
    assert!(event_logs[1].starts_with("measure:"));
    assert!(event_logs[2].starts_with("complete:"));
    assert!(event_logs[3].starts_with("measure:"));
}
