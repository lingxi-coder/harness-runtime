//! Current retry-decline priority reaches both physical SDK request modes.
use async_trait::async_trait;
use futures::StreamExt;
use llm_runtime::model::{thinking::ThinkingConfig, user_agent::UserAgentEnv};
use llm_runtime::{ApiService, ClientConfig, LlmRequest, ModelRuntime, SubscriberState, Transport};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

const MODEL: &str = "claude-sonnet-4-6";
struct Capture {
    status: u16,
    requests: Mutex<Vec<(Value, Vec<(String, String)>)>>,
}
#[async_trait]
impl Transport for Capture {
    async fn send(
        &self,
        request: lingxi_llm_client::HttpRequest,
    ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError> {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let index = {
            let mut requests = self.requests.lock().unwrap();
            let index = requests.len();
            requests.push((body.clone(), request.headers));
            index
        };
        if index == 0 || self.status == 503 && index == 1 {
            return Ok(lingxi_llm_client::HttpResponse {
                status: self.status,
                headers: vec![("x-should-retry".into(), "false".into())],
                body: serde_json::to_vec(&json!({"type":"error","error":{"type":match self.status {429=>"rate_limit_error",529=>"overloaded_error",_=>"api_error"},"message":"fixture declined"}})).unwrap().into(),
            }.into());
        }
        let response = json!({"id":"msg_success","type":"message","role":"assistant","model":MODEL,"content":[{"type":"text","text":"success"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}});
        let bytes = if body["stream"] == true {
            [
                json!({"type":"message_start","message":{"id":"msg_success","model":MODEL,"role":"assistant","content":[],"usage":{"input_tokens":1,"output_tokens":0}}}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"success"}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}),
                json!({"type":"message_stop"}),
            ].iter().map(|frame| format!("data: {frame}\n\n")).collect::<String>().into_bytes()
        } else {
            serde_json::to_vec(&response).unwrap()
        };
        Ok(lingxi_llm_client::HttpResponse {
            status: 200,
            headers: vec![],
            body: bytes.into(),
        }
        .into())
    }
}

#[tokio::test(start_paused = true)]
async fn current_capacity_acceptance_precedes_decline_in_stream_and_nonstream_calls() {
    std::env::remove_var(branding::MAX_RETRIES_ENV);
    std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");
    for streaming in [false, true] {
        for persistent in [false, true] {
            std::env::set_var(
                branding::RETRY_WATCHDOG_ENV,
                if persistent { "1" } else { "0" },
            );
            for status in [429, 529, 500, 503] {
                let capture = Arc::new(Capture {
                    status,
                    requests: Mutex::new(vec![]),
                });
                let cfg: ClientConfig = serde_json::from_value(json!({"providers":[{"provider_id":"anthropic_first_party","profile_name":"direct","base_url":"https://api.anthropic.com","protocol":"anthropic_messages","auth":"none","credential":{"type":"none"},"models":[{"display_model":MODEL,"request_model":MODEL,"billing_model":MODEL,"capabilities":{"streaming":true,"tools":true,"vision":false,"documents":false,"reasoning":false,"structured_output":false}}]}]})).unwrap();
                let api = ApiService::new_with_routing(
                    Arc::new(ModelRuntime::from_config(cfg).unwrap()),
                    capture.clone(),
                    SubscriberState::default(),
                    UserAgentEnv::default(),
                    "fixture",
                    None,
                    None,
                    None,
                    Default::default(),
                    Some(1),
                    None,
                )
                .with_thinking(ThinkingConfig::Disabled);
                let request = LlmRequest::new(MODEL).with_user_text("fixture");
                let succeeded = if streaming {
                    match api.stream_request(request).await {
                        Ok(stream) => stream.collect::<Vec<_>>().await.iter().all(Result::is_ok),
                        Err(_) => false,
                    }
                } else {
                    api.execute_side_query_request(request).await.is_ok()
                };
                let accepted = status == 529 || persistent && status == 429;
                assert_eq!(
                    succeeded, accepted,
                    "status={status}, streaming={streaming}, persistent={persistent}"
                );
                let requests = capture.requests.lock().unwrap();
                assert_eq!(
                    requests.len(),
                    if accepted || status == 503 { 2 } else { 1 }
                );
                if status == 503 {
                    assert!(
                        requests[1]
                            .1
                            .iter()
                            .any(|(name, value)| name == "anthropic-dispatch-id" && value == "v2p"),
                        "the native headerless repair precedes the terminal decline"
                    );
                }
                assert!(requests
                    .iter()
                    .all(|(body, _)| (body["stream"] == true) == streaming));
                assert!(requests
                    .iter()
                    .all(|(body, _)| body["messages"] == requests[0].0["messages"]));
            }
        }
    }
    std::env::remove_var(branding::RETRY_WATCHDOG_ENV);
}
