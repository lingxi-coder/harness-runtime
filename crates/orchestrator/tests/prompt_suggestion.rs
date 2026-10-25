//! Native filter and production post-turn lifecycle regression tests.
#[path = "../src/conversation/prompt_suggestion_filter.rs"]
mod filter;
use sha2::{Digest, Sha256};
#[test]
fn prompt_suggestion_matches_shared_fixture_cases_and_filters() {
    // This existing 2.1.296 fixture guards shared cases; it is not claimed as
    // a 2.1.295 parity oracle. The current production bindings are recorded in
    // acceptance/source-closure-r377/resume/suggestion-source-binding.md.
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

#[test]
fn prompt_suggestion_normalization_keeps_unpaired_utf16_and_native_candidate_order() {
    let raw_display = "<suggestion>  Run the � tests  </suggestion>";
    let mut raw_units = raw_display.encode_utf16().collect::<Vec<_>>();
    let surrogate = raw_units
        .iter()
        .position(|unit| *unit == 0xfffd)
        .expect("test source contains the display replacement");
    raw_units[surrogate] = 0xd800;
    let first = Utf16JsonProjection::root_string(raw_display.into(), raw_units).unwrap();
    let second = Utf16JsonProjection::from("Run several tests now");

    let selected = filter::first_prompt_suggestion(&[first, second]).unwrap();
    let expected_display = "Run the � tests";
    let mut expected_units = expected_display.encode_utf16().collect::<Vec<_>>();
    let surrogate = expected_units
        .iter()
        .position(|unit| *unit == 0xfffd)
        .expect("expected display contains the replacement");
    expected_units[surrogate] = 0xd800;
    assert_eq!(selected.string_units(""), Some(expected_units));
    assert_eq!(
        filter::prompt_suggestion_suppression_reason_projection(&selected),
        None
    );

    // Native q selects the first non-empty normalized candidate before B's
    // rejection pass; a later valid candidate cannot replace "done".
    let selected = filter::first_prompt_suggestion(&[
        Utf16JsonProjection::from("done"),
        Utf16JsonProjection::from("Run several tests now"),
    ])
    .unwrap();
    assert_eq!(selected.string_at(""), Some("done"));
    assert_eq!(
        filter::prompt_suggestion_suppression_reason_projection(&selected),
        Some("done")
    );
}

use async_trait::async_trait;
use lingxi_core::host::{CostSnapshot, OrchestratorHandle, OutputStream};
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
    source_rows: Mutex<Vec<Vec<u16>>>,
    notices: Mutex<Vec<String>>,
    enabled_states: Mutex<Vec<bool>>,
    ready: Notify,
}
#[async_trait]
impl OutputStream for SuggestionOutput {
    async fn emit_text(&self, _: &Utf16JsonProjection) {}
    async fn emit_tool_call(&self, _: &ToolUseId, _: &str, _: &Utf16JsonProjection) {}
    async fn emit_tool_result(
        &self,
        _: &ToolUseId,
        _: &str,
        _: &Utf16JsonProjection,
        _: &Utf16JsonProjection,
        _: Option<&Utf16JsonProjection>,
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
    async fn emit_prompt_suggestion_state(&self, enabled: bool, notice: Option<&str>) {
        self.enabled_states.lock().unwrap().push(enabled);
        if let Some(notice) = notice {
            self.notices.lock().unwrap().push(notice.to_owned());
        }
    }
    async fn emit_prompt_suggestion(
        &self,
        session: &str,
        after: &str,
        suggestion: &Utf16JsonProjection,
    ) {
        let units = suggestion
            .string_units("")
            .expect("suggestion output is a root string");
        let display = String::from_utf16_lossy(&units);
        self.source_rows.lock().unwrap().push(units);
        self.rows
            .lock()
            .unwrap()
            .push((session.into(), after.into(), display));
        self.ready.notify_one();
    }
}
struct SuggestionClient {
    requests: Mutex<Vec<SideQueryRequest>>,
    suggestion: Utf16JsonProjection,
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
            text: Some(self.suggestion.clone()),
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
    setup_with_suggestion(Utf16JsonProjection::from("建议：运行测试"))
}

fn setup_with_suggestion(
    suggestion: Utf16JsonProjection,
) -> (
    Arc<ConversationOrchestrator>,
    Arc<SuggestionClient>,
    Arc<SuggestionOutput>,
    Arc<sidequery::CacheSafeParamsSlot>,
) {
    setup_with_config(suggestion, true, "repl_main_thread", None)
}

struct SuggestionSettings(bool);

#[async_trait]
impl hooks::mods::ModSettingsReader for SuggestionSettings {
    async fn read(
        &self,
        _: Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, hooks::mods::ModError> {
        Ok(Utf16JsonProjection::from_serde_value(
            &serde_json::json!({"promptSuggestionEnabled": self.0}),
        )
        .unwrap())
    }
}

fn setup_with_config(
    suggestion: Utf16JsonProjection,
    interactive: bool,
    query_source: &str,
    setting: Option<bool>,
) -> (
    Arc<ConversationOrchestrator>,
    Arc<SuggestionClient>,
    Arc<SuggestionOutput>,
    Arc<sidequery::CacheSafeParamsSlot>,
) {
    let client = Arc::new(SuggestionClient {
        requests: Mutex::new(vec![]),
        suggestion,
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
    let mut orch = ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig {
            model: "claude-sonnet-4-6".into(),
            interactive_session: interactive,
            query_source: query_source.into(),
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
    if let Some(enabled) = setting {
        orch = orch.with_mod_settings_reader(Arc::new(SuggestionSettings(enabled)));
    }
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
            req.system_prompt
                .as_ref()
                .and_then(|prompt| prompt.string_units("")),
            cached.system_prompt.string_units("")
        );
        assert_eq!(
            req.tools,
            llm_runtime::convert::to_tool_declarations(cached.tools.clone()).unwrap()
        );
        assert_eq!(req.effort, cached.effort);
        assert_eq!(
            req.max_tokens, None,
            "Native leaves this optional override unset; the SDK resolves the selected model default at dispatch"
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
async fn post_turn_output_stream_emits_the_exact_normalized_suggestion_units() {
    let raw_display = "<suggestion>  Run the � tests  </suggestion>";
    let mut raw_units = raw_display.encode_utf16().collect::<Vec<_>>();
    let surrogate = raw_units
        .iter()
        .position(|unit| *unit == 0xfffd)
        .expect("test source contains the display replacement");
    raw_units[surrogate] = 0xd800;
    let suggestion = Utf16JsonProjection::root_string(raw_display.into(), raw_units).unwrap();
    let (orch, client, output, _) = setup_with_suggestion(suggestion);

    orch.run_turn_streaming("first turn").await.unwrap();
    orch.run_turn_streaming("second turn").await.unwrap();
    wait(&client.entered).await;
    client.release.add_permits(1);
    wait(&output.ready).await;

    let expected_display = "Run the � tests";
    let mut expected_units = expected_display.encode_utf16().collect::<Vec<_>>();
    let surrogate = expected_units
        .iter()
        .position(|unit| *unit == 0xfffd)
        .expect("expected display contains the replacement");
    expected_units[surrogate] = 0xd800;
    assert_eq!(
        output.source_rows.lock().unwrap().last(),
        Some(&expected_units)
    );
    assert_eq!(
        output.rows.lock().unwrap().last().unwrap().2,
        expected_display
    );
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
async fn sdk_composer_attaching_after_the_first_turn_enables_default_generation() {
    let (orch, client, output, _) = setup_with_config(
        Utf16JsonProjection::from("Run the focused tests"),
        false,
        "sdk",
        None,
    );
    orch.run_turn_streaming("first turn before context arrives")
        .await
        .unwrap();
    assert!(output.enabled_states.lock().unwrap().is_empty());
    context(&orch, "focused", true, false).await;
    assert_eq!(*output.enabled_states.lock().unwrap(), vec![true]);
    orch.run_turn_streaming("second turn with attached composer")
        .await
        .unwrap();
    wait(&client.entered).await;
    client.release.add_permits(1);
    wait(&output.ready).await;
    assert_eq!(
        output.rows.lock().unwrap().last().unwrap().2,
        "Run the focused tests"
    );
}

#[tokio::test]
async fn unattached_mobile_sdk_does_not_automatically_generate_suggestions() {
    let (orch, client, output, _) = setup_with_config(
        Utf16JsonProjection::from("Run the focused tests"),
        true,
        "sdk",
        None,
    );
    orch.run_turn_streaming("first scheduled turn")
        .await
        .unwrap();
    orch.run_turn_streaming("second scheduled turn")
        .await
        .unwrap();
    assert!(client.requests.lock().unwrap().is_empty());
    assert!(output.enabled_states.lock().unwrap().is_empty());

    // Headless SDK consumers retain their separate explicit opt-in path.
    client.release.add_permits(1);
    let result = orch
        .generate_prompt_suggestion_query(tokio_util::sync::CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        result.suggestion.string_at(""),
        Some("Run the focused tests")
    );
    assert_eq!(client.requests.lock().unwrap().len(), 1);
    assert_eq!(output.rows.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn persisted_user_disable_still_blocks_attached_sdk_suggestions() {
    let (orch, client, output, _) = setup_with_config(
        Utf16JsonProjection::from("Run the focused tests"),
        false,
        "sdk",
        Some(false),
    );
    context(&orch, "focused", true, false).await;
    orch.run_turn_streaming("first turn").await.unwrap();
    orch.run_turn_streaming("second turn").await.unwrap();
    assert_eq!(*output.enabled_states.lock().unwrap(), vec![false]);
    assert!(client.requests.lock().unwrap().is_empty());
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
