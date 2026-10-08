use super::*;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::OrchestratorConfig;
use hooks::mods::{ModHost, ModSessionContext};
use std::sync::Arc;

struct MeasureSession;

#[async_trait::async_trait]
impl ModSessionContext for MeasureSession {
    fn cwd(&self) -> std::path::PathBuf {
        std::env::current_dir().unwrap()
    }

    fn root(&self) -> std::path::PathBuf {
        std::env::current_dir().unwrap()
    }

    async fn id(&self) -> String {
        "idle-measure-test".to_owned()
    }

    async fn model(&self) -> String {
        "claude-opus-4-7".to_owned()
    }

    async fn turns(&self) -> u64 {
        0
    }
}

fn raw_utilization(percent: f64) -> llm_runtime::model::rate_limit::RawUtilization {
    llm_runtime::model::rate_limit::RawUtilization {
        five_hour: Some(llm_runtime::model::rate_limit::RawWindow {
            utilization: percent / 100.0,
            resets_at: 2_000_000_000,
        }),
        seven_day: None,
    }
}

async fn measure_logs(output: &MockOutputStream) -> Vec<String> {
    output
        .snapshot()
        .await
        .into_iter()
        .filter_map(|event| match event {
            lingxi_core::host::OutputEvent::ModLog { plugin, text } if plugin == "idle-measure" => {
                Some(text)
            }
            _ => None,
        })
        .collect()
}

async fn wait_for_measure_count(output: &MockOutputStream, count: usize) -> Vec<String> {
    tokio::time::timeout(std::time::Duration::from_secs(4), async {
        loop {
            let logs = measure_logs(output).await;
            if logs.len() >= count {
                return logs;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("expected idle session.measure Mod log")
}

#[tokio::test]
async fn observed_rate_limit_changes_dispatch_measure_while_no_turn_is_running() {
    let directory = tempfile::tempdir().unwrap();
    let module = directory.path().join("idle-measure.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
          on('session.measure', async ($, e, next) => {
            $.ui.log(`${e.changed.join(',')}:${e.rateLimits.map(limit => limit.percentUsed).join(',')}`);
            return next(e);
          });
        }"#,
    )
    .unwrap();

    let host = ModHost::start(None).await.unwrap();
    host.load(
        "idle-measure",
        directory.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    assert!(host.has_event("session.measure"));

    let session = Arc::new(MeasureSession);
    let session_trait: Arc<dyn ModSessionContext> = session.clone();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host.clone());
    registry.attach_mod_background_context(Arc::downgrade(&session_trait));

    let api = Arc::new(MockApiClient::new(vec![]));
    api.set_raw_utilization(Some(raw_utilization(25.0)));
    api.set_rate_limit_full(Some(crate::model::rate_limit::RateLimitInfo {
        status: Some("allowed".to_owned()),
        ..Default::default()
    }));
    let output = Arc::new(MockOutputStream::new());
    let orchestrator = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api.clone(),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        directory.path().to_path_buf(),
    )
    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)));

    // The cached provider state changes while no run_turn is in flight. The
    // same deduplicated snapshot seams used by live turns notify the sampler.
    orchestrator.emit_rate_limit_if_changed().await;
    orchestrator.emit_raw_utilization_if_changed().await;
    let first = wait_for_measure_count(&output, 1).await;
    assert_eq!(first[0], "context,rateLimits:25");

    api.set_raw_utilization(Some(raw_utilization(26.0)));
    api.set_rate_limit_full(Some(crate::model::rate_limit::RateLimitInfo {
        status: Some("allowed_warning".to_owned()),
        ..Default::default()
    }));
    orchestrator.emit_rate_limit_if_changed().await;
    orchestrator.emit_raw_utilization_if_changed().await;
    let second = wait_for_measure_count(&output, 2).await;
    assert_eq!(second[1], "rateLimits:26");

    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert_eq!(measure_logs(&output).await.len(), 2);
}
