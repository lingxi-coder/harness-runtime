use llm_runtime::services::sdk;
use llm_runtime::{ApiService, ClientConfig, ModelRuntime, SubscriberState};
use orchestrator::ProviderApiAdapter;
use serde_json::json;
use std::sync::{Arc, Mutex};
use tool_api::HostedWebSearchClient;

struct SearchTransport {
    prose_only: bool,
    requests: Mutex<Vec<sdk::HttpRequest>>,
}
#[async_trait::async_trait]
impl llm_runtime::Transport for SearchTransport {
    async fn send(
        &self,
        request: sdk::HttpRequest,
    ) -> Result<sdk::StreamResponse, sdk::protocol::LlmError> {
        use futures::StreamExt;
        self.requests.lock().unwrap().push(request);
        let block = if self.prose_only {
            json!({"type":"text","text":"An unverified answer mentions https://example.com", "citations":[{"type":"web_search_result_location","url":"https://example.com","title":"Unverified","cited_text":"provider prose"}]})
        } else {
            json!({"type":"web_search_tool_result","tool_use_id":"search-1","content":[{"type":"web_search_result","url":"https://example.com/weather","title":"Weather","encrypted_content":"opaque"}]})
        };
        let events = [
            json!({"type":"message_start","message":{"id":"m","model":"deepseek-flash","role":"assistant","content":[],"usage":{"input_tokens":7,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":block}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3,"server_tool_use":{"web_search_requests":1}}}),
            json!({"type":"message_stop"}),
        ];
        Ok(sdk::StreamResponse {
            status: 200,
            headers: vec![],
            body: futures::stream::iter(
                events
                    .into_iter()
                    .map(|event| Ok(format!("data: {event}\n\n").into())),
            )
            .boxed(),
        })
    }
}
fn adapter(
    source_url: &str,
    search_credential: &str,
    prose_only: bool,
) -> (ProviderApiAdapter, Arc<SearchTransport>, Arc<ModelRuntime>) {
    std::env::set_var("DEEPSEEK_ROUTE_TEST_KEY", "test-search-key");
    let profiles = [("deepseek", source_url, "open_ai_chat", "DEEPSEEK_ROUTE_TEST_KEY"), ("deepseek-search", "https://api.deepseek.com/anthropic", "anthropic_messages", search_credential)]
        .into_iter().map(|(name, url, protocol, credential)| {
            let mut profile: llm_runtime::ProviderProfile = serde_json::from_value(json!({
                "provider_id":{"open_ai_compatible":{"name":"deepseek"}}, "profile_name":name,"base_url":url,"protocol":protocol,"auth":"bearer","credential":{"type":"env","var":credential},
                "models":[{"display_model":"deepseek-flash","request_model":"deepseek-flash","billing_model":"deepseek-flash","capabilities":{"streaming":true,"tools":true,"reasoning":true,"vision":false,"documents":false,"structured_output":false}}]
            })).unwrap();
            profile.wire_profile = sdk::builtin_providers().unwrap().into_iter().find(|preset| preset.profile_name == name);
            profile
        }).collect();
    let client = Arc::new(
        ModelRuntime::from_config(ClientConfig {
            providers: profiles,
        })
        .unwrap(),
    );
    let transport = Arc::new(SearchTransport {
        prose_only,
        requests: Mutex::new(vec![]),
    });
    let service = Arc::new(ApiService::new(
        client.clone(),
        transport.clone(),
        SubscriberState::default(),
        llm_runtime::model::user_agent::UserAgentEnv::default(),
        "test",
        None,
        None,
    ));
    (ProviderApiAdapter::new(service), transport, client)
}
fn request(model: &str) -> tool_api::HostedSearchRequest {
    tool_api::HostedSearchRequest {
        model: model.into(),
        profile: Some("deepseek".into()),
        query: "武汉 天气".into(),
        allowed_domains: vec![],
        blocked_domains: vec![],
    }
}
#[tokio::test]
async fn deepseek_auxiliary_search_reuses_credentials_and_keeps_conversation_route() {
    let (adapter, transport, client) =
        adapter("https://api.deepseek.com", "DEEPSEEK_ROUTE_TEST_KEY", false);
    for model in ["deepseek-flash", "deepseek/deepseek-flash"] {
        let input = request(model);
        assert!(adapter.supports_request(&input));
        let (progress, _) = tokio::sync::mpsc::unbounded_channel();
        let output = adapter.search(input, progress).await.unwrap();
        assert_eq!(output.searches, 1);
        assert!(output
            .results
            .iter()
            .any(|entry| matches!(entry, tool_api::hosted_search::SearchResultEntry::Hit(_))));
    }
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests.iter() {
        assert_eq!(
            request.url.split('?').next(),
            Some("https://api.deepseek.com/anthropic/v1/messages")
        );
        assert!(
            request
                .headers
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("authorization")
                    && v == "Bearer test-search-key")
        );
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["model"], "deepseek-flash");
        assert_eq!(body["tools"][0]["type"], "web_search_20250305");
    }
    assert_eq!(
        client
            .protocol_for_model("deepseek-flash", Some("deepseek"))
            .unwrap(),
        llm_runtime::ProtocolFamily::OpenAiChat
    );
}
#[test]
fn deepseek_search_does_not_redirect_custom_gateways_or_other_credentials() {
    for (url, credential) in [
        ("https://custom.example/v1", "DEEPSEEK_ROUTE_TEST_KEY"),
        ("https://api.deepseek.com", "ANOTHER_ACCOUNT_KEY"),
    ] {
        let (adapter, transport, _) = adapter(url, credential, false);
        assert!(!adapter.supports_request(&request("deepseek-flash")));
        assert!(transport.requests.lock().unwrap().is_empty());
    }
}
#[tokio::test]
async fn deepseek_prose_without_native_results_is_an_error() {
    let (adapter, _, _) = adapter("https://api.deepseek.com", "DEEPSEEK_ROUTE_TEST_KEY", true);
    let (progress, _) = tokio::sync::mpsc::unbounded_channel();
    let error = adapter
        .search(request("deepseek-flash"), progress)
        .await
        .err()
        .expect("prose-only response must fail");
    assert!(error
        .message
        .string_at("")
        .is_some_and(|message| message.contains("no structured web_search_tool_result")));
}
