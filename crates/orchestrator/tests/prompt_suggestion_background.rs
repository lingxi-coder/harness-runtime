//! Evaluate the same native functions through the production QuickJS sandbox.
#[path = "../src/conversation/prompt_suggestion_background_js.rs"]
mod javascript;
#[path = "../src/conversation/prompt_suggestion_background_state.rs"]
mod state;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use serde_json::json;

#[test]
fn native_background_classifier_and_redactor_execute_in_quickjs() {
    let blocked = javascript::evaluate_result("prepare", json!({
        "text":"Which option do you prefer?", "previous":"working", "latestAsk":"Build it", "tools":"", "minutes":0
    })).expect("native regex must compile in production QuickJS");
    assert_eq!(
        blocked
            .string_at("/result/tempo")
            .expect("classifier tempo"),
        "blocked"
    );
    assert_eq!(
        blocked
            .string_at("/result/needs")
            .expect("classifier needs"),
        "Which option do you prefer?"
    );
    assert_eq!(
        javascript::evaluate("safeReply", json!({"text":"Use the first option"}))
            .and_then(|value| value.at_pointer("").and_then(|root| root.as_bool())),
        Some(true)
    );
    assert_eq!(
        javascript::evaluate(
            "safeReply",
            json!({"text":"Use sk-ant-api03-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaAAAA"})
        ).and_then(|value| value.at_pointer("").and_then(|root| root.as_bool())),
        Some(false)
    );
    let parsed = javascript::evaluate("parse", json!({"text":"```json\n{\"state\":\"blocked\",\"tempo\":\"blocked\",\"needs\":\"approve\",\"extra\":true}\n```", "previous":"working"})).unwrap();
    assert_eq!(parsed.string_at("/needs").expect("parsed needs"), "approve");
    assert!(!parsed.contains_property("extra"));
    let parsed_projection = javascript::evaluate_parse_projection(
        lingxi_core::types::utf16_json::Utf16JsonProjection::from(
            r#"{"state":"blocked","tempo":"blocked","needs":"Choose one option"}"#,
        ),
        "working",
    )
    .unwrap();
    assert_eq!(parsed_projection.string_at("/tempo"), Some("blocked"));
    assert_eq!(
        parsed_projection.string_at("/needs"),
        Some("Choose one option")
    );
}

#[test]
fn native_background_prepare_keeps_lone_need_units_in_projection() {
    for unit in [0xd800_u16, 0xdfff_u16] {
        let input = Utf16JsonProjection::parse(&format!(
            r#"{{"text":"Please provide \u{unit:04x} additional information","previous":"working","latestAsk":"","tools":"","minutes":0}}"#
        ))
        .expect("the source JSON escape is valid and projection preserves lone UTF-16 units");
        let result = javascript::evaluate_projection_result("prepare", input)
            .expect("the production classifier returns a projection without a serde Value hop");
        let expected = "Please provide "
            .encode_utf16()
            .chain([unit])
            .chain(" additional information".encode_utf16())
            .collect::<Vec<_>>();
        assert_eq!(result.string_at("/result/state"), Some("blocked"));
        assert_eq!(result.string_at("/result/tempo"), Some("blocked"));
        assert_eq!(result.string_units("/result/needs"), Some(expected));
    }
}

#[test]
fn native_background_parse_and_admission_keep_lone_need_units() {
    for unit in [0xd800_u16, 0xdfff_u16] {
        let response = Utf16JsonProjection::from(format!(
            r#"{{"state":"blocked","tempo":"blocked","needs":"\u{unit:04x}"}}"#
        ));
        let classifier_result = javascript::evaluate_parse_projection(response, "working")
            .expect("Native parser result must remain a UTF-16 projection");
        assert_eq!(classifier_result.string_at("/tempo"), Some("blocked"));
        assert_eq!(classifier_result.string_units("/needs"), Some(vec![unit]));

        let context = state::BackgroundSuggestionContext::from_blocked_classifier(
            "session".into(),
            7,
            &classifier_result,
            &classifier_result,
        )
        .expect("blocked, operation-free parse is eligible for suggestion focus bypass");
        assert_eq!(context.session_id, "session");
        assert_eq!(context.epoch, 7);
        assert_eq!(context.needs, Some(vec![unit]));

        let mut null_block = classifier_result.clone();
        null_block
            .set_field("block", Utf16JsonProjection::parse("null").unwrap())
            .unwrap();
        assert!(state::BackgroundSuggestionContext::from_blocked_classifier(
            "session".into(),
            7,
            &classifier_result,
            &null_block,
        )
        .is_some());

        let mut operation_active = classifier_result.clone();
        operation_active
            .set_field("block", Utf16JsonProjection::from("permission"))
            .unwrap();
        assert!(state::BackgroundSuggestionContext::from_blocked_classifier(
            "session".into(),
            7,
            &classifier_result,
            &operation_active,
        )
        .is_none());
    }
}

#[test]
fn background_reply_requires_unchanged_blocked_need_and_epoch_resets() {
    let mut state = state::BackgroundSuggestionState {
        session_id: "previous-session".into(),
        last_message_count: 9,
        previous_state: "blocked".into(),
        previous_state_since_ms: 123,
        persisted: Some(
            Utf16JsonProjection::from_serde_value(&json!({"tempo":"blocked", "needs":"pick one"}))
                .unwrap(),
        ),
        ..Default::default()
    };
    let pick_one = "pick one".encode_utf16().collect::<Vec<_>>();
    let different = "different".encode_utf16().collect::<Vec<_>>();
    assert!(!state.record_reply("Use the first option", Some(&different)));
    assert!(state.record_reply("Use the first option", Some(&pick_one)));
    assert!(!state.record_reply("Use the first option", Some(&pick_one)));
    state.mark_active();
    assert!(!state.record_reply("Use the second option", Some(&pick_one)));
    let epoch = state.epoch;
    state.reset();
    assert_ne!(state.epoch, epoch);
    assert!(state.eligible_context().is_none());
    assert!(state.session_id.is_empty());
    assert_eq!(state.last_message_count, 0);
    assert!(state.previous_state.is_empty());
    assert_eq!(state.previous_state_since_ms, 0);
}

#[test]
fn background_reply_persistence_round_trips_lone_needs_units_exactly() {
    for unit in [0xd800_u16, 0xdfff_u16] {
        let source = format!(r#"{{"tempo":"blocked","needs":"\u{unit:04x}"}}"#);
        let persisted = Utf16JsonProjection::parse(&source).unwrap();
        let mut state = state::BackgroundSuggestionState {
            session_id: "same-session".into(),
            epoch: 7,
            persisted: Some(persisted),
            ..Default::default()
        };
        let expected = [unit];
        let wrong = [if unit == 0xd800 { 0xdfff } else { 0xd800 }];
        assert!(!state.record_reply("Use the first option", Some(&wrong)));
        assert!(state.record_reply("Use the first option", Some(&expected)));

        let serialized = state.persisted.as_ref().unwrap().to_json_string().unwrap();
        let reloaded = Utf16JsonProjection::parse(&serialized).unwrap();
        assert_eq!(reloaded.string_at("/tempo"), Some("blocked"));
        assert_eq!(reloaded.string_units("/needs"), Some(expected.to_vec()));
        assert_eq!(
            reloaded.string_at("/suggestedReply"),
            Some("Use the first option")
        );
        assert_eq!(
            state.epoch, 7,
            "a same-session persistence round trip keeps its epoch"
        );
    }
}

#[test]
fn background_pipeline_persists_only_the_classifiers_current_safe_reply() {
    const CHILD: &str = "LINGXI_TEST_BACKGROUND_SUGGESTION_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "background_pipeline_persists_only_the_classifiers_current_safe_reply",
                "--nocapture",
            ])
            .env_remove("ANTHROPIC_SMALL_FAST_MODEL")
            .env_remove("ANTHROPIC_DEFAULT_HAIKU_MODEL")
            .env_remove("ANTHROPIC_DEFAULT_SONNET_MODEL")
            .env_remove("ANTHROPIC_DEFAULT_OPUS_MODEL")
            .env_remove("ANTHROPIC_DEFAULT_MODEL")
            .env_remove("ANTHROPIC_MODEL")
            .env_remove("DISABLE_TELEMETRY")
            .env_remove("DO_NOT_TRACK")
            .env_remove("LINGXI_DISABLE_NONESSENTIAL_TRAFFIC")
            .env_remove("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC")
            .env(CHILD, "1")
            .env("CLAUDE_CODE_SESSION_KIND", "bg")
            .env("CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION", "true")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(background_pipeline());
}

async fn background_pipeline() {
    use async_trait::async_trait;
    use lingxi_core::host::{CostSnapshot, OrchestratorHandle, OutputStream};
    use lingxi_core::types::{utf16_json::Utf16JsonProjection, ToolUseId};
    use orchestrator::test_support::{
        noop_hook_executor, NoOpPermissionGate, StaticMemoryProvider,
    };
    use orchestrator::test_support_stream::*;
    use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
    use std::sync::{Arc, Mutex};
    use tokio::sync::{Notify, Semaphore};
    let analytics_sink = Arc::new(telemetry::InMemorySink::new());
    let analytics_bus = Arc::new(telemetry::AnalyticsBus::new());
    analytics_bus.attach_sink(analytics_sink.clone()).await;
    #[derive(Default)]
    struct Output {
        ready: Notify,
        rows: Mutex<Vec<String>>,
    }
    impl Output {
        fn row_count(&self) -> usize {
            self.rows.lock().unwrap().len()
        }

        async fn wait_for_row_count(&self, target: usize) {
            loop {
                let notified = self.ready.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.row_count() >= target {
                    return;
                }
                notified.await;
            }
        }
    }
    #[async_trait]
    impl OutputStream for Output {
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
        async fn emit_end_turn(&self, _: &str, _: &CostSnapshot) {}
        fn wants_prompt_suggestions(&self) -> bool {
            true
        }
        async fn emit_prompt_suggestion(&self, _: &str, _: &str, text: &str) {
            self.rows.lock().unwrap().push(text.into());
            self.ready.notify_one();
        }
    }
    struct RouteApi;
    #[async_trait]
    impl orchestrator::OrchestratorApiClient for RouteApi {
        async fn messages_create(
            &self,
            _: orchestrator::OrchestratorApiRequest,
        ) -> Result<llm_runtime::HistoryResponse, llm_runtime::LlmError> {
            Err(llm_runtime::LlmError::ModelUnavailable)
        }
        async fn classifier_model_route_facts(
            &self,
            _: &str,
            _: Option<&str>,
        ) -> Result<
            llm_runtime::model::classifier_model_policy::ClassifierModelRouteFacts,
            llm_runtime::LlmError,
        > {
            Ok(
                llm_runtime::model::classifier_model_policy::ClassifierModelRouteFacts {
                    provider: Some("firstParty".into()),
                    native_base_url: true,
                    native_api_key: false,
                },
            )
        }
    }
    struct Routes;
    impl agent::ModelResolutionContextProvider for Routes {
        fn context_for_route(
            &self,
            model: &str,
            _: Option<&str>,
        ) -> Result<agent::ModelResolutionContext, agent::ModelResolutionError> {
            Ok(agent::ModelResolutionContext {
                route: agent::ModelRouteFacts {
                    model: model.into(),
                    profile: Some("native-profile".into()),
                    provider: Some(agent::ModelProviderKind::FirstParty),
                    ..Default::default()
                },
                family_defaults: agent::FamilyModelDefaults {
                    haiku: Some("claude-haiku-4-5-20251001".into()),
                    sonnet: Some("claude-sonnet-4-6".into()),
                    ..Default::default()
                },
                registered_model_ids: [
                    "claude-haiku-4-5-20251001".into(),
                    "claude-sonnet-4-6".into(),
                ]
                .into_iter()
                .collect(),
                ..Default::default()
            })
        }
    }
    #[derive(Default)]
    struct Settings(Mutex<serde_json::Value>);
    #[async_trait]
    impl hooks::mods::ModSettingsReader for Settings {
        async fn read(
            &self,
            _: Utf16JsonProjection,
        ) -> Result<Utf16JsonProjection, hooks::mods::ModError> {
            Ok(Utf16JsonProjection::plain(self.0.lock().unwrap().clone()))
        }
    }
    struct Client {
        classifiers: Mutex<Vec<sidequery::SideQueryRequest>>,
        classifier_response: Mutex<Utf16JsonProjection>,
        classifier_entered: Notify,
        entered: Notify,
        suggestion_query_count: std::sync::atomic::AtomicUsize,
        active_suggestion_queries: std::sync::atomic::AtomicUsize,
        suggestion_queries_idle: Notify,
        release: Semaphore,
    }
    impl Client {
        fn classifier_count(&self) -> usize {
            self.classifiers.lock().unwrap().len()
        }

        fn suggestion_query_count(&self) -> usize {
            self.suggestion_query_count
                .load(std::sync::atomic::Ordering::SeqCst)
        }

        async fn wait_for_classifier_count(&self, target: usize) {
            loop {
                let notified = self.classifier_entered.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.classifier_count() >= target {
                    return;
                }
                notified.await;
            }
        }

        async fn wait_for_suggestion_query_count(&self, target: usize) {
            loop {
                let notified = self.entered.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.suggestion_query_count() >= target {
                    return;
                }
                notified.await;
            }
        }

        async fn wait_for_suggestion_queries_idle(&self) {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    let notified = self.suggestion_queries_idle.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    if self
                        .active_suggestion_queries
                        .load(std::sync::atomic::Ordering::SeqCst)
                        == 0
                    {
                        return;
                    }
                    notified.await;
                }
            })
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "timed out waiting for suggestion queries to become idle; active={}",
                    self.active_suggestion_queries
                        .load(std::sync::atomic::Ordering::SeqCst)
                )
            });
        }
    }
    struct ActiveSuggestionQuery<'a> {
        active: &'a std::sync::atomic::AtomicUsize,
        idle: &'a Notify,
    }
    impl Drop for ActiveSuggestionQuery<'_> {
        fn drop(&mut self) {
            self.active
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            self.idle.notify_waiters();
        }
    }
    #[async_trait]
    impl sidequery::SideQueryClient for Client {
        async fn query_fork(
            &self,
            request: sidequery::SideQueryRequest,
            options: sidequery::ForkedSideQueryOptions,
        ) -> Result<sidequery::ForkedSideQueryResponse, sidequery::SideQueryError> {
            let response = if request.query_source.as_str() == "agent_classifier" {
                let blocks = options
                    .system_blocks
                    .expect("classifier must retain explicit native system blocks");
                assert_eq!(
                    blocks[0].text.string_at(""),
                    Some(include_str!(
                        "../src/conversation/prompt_suggestion_classifier_prompt.txt"
                    ))
                );
                assert_eq!(request.profile.as_deref(), Some("native-profile"));
                assert!(request.system_prompt.is_none());
                assert_eq!(
                    request.max_tokens,
                    if request.model.contains("haiku") {
                        1024
                    } else {
                        3072
                    }
                );
                self.classifiers.lock().unwrap().push(request);
                self.classifier_entered.notify_waiters();
                sidequery::SideQueryResponse {
                    text: Some(self.classifier_response.lock().unwrap().clone()),
                    structured: None,
                    tool_calls: vec![],
                    usage: Default::default(),
                    stop_reason: Some("end_turn".into()),
                    retry_count: 0,
                }
            } else {
                self.query(request).await?
            };
            let text = response
                .text
                .as_ref()
                .and_then(|text| text.string_at(""))
                .unwrap_or_default()
                .to_owned();
            Ok(sidequery::ForkedSideQueryResponse {
                response,
                assistant_content: vec![],
                candidates: vec![text],
                request_id: None,
            })
        }
        async fn query(
            &self,
            request: sidequery::SideQueryRequest,
        ) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
            assert_eq!(
                request.query_source,
                sidequery::QuerySource::PromptSuggestion,
                "explicit question must use native preclassification without a paid classifier request"
            );
            self.active_suggestion_queries
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let _active = ActiveSuggestionQuery {
                active: &self.active_suggestion_queries,
                idle: &self.suggestion_queries_idle,
            };
            self.suggestion_query_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.entered.notify_waiters();
            self.release.acquire().await.unwrap().forget();
            Ok(sidequery::SideQueryResponse {
                text: Some("Use the first option".into()),
                structured: None,
                tool_calls: vec![],
                usage: Default::default(),
                stop_reason: Some("end_turn".into()),
                retry_count: 0,
            })
        }
    }
    let home = tempfile::tempdir().unwrap();
    let output = Arc::new(Output::default());
    let client = Arc::new(Client {
        classifiers: Mutex::new(vec![]),
        classifier_response: Mutex::new(Utf16JsonProjection::from(
            r#"{"state":"blocked","tempo":"blocked","needs":"Choose one option"}"#,
        )),
        classifier_entered: Notify::new(),
        entered: Notify::new(),
        suggestion_query_count: std::sync::atomic::AtomicUsize::new(0),
        active_suggestion_queries: std::sync::atomic::AtomicUsize::new(0),
        suggestion_queries_idle: Notify::new(),
        release: Semaphore::new(0),
    });
    let settings = Arc::new(Settings::default());
    // Three heuristic turns, two model-route checks, and two UTF-16 needs
    // cases each require their own scripted assistant response.
    let turns = (0..7)
        .map(|i| {
            vec![
                message_start(&format!("reply-{i}"), "claude-sonnet-4-6"),
                content_block_start_text(0),
                text_delta(
                    0,
                    if i < 3 {
                        "Which option do you prefer?"
                    } else {
                        "The implementation consists of two components."
                    },
                ),
                content_block_stop(0),
                message_delta_stop("end_turn"),
                message_stop(),
            ]
        })
        .collect();
    let orch = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig {
                model: "claude-sonnet-4-6".into(),
                interactive_session: true,
                ..Default::default()
            },
            Arc::new(RouteApi),
            Arc::new(MockStreamingApiClient::with_turns(turns)),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            output.clone(),
            Arc::new(StaticMemoryProvider::empty()),
            home.path().into(),
        )
        .with_model_resolution_context_provider(Arc::new(Routes))
        .with_mod_settings_reader(settings.clone())
        .with_config_home(home.path().into())
        .with_analytics_bus(analytics_bus)
        .with_cache_safe_slot(Arc::new(sidequery::CacheSafeParamsSlot::new()))
        .with_recap_runner(Arc::new(
            sidequery::ForkedAgentRunner::new()
                .with_side_query_client(client.clone(), "claude-sonnet-4-6".into()),
        )),
    );
    orch.seed_initial_model_profile("claude-sonnet-4-6", "native-profile")
        .await;
    let session = orch.current_session_id().await.as_uuid().to_string();
    let path = home
        .path()
        .join("prompt-suggestions")
        .join(format!("{session}.json"));
    orch.update_prompt_suggestion_context(lingxi_core::host::PromptSuggestionContext {
        session_id: session.clone(),
        focus: "blurred".into(),
        input_empty: true,
        pending_permission: false,
        elicitation_active: false,
    })
    .await
    .unwrap();
    let first_query = client.suggestion_query_count() + 1;
    orch.run_turn_streaming("Prepare alternatives")
        .await
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        client.wait_for_suggestion_query_count(first_query),
    )
    .await
    .unwrap();
    // Keep the first generation pending and start a newer turn. Its cancellation
    // must not be confused with a stale `Notify` permit from the first query.
    let second_query = first_query + 1;
    let first_output = output.row_count() + 1;
    orch.run_turn_streaming("Explain those options")
        .await
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        client.wait_for_suggestion_query_count(second_query),
    )
    .await
    .unwrap();
    client.release.add_permits(1);
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        output.wait_for_row_count(first_output),
    )
    .await
    .unwrap();
    client.wait_for_suggestion_queries_idle().await;
    let saved: serde_json::Value =
        serde_json::from_slice(&tokio::fs::read(&path).await.unwrap()).unwrap();
    assert_eq!(saved["tempo"], "blocked");
    assert_eq!(saved["needs"], "Which option do you prefer?");
    assert_eq!(saved["suggestedReply"], "Use the first option");
    assert_eq!(
        output.rows.lock().unwrap().len(),
        1,
        "unfocused bg may show a classifier-backed reply"
    );

    let changed_query = client.suggestion_query_count() + 1;
    let changed_output = output.row_count() + 1;
    orch.run_turn_streaming("Consider another option")
        .await
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        client.wait_for_suggestion_query_count(changed_query),
    )
    .await
    .unwrap();
    let mut changed: serde_json::Value =
        serde_json::from_slice(&tokio::fs::read(&path).await.unwrap()).unwrap();
    changed["needs"] = json!("An external permission now needs attention");
    tokio::fs::write(&path, serde_json::to_vec(&changed).unwrap())
        .await
        .unwrap();
    client.release.add_permits(1);
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        output.wait_for_row_count(changed_output),
    )
    .await
    .unwrap();
    client.wait_for_suggestion_queries_idle().await;
    let saved: serde_json::Value =
        serde_json::from_slice(&tokio::fs::read(&path).await.unwrap()).unwrap();
    assert!(
        saved.get("suggestedReply").is_none(),
        "reply never persists over changed needs"
    );
    assert!(
        client.classifiers.lock().unwrap().is_empty(),
        "native question preclassification skips provider classifier requests"
    );
    for denied in [false, true] {
        if denied {
            *settings.0.lock().unwrap() = json!({"deniedModels":["haiku"]});
        }
        let classifier_target = client.classifier_count() + 1;
        let suggestion_target = client.suggestion_query_count() + 1;
        let output_target = output.row_count() + 1;
        orch.run_turn_streaming("Compare the implementation components")
            .await
            .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.wait_for_classifier_count(classifier_target),
        )
        .await
        .unwrap();
        let requests = client.classifiers.lock().unwrap();
        assert_eq!(
            requests.last().unwrap().model,
            if denied {
                "claude-sonnet-4-6"
            } else {
                "claude-haiku-4-5-20251001"
            }
        );
        drop(requests);
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.wait_for_suggestion_query_count(suggestion_target),
        )
        .await
        .unwrap();
        client.release.add_permits(1);
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            output.wait_for_row_count(output_target),
        )
        .await
        .unwrap();
        client.wait_for_suggestion_queries_idle().await;
    }
    for unit in [0xd800_u16, 0xdfff_u16] {
        *client.classifier_response.lock().unwrap() = Utf16JsonProjection::from(format!(
            r#"{{"state":"blocked","tempo":"blocked","needs":"\u{unit:04x}"}}"#
        ));
        let event_start = analytics_sink.events().await.len();
        let classifier_target = client.classifier_count() + 1;
        let suggestion_target = client.suggestion_query_count() + 1;
        let output_target = output.row_count() + 1;
        orch.run_turn_streaming("Compare the implementation components")
            .await
            .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.wait_for_classifier_count(classifier_target),
        )
        .await
        .unwrap();
        if tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.wait_for_suggestion_query_count(suggestion_target),
        )
        .await
        .is_err()
        {
            let classifier_response = client.classifier_response.lock().unwrap().clone();
            let disk_projection = tokio::fs::read(&path)
                .await
                .ok()
                .and_then(|bytes| std::str::from_utf8(&bytes).ok().map(ToOwned::to_owned))
                .and_then(|text| Utf16JsonProjection::parse(&text).ok());
            let previous_state = disk_projection
                .as_ref()
                .and_then(|value| value.string_at("/state"))
                .unwrap_or("working")
                .to_owned();
            let classifier_result =
                javascript::evaluate_parse_projection(classifier_response, &previous_state)
                    .and_then(|result| {
                        result.to_json_string().map_err(|error| {
                            hooks::function_hook::FunctionHookError::NotSerialisable(
                                error.to_string(),
                            )
                        })
                    })
                    .unwrap_or_else(|error| format!("<parse error: {error:?}>"));
            let disk_state = disk_projection
                .as_ref()
                .and_then(|value| value.to_json_string().ok())
                .unwrap_or_else(|| "<missing or invalid>".into());
            let suppression_reasons = analytics_sink
                .events()
                .await
                .into_iter()
                .skip(event_start)
                .filter(|event| event.name == "tengu_prompt_suggestion")
                .filter_map(|event| event.metadata.get("reason").cloned())
                .collect::<Vec<_>>();
            panic!(
                "UTF-16 need U+{unit:04X} classifier did not start suggestion query; suppression reasons={suppression_reasons:?}; classifier result={classifier_result}; persisted state={disk_state}"
            );
        }
        client.release.add_permits(1);
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            output.wait_for_row_count(output_target),
        )
        .await
        .unwrap();
        client.wait_for_suggestion_queries_idle().await;

        let bytes = tokio::fs::read(&path).await.unwrap();
        let json_text = std::str::from_utf8(&bytes).unwrap();
        let saved = Utf16JsonProjection::parse(json_text).unwrap();
        assert_eq!(saved.string_at("/tempo"), Some("blocked"));
        assert_eq!(saved.string_units("/needs"), Some(vec![unit]));
        assert_eq!(
            saved.string_at("/suggestedReply"),
            Some("Use the first option"),
            "same-epoch classifier context must survive exact disk reload"
        );
    }
    assert_eq!(client.classifiers.lock().unwrap().len(), 4);
}

#[test]
fn native_background_input_preserves_tool_tie_order_whole_history_and_text_window() {
    let messages = json!([
        {"type":"assistant","message":{"content":[{"type":"text","text":"  Prior text  "},{"type":"tool_use","name":"Zulu","input":{}},{"type":"tool_use","name":"Alpha","input":{}},{"type":"tool_use","name":"ScheduleWakeup","input":{}},{"type":"tool_use","name":"Agent","input":{}}]}},
        {"type":"assistant","message":{"content":[{"type":"text","text":""},{"type":"text","text":"  Latest one"},{"type":"text","text":"Latest two  "},{"type":"tool_use","name":"Zulu","input":{}}]}},
        {"type":"assistant","isApiErrorMessage":true,"message":{"content":[{"type":"text","text":"Do not classify this API error"}]}}
    ]);
    let assembled =
        javascript::evaluate("assemble", json!({"messages":messages,"lastCount":1})).unwrap();
    assert_eq!(
        assembled.string_at("/text").expect("assembled text"),
        "Latest one\nLatest two"
    );
    assert_eq!(
        assembled.string_at("/tools").expect("assembled tools"),
        "Zulu×2, Alpha, Agent"
    );
    assert_eq!(assembled.number_at("/count"), Some(3.0));
    assert_eq!(
        assembled.to_json_string().unwrap(),
        r#"{"count":3,"start":1,"text":"Latest one\nLatest two","tools":"Zulu×2, Alpha, Agent"}"#,
        "source JSON must use Native's JavaScript number rendering"
    );
    let text = assembled.string_at("/text").expect("assembled source text");
    let tools = assembled
        .string_at("/tools")
        .expect("assembled tool summary");
    let prepared = javascript::evaluate(
        "prepare",
        json!({"text":text,"tools":tools,"latestAsk":"<system-reminder>private system</system-reminder>  Keep ordering  ","previous":"working","minutes":2}),
    )
    .unwrap();
    assert_eq!(
        prepared.string_at("/prompt"),
        Some(
        "Current state: working (for 2m)\nTool calls so far: Zulu×2, Alpha, Agent\nUser's most recent ask: \"Keep ordering\"\n\nAssistant message tail (last 21 chars):\nLatest one\nLatest two"
        )
    );
    assert_eq!(
        javascript::evaluate(
            "prepare",
            json!({"text":"🎉 ...","tools":"","latestAsk":"","previous":"blocked","minutes":0})
        )
        .unwrap()
        .at_pointer("/hasText")
        .and_then(|value| value.as_bool()),
        Some(false)
    );
}
