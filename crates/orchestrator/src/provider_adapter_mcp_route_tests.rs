use super::*;
use llm_runtime::services::sdk;
use std::sync::Mutex;
use tool_api::McpTokenCounter;

#[derive(Debug)]
struct RouteCredentials;

impl llm_runtime::CredentialProvider for RouteCredentials {
    fn load<'a>(
        &'a self,
        scope: &'a llm_runtime::CredentialScope,
    ) -> llm_runtime::BoxFuture<'a, Result<llm_runtime::Credential, LlmError>> {
        Box::pin(async move {
            Ok(llm_runtime::Credential::ApiKey(format!(
                "token-{}",
                scope.profile_name
            )))
        })
    }
}

#[derive(Default)]
struct CountTransport {
    seen: Mutex<Vec<sdk::HttpRequest>>,
}

#[async_trait]
impl llm_runtime::Transport for CountTransport {
    async fn send(
        &self,
        request: sdk::HttpRequest,
    ) -> Result<sdk::StreamResponse, sdk::protocol::LlmError> {
        self.seen.lock().unwrap().push(request);
        Ok(sdk::HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"{"input_tokens":19}"#.to_vec().into(),
        }
        .into())
    }
}

#[tokio::test]
async fn mcp_exact_count_pins_the_invoking_endpoint_and_credential() {
    let profiles = [
        ("counter-a", "https://account-a.test"),
        ("counter-b", "https://account-b.test"),
    ]
    .into_iter()
    .map(|(name, endpoint)| {
        serde_json::from_value(serde_json::json!({
            "provider_id":{"custom":{"name":"counter-provider"}},
            "profile_name":name,"base_url":endpoint,"protocol":"anthropic_messages",
            "auth":"api_key","credential":{"type":"static","id":name},
            "models":[{"display_model":"shared-model","request_model":"shared-model",
                "billing_model":"shared-model","capabilities":{"streaming":true,"tools":true,
                    "vision":false,"documents":false,"reasoning":false,"structured_output":false}}]
        }))
        .unwrap()
    })
    .collect();
    let client = llm_runtime::ModelRuntime::from_config(llm_runtime::ClientConfig {
        providers: profiles,
    })
    .unwrap()
    .with_credential_provider(Arc::new(RouteCredentials));
    let transport = Arc::new(CountTransport::default());
    let adapter = ProviderApiAdapter::new(Arc::new(llm_runtime::ApiService::new(
        Arc::new(client),
        transport.clone(),
        Default::default(),
        Default::default(),
        "test",
        None,
        None,
    )));
    let content =
        Utf16JsonProjection::parse(r#"[{"type":"text","text":"private\ud800 data"}]"#).unwrap();
    for profile in ["counter-b", "counter-a"] {
        assert_eq!(
            adapter
                .count_mcp_content_tokens("shared-model", profile, &content)
                .await
                .unwrap(),
            Some(19)
        );
    }
    assert_eq!(
        adapter
            .count_mcp_content_tokens("shared-model", "", &content)
            .await
            .unwrap(),
        None
    );
    let seen = transport.seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "an absent pinned profile must not dispatch");
    for (request, account) in seen.iter().zip(["b", "a"]) {
        assert_eq!(
            request.url,
            format!("https://account-{account}.test/v1/messages/count_tokens?beta=true")
        );
        assert!(request
            .headers
            .iter()
            .any(|(name, value)| name.eq_ignore_ascii_case("x-api-key")
                && value == &format!("token-counter-{account}")));
        let body = Utf16JsonProjection::parse(std::str::from_utf8(&request.body).unwrap()).unwrap();
        assert_eq!(body.string_at("/model"), Some("shared-model"));
        assert_eq!(
            body.string_units("/messages/0/content/0/text"),
            content.string_units("/0/text")
        );
    }
}
