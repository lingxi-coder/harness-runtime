//! Evaluate the same native functions through the production QuickJS sandbox.
#[path = "../src/conversation/prompt_suggestion_background_js.rs"]
mod javascript;
#[path = "../src/conversation/prompt_suggestion_background_state.rs"]
mod state;
use serde_json::json;

#[test]
fn native_background_classifier_and_redactor_execute_in_quickjs() {
    let blocked = javascript::evaluate_result("prepare", json!({
        "text":"Which option do you prefer?", "previous":"working", "latestAsk":"Build it", "tools":"", "minutes":0
    })).expect("native regex must compile in production QuickJS");
    assert_eq!(blocked["result"]["tempo"], "blocked");
    assert_eq!(blocked["result"]["needs"], "Which option do you prefer?");
    assert_eq!(
        javascript::evaluate("safeReply", json!({"text":"Use the first option"})),
        Some(json!(true))
    );
    assert_eq!(
        javascript::evaluate(
            "safeReply",
            json!({"text":"Use sk-ant-api03-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaAAAA"})
        ),
        Some(json!(false))
    );
    let parsed = javascript::evaluate("parse", json!({"text":"```json\n{\"state\":\"blocked\",\"tempo\":\"blocked\",\"needs\":\"approve\",\"extra\":true}\n```", "previous":"working"})).unwrap();
    assert_eq!(parsed["needs"], "approve");
    assert!(parsed.get("extra").is_none());
}

#[test]
fn background_reply_requires_unchanged_blocked_need_and_epoch_resets() {
    let mut state = state::BackgroundSuggestionState {
        session_id: "previous-session".into(),
        last_message_count: 9,
        previous_state: "blocked".into(),
        previous_state_since_ms: 123,
        persisted: Some(json!({"tempo":"blocked", "needs":"pick one"})),
        ..Default::default()
    };
    assert!(!state.record_reply("Use the first option", Some("different")));
    assert!(state.record_reply("Use the first option", Some("pick one")));
    assert!(!state.record_reply("Use the first option", Some("pick one")));
    state.mark_active();
    assert!(!state.record_reply("Use the second option", Some("pick one")));
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
    #[derive(Default)]
    struct Output {
        ready: Notify,
        rows: Mutex<Vec<String>>,
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
        entered: Notify,
        release: Semaphore,
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
                sidequery::SideQueryResponse {
                    text: Some(
                        r#"{"state":"blocked","tempo":"blocked","needs":"Choose one option"}"#
                            .into(),
                    ),
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
            self.entered.notify_one();
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
        entered: Notify::new(),
        release: Semaphore::new(0),
    });
    let settings = Arc::new(Settings::default());
    let turns = (0..5)
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
        .with_cache_safe_slot(Arc::new(sidequery::CacheSafeParamsSlot::new()))
        .with_recap_runner(Arc::new(
            sidequery::ForkedAgentRunner::new()
                .with_side_query_client(client.clone(), "claude-sonnet-4-6".into()),
        )),
    );
    orch.seed_initial_model_profile("claude-sonnet-4-6", "native-profile")
        .await;
    let session = orch.current_session_id().await.as_uuid().to_string();
    orch.update_prompt_suggestion_context(lingxi_core::host::PromptSuggestionContext {
        session_id: session.clone(),
        focus: "blurred".into(),
        input_empty: true,
        pending_permission: false,
        elicitation_active: false,
    })
    .await
    .unwrap();
    orch.run_turn_streaming("Prepare alternatives")
        .await
        .unwrap();
    orch.run_turn_streaming("Explain those options")
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), client.entered.notified())
        .await
        .unwrap();
    client.release.add_permits(1);
    tokio::time::timeout(std::time::Duration::from_secs(5), output.ready.notified())
        .await
        .unwrap();
    let path = home
        .path()
        .join("prompt-suggestions")
        .join(format!("{session}.json"));
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

    orch.run_turn_streaming("Consider another option")
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), client.entered.notified())
        .await
        .unwrap();
    let mut changed: serde_json::Value =
        serde_json::from_slice(&tokio::fs::read(&path).await.unwrap()).unwrap();
    changed["needs"] = json!("An external permission now needs attention");
    tokio::fs::write(&path, serde_json::to_vec(&changed).unwrap())
        .await
        .unwrap();
    client.release.add_permits(1);
    tokio::time::timeout(std::time::Duration::from_secs(5), output.ready.notified())
        .await
        .unwrap();
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
        orch.run_turn_streaming("Compare the implementation components")
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), client.entered.notified())
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
        client.release.add_permits(1);
        tokio::time::timeout(std::time::Duration::from_secs(5), output.ready.notified())
            .await
            .unwrap();
    }
    assert_eq!(client.classifiers.lock().unwrap().len(), 2);
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
    assert_eq!(assembled["text"], "Latest one\nLatest two");
    assert_eq!(assembled["tools"], "Zulu×2, Alpha, Agent");
    assert_eq!(assembled["count"], 3);
    let prepared=javascript::evaluate("prepare",json!({"text":assembled["text"],"tools":assembled["tools"],"latestAsk":"<system-reminder>private system</system-reminder>  Keep ordering  ","previous":"working","minutes":2})).unwrap();
    assert_eq!(
        prepared["prompt"],
        "Current state: working (for 2m)\nTool calls so far: Zulu×2, Alpha, Agent\nUser's most recent ask: \"Keep ordering\"\n\nAssistant message tail (last 21 chars):\nLatest one\nLatest two"
    );
    assert_eq!(
        javascript::evaluate(
            "prepare",
            json!({"text":"🎉 ...","tools":"","latestAsk":"","previous":"blocked","minutes":0})
        )
        .unwrap()["hasText"],
        false
    );
}
