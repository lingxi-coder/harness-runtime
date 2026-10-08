//! Paired Native 2.1.293 HTTP fixtures through the actual SDK and host formatter.
//! The Agent/CLI envelope is a separate acceptance gate.
use async_trait::async_trait;
use futures::StreamExt;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use llm_runtime::model::{thinking::ThinkingConfig, user_agent::UserAgentEnv};
use llm_runtime::{ApiService, ClientConfig, LlmRequest, ModelRuntime, SubscriberState, Transport};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

struct NativeErrorBody {
    body: Vec<u8>,
    calls: AtomicUsize,
}

#[async_trait]
impl Transport for NativeErrorBody {
    async fn send(
        &self,
        _: lingxi_llm_client::HttpRequest,
    ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(lingxi_llm_client::HttpResponse {
            status: 400,
            headers: vec![("content-type".into(), "application/json".into())],
            body: self.body.clone().into(),
        }
        .into())
    }
}

#[tokio::test(start_paused = true)]
async fn native_http_error_source_reaches_host_formatter_in_both_request_modes() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/parity/current-alignment/fixtures/provider-error/native-baseline");
    let prefix = "API Error: ".encode_utf16().collect::<Vec<_>>();
    for case in [
        "nested-error-message-lone-surrogate",
        "empty-response-body",
        "nested-error-message-null",
    ] {
        let native = Utf16JsonProjection::parse(
            &std::fs::read_to_string(root.join(case).join("native.stdout.json.raw")).unwrap(),
        )
        .unwrap();
        let result = native.string_units("/result").unwrap();
        assert!(result.starts_with(&prefix));
        let expected = &result[prefix.len()..];
        for streaming in [false, true] {
            let transport = Arc::new(NativeErrorBody {
                body: std::fs::read(root.join(case).join("http-response.body.raw")).unwrap(),
                calls: AtomicUsize::new(0),
            });
            let config: ClientConfig = serde_json::from_value(serde_json::json!({
                "providers":[{
                    "provider_id":"anthropic_first_party", "profile_name":"direct",
                    "base_url":"https://api.anthropic.com", "protocol":"anthropic_messages",
                    "auth":"none", "credential":{"type":"none"},
                    "models":[{
                        "display_model":"claude-sonnet-4-6", "request_model":"claude-sonnet-4-6",
                        "billing_model":"claude-sonnet-4-6",
                        "capabilities":{"streaming":true,"tools":true,"vision":false,
                            "documents":false,"reasoning":false,"structured_output":false}
                    }]
                }]
            }))
            .unwrap();
            let api = ApiService::new_with_routing(
                Arc::new(ModelRuntime::from_config(config).unwrap()),
                transport.clone(),
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
            .with_thinking(ThinkingConfig::Disabled);
            let request = LlmRequest::new("claude-sonnet-4-6").with_user_text("fixture");
            let error = if streaming {
                match api.stream_request(request).await {
                    Err(error) => error,
                    Ok(stream) => stream
                        .collect::<Vec<_>>()
                        .await
                        .into_iter()
                        .find_map(Result::err)
                        .expect("HTTP 400 must fail"),
                }
            } else {
                api.execute_side_query_request(request).await.unwrap_err()
            };
            assert_eq!(
                transport.calls.load(Ordering::SeqCst),
                1,
                "{case} streaming={streaming}"
            );
            assert_eq!(error.http_status(), Some(400), "{case}");
            assert_eq!(
                llm_runtime::error_display_source(&error)
                    .string_units("")
                    .unwrap(),
                expected,
                "{case} streaming={streaming}",
            );
        }
    }
}
