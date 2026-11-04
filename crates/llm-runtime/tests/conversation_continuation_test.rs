//! Host admission, history ownership and recovery through the real SDK path.
use async_trait::async_trait;
use futures::StreamExt;
use lingxi_llm_client::{protocol as wire, HttpRequest, HttpResponse, StreamResponse};
use llm_runtime::{
    model::{thinking::ThinkingConfig, user_agent::UserAgentEnv},
    ApiAttemptsLeft, ApiService, ClientConfig, LlmRequest, ModelRuntime, NonStreamingRequestClass,
    NonStreamingRetryOptions, SubscriberState, Transport,
};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct Script {
    requests: Mutex<Vec<Value>>,
    replies: Mutex<VecDeque<(u16, Vec<u8>)>>,
}
impl Script {
    fn reply(&self, status: u16, value: Value) {
        self.raw(status, serde_json::to_vec(&value).unwrap());
    }
    fn raw(&self, status: u16, bytes: Vec<u8>) {
        self.replies.lock().unwrap().push_back((status, bytes));
    }
    fn bodies(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
    fn success(&self, family: &str, id: &str, streaming: bool) {
        let body = if family == "anthropic_messages" {
            json!({"id":id,"type":"message","role":"assistant","model":"m","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":5,"output_tokens":2}})
        } else {
            json!({"id":id,"model":"m","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok","annotations":[]}]}],"usage":{"input_tokens":5,"output_tokens":2,"total_tokens":7}})
        };
        if !streaming {
            self.reply(200, body);
            return;
        }
        let events = if family == "anthropic_messages" {
            vec![
                json!({"type":"message_start","message":{"id":id,"type":"message","role":"assistant","model":"m","content":[],"usage":{"input_tokens":5,"output_tokens":0}}}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
                json!({"type":"message_stop"}),
            ]
        } else {
            vec![
                json!({"type":"response.created","response":{"id":id,"model":"m"}}),
                json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"ok"}),
                json!({"type":"response.completed","response":body}),
            ]
        };
        self.raw(200, sse(&events));
    }
}
fn sse(events: &[Value]) -> Vec<u8> {
    events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>()
        .into_bytes()
}
#[async_trait]
impl Transport for Script {
    async fn send(&self, request: HttpRequest) -> Result<StreamResponse, wire::LlmError> {
        self.requests
            .lock()
            .unwrap()
            .push(serde_json::from_slice(&request.body).unwrap());
        let (status, bytes) = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected physical dispatch");
        Ok(HttpResponse {
            status,
            headers: vec![],
            body: bytes.into(),
        }
        .into())
    }
}
fn service(script: &Arc<Script>, family: &str) -> ApiService {
    let (provider, host_provider, endpoint, extra) = if family == "anthropic_messages" {
        (
            "anthropic",
            "anthropic_first_party",
            "https://api.anthropic.com",
            json!({"supports_message_threads":true}),
        )
    } else {
        (
            "openai",
            "open_ai",
            "https://api.openai.com/v1",
            json!({"supports_previous_response_id":true}),
        )
    };
    let mut config: ClientConfig = serde_json::from_value(json!({"providers":[{"provider_id":host_provider,"profile_name":"one","base_url":endpoint,"protocol":family,"auth":"none","credential":{"type":"none"},"models":[{"display_model":"m","request_model":"m","billing_model":"m","capabilities":{"streaming":true,"tools":true,"reasoning":false,"vision":false,"documents":false,"structured_output":false}}]}]})).unwrap();
    config.providers[0].wire_profile = Some(serde_json::from_value(json!({"provider_id":provider,"profile_name":"one","base_url":endpoint,"protocol":family,"auth":"none","extra":extra,"models":[{"display_model":"m","request_model":"m","billing_model":"m"}]})).unwrap());
    ApiService::new_with_routing(
        Arc::new(ModelRuntime::from_config(config).unwrap()),
        script.clone(),
        SubscriberState::default(),
        UserAgentEnv::default(),
        "test",
        None,
        None,
        None,
        Default::default(),
        Some(0),
        None,
    )
    .with_thinking(ThinkingConfig::Disabled)
}
fn request() -> LlmRequest {
    let mut request = LlmRequest::new("m").with_user_text("remember");
    request.execution.account_scope = Some("trusted-account-a".into());
    request.execution.query_source = Some("sdk".into());
    request.input.controls.responses.store = Some(true);
    request.input.max_tokens = Some(256);
    request
}
fn followup(request: &mut LlmRequest) {
    request.input.messages.push(
        serde_json::from_value(json!({"role":"assistant","content":[{"type":"text","text":"ok"}]}))
            .unwrap(),
    );
    request
        .input
        .messages
        .push(wire::ConversationMessage::user_text("next"));
}
async fn drain(api: &ApiService, request: LlmRequest) {
    let mut stream = api.stream_request(request).await.unwrap();
    while let Some(event) = stream.next().await {
        event.unwrap();
    }
}
fn input_count(body: &Value, family: &str) -> usize {
    body[if family == "anthropic_messages" {
        "messages"
    } else {
        "input"
    }]
    .as_array()
    .unwrap()
    .len()
}
#[tokio::test]
async fn main_http_turns_continue_and_auxiliary_sidequeries_never_inherit_or_evict_them() {
    for family in ["anthropic_messages", "open_ai_responses"] {
        let script = Arc::new(Script::default());
        let api = service(&script, family);
        let mut req = request();
        if family == "anthropic_messages" {
            req.input.controls.responses.store = None;
        }
        script.success(family, "main-first", true);
        drain(&api, req.clone()).await;
        followup(&mut req);
        for source in ["btw", "compact", "sidequery", "agent:classifier"] {
            let mut side = req.clone();
            side.execution.query_source = Some(source.into());
            script.success(family, source, true);
            drain(&api, side).await;
        }
        script.success(family, "main-next", true);
        drain(&api, req).await;
        let sent = script.bodies();
        for body in &sent[1..5] {
            assert_eq!(input_count(body, family), 3);
            assert!(body.get("previous_response_id").is_none());
            assert_ne!(body["thread"]["type"], "continue");
        }
        assert_eq!(input_count(&sent[5], family), 1, "{family}");
    }
}
#[tokio::test]
async fn side_chat_forks_have_independent_multi_turn_chains() {
    let family = "open_ai_responses";
    let script = Arc::new(Script::default());
    let api = service(&script, family);
    let mut req = request();
    script.success(family, "parent", true);
    drain(&api, req.clone()).await;
    followup(&mut req);
    let side_chat = api.fork_independent();
    script.success(family, "side-first", true);
    drain(&side_chat, req.clone()).await;
    let mut side_req = req.clone();
    followup(&mut side_req);
    script.success(family, "side-next", true);
    drain(&side_chat, side_req).await;
    script.success(family, "parent-next", true);
    drain(&api, req).await;
    let sent = script.bodies();
    assert!(sent[1].get("previous_response_id").is_none());
    assert_eq!(sent[2]["previous_response_id"], "side-first");
    assert_eq!(sent[3]["previous_response_id"], "parent");
}
#[tokio::test]
async fn compact_reset_prevents_an_old_stream_lease_from_reinstalling_history() {
    let family = "open_ai_responses";
    let script = Arc::new(Script::default());
    let api = service(&script, family);
    script.success(family, "old", true);
    let mut old = api.stream_request(request()).await.unwrap();
    api.reset_conversation_context();
    let mut req = request();
    req.input.messages = vec![wire::ConversationMessage::user_text("compact summary")];
    script.success(family, "compacted", true);
    drain(&api, req.clone()).await;
    while let Some(event) = old.next().await {
        event.unwrap();
    }
    drop(old);
    followup(&mut req);
    script.success(family, "next", true);
    drain(&api, req).await;
    let sent = script.bodies();
    assert!(sent[1].get("previous_response_id").is_none());
    assert_eq!(sent[2]["previous_response_id"], "compacted");
}
#[tokio::test]
async fn both_http_and_first_event_rejections_replay_once_with_fresh_physical_budget() {
    for event_error in [false, true] {
        let family = "open_ai_responses";
        let script = Arc::new(Script::default());
        let api = service(&script, family);
        let mut req = request();
        script.success(family, "first", true);
        drain(&api, req.clone()).await;
        followup(&mut req);
        if event_error {
            script.raw(200,sse(&[json!({"type":"error","status":400,"error":{"code":"previous_response_not_found","message":"expired"}})]));
        } else {
            script.reply(
                400,
                json!({"error":{"code":"previous_response_not_found","message":"expired"}}),
            );
        }
        script.success(family, "recovered", true);
        let budget = ApiAttemptsLeft::new(2);
        req.execution.api_attempts_left = Some(budget.clone());
        drain(&api, req).await;
        let sent = script.bodies();
        assert_eq!(sent.len(), 3);
        assert_eq!(budget.remaining(), 0);
        assert_eq!(input_count(&sent[1], family), 1);
        assert_eq!(input_count(&sent[2], family), 3);
        assert!(sent[2].get("previous_response_id").is_none());
    }
}
#[tokio::test]
async fn expired_reference_cannot_bypass_a_rejected_recovery_budget() {
    let family = "open_ai_responses";
    let script = Arc::new(Script::default());
    let api = service(&script, family);
    let mut req = request();
    script.success(family, "first", true);
    drain(&api, req.clone()).await;
    followup(&mut req);
    script.reply(
        400,
        json!({"error":{"code":"previous_response_not_found","message":"expired"}}),
    );
    req.execution.api_attempts_left = Some(ApiAttemptsLeft::new(1));
    assert!(api.stream_request(req).await.is_err());
    assert_eq!(script.bodies().len(), 2);
}
#[tokio::test]
async fn nonstream_turns_continue_with_one_full_history_recovery() {
    let family = "open_ai_responses";
    let script = Arc::new(Script::default());
    let api = service(&script, family);
    let mut req = request();
    script.success(family, "first", false);
    api.execute_non_stream_request(
        req.clone(),
        NonStreamingRequestClass::Main,
        NonStreamingRetryOptions::default(),
    )
    .await
    .unwrap();
    followup(&mut req);
    script.reply(
        400,
        json!({"error":{"code":"previous_response_not_found","message":"expired"}}),
    );
    script.success(family, "recovered", false);
    api.execute_non_stream_request(
        req,
        NonStreamingRequestClass::Main,
        NonStreamingRetryOptions::default(),
    )
    .await
    .unwrap();
    let sent = script.bodies();
    assert_eq!(sent.len(), 3);
    assert_eq!(input_count(&sent[1], family), 1);
    assert_eq!(input_count(&sent[2], family), 3);
}
#[tokio::test]
async fn store_false_account_changes_and_history_edits_send_full_history() {
    for change in ["store", "account", "edit"] {
        let family = "open_ai_responses";
        let script = Arc::new(Script::default());
        let api = service(&script, family);
        let mut req = request();
        script.success(family, "first", true);
        drain(&api, req.clone()).await;
        followup(&mut req);
        match change {
            "store" => req.input.controls.responses.store = Some(false),
            "account" => req.execution.account_scope = Some("trusted-account-b".into()),
            _ => req.input.messages[0] = wire::ConversationMessage::user_text("edited"),
        }
        script.success(family, "next", true);
        drain(&api, req).await;
        assert!(
            script.bodies()[1].get("previous_response_id").is_none(),
            "{change}"
        );
        assert_eq!(input_count(&script.bodies()[1], family), 3);
    }
}

#[tokio::test]
async fn sidequery_while_main_stream_is_open_neither_blocks_nor_replaces_its_chain() {
    let family = "open_ai_responses";
    let script = Arc::new(Script::default());
    let api = service(&script, family);
    script.success(family, "main", true);
    let mut main = api.stream_request(request()).await.unwrap();
    let mut side = request();
    side.execution.query_source = Some("sidequery".into());
    script.success(family, "side", true);
    tokio::time::timeout(std::time::Duration::from_secs(2), drain(&api, side))
        .await
        .unwrap();
    while let Some(event) = main.next().await {
        event.unwrap();
    }
    drop(main);
    let mut next = request();
    followup(&mut next);
    script.success(family, "next", true);
    drain(&api, next).await;
    assert_eq!(script.bodies()[2]["previous_response_id"], "main");
}

#[tokio::test]
async fn tool_result_delta_and_full_recovery_preserve_exact_arguments() {
    let family = "open_ai_responses";
    let script = Arc::new(Script::default());
    let api = service(&script, family);
    let mut req = request();
    req.input.tools=serde_json::from_value(json!([{"name":"lookup","description":"lookup","input_schema":{"type":"object","properties":{"q":{"type":"integer"}}}}])).unwrap();
    let response = json!({"id":"tool-response","model":"m","status":"completed","output":[{"type":"function_call","id":"fc1","call_id":"call1","name":"lookup","arguments":"{\"q\":9007199254740993}","status":"completed"}],"usage":{"input_tokens":5,"output_tokens":2,"total_tokens":7}});
    script.raw(
        200,
        sse(&[
            json!({"type":"response.created","response":{"id":"tool-response","model":"m"}}),
            json!({"type":"response.completed","response":response}),
        ]),
    );
    drain(&api, req.clone()).await;
    req.input.messages.push(serde_json::from_value(json!({"role":"assistant","content":[{"type":"tool_use","id":"call1","name":"lookup","input":{"q":9007199254740993u64}}]})).unwrap());
    req.input.messages.push(serde_json::from_value(json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"call1","content":"observed result","is_error":false}]})).unwrap());
    script.reply(
        400,
        json!({"error":{"code":"previous_response_not_found","message":"expired"}}),
    );
    script.success(family, "next", true);
    drain(&api, req).await;
    let sent = script.bodies();
    assert_eq!(input_count(&sent[1], family), 1);
    assert_eq!(sent[1]["input"][0]["type"], "function_call_output");
    assert_eq!(input_count(&sent[2], family), 3);
    assert!(sent[2].to_string().contains("9007199254740993"));
}

#[tokio::test]
async fn auxiliary_ownership_wins_over_a_copied_main_query_source() {
    let family = "open_ai_responses";
    let script = Arc::new(Script::default());
    let api = service(&script, family);
    let mut req = request();
    script.success(family, "main", true);
    drain(&api, req.clone()).await;
    followup(&mut req);
    let mut side = req.clone();
    side.execution.isolated_query = true;
    script.success(family, "auxiliary", true);
    drain(&api, side).await;
    script.success(family, "next", true);
    drain(&api, req).await;
    let sent = script.bodies();
    assert!(sent[1].get("previous_response_id").is_none());
    assert_eq!(sent[2]["previous_response_id"], "main");
}

#[tokio::test]
async fn a_different_rejection_during_recreate_does_not_start_a_second_recovery() {
    let family = "anthropic_messages";
    let script = Arc::new(Script::default());
    let api = service(&script, family);
    let mut req = request();
    req.input.controls.responses.store = None;
    script.success(family, "first", true);
    drain(&api, req.clone()).await;
    followup(&mut req);
    script.reply(404, json!({"error":{"type":"not_found_error","message":"expired","details":{"error_code":"thread_not_found"}}}));
    script.reply(400, json!({"error":{"type":"invalid_request_error","message":"unsupported","details":{"error_code":"thread_unsupported_request"}}}));
    assert!(api.stream_request(req).await.is_err());
    let sent = script.bodies();
    assert_eq!(sent.len(), 3);
    assert_eq!(sent[1]["thread"]["type"], "continue");
    assert_eq!(sent[2]["thread"]["type"], "create");
}
