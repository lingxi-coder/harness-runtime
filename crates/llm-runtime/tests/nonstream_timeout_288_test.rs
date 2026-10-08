//! Native deadline selection reaches real SDK requests and response collection.
use async_trait::async_trait;
use llm_runtime::model::{thinking::ThinkingConfig, user_agent::UserAgentEnv};
use llm_runtime::{
    ApiService, ClientConfig, LlmError, LlmRequest, ModelRuntime, SubscriberState, Transport,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Capture {
    requests: Mutex<Vec<lingxi_llm_client::HttpRequest>>,
    body: bool,
    switch_timeout: bool,
}
#[async_trait]
impl Transport for Capture {
    async fn send(
        &self,
        request: lingxi_llm_client::HttpRequest,
    ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError> {
        let index = {
            let mut requests = self.requests.lock().unwrap();
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert_ne!(
                body["stream"], true,
                "deadline test must use nonstream execution"
            );
            let index = requests.len();
            requests.push(request);
            index
        };
        if self.switch_timeout {
            if index == 0 {
                std::env::set_var("API_TIMEOUT_MS", "1");
                return Ok(lingxi_llm_client::HttpResponse {
                    status: 500,
                    headers: vec![],
                    body: br#"{"error":{"type":"api_error","message":"fixture"}}"#
                        .to_vec()
                        .into(),
                }
                .into());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            return Ok(lingxi_llm_client::HttpResponse {
                status: 200,
                headers: vec![],
                body: json!({"id":"msg_fixture","type":"message","role":"assistant",
                    "model":"claude-sonnet-4-6","content":[{"type":"text","text":"ok"}],
                    "stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}})
                .to_string()
                .into_bytes()
                .into(),
            }
            .into());
        }
        if !self.body {
            return std::future::pending().await;
        }
        use futures::StreamExt;
        Ok(lingxi_llm_client::StreamResponse {
            status: 200,
            headers: vec![],
            body: futures::stream::pending().boxed(),
        })
    }
}
fn service(capture: Arc<Capture>, retries: u32) -> ApiService {
    let cfg: ClientConfig = serde_json::from_value(json!({"providers":[{
        "provider_id":"anthropic_first_party","profile_name":"direct",
        "base_url":"https://api.anthropic.com","protocol":"anthropic_messages",
        "auth":"none","credential":{"type":"none"},"models":[{
            "display_model":"claude-sonnet-4-6","request_model":"claude-sonnet-4-6",
            "billing_model":"claude-sonnet-4-6","capabilities":{"streaming":true,
                "tools":true,"vision":false,"documents":false,"reasoning":false,
                "structured_output":false}}]}]}))
    .unwrap();
    ApiService::new_with_routing(
        Arc::new(ModelRuntime::from_config(cfg).unwrap()),
        capture,
        SubscriberState::default(),
        UserAgentEnv::default(),
        "test",
        None,
        None,
        None,
        Default::default(),
        Some(retries),
        None,
    )
    .with_thinking(ThinkingConfig::Disabled)
}
fn variable(name: &str, value: Option<&str>) {
    if let Some(value) = value {
        std::env::set_var(name, value);
    } else {
        std::env::remove_var(name);
    }
}
fn request() -> LlmRequest {
    LlmRequest::new("claude-sonnet-4-6").with_user_text("fixture")
}

#[tokio::test(start_paused = true)]
async fn current_nonstream_deadlines_bound_sdk_headers_and_body_and_validate_before_dispatch() {
    variable(branding::MAX_RETRIES_ENV, None);
    variable(branding::RETRY_WATCHDOG_ENV, None);
    for (api_timeout, remote, millis) in [
        (None, None, 300_000),
        (None, Some("true"), 120_000),
        (Some("0"), Some("yes"), 120_000),
        (Some("abc"), Some("false"), 300_000),
        (Some("1e3"), None, 1000),
        (Some("1_000"), Some("true"), 1000),
        (Some("\u{feff}1e3\u{feff}"), None, 1000),
        (Some("1000ms"), None, 1000),
        (Some("2147483648"), None, i32::MAX as u64),
        (None, Some("\u{feff}true\u{feff}"), 120_000),
        (None, Some("\u{0085}true\u{0085}"), 300_000),
    ] {
        variable("API_TIMEOUT_MS", api_timeout);
        variable(branding::REMOTE_ENV, remote);
        for body in [false, true] {
            let capture = Arc::new(Capture {
                requests: Mutex::new(vec![]),
                body,
                switch_timeout: false,
            });
            let api = service(capture.clone(), 0);
            let started = tokio::time::Instant::now();
            let result = api.execute_side_query_request(request()).await;
            assert!(
                matches!(result, Err(LlmError::TransportTimeout { .. })),
                "{result:?}"
            );
            assert_eq!(started.elapsed(), Duration::from_millis(millis));
            assert_eq!(capture.requests.lock().unwrap().len(), 1);
        }
    }
    for value in ["-1", "-1_000", "-1e3", &format!("-1{}", "0".repeat(400))] {
        variable("API_TIMEOUT_MS", Some(value));
        let capture = Arc::new(Capture {
            requests: Mutex::new(vec![]),
            body: false,
            switch_timeout: false,
        });
        let result = service(capture.clone(), 5)
            .execute_side_query_request(request())
            .await;
        assert!(
            matches!(result, Err(LlmError::InvalidRequest { .. })),
            "{result:?}"
        );
        assert!(capture.requests.lock().unwrap().is_empty());
    }
    variable("API_TIMEOUT_MS", Some("50"));
    variable(branding::REMOTE_ENV, None);
    let capture = Arc::new(Capture {
        requests: Mutex::new(vec![]),
        body: false,
        switch_timeout: true,
    });
    service(capture.clone(), 1)
        .execute_side_query_request(request())
        .await
        .unwrap();
    assert_eq!(capture.requests.lock().unwrap().len(), 2);
    variable("API_TIMEOUT_MS", None);
}
