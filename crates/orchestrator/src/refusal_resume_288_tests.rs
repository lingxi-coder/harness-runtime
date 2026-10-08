//! Native flat refusal notices survive loader, cold/hot resume and SDK dispatch.
use crate::messages_288_fixture as physical;
use crate::test_support::{
    noop_hook_executor, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::{ConversationOrchestrator, OrchestratorConfig, ProviderApiAdapter};
use lingxi_core::host::{OrchestratorHandle, ResumeRuntimeSnapshot};
use lingxi_core::types::{ConversationMessage, SessionId};
use physical::{captures, service, Action, MODEL};
use platform_posix::fs::PosixFileSystem;
use serde_json::{json, Value};
use session::JsonlMessage;
use std::sync::Arc;

fn native_rows(sid: uuid::Uuid, neutralized: bool) -> Vec<JsonlMessage> {
    let ids: Vec<_> = (0..3).map(|_| uuid::Uuid::new_v4().to_string()).collect();
    [
        json!({"type":"user","uuid":ids[0],"parentUuid":null,"message":{"role":"user","content":[{"type":"text","text":"fixture"}]}}),
        json!({"type":"system","uuid":ids[1],"parentUuid":ids[0],"subtype":"model_refusal_fallback","direction":"retry","scope":"session","content":"native refusal notice","level":"warning","trigger":"refusal","originalModel":"origin","fallbackModel":MODEL,"requestId":"resume-origin ","apiRefusalCategory":"cyber","apiRefusalExplanation":"provider explanation","sawCyberRefusal":true,"neutralizedByFork":neutralized,"isMeta":false}),
        json!({"type":"assistant","uuid":ids[2],"parentUuid":ids[1],"message":{"role":"assistant","model":MODEL,"id":"msg_saved","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}}),
    ].into_iter().map(|mut row| {
        let object=row.as_object_mut().unwrap();
        object.insert("sessionId".into(),json!(sid.to_string()));object.insert("timestamp".into(),json!("2026-10-03T00:00:00.000Z"));object.insert("cwd".into(),json!("/repo"));object.insert("version".into(),json!("2.1.288"));object.insert("isSidechain".into(),json!(false));
        serde_json::from_value(row).unwrap()
    }).collect()
}
fn root(capture: Arc<physical::Capture>, cwd: &std::path::Path) -> Arc<ConversationOrchestrator> {
    let adapter = Arc::new(ProviderApiAdapter::new(Arc::new(service(
        capture, true, false, MODEL,
    ))));
    let root = ConversationOrchestrator::into_shared(ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig {
            model: MODEL.into(),
            max_turns: 1,
            ..Default::default()
        },
        adapter.clone(),
        adapter,
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        cwd.to_owned(),
    ));
    root.attach_owned_session_switches();
    root
}
fn header(rows: &[(Value, Vec<(String, String)>)], index: usize, name: &str) -> Option<String> {
    rows[index]
        .1
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
}

#[tokio::test]
async fn hot_resume_pick_clear_and_neutralized_notice_keep_the_native_sdk_header_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let sid = uuid::Uuid::new_v4();
    let rows = native_rows(sid, false);
    let state = crate::resume::state_from_messages(sid, &rows);
    let marker = state
        .history
        .iter()
        .find(|message| {
            matches!(
                message,
                ConversationMessage::System {
                    refusal_fallback: Some(_),
                    ..
                }
            )
        })
        .unwrap();
    let ConversationMessage::System {
        refusal_fallback: Some(metadata),
        ..
    } = marker
    else {
        unreachable!()
    };
    assert_eq!(
        metadata.api_refusal_explanation.as_deref(),
        Some("provider explanation")
    );
    assert_eq!(metadata.saw_cyber_refusal, Some(true));
    let capture = captures(vec![Action::Success; 4]);
    let root = root(capture.clone(), dir.path());
    root.resume_session(
        SessionId::from_uuid(sid),
        state.history,
        None,
        None,
        ResumeRuntimeSnapshot {
            model: MODEL.into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(root
        .model_runtime
        .refusal_selection
        .lock()
        .unwrap()
        .live_latch()
        .is_some());
    assert!(root.run_turn_streaming("first").await.is_ok());
    root.switch_model(MODEL, None).await.unwrap();
    assert!(root
        .model_runtime
        .refusal_selection
        .lock()
        .unwrap()
        .live_latch()
        .is_none());
    assert!(root.run_turn_streaming("picked").await.is_ok());
    root.clear_session().await.unwrap();
    assert!(root.run_turn_streaming("cleared").await.is_ok());
    let neutral = native_rows(sid, true);
    let state = crate::resume::state_from_messages(sid, &neutral);
    root.resume_session(
        SessionId::from_uuid(sid),
        state.history,
        None,
        None,
        ResumeRuntimeSnapshot {
            model: MODEL.into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(root
        .model_runtime
        .refusal_selection
        .lock()
        .unwrap()
        .live_latch()
        .is_none());
    assert!(root.run_turn_streaming("neutralized").await.is_ok());
    let requests = capture.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    for index in [0, 1] {
        assert_eq!(
            header(&requests, index, "x-is-refusal-fallback"),
            Some("true".into())
        );
        assert_eq!(
            header(&requests, index, "x-cc-fallback-latched-by"),
            Some("resume-origin".into())
        );
    }
    for index in [2, 3] {
        assert_eq!(header(&requests, index, "x-is-refusal-fallback"), None);
    }
    assert!(requests
        .iter()
        .all(|(body, _)| !body.to_string().contains("native refusal notice")));
}

#[tokio::test]
async fn cold_loader_restores_flat_refusal_metadata_and_dispatches_the_sdk_header() {
    let dir = tempfile::tempdir().unwrap();
    let sid = uuid::Uuid::new_v4();
    let rows = native_rows(sid, false);
    let cwd = dir.path().to_string_lossy().to_string();
    let path = session::jsonl::session_path(dir.path(), &cwd, &sid.to_string());
    tokio::fs::create_dir_all(path.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(
        &path,
        rows.iter()
            .map(|row| serde_json::to_string(row).unwrap() + "\n")
            .collect::<String>(),
    )
    .await
    .unwrap();
    let capture = captures(vec![Action::Success]);
    let adapter = Arc::new(ProviderApiAdapter::new(Arc::new(service(
        capture.clone(),
        true,
        false,
        MODEL,
    ))));
    let cold = ConversationOrchestrator::with_resume(
        OrchestratorConfig {
            model: MODEL.into(),
            max_turns: 1,
            ..Default::default()
        },
        sid,
        dir.path().to_owned(),
        cwd,
        Arc::new(PosixFileSystem::new(dir.path().to_owned())),
        adapter,
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_owned(),
        None,
    )
    .await
    .unwrap();
    assert!(cold
        .model_runtime
        .refusal_selection
        .lock()
        .unwrap()
        .live_latch()
        .is_some());
    assert!(cold.run_turn("cold").await.is_ok());
    let requests = capture.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        header(&requests, 0, "x-is-refusal-fallback"),
        Some("true".into())
    );
}

#[tokio::test]
async fn local_notice_write_fork_projection_and_clear_preserve_the_original_model() {
    #[derive(Default)]
    struct ForkProbe(
        std::sync::Mutex<
            Option<(
                lingxi_core::host::bg_session_forker::BgSessionSnapshot,
                String,
            )>,
        >,
    );
    #[async_trait::async_trait]
    impl lingxi_core::host::bg_session_forker::BgSessionForker for ForkProbe {
        async fn fork_to_background(
            &self,
            snapshot: &lingxi_core::host::bg_session_forker::BgSessionSnapshot,
            _: Option<Arc<str>>,
            _: &str,
            model: &str,
        ) -> Result<String, lingxi_core::host::bg_session_forker::BgForkError> {
            *self.0.lock().unwrap() = Some((snapshot.clone(), model.into()));
            Ok("forked".into())
        }
        async fn resume_to_background(
            &self,
            _: &str,
        ) -> Result<String, lingxi_core::host::bg_session_forker::BgForkError> {
            unreachable!()
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let forker = Arc::new(ForkProbe::default());
    // The normal local producer captures the restoration snapshot.
    let adapter = Arc::new(crate::test_support::MockApiClient::new(vec![]));
    let producer = ConversationOrchestrator::new(
        OrchestratorConfig {
            model: "original".into(),
            refusal_fallback_model: Some(MODEL.into()),
            ..Default::default()
        },
        adapter,
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_owned(),
    )
    .with_bg_session_forker(forker.clone());
    producer.session.lock().await.model_profile = Some("original-profile".into());
    assert!(producer.maybe_swap_to_refusal_fallback().await);
    let notice = producer
        .session
        .lock()
        .await
        .history
        .iter()
        .find(|message| {
            matches!(
                message,
                ConversationMessage::System {
                    refusal_fallback: Some(_),
                    ..
                }
            )
        })
        .unwrap()
        .clone();
    let row = producer.to_jsonl_message(
        &notice,
        &uuid::Uuid::new_v4().to_string(),
        None,
        None,
        None,
        None,
    );
    let encoded = serde_json::to_value(row).unwrap();
    assert_eq!(encoded["subtype"], "model_refusal_fallback");
    assert!(encoded.get("message").is_none());
    assert_eq!(encoded["fallbackModel"], MODEL);
    producer.fork_to_background_session("").await.unwrap();
    let (snapshot, fork_model) = forker.0.lock().unwrap().clone().unwrap();
    assert_eq!(fork_model, "original");
    assert_eq!(producer.session.lock().await.model, MODEL);
    let lines = crate::bg_snapshot::history_to_jsonl_lines(
        &snapshot,
        &uuid::Uuid::new_v4().to_string(),
        "/repo",
        "2.1.288",
        &fork_model,
    );
    assert_eq!(lines[0].extra["neutralizedByFork"], true);
    assert!(lines[0].message.is_null());
    assert!(
        matches!(notice,ConversationMessage::System { refusal_fallback:Some(ref meta),.. } if meta.neutralized_by_fork.is_none())
    );
    producer.clear_session().await.unwrap();
    assert_eq!(producer.session.lock().await.model, "original");
    assert_eq!(
        producer.session.lock().await.model_profile.as_deref(),
        Some("original-profile")
    );
}
