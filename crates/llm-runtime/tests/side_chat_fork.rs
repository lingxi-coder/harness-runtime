//! Independent side API state verified through public request/control methods.
use async_trait::async_trait;
use lingxi_core::host::effort_table::SessionEffort;
use llm_runtime::model::{thinking::ThinkingConfig, user_agent::UserAgentEnv};
use llm_runtime::{
    ApiService, ClientConfig, MessagesCreateRequest, ModelRuntime, SubscriberState, Transport,
};
use serde_json::json;
use std::sync::{Arc, Mutex};
const MODEL: &str = "claude-opus-4-7";
struct Fixture(Mutex<usize>);
#[async_trait]
impl Transport for Fixture {
    async fn send(
        &self,
        _: lingxi_llm_client::HttpRequest,
    ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError> {
        let index = {
            let mut calls = self.0.lock().unwrap();
            let index = *calls;
            *calls += 1;
            index
        };
        let response = if index == 0 {
            lingxi_llm_client::HttpResponse {
                status: 500,
                headers: vec![("x-should-retry".into(), "true".into())],
                body: serde_json::to_vec(
                    &json!({"type":"error","error":{"type":"api_error","message":"fixture retry"}}),
                )
                .unwrap()
                .into(),
            }
        } else {
            lingxi_llm_client::HttpResponse{status:200,headers:vec![("request-id".into(),format!("request-{index}"))],body:serde_json::to_vec(&json!({"id":format!("msg-{index}"),"type":"message","role":"assistant","model":MODEL,"content":[{"type":"text","text":"private side response"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}})).unwrap().into()}
        };
        Ok(response.into())
    }
}
#[tokio::test(start_paused = true)]
async fn side_fork_owns_request_retry_effort_and_thinking_state() {
    let config:ClientConfig=serde_json::from_value(json!({"providers":[{"provider_id":"anthropic_first_party","profile_name":"ready","base_url":"https://fixture.invalid","protocol":"anthropic_messages","auth":"none","credential":{"type":"none"},"models":[{"display_model":MODEL,"request_model":MODEL,"billing_model":MODEL,"capabilities":{"streaming":true,"tools":true,"vision":false,"documents":false,"reasoning":true,"structured_output":false}}]}]})).unwrap();
    let transport = Arc::new(Fixture(Mutex::new(0)));
    let parent = ApiService::new_with_routing(
        Arc::new(ModelRuntime::from_config(config).unwrap()),
        transport.clone(),
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
    .with_thinking(ThinkingConfig::Adaptive);
    parent.set_session_effort(SessionEffort::Level(json!("high")));
    parent
        .messages_create(MessagesCreateRequest::new(
            MODEL,
            None,
            None,
            vec![],
            vec![],
        ))
        .await
        .unwrap();
    let parent_id = parent.last_request_id().unwrap();
    assert_eq!(parent.last_retry_count(), 1);
    let child = parent.fork_independent();
    assert_eq!(child.last_retry_count(), 0);
    assert!(child.last_request_id().is_none());
    assert!(matches!(
        child
            .effort_command_snapshot(MODEL, None)
            .unwrap()
            .unwrap()
            .session,
        SessionEffort::Inherit
    ));
    child.set_session_effort(SessionEffort::Level(json!("low")));
    child.set_thinking(ThinkingConfig::Disabled);
    child
        .messages_create(MessagesCreateRequest::new(
            MODEL,
            None,
            None,
            vec![],
            vec![],
        ))
        .await
        .unwrap();
    assert_ne!(child.last_request_id().unwrap(), parent_id);
    assert_eq!(parent.last_request_id().unwrap(), parent_id);
    assert_eq!(parent.last_retry_count(), 1);
    assert_eq!(child.last_retry_count(), 0);
    assert_eq!(parent.thinking().unwrap(), ThinkingConfig::Adaptive);
    assert_eq!(child.thinking().unwrap(), ThinkingConfig::Disabled);
    assert!(
        matches!(parent.effort_command_snapshot(MODEL,None).unwrap().unwrap().session,SessionEffort::Level(value) if value==json!("high"))
    );
    assert_eq!(*transport.0.lock().unwrap(), 3);
}
