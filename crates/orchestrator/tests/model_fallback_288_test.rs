//! Current native query-owned model fallback through the actual SDK.
use async_trait::async_trait;
use futures::StreamExt;
use lingxi_core::types::ConversationMessage;
use llm_runtime::model::{thinking::ThinkingConfig, user_agent::UserAgentEnv};
use llm_runtime::{ApiService, ClientConfig, ModelRuntime, SubscriberState, Transport};
use orchestrator::test_support::{
    noop_hook_executor, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{
    state_from_messages, ConversationOrchestrator, OrchestratorConfig, ProviderApiAdapter,
};
use platform_posix::fs::PosixFileSystem;
use serde_json::{json, Value};
use session::JsonlMessage;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

const PRIMARY: &str = "claude-opus-4-7";
const FALLBACK: &str = "claude-sonnet-4-6";
const FAST_FALLBACK: &str = "claude-opus-4-8";
#[derive(Clone)]
enum Action {
    Http(u16),
    HttpKind(u16, &'static str),
    NonstreamHttp(u16),
    HttpDeclined(u16),
    HttpWatchdog(u16, bool),
    FastRejection,
    ConnectionLost,
    Thinking(&'static str),
    Text,
    Tool,
    Partial(&'static str),
    StopThinking(&'static str),
}
struct Capture {
    requests: Mutex<Vec<(String, Value)>>,
    actions: Mutex<VecDeque<(String, Action)>>,
}
#[async_trait]
impl Transport for Capture {
    async fn send(
        &self,
        request: lingxi_llm_client::HttpRequest,
    ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError> {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let (expected, action) = self
            .actions
            .lock()
            .unwrap()
            .pop_front()
            .expect("no extra physical dispatch");
        assert_eq!(body["model"], expected);
        assert_eq!(
            body["stream"] == true,
            !matches!(action, Action::NonstreamHttp(_)),
            "each physical recovery keeps its selected mode"
        );
        self.requests
            .lock()
            .unwrap()
            .push((request.url.clone(), body.clone()));
        if matches!(action, Action::FastRejection) {
            return Ok(lingxi_llm_client::HttpResponse {
                status: 400,
                headers: vec![("x-should-retry".into(), "false".into())],
                body: serde_json::to_vec(&json!({"type":"error","error":{
                    "type":"invalid_request_error",
                    "message":format!("'{expected}' does not support the `speed` parameter")
                }}))
                .unwrap()
                .into(),
            }
            .into());
        }
        if let Action::Http(status)
        | Action::HttpDeclined(status)
        | Action::HttpWatchdog(status, _)
        | Action::HttpKind(status, _)
        | Action::NonstreamHttp(status) = action
        {
            let headers = if matches!(action, Action::HttpDeclined(_)) {
                vec![("x-should-retry".into(), "false".into())]
            } else {
                vec![]
            };
            if let Action::HttpWatchdog(_, persistent) = action {
                variable(
                    branding::RETRY_WATCHDOG_ENV,
                    Some(if persistent { "1" } else { "0" }),
                );
            }
            return Ok(lingxi_llm_client::HttpResponse {status,headers,body:serde_json::to_vec(&json!({"type":"error","error":{"type":if let Action::HttpKind(_, kind) = action {kind} else {match status {429=>"rate_limit_error",529=>"overloaded_error",_=>"api_error"}},"message":"fixture retry"}})).unwrap().into()}.into());
        }
        let mut frames = vec![
            json!({"type":"message_start","message":{"id":"msg_fixture","type":"message","role":"assistant","model":expected,"content":[],"usage":{"input_tokens":2,"output_tokens":1}}}),
        ];
        match action {
            Action::Thinking(kind) | Action::StopThinking(kind) => {
                frames.extend([
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
                    json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"abandoned reasoning"}}),
                    json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"fixture-signature"}}),
                    json!({"type":"content_block_stop","index":0}),
                ]);
                if matches!(action, Action::StopThinking(_)) {
                    frames.push(json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}));
                }
                frames.push(json!({"type":"error","error":{"type":kind,"message":"fixture stream failure"}}));
            }
            Action::Partial(kind) => {
                frames.extend([
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                    json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"retain completed output"}}),
                    json!({"type":"content_block_stop","index":0}),
                    json!({"type":"error","error":{"type":kind,"message":"fixture failure after useful output"}}),
                ]);
            }
            Action::Tool => {
                frames.extend([
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"fixture_tool","name":"Fixture","input":{}}}),
                    json!({"type":"content_block_stop","index":0}),
                    json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":2}}),
                    json!({"type":"message_stop"}),
                ]);
            }
            Action::Text => {
                frames.extend([
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                    json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"success"}}),
                    json!({"type":"content_block_stop","index":0}),
                    json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}),
                    json!({"type":"message_stop"}),
                ]);
            }
            Action::ConnectionLost => {}
            Action::Http(_)
            | Action::HttpKind(_, _)
            | Action::NonstreamHttp(_)
            | Action::HttpDeclined(_)
            | Action::HttpWatchdog(_, _) => {
                unreachable!()
            }
            Action::FastRejection => unreachable!(),
        }
        let bytes = frames
            .iter()
            .map(|f| format!("event: {}\ndata: {f}\n\n", f["type"].as_str().unwrap()))
            .collect::<String>()
            .into_bytes();
        if matches!(action, Action::ConnectionLost) {
            return Ok(lingxi_llm_client::StreamResponse {
                status: 200,
                headers: vec![("content-type".into(), "text/event-stream".into())],
                body: futures::stream::iter(vec![
                    Ok(bytes.into()),
                    Err(lingxi_llm_client::protocol::LlmError::Transport {
                        message: "fixture connection closed while reading the stream".into(),
                    }),
                ])
                .boxed(),
            });
        }
        Ok(lingxi_llm_client::HttpResponse {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: bytes.into(),
        }
        .into())
    }
}
fn variable(name: &str, value: Option<&str>) {
    if let Some(value) = value {
        std::env::set_var(name, value)
    } else {
        std::env::remove_var(name)
    }
}
fn config() -> ClientConfig {
    let model = |id: &str, label: &str| json!({"display_model":label,"request_model":id,"billing_model":id,"capabilities":{"streaming":true,"tools":true,"vision":false,"documents":false,"reasoning":true,"structured_output":false}});
    serde_json::from_value(json!({"providers":[
        {"provider_id":"anthropic_first_party","profile_name":"direct","base_url":"https://api.anthropic.com","protocol":"anthropic_messages","auth":"none","credential":{"type":"none"},"models":[model(PRIMARY,"Opus 4.7"),model(FALLBACK,"Sonnet 4.6"),model(FAST_FALLBACK,"Opus 4.8")]},
        {"provider_id":{"custom":{"name":"other"}},"profile_name":"other","base_url":"https://second.fixture.invalid","protocol":"anthropic_messages","auth":"none","credential":{"type":"none"},"models":[model("remote/model","Other Model")]}
    ]})).unwrap()
}
fn make(
    capture: Arc<Capture>,
    output: Arc<MockOutputStream>,
    fallback: &str,
    max_retries: u32,
    max_turns: u32,
    path: &std::path::Path,
    fast: bool,
) -> ConversationOrchestrator {
    let service = Arc::new(
        ApiService::new_with_routing(
            Arc::new(ModelRuntime::from_config(config()).unwrap()),
            capture,
            SubscriberState::default(),
            UserAgentEnv::default(),
            "fixture",
            None,
            None,
            None,
            Default::default(),
            Some(max_retries),
            None,
        )
        .with_thinking(if fast {
            ThinkingConfig::Adaptive
        } else {
            ThinkingConfig::Disabled
        })
        .with_fast_policy_source(Arc::new(|| llm_runtime::model::fast_admission::Policy {
            flag_fast: true,
            cached_org_enabled: true,
            remote: true,
            agent_owned_remote: false,
            ..Default::default()
        })),
    );
    let adapter = Arc::new(
        ProviderApiAdapter::new(service)
            .with_fast_mode(Arc::new(std::sync::atomic::AtomicBool::new(fast))),
    );
    ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig {
            model: PRIMARY.into(),
            fallback_model: Some(fallback.into()),
            interactive_session: true,
            max_turns,
            ..Default::default()
        },
        adapter.clone(),
        adapter,
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output,
        Arc::new(StaticMemoryProvider::empty()),
        path.parent().unwrap().to_path_buf(),
    )
    .with_jsonl_writer(Arc::new(session::JsonlWriter::new(
        path.to_path_buf(),
        Arc::new(PosixFileSystem::new(path.parent().unwrap().to_path_buf())),
    )))
}
#[tokio::test(start_paused = true)]
async fn current_configured_fallback_budget_route_notice_and_resume_lifecycle() {
    variable(branding::MAX_RETRIES_ENV, None);
    variable("CLAUDE_CODE_EXTRA_BODY", None);
    variable(branding::DISABLE_NONSTREAMING_FALLBACK_ENV, Some("1"));
    variable(branding::RETRY_WATCHDOG_ENV, Some("0"));
    let scenarios = vec![
        (
            "http-chain-fast-rejection",
            FAST_FALLBACK,
            0,
            1,
            false,
            vec![
                (PRIMARY, Action::Http(500)),
                (FAST_FALLBACK, Action::FastRejection),
                (FAST_FALLBACK, Action::Text),
            ],
            1,
            1,
        ),
        (
            "http-no-double-body-overload",
            FALLBACK,
            0,
            1,
            false,
            vec![
                (PRIMARY, Action::Thinking("overloaded_error")),
                (PRIMARY, Action::NonstreamHttp(529)),
            ],
            0,
            1,
        ),
        (
            "http-529-api-payload",
            FALLBACK,
            2,
            1,
            false,
            vec![
                (PRIMARY, Action::HttpKind(529, "api_error")),
                (PRIMARY, Action::HttpKind(529, "api_error")),
                (PRIMARY, Action::HttpKind(529, "api_error")),
                (FALLBACK, Action::Text),
            ],
            1,
            1,
        ),
        (
            "http-400-payload-overload",
            FALLBACK,
            2,
            1,
            false,
            vec![
                (PRIMARY, Action::HttpKind(400, "overloaded_error")),
                (PRIMARY, Action::HttpKind(400, "overloaded_error")),
                (PRIMARY, Action::HttpKind(400, "overloaded_error")),
                (FALLBACK, Action::Text),
            ],
            1,
            1,
        ),
        (
            "http-500-payload-overload-persistent",
            FALLBACK,
            2,
            1,
            true,
            vec![
                (PRIMARY, Action::HttpKind(500, "overloaded_error")),
                (PRIMARY, Action::HttpKind(500, "overloaded_error")),
                (PRIMARY, Action::HttpKind(500, "overloaded_error")),
                (FALLBACK, Action::Text),
            ],
            1,
            1,
        ),
        (
            "http-500-immediate",
            FALLBACK,
            0,
            1,
            false,
            vec![(PRIMARY, Action::Http(500)), (FALLBACK, Action::Text)],
            1,
            1,
        ),
        (
            "http-503-immediate",
            FALLBACK,
            1,
            1,
            false,
            vec![(PRIMARY, Action::Http(503)), (FALLBACK, Action::Text)],
            1,
            1,
        ),
        (
            "http-599-immediate",
            FALLBACK,
            0,
            1,
            false,
            vec![(PRIMARY, Action::Http(599)), (FALLBACK, Action::Text)],
            1,
            1,
        ),
        (
            "http-500-declined-immediate",
            FALLBACK,
            0,
            1,
            false,
            vec![
                (PRIMARY, Action::HttpDeclined(500)),
                (FALLBACK, Action::Text),
            ],
            1,
            1,
        ),
        (
            "http-503-persistent",
            FALLBACK,
            1,
            1,
            true,
            vec![(PRIMARY, Action::Http(503)), (PRIMARY, Action::Text)],
            0,
            1,
        ),
        (
            "http-529-threshold",
            FALLBACK,
            2,
            1,
            false,
            vec![
                (PRIMARY, Action::Http(529)),
                (PRIMARY, Action::Http(529)),
                (PRIMARY, Action::Http(529)),
                (FALLBACK, Action::Text),
            ],
            1,
            1,
        ),
        (
            "http-529-persistent-threshold",
            FALLBACK,
            2,
            1,
            true,
            vec![
                (PRIMARY, Action::Http(529)),
                (PRIMARY, Action::Http(529)),
                (PRIMARY, Action::Http(529)),
                (FALLBACK, Action::Text),
            ],
            1,
            1,
        ),
        (
            "http-529-declined-threshold",
            FALLBACK,
            2,
            1,
            false,
            vec![
                (PRIMARY, Action::HttpDeclined(529)),
                (PRIMARY, Action::HttpDeclined(529)),
                (PRIMARY, Action::HttpDeclined(529)),
                (FALLBACK, Action::Text),
            ],
            1,
            1,
        ),
        (
            "http-529-early-budget-exhaustion",
            FALLBACK,
            1,
            1,
            false,
            vec![(PRIMARY, Action::Http(529)), (PRIMARY, Action::Http(529))],
            0,
            1,
        ),
        (
            "http-500-same-model",
            PRIMARY,
            1,
            1,
            false,
            vec![(PRIMARY, Action::Http(500)), (PRIMARY, Action::Text)],
            0,
            1,
        ),
        (
            "http-fresh-budget",
            FALLBACK,
            1,
            1,
            false,
            vec![
                (PRIMARY, Action::Http(500)),
                (FALLBACK, Action::Http(500)),
                (FALLBACK, Action::Text),
            ],
            1,
            1,
        ),
        (
            "http-ordered-chain",
            "claude-sonnet-4-6,other/remote/model",
            0,
            1,
            false,
            vec![
                (PRIMARY, Action::Http(500)),
                (FALLBACK, Action::Http(503)),
                ("remote/model", Action::Text),
            ],
            2,
            1,
        ),
        (
            "http-provider-qualified",
            "other/remote/model",
            0,
            1,
            false,
            vec![(PRIMARY, Action::Http(503)), ("remote/model", Action::Text)],
            1,
            1,
        ),
        (
            "http-watchdog-enabled-during-response",
            FALLBACK,
            1,
            1,
            false,
            vec![
                (PRIMARY, Action::HttpWatchdog(503, true)),
                (PRIMARY, Action::Text),
            ],
            0,
            1,
        ),
        (
            "http-watchdog-disabled-during-response",
            FALLBACK,
            1,
            1,
            true,
            vec![
                (PRIMARY, Action::HttpWatchdog(503, false)),
                (FALLBACK, Action::Text),
            ],
            1,
            1,
        ),
        (
            "http-on-stream-reopen",
            FALLBACK,
            1,
            1,
            false,
            vec![
                (PRIMARY, Action::ConnectionLost),
                (PRIMARY, Action::Http(500)),
                (FALLBACK, Action::Text),
            ],
            1,
            1,
        ),
        (
            "http-shared-body-overload-count",
            FALLBACK,
            2,
            1,
            false,
            vec![
                (PRIMARY, Action::Thinking("overloaded_error")),
                (PRIMARY, Action::Http(529)),
                (PRIMARY, Action::Http(529)),
                (FALLBACK, Action::Text),
            ],
            1,
            1,
        ),
        (
            "server",
            FALLBACK,
            0,
            1,
            false,
            vec![
                (PRIMARY, Action::Thinking("api_error")),
                (FALLBACK, Action::Text),
            ],
            1,
            1,
        ),
        (
            "timeout",
            FALLBACK,
            0,
            1,
            false,
            vec![
                (PRIMARY, Action::Thinking("timeout_error")),
                (FALLBACK, Action::Text),
            ],
            1,
            1,
        ),
        (
            "fresh-budget",
            FALLBACK,
            1,
            1,
            false,
            vec![
                (PRIMARY, Action::Http(429)),
                (PRIMARY, Action::Thinking("api_error")),
                (FALLBACK, Action::Http(500)),
                (FALLBACK, Action::Text),
            ],
            1,
            1,
        ),
        (
            "tool-and-next-query",
            FALLBACK,
            0,
            3,
            false,
            vec![
                (PRIMARY, Action::Thinking("api_error")),
                (FALLBACK, Action::Tool),
                (FALLBACK, Action::Text),
                (PRIMARY, Action::Text),
            ],
            1,
            2,
        ),
        (
            "provider-qualified",
            "other/remote/model",
            0,
            1,
            false,
            vec![
                (PRIMARY, Action::Thinking("api_error")),
                ("remote/model", Action::Text),
            ],
            1,
            1,
        ),
        (
            "bare-other-provider",
            "remote/model",
            0,
            1,
            false,
            vec![
                (PRIMARY, Action::Thinking("api_error")),
                ("remote/model", Action::Text),
            ],
            1,
            1,
        ),
        (
            "ordered-chain",
            "claude-sonnet-4-6,other/remote/model",
            0,
            1,
            false,
            vec![
                (PRIMARY, Action::Thinking("api_error")),
                (FALLBACK, Action::Thinking("timeout_error")),
                ("remote/model", Action::Text),
            ],
            2,
            1,
        ),
        (
            "same-model-no-retry",
            PRIMARY,
            0,
            1,
            false,
            vec![(PRIMARY, Action::Thinking("api_error"))],
            0,
            1,
        ),
        (
            "same-model-one-retry",
            PRIMARY,
            1,
            1,
            false,
            vec![
                (PRIMARY, Action::Thinking("api_error")),
                (PRIMARY, Action::Text),
            ],
            0,
            1,
        ),
        (
            "persistent-no-model-hop",
            FALLBACK,
            2,
            1,
            true,
            vec![
                (PRIMARY, Action::Thinking("api_error")),
                (PRIMARY, Action::Thinking("api_error")),
                (PRIMARY, Action::Thinking("api_error")),
            ],
            0,
            1,
        ),
        (
            "completed-output",
            FALLBACK,
            0,
            1,
            false,
            vec![(PRIMARY, Action::Partial("api_error"))],
            0,
            1,
        ),
        (
            "thinking-stop-received",
            FALLBACK,
            0,
            1,
            false,
            vec![(PRIMARY, Action::StopThinking("api_error"))],
            0,
            1,
        ),
    ];
    for (name, fallback, retries, max_turns, persistent, actions, notices, queries) in scenarios {
        variable(
            branding::DISABLE_NONSTREAMING_FALLBACK_ENV,
            Some(if name == "http-no-double-body-overload" {
                "0"
            } else {
                "1"
            }),
        );
        variable(
            branding::RETRY_WATCHDOG_ENV,
            Some(if persistent { "1" } else { "0" }),
        );
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("session.jsonl");
        let capture = Arc::new(Capture {
            requests: Mutex::new(vec![]),
            actions: Mutex::new(actions.into_iter().map(|(m, a)| (m.into(), a)).collect()),
        });
        let output = Arc::new(MockOutputStream::new());
        let mut orch = make(
            capture.clone(),
            output.clone(),
            fallback,
            retries,
            max_turns,
            &path,
            name == "http-chain-fast-rejection",
        );
        let (cost_tx, _cost_rx) = tokio::sync::mpsc::channel(16);
        let tracker = Arc::new(cost::CostTracker::new(
            orch.session().lock().await.session_id,
            Arc::new(cost::PricingCatalog::builtin_reference()),
            cost_tx,
        ));
        if name == "server" {
            orch = orch.with_cost_tracker(tracker.clone());
        }
        orch.session().lock().await.model_profile = Some("direct".into());
        for _ in 0..queries {
            let outcome = orch.run_turn_streaming("fixture").await;
            if name == "http-no-double-body-overload" {
                assert!(
                    matches!(
                        outcome,
                        Err(orchestrator::OrchestratorError::ApiCall(
                            llm_runtime::LlmError::Overloaded { .. }
                        ))
                    ),
                    "{outcome:?}"
                );
            } else if name == "http-529-early-budget-exhaustion" {
                assert!(
                    matches!(
                        outcome,
                        Err(orchestrator::OrchestratorError::Streaming(
                            llm_runtime::LlmError::Overloaded { .. }
                        ))
                    ),
                    "{outcome:?}"
                );
            } else if !persistent && name != "thinking-stop-received" {
                assert!(outcome.is_ok(), "{name}: {outcome:?}");
            }
        }
        if name == "server" {
            let cost = tracker.snapshot().await;
            assert_eq!(cost.per_model_usage.len(), 2);
            let primary = cost
                .per_model_usage
                .values()
                .find(|usage| usage.model_ref.model == PRIMARY)
                .unwrap();
            let fallback = cost
                .per_model_usage
                .values()
                .find(|usage| usage.model_ref.model == FALLBACK)
                .unwrap();
            assert_eq!(
                (primary.usage.tokens.input, primary.usage.tokens.output),
                (2, 1)
            );
            assert_eq!(
                (fallback.usage.tokens.input, fallback.usage.tokens.output),
                (2, 3)
            );
        }
        let remaining = capture.actions.lock().unwrap().len();
        assert_eq!(remaining, 0, "{name}");
        let requests = capture.requests.lock().unwrap().clone();
        if name == "http-chain-fast-rejection" {
            assert_eq!(requests.len(), 3);
            assert_eq!(requests[1].1["speed"], "fast");
            assert!(requests[2].1.get("speed").is_none());
            assert!(requests
                .iter()
                .all(|(_, body)| body["thinking"]["display"] == "updates"));
            assert!(
                lingxi_core::host::fast_mode::ModelRejections::for_process().blocked(FAST_FALLBACK)
            );
            lingxi_core::host::fast_mode::ModelRejections::for_process().reset();
        }
        for (url, body) in &requests {
            if body["model"] == "remote/model" {
                assert!(
                    url.starts_with("https://second.fixture.invalid"),
                    "{name}: {url}"
                );
            }
            if body["model"] != PRIMARY {
                assert!(
                    !body["messages"].to_string().contains("abandoned reasoning"),
                    "{name}"
                );
            }
            assert!(
                !body["messages"].to_string().contains("Switched to"),
                "notices stay outside model instructions"
            );
        }
        if name.starts_with("http-")
            || matches!(
                name,
                "server" | "timeout" | "fresh-budget" | "ordered-chain"
            )
        {
            for (_, body) in &requests {
                assert_eq!(
                    body["messages"], requests[0].1["messages"],
                    "{name}: preserve the original step context across model hops"
                );
            }
        }
        let live = orch.session();
        let live = live.lock().await;
        assert_eq!(live.model, PRIMARY, "{name}: user choice is preserved");
        assert_eq!(live.model_profile.as_deref(), Some("direct"));
        let hot: Vec<_> = live
            .history
            .iter()
            .filter(|m| {
                matches!(
                    m,
                    ConversationMessage::System {
                        model_fallback: Some(_),
                        ..
                    }
                )
            })
            .cloned()
            .collect();
        assert_eq!(hot.len(), notices, "{name}");
        let session_id = live.session_id.as_uuid();
        drop(live);
        let frames = output.model_fallback_frame_snapshot().await;
        assert_eq!(frames.len(), notices, "{name}");
        for frame in &frames {
            assert_eq!(
                frame
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                [
                    "type",
                    "subtype",
                    "uuid",
                    "trigger",
                    "original_model",
                    "fallback_model",
                    "content",
                    "session_id"
                ]
            );
        }
        assert!(
            !output
                .partial_stream_event_snapshot()
                .await
                .iter()
                .any(|frame| frame.contains("model_fallback")),
            "SDK notices are independent of provider partial frames"
        );

        let data = std::fs::read_to_string(&path).unwrap();
        let rows: Vec<JsonlMessage> = data
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let persisted: Vec<_> = rows
            .iter()
            .filter(|row| row.extra.get("subtype") == Some(&json!("model_fallback")))
            .collect();
        assert_eq!(persisted.len(), notices, "{name}");
        for ((row, frame), message) in persisted.iter().zip(&frames).zip(&hot) {
            assert_eq!(row.uuid, frame["uuid"].as_str().unwrap());
            assert_eq!(row.extra["content"], frame["content"]);
            assert_eq!(row.extra["originalModel"], frame["original_model"]);
            assert_eq!(row.extra["fallbackModel"], frame["fallback_model"]);
            assert_eq!(row.extra["level"], "warning");
            assert_eq!(row.extra["isMeta"], false);
            assert_eq!(
                row.extra["trigger"],
                if name.contains("529")
                    || name.contains("payload-overload")
                    || name == "http-shared-body-overload-count"
                {
                    "overloaded"
                } else {
                    "server_error"
                }
            );
            assert!(row.message.is_null());
            assert_eq!(message.id().as_uuid().to_string(), row.uuid);
            let raw = data.lines().find(|l| l.contains(&row.uuid)).unwrap();
            let keys = [
                "parentUuid",
                "isSidechain",
                "type",
                "subtype",
                "content",
                "level",
                "trigger",
                "originalModel",
                "fallbackModel",
                "isMeta",
                "uuid",
                "timestamp",
            ];
            let mut last = 0;
            for key in keys {
                let index = raw.find(&format!("\"{key}\":")).unwrap();
                assert!(index >= last, "{name}: {raw}");
                last = index;
            }
        }
        if name == "server"
            || name == "timeout"
            || name == "fresh-budget"
            || name == "tool-and-next-query"
        {
            assert_eq!(frames[0]["content"],"Switched to Sonnet 4.6 due to high demand for Opus 4.7 · context window 1M → 200K tokens");
        }
        if name == "tool-and-next-query" {
            assert!(requests[2].1["messages"]
                .to_string()
                .contains("tool_result"));
        }
        let cold = state_from_messages(session_id, &rows);
        let cold_notices: Vec<_> = cold
            .history
            .iter()
            .filter(|m| {
                matches!(
                    m,
                    ConversationMessage::System {
                        model_fallback: Some(_),
                        ..
                    }
                )
            })
            .cloned()
            .collect();
        assert_eq!(hot, cold_notices, "{name}: typed metadata survives resume");
    }
    variable(branding::DISABLE_NONSTREAMING_FALLBACK_ENV, None);
    variable(branding::RETRY_WATCHDOG_ENV, None);
}
