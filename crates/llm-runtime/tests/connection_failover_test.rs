//! A failing connection must hand the SAME model to the next connection.
//!
//! Before this existed, `DriveStep::Fallback` had exactly one production site —
//! `model/retry.rs`, inside the 529 arm — so a 429 never advanced anything; and
//! the streaming drive passed `None` for fallback outright, which is the path
//! desktop and mobile actually run. Both are covered here.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use llm_runtime::model::user_agent::UserAgentEnv;
use llm_runtime::{
    ApiService, AuthStrategy, Capabilities, ClientConfig, ConnectionSpec, CredentialConfig,
    FailoverTriggers, LlmError, ModelProfile, ModelRuntime, PricingConfig, ProtocolFamily,
    ProviderId, ProviderProfile, SubscriberState,
};
use llm_runtime::{BoxFuture, ProviderRequest, ProviderResponse, StreamingResponse};

const INTL: &str = "https://intl.example.com/v1";
const CN: &str = "https://cn.example.com/v1";

/// Records every request URL, and answers with a scripted status sequence.
struct ScriptedTransport {
    statuses: Vec<u16>,
    urls: Mutex<Vec<String>>,
}

impl ScriptedTransport {
    fn new(statuses: Vec<u16>) -> Arc<Self> {
        Arc::new(Self {
            statuses,
            urls: Mutex::new(Vec::new()),
        })
    }

    /// Which connection each attempt went to, in order.
    fn connections(&self) -> Vec<String> {
        self.urls
            .lock()
            .unwrap()
            .iter()
            .map(|u| {
                if u.starts_with(INTL) {
                    "intl".to_string()
                } else if u.starts_with(CN) {
                    "cn".to_string()
                } else {
                    format!("unknown({u})")
                }
            })
            .collect()
    }

    fn next_status(&self, url: &str) -> u16 {
        let mut urls = self.urls.lock().unwrap();
        urls.push(url.to_string());
        let idx = (urls.len() - 1).min(self.statuses.len() - 1);
        self.statuses[idx]
    }
}

fn ok_body() -> serde_json::Value {
    serde_json::json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "shared-model",
        "content": [{ "type": "text", "text": "hi" }],
        "stop_reason": "end_turn",
        "usage": { "input_tokens": 1, "output_tokens": 1 }
    })
}

impl llm_runtime::test_support::FixtureTransport for ScriptedTransport {
    fn execute<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
        let status = self.next_status(&request.url);
        Box::pin(async move {
            Ok(ProviderResponse {
                status,
                headers: BTreeMap::new(),
                body_json: if status == 200 && request.url.ends_with("/chat/completions") {
                    assert!(request
                        .body_json
                        .to_string()
                        .find("cache_control")
                        .is_none());
                    serde_json::json!({"id":"chat_1","model":"shared-model","choices":[{"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}})
                } else if status == 200 {
                    ok_body()
                } else {
                    serde_json::json!({ "error": { "message": "boom" } })
                },
                request_id: None,
            })
        })
    }

    fn open_stream<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
        // Fail the CONNECT phase with a status, exactly like a real 429 on open.
        let status = self.next_status(&request.url);
        Box::pin(async move {
            if status == 200 {
                // Terminal and NOT a failover trigger, so the drive stops the
                // moment the good connection is reached: the recorded URL list
                // is then exactly the hops taken, with no retry noise.
                return Err(LlmError::InvalidRequest {
                    message: "reached-the-good-connection".to_string(),
                });
            }
            Err(LlmError::RateLimited {
                retry_after: None,
                scope: None,
            })
        })
    }
}
llm_runtime::impl_fixture_transport!(ScriptedTransport);

fn connection(conn_id: &str, base_url: &str, order: u32) -> ProviderProfile {
    ProviderProfile {
        wire_profile: None,
        regions: lingxi_llm_client::protocol::Region::all(),
        provider_id: ProviderId::OpenAICompatible {
            name: "grouped".to_string(),
        },
        profile_name: format!("grouped:{conn_id}"),
        base_url: base_url.to_string(),
        protocol: ProtocolFamily::AnthropicMessages,
        auth: AuthStrategy::None,
        credential: CredentialConfig::None,
        models: vec![ModelProfile {
            display_model: "shared-model".to_string(),
            request_model: "shared-model".to_string(),
            billing_model: "shared-model".to_string(),
            aliases: Vec::new(),
            description: None,
            metadata: Default::default(),
            capabilities: Capabilities {
                streaming: true,
                tools: true,
                ..Default::default()
            },
        }],
        pricing: PricingConfig::default(),
        signing: None,
        azure: None,
        supports_websockets: false,
        supports_websocket_compression: false,
        websocket_connect_timeout_ms: None,
        vision_delegate: None,
        connection: ConnectionSpec {
            group: Some("grouped".to_string()),
            connection_id: Some(conn_id.to_string()),
            order,
            hidden: false,
            failover: FailoverTriggers::DEFAULT,
        },
    }
}

fn service(transport: Arc<ScriptedTransport>) -> ApiService {
    let client = Arc::new(
        ModelRuntime::from_config(ClientConfig {
            providers: vec![connection("intl", INTL, 0), connection("cn", CN, 1)],
        })
        .expect("client"),
    );
    ApiService::new(
        client,
        transport,
        SubscriberState::default(),
        UserAgentEnv {
            user_type: Some("external".to_string()),
            entrypoint: Some("cli".to_string()),
            ..Default::default()
        },
        "0.0.0",
        None,
        None,
    )
}

/// Non-streaming: 429 on the first connection, 200 on the second.
#[tokio::test]
async fn a_rate_limited_connection_hands_the_request_to_the_next_one() {
    let transport = ScriptedTransport::new(vec![429, 200]);
    let api = service(transport.clone());

    let response = api
        .messages_create(llm_runtime::MessagesCreateRequest::new(
            "shared-model",
            None,
            None,
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("the second connection must serve the request");

    assert_eq!(
        transport.connections(),
        vec!["intl".to_string(), "cn".to_string()],
        "the 429 must move to the next CONNECTION, not burn the retry ladder on the first"
    );
    assert_eq!(response.model, "shared-model", "the model is unchanged");
}

/// The shape a real session sends: the picker hands back a CONNECTION profile,
/// so `req.profile` is `grouped:intl`, not `None`.
///
/// Every other case here resolves unscoped, which is why the chain being empty
/// under a connection scope went unnoticed — the feature was dead on the only
/// path that matters.
#[tokio::test]
async fn a_request_scoped_to_a_connection_still_fails_over() {
    let transport = ScriptedTransport::new(vec![429, 200]);
    let api = service(transport.clone());

    let _ = api
        .messages_create(llm_runtime::MessagesCreateRequest::new(
            "shared-model",
            Some("grouped:intl"),
            None,
            Vec::new(),
            Vec::new(),
        ))
        .await;

    assert_eq!(
        transport.connections(),
        vec!["intl".to_string(), "cn".to_string()],
        "a session pinned to one connection must still reach its sibling"
    );
}

/// Streaming is the path desktop and mobile actually drive, and it had no
/// fallback of any kind. A 429 on connect must reach the second connection.
#[tokio::test]
async fn the_streaming_connect_phase_also_fails_over() {
    let transport = ScriptedTransport::new(vec![429, 200]);
    let api = service(transport.clone());

    let _ = api
        .stream(
            "shared-model",
            None,
            None,
            Vec::new(),
            Vec::new(),
            None,
            None,
        )
        .await;

    assert_eq!(
        transport.connections(),
        vec!["intl".to_string(), "cn".to_string()],
        "the stream connect phase must fail over too"
    );
}

/// A provider that never opted in must behave exactly as before: one connection,
/// no chain, and the retry ladder reached untouched.
#[tokio::test]
async fn a_single_connection_provider_does_not_fail_over() {
    let transport = ScriptedTransport::new(vec![429, 200]);
    let mut only = connection("intl", INTL, 0);
    only.profile_name = "solo".to_string();
    only.connection = ConnectionSpec::default();
    let client = Arc::new(
        ModelRuntime::from_config(ClientConfig {
            providers: vec![only],
        })
        .expect("client"),
    );
    let api = ApiService::new(
        client,
        transport.clone(),
        SubscriberState::default(),
        UserAgentEnv::default(),
        "0.0.0",
        None,
        None,
    );

    let _ = api
        .messages_create(llm_runtime::MessagesCreateRequest::new(
            "shared-model",
            None,
            None,
            Vec::new(),
            Vec::new(),
        ))
        .await;

    let hops = transport.connections();
    assert!(
        hops.iter().all(|c| c == "intl"),
        "with no connections configured nothing may be re-pointed; got {hops:?}"
    );
}

/// A sibling connection can expose the same model through a different wire.
#[tokio::test]
async fn cross_protocol_failover_adapts_cached_history_for_both_drivers() {
    for streaming in [false, true] {
        let transport = ScriptedTransport::new(vec![429, 200]);
        let first = connection("intl", INTL, 0);
        let mut second = connection("cn", CN, 1);
        second.protocol = ProtocolFamily::OpenAiChat;
        let client = Arc::new(
            ModelRuntime::from_config(ClientConfig {
                providers: vec![first, second],
            })
            .unwrap(),
        );
        let api = ApiService::new(
            client,
            transport.clone(),
            SubscriberState::default(),
            UserAgentEnv::default(),
            "test",
            None,
            None,
        );
        let mut request = llm_runtime::LlmRequest::new("shared-model").with_user_text("hello");
        request
            .input
            .prompt_cache
            .breakpoints
            .push(lingxi_llm_client::protocol::CacheBreakpoint {
                position: lingxi_llm_client::protocol::CachePosition::Message {
                    index: 0,
                    block: 0,
                },
                scope: None,
                ttl: lingxi_llm_client::protocol::CacheTtl::FiveMinutes,
            });
        if streaming {
            let error = api
                .stream_request(request)
                .await
                .err()
                .expect("second connection sentinel");
            assert!(
                matches!(error, LlmError::InvalidRequest {message} if message == "reached-the-good-connection")
            );
        } else {
            let response = api
                .execute_side_query_request(request)
                .await
                .expect("adapted fallback request");
            assert_eq!(response.model, "shared-model");
        }
        assert_eq!(transport.connections(), vec!["intl", "cn"]);
    }
}

#[tokio::test(start_paused = true)]
async fn cross_protocol_model_fallback_rebuilds_history_cache_policy() {
    let transport = ScriptedTransport::new(vec![529, 529, 529, 200]);
    let mut primary = connection("intl", INTL, 0);
    primary.connection = Default::default();
    primary.provider_id = ProviderId::Custom {
        name: "primary".into(),
    };
    primary.models[0].display_model = "claude-opus-4-6".into();
    primary.models[0].request_model = "claude-opus-4-6".into();
    let mut fallback = connection("cn", CN, 1);
    fallback.connection = Default::default();
    fallback.provider_id = ProviderId::Custom {
        name: "secondary".into(),
    };
    fallback.protocol = ProtocolFamily::OpenAiChat;
    let client = Arc::new(
        ModelRuntime::from_config(ClientConfig {
            providers: vec![primary, fallback],
        })
        .unwrap(),
    );
    let api = ApiService::new(
        client,
        transport.clone(),
        SubscriberState::default(),
        UserAgentEnv::default(),
        "test",
        None,
        None,
    );
    api.messages_create({
        let mut request = llm_runtime::MessagesCreateRequest::new(
            "claude-opus-4-6",
            None,
            Some(llm_runtime::SystemPromptInput::source_vector(
                vec!["cached system".into()],
                None,
                None,
            )),
            vec![],
            vec![],
        );
        request.opts.fallback = match Some("shared-model") {
            Some(models) => llm_runtime::FallbackPolicy::from_models_csv(models),
            None => llm_runtime::FallbackPolicy::Configured,
        };
        request
    })
    .await
    .expect("cross-protocol model fallback must succeed");
    assert_eq!(transport.connections(), vec!["intl", "intl", "intl", "cn"]);
}
