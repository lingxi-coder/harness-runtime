use super::*;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use hooks::mods::ModSessionContext;
use lingxi_core::host::OrchestratorHandle;
use lingxi_core::types::utf16_json::Utf16JsonProjection as Source;
use std::sync::Arc;

async fn enqueue_completion(orch: &ConversationOrchestrator, turn_id: &str) {
    *orch.mod_turn.lock().unwrap() = Some(ModTurnFacts {
        id: turn_id.to_owned(),
        started_at: std::time::Instant::now(),
        answer: "old answer".into(),
        usage: None,
        refusal: None,
        error: false,
    });
    orch.fire_mod_turn_complete(false, false).await;
}

async fn logs(output: &MockOutputStream) -> Vec<String> {
    output
        .snapshot()
        .await
        .into_iter()
        .filter_map(|event| match event {
            lingxi_core::host::OutputEvent::ModLog { plugin, text }
                if plugin == "session-lease-probe" =>
            {
                Some(text.string_at("").unwrap_or_default().to_owned())
            }
            _ => None,
        })
        .collect()
}

async fn wait_log(output: &MockOutputStream, wanted: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if logs(output).await.iter().any(|line| line == wanted) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("probe reaches the requested deterministic barrier");
}

async fn assert_transition_retires_queued_mod_jobs(resume: bool) {
    let root = tempfile::tempdir().unwrap();
    let module = root.path().join("session-lease-probe.js");
    std::fs::write(
        &module,
        r#"
        let release;
        export function register(on) {
          on('turn.start', ($, e, next) => {
            if (e.turnId === 'release-old-queue') release?.();
            return next(e);
          });
          on('turn.complete', async ($, e, next) => {
            if (e.turnId === 'old-first') {
              $.ui.log('held');
              await new Promise(resolve => { release = resolve; });
            }
            $.ui.log(`${e.turnId}:${await $.session.id()}`);
            return next(e);
          });
        }
    "#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "session-lease-probe",
        root.path(),
        &module,
        Source::parse("{}").unwrap(),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host.clone());
    let registry = Arc::new(tokio::sync::RwLock::new(registry));
    let output = Arc::new(MockOutputStream::new());
    let orch = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(tool_api::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            output.clone(),
            Arc::new(StaticMemoryProvider::empty()),
            root.path().to_path_buf(),
        )
        .with_hook_registry(registry.clone()),
    );
    orch.attach_owned_session_switches();
    let session: Arc<dyn ModSessionContext> = orch.clone();
    registry
        .write()
        .await
        .attach_mod_background_context(Arc::downgrade(&session));
    let old_id = orch.current_session_id().await;
    enqueue_completion(&orch, "old-first").await;
    wait_log(&output, "held").await;
    enqueue_completion(&orch, "old-second").await;
    if resume {
        let resumed = lingxi_core::types::SessionId::new();
        let metadata = Source::from_serde_value(&serde_json::json!({
            "type":"fixed-session-mode", "sessionId":resumed.as_uuid().to_string(),
            "sessionMode":"code", "instructionEpoch":0,
        }))
        .unwrap();
        orch.resume_session(
            resumed,
            Vec::new(),
            None,
            None,
            lingxi_core::host::ResumeRuntimeSnapshot {
                transcript_metadata: Some(vec![metadata]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    } else {
        orch.clear_session().await.unwrap();
    }
    let new_id = orch.current_session_id().await;
    assert_ne!(old_id, new_id);
    enqueue_completion(&orch, "new-turn").await;
    host.dispatch(
        "turn.start",
        Source::parse(r#"{"text":"release","turnId":"release-old-queue"}"#).unwrap(),
        |input| async move { Ok(input) },
    )
    .await
    .unwrap();
    wait_log(&output, &format!("new-turn:{new_id}")).await;
    let observed = logs(&output).await;
    assert!(
        !observed
            .iter()
            .any(|line| line == &format!("old-first:{new_id}")
                || line == &format!("old-second:{new_id}")),
        "retired completion used the replacement session authority: {observed:?}"
    );
}

#[tokio::test]
async fn clear_retires_queued_mod_jobs_before_their_session_apis_can_rebind() {
    assert_transition_retires_queued_mod_jobs(false).await;
}

#[tokio::test]
async fn hot_resume_retires_queued_mod_jobs_before_their_session_apis_can_rebind() {
    assert_transition_retires_queued_mod_jobs(true).await;
}
