//! Production adapter contracts for DeepSeek hosted web-search responses.
use llm_runtime::services::sdk;
use llm_runtime::{ApiService, ClientConfig, ModelRuntime, SubscriberState};
use orchestrator::ProviderApiAdapter;
use serde_json::json;
use std::sync::{Arc, Mutex};
use tool_api::HostedWebSearchClient;

struct SearchTransport {
    prose_only: bool,
    status: std::sync::atomic::AtomicU16,
    requests: Mutex<Vec<sdk::HttpRequest>>,
}
#[async_trait::async_trait]
impl llm_runtime::Transport for SearchTransport {
    async fn send(
        &self,
        request: sdk::HttpRequest,
    ) -> Result<sdk::StreamResponse, sdk::protocol::LlmError> {
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        let output = if self.prose_only {
            json!({"text":"unverified prose"})
        } else if request.url.ends_with("/fetch") {
            json!({"title":"Article", "content":"# Native Markdown"})
        } else if body["query"].as_str().unwrap().starts_with("empty") {
            json!({"results":[]})
        } else {
            json!({"results":[
                {"rerank_score":2.5,"title":"Weather","url":"https://example.com/weather","snippet":"Weather snippet","summary":"Weather summary", "content":"", "site_name":null,"site_icon":null,"date":null,"authority":3},
                {"rerank_score":2.0,"title":"Other","url":"https://other.test/news","snippet":"Other snippet","summary":"Other summary", "content":"", "site_name":null,"site_icon":null,"date":null,"authority":1}
            ]})
        };
        self.requests.lock().unwrap().push(request);
        Ok(sdk::HttpResponse {
            status: self.status.load(std::sync::atomic::Ordering::Relaxed),
            headers: vec![],
            body: output.to_string().into(),
        }
        .into())
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
        status: std::sync::atomic::AtomicU16::new(200),
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
            Some("https://api.deepseek.com/v1/tools/search")
        );
        assert!(
            request
                .headers
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("authorization")
                    && v == "Bearer test-search-key")
        );
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["query"], "武汉 天气");
        assert!(body.get("model").is_none());
        assert!(body.get("tools").is_none());
    }
    assert_eq!(
        client
            .protocol_for_model("deepseek-flash", Some("deepseek"))
            .unwrap(),
        llm_runtime::ProtocolFamily::OpenAiChat
    );
}
#[test]
fn deepseek_search_does_not_redirect_custom_gateways() {
    let (adapter, transport, _) = adapter(
        "https://custom.example/v1",
        "DEEPSEEK_ROUTE_TEST_KEY",
        false,
    );
    assert!(!adapter.supports_request(&request("deepseek-flash")));
    assert!(transport.requests.lock().unwrap().is_empty());
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
        .is_some_and(|message| message.contains("invalid DeepSeek web API response")));
}

#[tokio::test]
async fn independent_search_keeps_source_credentials_with_a_different_search_account() {
    let (adapter, transport, _) = adapter("https://api.deepseek.com", "ANOTHER_ACCOUNT_KEY", false);
    let (progress, _) = tokio::sync::mpsc::unbounded_channel();
    assert!(adapter.supports_request(&request("deepseek-flash")));
    adapter
        .search(request("deepseek-flash"), progress)
        .await
        .unwrap();
    assert!(transport.requests.lock().unwrap()[0]
        .headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("authorization") && v == "Bearer test-search-key"));
}
#[tokio::test]
async fn independent_search_domains_snippets_and_empty_billing() {
    let (adapter, transport, _) =
        adapter("https://api.deepseek.com", "DEEPSEEK_ROUTE_TEST_KEY", false);
    let mut input = request("deepseek-flash");
    input.allowed_domains = vec!["example.com".into()];
    assert!(adapter.supports_request(&input));
    let (progress, _) = tokio::sync::mpsc::unbounded_channel();
    let output = adapter.search(input, progress).await.unwrap();
    assert_eq!(output.searches, 1);
    assert_eq!(output.input_tokens, 0);
    assert_eq!(output.output_tokens, 0);
    let tool_api::hosted_search::SearchResultEntry::Hit(hit) = &output.results[0] else {
        panic!("missing hits")
    };
    assert_eq!(hit.array_len("/content"), Some(1));
    assert_eq!(
        hit.string_at("/content/0/summary").as_deref(),
        Some("Weather summary")
    );
    let body: serde_json::Value =
        serde_json::from_slice(&transport.requests.lock().unwrap()[0].body).unwrap();
    assert_eq!(body["query"], "武汉 天气 site:example.com");
    let mut empty = request("deepseek-flash");
    empty.query = "empty search".into();
    let (progress, _) = tokio::sync::mpsc::unbounded_channel();
    let output = adapter.search(empty, progress).await.unwrap();
    assert_eq!(output.searches, 0);
    assert!(output.results.is_empty());
}
#[tokio::test]
async fn independent_fetch_uses_selected_route_and_no_model_generation() {
    let (adapter, transport, _) = adapter("https://api.deepseek.com", "ANOTHER_ACCOUNT_KEY", false);
    assert!(adapter.supports_fetch("deepseek-flash", Some("deepseek")));
    let fetched = adapter
        .fetch(tool_api::HostedFetchRequest {
            model: "deepseek-flash".into(),
            profile: Some("deepseek".into()),
            url: "https://example.com/article".into(),
        })
        .await
        .unwrap();
    assert_eq!(fetched.title, "Article");
    assert_eq!(fetched.content, "# Native Markdown");
    assert_eq!(fetched.billable_requests, 1);
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, "https://api.deepseek.com/v1/tools/fetch");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&requests[0].body).unwrap(),
        json!({"url":"https://example.com/article"})
    );
}
#[test]
fn invalid_unicode_and_oversized_search_fail_admission() {
    let (adapter, transport, _) =
        adapter("https://api.deepseek.com", "DEEPSEEK_ROUTE_TEST_KEY", false);
    let mut input = request("deepseek-flash");
    input.query = "a".repeat(129).into();
    assert!(!adapter.supports_request(&input));
    input.query =
        lingxi_core::types::utf16_json::Utf16JsonProjection::parse(r#""\ud800""#).unwrap();
    assert!(!adapter.supports_request(&input));
    input.query = "q".into();
    input.allowed_domains = vec!["example.com OR evil.test".into()];
    assert!(!adapter.supports_request(&input));
    assert!(transport.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn provider_errors_retain_http_status_and_timeout_without_retry() {
    for (status, timeout) in [
        (402, false),
        (403, false),
        (408, true),
        (429, false),
        (500, false),
        (504, true),
    ] {
        let (adapter, transport, _) =
            adapter("https://api.deepseek.com", "DEEPSEEK_ROUTE_TEST_KEY", false);
        transport
            .status
            .store(status, std::sync::atomic::Ordering::Relaxed);
        let (progress, _) = tokio::sync::mpsc::unbounded_channel();
        let error = adapter
            .search(request("deepseek-flash"), progress)
            .await
            .err()
            .expect("provider error");
        assert_eq!(error.http_status, Some(status));
        assert_eq!(error.timeout, timeout);
        assert_eq!(transport.requests.lock().unwrap().len(), 1);
    }
}
