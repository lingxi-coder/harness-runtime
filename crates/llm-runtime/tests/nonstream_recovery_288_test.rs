//! Native timeout retry ceilings reach the SDK physical request loop.
use async_trait::async_trait;
use llm_runtime::model::{thinking::ThinkingConfig, user_agent::UserAgentEnv};
use llm_runtime::{
    ApiService, ClientConfig, LlmError, LlmRequest, ModelRuntime, SubscriberState, Transport,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
#[derive(Clone, Copy)]
enum Mode {
    Headers,
    Body,
    EarlyTimeout,
    ProviderTimeout,
}
struct Capture {
    requests: Mutex<Vec<lingxi_llm_client::HttpRequest>>,
    mode: Mode,
    change_limit: bool,
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
            assert_ne!(body["stream"], true);
            assert!(
                !String::from_utf8_lossy(&request.body).contains("failed_stream_outlasted_timeout")
            );
            let index = requests.len();
            requests.push(request);
            index
        };
        if self.change_limit && index == 1 {
            std::env::set_var(branding::NONSTREAMING_TIMEOUT_RETRIES_ENV, "0");
        }
        match self.mode {
            Mode::Headers => std::future::pending().await,
            Mode::Body => {
                use futures::StreamExt;
                Ok(lingxi_llm_client::StreamResponse {
                    status: 200,
                    headers: vec![],
                    body: futures::stream::pending().boxed(),
                })
            }
            Mode::EarlyTimeout => {
                tokio::time::sleep(Duration::from_millis(1)).await;
                Err(lingxi_llm_client::protocol::LlmError::TransportTimeout {
                    message: "fixture early timeout".into(),
                })
            }
            Mode::ProviderTimeout => {
                tokio::time::sleep(Duration::from_millis(9)).await;
                Ok(lingxi_llm_client::HttpResponse {status:500,headers:vec![],body:br#"{"error":{"type":"timeout_error","message":"fixture provider timeout"}}"#.to_vec().into()}.into())
            }
        }
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
async fn current_timeout_limits_bound_requests_and_keep_early_and_provider_timeouts_distinct() {
    variable("API_TIMEOUT_MS", Some("10"));
    variable(branding::REMOTE_ENV, None);
    variable(branding::MAX_RETRIES_ENV, None);
    let scenarios = [
        (None, true, true, 8, Mode::Headers, false, 3),
        (None, true, true, 8, Mode::Body, false, 3),
        (None, true, false, 8, Mode::Headers, false, 9),
        (None, false, true, 8, Mode::Headers, false, 9),
        (Some("0"), false, false, 8, Mode::Headers, false, 1),
        (Some("1"), false, false, 8, Mode::Headers, false, 2),
        (Some("2"), false, false, 8, Mode::Headers, false, 3),
        (Some("+2"), false, false, 8, Mode::Headers, false, 3),
        (Some("02"), false, false, 8, Mode::Headers, false, 3),
        (Some("1e3"), true, true, 8, Mode::Headers, false, 3),
        (Some("1_000"), false, false, 8, Mode::Headers, false, 9),
        (Some("-1"), true, true, 8, Mode::Headers, false, 3),
        (
            Some("\u{feff}2\u{feff}"),
            false,
            false,
            8,
            Mode::Headers,
            false,
            3,
        ),
        (Some("\u{0085}2"), false, false, 8, Mode::Headers, false, 9),
        (None, true, true, 1, Mode::Headers, false, 2),
        (None, true, true, 0, Mode::Headers, false, 1),
        (Some("0"), true, true, 3, Mode::EarlyTimeout, false, 4),
        (Some("0"), true, true, 3, Mode::ProviderTimeout, false, 4),
        (Some("2"), false, false, 8, Mode::Headers, true, 2),
    ];
    for (explicit, persistent, long_stream, max_retries, mode, change_limit, expected) in scenarios
    {
        variable(branding::NONSTREAMING_TIMEOUT_RETRIES_ENV, explicit);
        variable(
            branding::RETRY_WATCHDOG_ENV,
            Some(if persistent { "1" } else { "0" }),
        );
        let capture = Arc::new(Capture {
            requests: Mutex::new(vec![]),
            mode,
            change_limit,
        });
        let api = service(capture.clone(), max_retries);
        let mut req = request();
        req.execution.failed_stream_outlasted_timeout = long_stream;
        let result = api.execute_side_query_request(req).await;
        assert!(
            matches!(
                result,
                Err(LlmError::TransportTimeout { .. } | LlmError::ProviderTimeout { .. })
            ),
            "{result:?}"
        );
        assert_eq!(capture.requests.lock().unwrap().len(),expected,"explicit={explicit:?}, persistent={persistent}, long={long_stream}, max={max_retries}, changing={change_limit}");
    }
    variable(branding::NONSTREAMING_TIMEOUT_RETRIES_ENV, None);
    variable(branding::RETRY_WATCHDOG_ENV, Some("1"));
    let capture = Arc::new(Capture {
        requests: Mutex::new(vec![]),
        mode: Mode::Headers,
        change_limit: false,
    });
    let api = service(capture.clone(), 8);
    for expected in [3, 6] {
        let mut req = request();
        req.execution.failed_stream_outlasted_timeout = true;
        assert!(api.execute_side_query_request(req).await.is_err());
        assert_eq!(capture.requests.lock().unwrap().len(), expected);
    }
    for name in ["API_TIMEOUT_MS", branding::RETRY_WATCHDOG_ENV] {
        variable(name, None);
    }
}
