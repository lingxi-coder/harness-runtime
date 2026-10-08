//! Shared physical Messages fixture for display and named-beta regressions.
#![allow(dead_code)]
use async_trait::async_trait;
use futures::StreamExt;
use lingxi_llm_client::protocol::{ThinkingConfig, ThinkingMode};
use llm_runtime::model::user_agent::UserAgentEnv;
use llm_runtime::{
    ApiService, ClientConfig, LlmRequest, ModelRuntime, NonStreamingRequestClass,
    NonStreamingRetryOptions, SubscriberState, Transport,
};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
pub(crate) const MODEL: &str = "claude-opus-4-7";
pub(crate) const BETA: &str = "thinking-display-updates-2026-08-18";
#[derive(Clone, Copy)]
pub(crate) enum Action {
    Error(u16, &'static str),
    Rejection(&'static str),
    RejectionStatus(u16, &'static str),
    PlainRejection(&'static str),
    TopLevelRejection(&'static str),
    EmptySuccess,
    Success,
}
type CapturedRequest = (Value, Vec<(String, String)>);
pub(crate) struct Capture {
    pub(crate) actions: Mutex<VecDeque<Action>>,
    pub(crate) requests: Mutex<Vec<CapturedRequest>>,
}
#[async_trait]
impl Transport for Capture {
    async fn send(
        &self,
        request: lingxi_llm_client::HttpRequest,
    ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError> {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        self.requests
            .lock()
            .unwrap()
            .push((body.clone(), request.headers));
        let action = self
            .actions
            .lock()
            .unwrap()
            .pop_front()
            .expect("no extra dispatch");
        if let Action::PlainRejection(message) | Action::TopLevelRejection(message) = action {
            return Ok(lingxi_llm_client::HttpResponse {
                status: 400,
                headers: vec![("x-should-retry".into(), "false".into())],
                body: if matches!(action, Action::PlainRejection(_)) {
                    message.as_bytes().to_vec().into()
                } else {
                    serde_json::to_vec(&json!({"message":message}))
                        .unwrap()
                        .into()
                },
            }
            .into());
        }
        let rejection = match action {
            Action::Error(status, kind) => Some((
                status,
                kind,
                if kind == "invalid_request_error" {
                    "fixture unknown request rejection"
                } else {
                    "fixture"
                },
            )),
            Action::Rejection(message) => Some((400, "invalid_request_error", message)),
            Action::RejectionStatus(status, message) => {
                Some((status, "invalid_request_error", message))
            }
            _ => None,
        };
        if let Some((status, kind, message)) = rejection {
            return Ok(lingxi_llm_client::HttpResponse {
                status,
                headers: vec![("x-should-retry".into(), "false".into())],
                body: serde_json::to_vec(
                    &json!({"type":"error","error":{"type":kind,"message":message}}),
                )
                .unwrap()
                .into(),
            }
            .into());
        }
        if matches!(action, Action::EmptySuccess) {
            return Ok(lingxi_llm_client::HttpResponse {
                status: 200,
                headers: vec![],
                body: Vec::<u8>::new().into(),
            }
            .into());
        }
        let response = json!({"id":"msg_fixture","type":"message","role":"assistant","model":body["model"],"content":[{"type":"text","text":"success"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}});
        let bytes = if body["stream"] == true {
            [json!({"type":"message_start","message":{"id":"msg_fixture","type":"message","role":"assistant","model":body["model"],"content":[],"usage":{"input_tokens":1,"output_tokens":0}}}),json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"success"}}),json!({"type":"content_block_stop","index":0}),json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}),json!({"type":"message_stop"})].iter().map(|frame|format!("data: {frame}\n\n")).collect::<String>().into_bytes()
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
pub(crate) fn variable(name: &str, value: Option<&str>) {
    if let Some(value) = value {
        std::env::set_var(name, value)
    } else {
        std::env::remove_var(name)
    }
}
pub(crate) fn service(
    capture: Arc<Capture>,
    official: bool,
    custom: bool,
    model: &str,
) -> ApiService {
    let provider = if custom {
        json!({"custom":{"name":"other"}})
    } else {
        json!("anthropic_first_party")
    };
    let cfg:ClientConfig=serde_json::from_value(json!({"providers":[{"provider_id":provider,"profile_name":"direct","base_url":if official {"https://api.anthropic.com"}else{"https://gateway.fixture.invalid"},"protocol":"anthropic_messages","auth":"none","credential":{"type":"none"},"models":[{"display_model":model,"request_model":model,"billing_model":model,"capabilities":{"streaming":true,"tools":true,"vision":false,"documents":false,"reasoning":true,"structured_output":false}}]}]})).unwrap();
    ApiService::new_with_routing(
        Arc::new(ModelRuntime::from_config(cfg).unwrap()),
        capture,
        SubscriberState::default(),
        UserAgentEnv::default(),
        "fixture",
        None,
        None,
        None,
        Default::default(),
        Some(0),
        None,
    )
}
pub(crate) fn request(
    scope: &llm_runtime::thinking_scope::ThinkingRecoveryScope,
    model: &str,
    side: bool,
) -> LlmRequest {
    let mut request = LlmRequest::new(model).with_user_text("fixture");
    request.input.thinking = Some(ThinkingConfig {
        mode: Some(if model == "claude-haiku-4-5" {
            ThinkingMode::Enabled
        } else {
            ThinkingMode::Adaptive
        }),
        budget: (model == "claude-haiku-4-5")
            .then_some(lingxi_llm_client::protocol::ThinkingBudget::Tokens(1024)),
        ..Default::default()
    });
    request.execution.thinking_recovery_scope = Some(scope.clone());
    if side {
        request.execution.anthropic_request_kind=lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::SideQuery;
    }
    request
}
pub(crate) async fn run(api: &ApiService, request: LlmRequest, streaming: bool) -> bool {
    if streaming {
        match api.stream_request(request).await {
            Ok(stream) => stream.collect::<Vec<_>>().await.iter().all(Result::is_ok),
            Err(error) => {
                eprintln!("stream open: {error:?}");
                false
            }
        }
    } else {
        match api
            .execute_non_stream_request(
                request,
                NonStreamingRequestClass::Main,
                NonStreamingRetryOptions::default(),
            )
            .await
        {
            Ok(_) => true,
            Err(error) => {
                eprintln!("nonstream: {error:?}");
                false
            }
        }
    }
}
pub(crate) fn captures(actions: Vec<Action>) -> Arc<Capture> {
    Arc::new(Capture {
        actions: Mutex::new(actions.into()),
        requests: Mutex::new(vec![]),
    })
}
pub(crate) fn has_beta(headers: &[(String, String)]) -> bool {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
        .is_some_and(|(_, value)| value.split(',').any(|token| token == BETA))
}
