//! Native filter and production post-turn lifecycle regression tests.
#[path = "../src/conversation/prompt_suggestion_filter.rs"]
mod filter;
use sha2::{Digest, Sha256};
#[test]
fn prompt_suggestion_matches_native_296_bytes_and_filters() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/prompt_suggestion_2_1_296.json")).unwrap();
    let prompt = include_str!("../src/conversation/prompt_suggestion_prompt.txt");
    assert_eq!(
        format!("{:x}", Sha256::digest(prompt.as_bytes())),
        fixture["prompt_sha256"].as_str().unwrap()
    );
    let mut mismatches = Vec::new();
    for case in fixture["cases"].as_array().unwrap() {
        let raw = case["raw"].as_str().unwrap();
        let actual = filter::parse_prompt_suggestion_response(raw);
        if actual.as_deref() != case["expected"].as_str() {
            mismatches.push(format!(
                "{raw:?}: actual={actual:?}, expected={:?}",
                case["expected"]
            ));
        }
    }
    for case in fixture["coldCases"].as_array().unwrap() {
        assert_eq!(
            filter::prompt_suggestion_cache_is_cold(
                case["input"].as_u64().unwrap(),
                case["output"].as_u64().unwrap(),
                case["write"].as_u64().unwrap()
            ),
            case["expected"].is_string()
        );
    }
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

use async_trait::async_trait;
use lingxi_core::host::{CostSnapshot, OrchestratorHandle, OutputStream, ToolResultProjection};
use lingxi_core::types::{utf16_json::Utf16JsonProjection, ToolUseId};
use orchestrator::test_support::{
    noop_hook_executor, MockApiClient, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::test_support_stream::*;
use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
use sidequery::{SideQueryClient, SideQueryError, SideQueryRequest, SideQueryResponse};
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, Semaphore};

#[derive(Default)]
struct SuggestionOutput {
    rows: Mutex<Vec<(String, String, String)>>,
    notices: Mutex<Vec<String>>,
    ready: Notify,
}
#[async_trait]
impl OutputStream for SuggestionOutput {
    async fn emit_text(&self, _: &str, _: Option<&[u16]>) {}
    async fn emit_tool_call(
        &self,
        _: &ToolUseId,
        _: &str,
        _: &serde_json::Value,
        _: Option<&Utf16JsonProjection>,
    ) {
    }
    async fn emit_tool_result(
        &self,
        _: &ToolUseId,
        _: &str,
        _: &str,
        _: &serde_json::Value,
        _: Option<&ToolResultProjection>,
    ) {
    }
    async fn emit_end_turn(&self, _: &str, _: &CostSnapshot) {
        self.rows
            .lock()
            .unwrap()
            .push(("end".into(), String::new(), String::new()));
    }
    fn wants_prompt_suggestions(&self) -> bool {
        true
    }
    async fn emit_prompt_suggestion_state(&self, _: bool, notice: Option<&str>) {
        if let Some(notice) = notice {
            self.notices.lock().unwrap().push(notice.to_owned());
        }
    }
    async fn emit_prompt_suggestion(&self, session: &str, after: &str, suggestion: &str) {
        self.rows
            .lock()
            .unwrap()
            .push((session.into(), after.into(), suggestion.into()));
        self.ready.notify_one();
    }
}
struct SuggestionClient {
    requests: Mutex<Vec<SideQueryRequest>>,
    entered: Notify,
    release: Semaphore,
}
#[async_trait]
impl SideQueryClient for SuggestionClient {
    async fn query(&self, request: SideQueryRequest) -> Result<SideQueryResponse, SideQueryError> {
        self.requests.lock().unwrap().push(request);
        self.entered.notify_one();
        self.release.acquire().await.unwrap().forget();
        Ok(SideQueryResponse {
            text: Some("建议：运行测试".into()),
            structured: None,
            tool_calls: vec![],
            usage: cost::Usage::default(),
            stop_reason: Some("end_turn".into()),
            retry_count: 0,
        })
    }
}
fn setup() -> (
    Arc<ConversationOrchestrator>,
    Arc<SuggestionClient>,
    Arc<SuggestionOutput>,
    Arc<sidequery::CacheSafeParamsSlot>,
) {
    telemetry::feature_flags::test_set_flag("tengu_chomp_inflection", true);
    let client = Arc::new(SuggestionClient {
        requests: Mutex::new(vec![]),
        entered: Notify::new(),
        release: Semaphore::new(0),
    });
    let output = Arc::new(SuggestionOutput::default());
    let slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
    let turns = (0..60)
        .map(|i| {
            vec![
                message_start(&format!("reply-{i}"), "claude-sonnet-4-6"),
                content_block_start_text(0),
                text_delta(0, "Finished the change."),
                content_block_stop(0),
                message_delta_stop("end_turn"),
                message_stop(),
            ]
        })
        .collect();
    let orch = ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig {
            model: "claude-sonnet-4-6".into(),
            interactive_session: true,
            ..Default::default()
        },
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(MockStreamingApiClient::with_turns(turns)),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
    .with_cache_safe_slot(slot.clone())
    .with_recap_runner(Arc::new(
        sidequery::ForkedAgentRunner::new()
            .with_side_query_client(client.clone(), "fallback-model".into()),
    ));
    (
        ConversationOrchestrator::into_shared(orch),
        client,
        output,
        slot,
    )
}
async fn wait(notify: &Notify) {
    tokio::time::timeout(std::time::Duration::from_secs(5), notify.notified())
        .await
        .unwrap();
}

#[tokio::test]
async fn post_turn_suggestion_is_nonblocking_uses_parent_prefix_and_never_changes_history() {
    let (orch, client, output, slot) = setup();
    orch.run_turn_streaming("fix the bug").await.unwrap();
    assert!(
        client.requests.lock().unwrap().is_empty(),
        "native skips the first assistant reply"
    );
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        orch.run_turn_streaming("finish the change"),
    )
    .await
    .unwrap()
    .unwrap();
    wait(&client.entered).await;
    let before = orch.conversation_transcript().await;
    let cached = slot.get_last().await.unwrap();
    {
        let calls = client.requests.lock().unwrap();
        let req = &calls[0];
        assert_eq!(req.query_source, sidequery::QuerySource::PromptSuggestion);
        assert_eq!(req.model, "claude-sonnet-4-6");
        assert_eq!(
            req.system_prompt.as_deref(),
            Some(cached.system_prompt.as_ref())
        );
        assert_eq!(req.tools, cached.tools);
        assert_eq!(req.effort, cached.effort);
        assert_eq!(
            req.max_tokens,
            llm_runtime::model::context_window::default_output_tokens_for_model(&req.model) as u32
        );
        let serialized = serde_json::to_value(req.messages.last().unwrap()).unwrap();
        assert_eq!(
            serialized["content"][0]["text"],
            include_str!("../src/conversation/prompt_suggestion_prompt.txt")
        );
        assert!(
            req.messages
                .iter()
                .any(|m| m.id() == before.last().unwrap().id()),
            "the final assistant reply must be in the fork"
        );
    }
    client.release.add_permits(1);
    wait(&output.ready).await;
    let rows = output.rows.lock().unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[1].0, "end");
    assert_eq!(rows[2].2, "运行测试");
    assert!(
        uuid::Uuid::parse_str(&rows[2].0).is_ok(),
        "wire session_id is a raw UUID"
    );
    assert!(uuid::Uuid::parse_str(&rows[2].1).is_ok());
    drop(rows);
    assert_eq!(
        serde_json::to_value(orch.conversation_transcript().await).unwrap(),
        serde_json::to_value(before).unwrap()
    );
    assert_eq!(slot.get_last().await.unwrap().generation, cached.generation);
}

#[tokio::test]
async fn clearing_session_cancels_inflight_suggestion() {
    let (orch, client, output, _) = setup();
    orch.run_turn_streaming("first").await.unwrap();
    orch.run_turn_streaming("second").await.unwrap();
    wait(&client.entered).await;
    orch.clear_session().await.unwrap();
    client.release.add_permits(1);
    assert!(tokio::time::timeout(
        std::time::Duration::from_millis(100),
        output.ready.notified()
    )
    .await
    .is_err());
    assert_eq!(output.rows.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn new_turn_cancels_old_suggestion_and_only_emits_the_new_reply() {
    let (orch, client, output, _) = setup();
    orch.run_turn_streaming("first").await.unwrap();
    orch.run_turn_streaming("second").await.unwrap();
    wait(&client.entered).await;
    orch.run_turn_streaming("third").await.unwrap();
    wait(&client.entered).await;
    client.release.add_permits(1);
    wait(&output.ready).await;
    let rows = output.rows.lock().unwrap();
    assert_eq!(rows.len(), 4);
    assert!(uuid::Uuid::parse_str(&rows[3].1).is_ok());
    assert_eq!(rows[3].2, "运行测试");
}
#[tokio::test]
async fn plan_mode_skips_the_post_turn_query() {
    let (orch, client, _, _) = setup();
    orch.set_permission_mode("plan").await.unwrap();
    orch.run_turn_streaming("first").await.unwrap();
    orch.run_turn_streaming("second").await.unwrap();
    assert!(client.requests.lock().unwrap().is_empty());
}

async fn context(orch: &ConversationOrchestrator, focus: &str, empty: bool, pending: bool) {
    orch.update_prompt_suggestion_context(lingxi_core::host::PromptSuggestionContext {
        session_id: orch.current_session_id().await.as_uuid().to_string(),
        focus: focus.into(),
        input_empty: empty,
        pending_permission: pending,
        elicitation_active: false,
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn composer_context_accepts_only_the_current_raw_session_uuid() {
    let (orch, client, output, _) = setup();
    client.release.add_permits(100);
    orch.run_turn_streaming("first").await.unwrap();
    let session = orch.current_session_id().await;
    for invalid in [session.to_string(), uuid::Uuid::new_v4().to_string()] {
        orch.update_prompt_suggestion_context(lingxi_core::host::PromptSuggestionContext {
            session_id: invalid,
            focus: "blurred".into(),
            input_empty: true,
            pending_permission: false,
            elicitation_active: false,
        })
        .await
        .unwrap();
        orch.run_turn_streaming("invalid context must not blur this session")
            .await
            .unwrap();
        wait(&output.ready).await;
    }
    assert_eq!(client.requests.lock().unwrap().len(), 2);
    context(&orch, "blurred", true, false).await;
    orch.run_turn_streaming("valid raw context prevents generation")
        .await
        .unwrap();
    assert_eq!(client.requests.lock().unwrap().len(), 2);
    context(&orch, "focused", true, false).await;
    orch.run_turn_streaming("valid raw context restores generation")
        .await
        .unwrap();
    wait(&output.ready).await;
    assert_eq!(client.requests.lock().unwrap().len(), 3);
    assert_eq!(
        output.rows.lock().unwrap().last().unwrap().0,
        session.as_uuid().to_string()
    );
}

#[tokio::test]
async fn focus_permission_and_toggle_gate_actual_generation() {
    let (orch, client, _, _) = setup();
    orch.run_turn_streaming("first").await.unwrap();
    context(&orch, "blurred", true, false).await;
    orch.run_turn_streaming("second").await.unwrap();
    context(&orch, "focused", true, true).await;
    orch.run_turn_streaming("third").await.unwrap();
    context(&orch, "focused", true, false).await;
    orch.prompt_suggestion_setting_changed(false).await.unwrap();
    orch.run_turn_streaming("fourth").await.unwrap();
    assert!(client.requests.lock().unwrap().is_empty());
    orch.prompt_suggestion_setting_changed(true).await.unwrap();
    orch.run_turn_streaming("fifth").await.unwrap();
    wait(&client.entered).await;
    assert_eq!(client.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn typing_cancels_generation_before_it_can_replace_the_draft() {
    let (orch, client, output, _) = setup();
    orch.run_turn_streaming("first").await.unwrap();
    orch.run_turn_streaming("second").await.unwrap();
    wait(&client.entered).await;
    context(&orch, "focused", false, false).await;
    client.release.add_permits(1);
    assert!(tokio::time::timeout(
        std::time::Duration::from_millis(100),
        output.ready.notified()
    )
    .await
    .is_err());
    context(&orch, "focused", true, false).await;
    assert_eq!(output.rows.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn disabling_preserves_an_inflight_preview_and_its_submission_lifecycle() {
    let (orch, client, output, _) = setup();
    orch.run_turn_streaming("first").await.unwrap();
    orch.run_turn_streaming("second").await.unwrap();
    wait(&client.entered).await;
    orch.prompt_suggestion_setting_changed(false).await.unwrap();
    client.release.add_permits(1);
    wait(&output.ready).await;
    let (session, uuid, _) = output.rows.lock().unwrap().last().unwrap().clone();
    orch.prompt_suggestion_feedback(&session, &uuid, "shown")
        .await
        .unwrap();
    orch.prompt_suggestion_feedback(&session, &uuid, "accepted")
        .await
        .unwrap();
    orch.record_prompt_suggestion_submission("运行测试")
        .await
        .unwrap();
    orch.run_turn_streaming("运行测试").await.unwrap();
    assert_eq!(
        client.requests.lock().unwrap().len(),
        1,
        "disabling blocks future generation without cancelling the admitted query"
    );
}

#[tokio::test]
async fn generated_previews_that_never_reach_the_composer_do_not_back_off() {
    let (orch, client, output, _) = setup();
    client.release.add_permits(100);
    orch.run_turn_streaming("first").await.unwrap();
    for n in 0..24 {
        orch.run_turn_streaming(&format!("continue {n}"))
            .await
            .unwrap();
        wait(&output.ready).await;
        let (raw_session, suggestion_uuid, _) = output.rows.lock().unwrap().last().unwrap().clone();
        for invalid_session in [
            format!("sess:{raw_session}"),
            uuid::Uuid::new_v4().to_string(),
        ] {
            orch.prompt_suggestion_feedback(&invalid_session, &suggestion_uuid, "shown")
                .await
                .unwrap();
        }
    }
    assert_eq!(client.requests.lock().unwrap().len(), 24);
    assert!(output.notices.lock().unwrap().is_empty());
}

#[tokio::test]
async fn only_shown_unused_suggestions_back_off_and_reenable_resets_frequency() {
    let (orch, client, output, _) = setup();
    client.release.add_permits(100);
    orch.run_turn_streaming("first").await.unwrap();
    orch.run_turn_streaming("second").await.unwrap();
    wait(&output.ready).await;
    let session = orch.current_session_id().await.as_uuid().to_string();
    // Twenty ignored submissions after actually displaying a suggestion.
    for n in 0..20 {
        let uuid = output.rows.lock().unwrap().last().unwrap().1.clone();
        orch.prompt_suggestion_feedback(&session, &uuid, "shown")
            .await
            .unwrap();
        // A local command consumes the displayed preview even without an LLM
        // turn. Repeated dispatch/driver accounting must not count it twice.
        orch.record_prompt_suggestion_submission("/help")
            .await
            .unwrap();
        orch.record_prompt_suggestion_submission("/help")
            .await
            .unwrap();
        assert!(
            output.notices.lock().unwrap().is_empty(),
            "local-only submissions must not claim the back-off notice"
        );
        orch.run_turn_streaming(&format!("do something else {n}"))
            .await
            .unwrap();
        wait(&output.ready).await;
    }
    assert_eq!(
        *output.notices.lock().unwrap(),
        vec!["Showing fewer prompt suggestions · use one to bring them back"]
    );
    let at_threshold = client.requests.lock().unwrap().len();
    for n in 0..9 {
        orch.run_turn_streaming(&format!("backed off {n}"))
            .await
            .unwrap();
    }
    assert_eq!(client.requests.lock().unwrap().len(), at_threshold);
    orch.run_turn_streaming("tenth eligible turn")
        .await
        .unwrap();
    wait(&output.ready).await;
    assert_eq!(client.requests.lock().unwrap().len(), at_threshold + 1);
    orch.prompt_suggestion_setting_changed(false).await.unwrap();
    orch.prompt_suggestion_setting_changed(true).await.unwrap();
    orch.run_turn_streaming("re-enabled").await.unwrap();
    wait(&output.ready).await;
    assert_eq!(client.requests.lock().unwrap().len(), at_threshold + 2);
}
