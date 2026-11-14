use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug)]
struct Published {
    runtime: Mutex<ModelRuntime>,
    reads: AtomicUsize,
}
impl ModelRuntimeSource for Published {
    fn current(&self, template: &ModelRuntime) -> Arc<ModelRuntime> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.runtime.lock().unwrap().with_owner_state(template)
    }
}

fn config(endpoint: &str, protocol: ProtocolFamily, name: &str) -> ClientConfig {
    let mut p = crate::builtin_presets()
        .providers
        .into_iter()
        .find(|p| p.profile_name == "anthropic")
        .unwrap();
    p.profile_name = "count-snapshot".into();
    p.provider_id = crate::ProviderId::Custom {
        name: "count-snapshot".into(),
    };
    p.protocol = protocol;
    p.base_url = endpoint.into();
    p.auth = AuthStrategy::None;
    p.credential = crate::CredentialConfig::None;
    p.wire_profile = None;
    p.connection = Default::default();
    p.models.truncate(1);
    let model = &mut p.models[0];
    model.display_model = name.into();
    model.request_model = name.into();
    model.billing_model = name.into();
    model.aliases.clear();
    ClientConfig { providers: vec![p] }
}

#[derive(Default)]
struct Counter {
    urls: Mutex<Vec<String>>,
    replace_on_send: Option<(Arc<Published>, Mutex<Option<ModelRuntime>>)>,
}
#[async_trait::async_trait]
impl Transport for Counter {
    async fn send(
        &self,
        request: lingxi_llm_client::HttpRequest,
    ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError> {
        use futures::StreamExt;
        self.urls.lock().unwrap().push(request.url);
        let status = if let Some((source, next)) = &self.replace_on_send {
            if let Some(next) = next.lock().unwrap().take() {
                *source.runtime.lock().unwrap() = next;
            }
            404
        } else {
            200
        };
        let body: &'static [u8] = if status == 200 {
            br#"{"input_tokens":17}"#
        } else {
            br#"{"error":{"type":"not_found_error","message":"count endpoint unavailable"}}"#
        };
        Ok(lingxi_llm_client::StreamResponse {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: futures::stream::once(async move { Ok(body.into()) }).boxed(),
        })
    }
}

#[tokio::test]
async fn live_count_entries_follow_one_current_catalog_like_generation() {
    let old = ModelRuntime::from_config(config(
        "https://retired.invalid",
        ProtocolFamily::AnthropicMessages,
        "claude-sonnet-4-6",
    ))
    .unwrap();
    let source = Arc::new(Published {
        runtime: Mutex::new(
            ModelRuntime::from_config(config(
                "https://published.invalid",
                ProtocolFamily::AnthropicMessages,
                "claude-sonnet-4-6",
            ))
            .unwrap(),
        ),
        reads: AtomicUsize::new(0),
    });
    let live = old.with_catalog_source(source.clone());
    let request = LlmRequest::new("claude-sonnet-4-6")
        .with_profile("count-snapshot")
        .with_user_text("input");
    assert!(live
        .prepare_count_tokens(&request)
        .await
        .unwrap()
        .url
        .contains("published.invalid"));
    assert_eq!(source.reads.load(Ordering::SeqCst), 1);
    let counter = Arc::new(Counter::default());
    assert_eq!(
        crate::model::count_tokens::try_count_tokens_exact(&live, counter.clone(), &request)
            .await
            .unwrap(),
        Some(17)
    );
    assert!(counter.urls.lock().unwrap()[0].contains("published.invalid"));
    assert_eq!(source.reads.load(Ordering::SeqCst), 2);
    assert!(live
        .prepare(&request)
        .await
        .unwrap()
        .provider_request
        .url
        .contains("published.invalid"));
    // A protocol change must not continue dispatching to a retired count API.
    *source.runtime.lock().unwrap() = ModelRuntime::from_config(config(
        "https://chat.invalid",
        ProtocolFamily::OpenAiChat,
        "claude-sonnet-4-6",
    ))
    .unwrap();
    assert_eq!(
        crate::model::count_tokens::try_count_tokens_exact(&live, counter.clone(), &request)
            .await
            .unwrap(),
        None
    );
    assert_eq!(counter.urls.lock().unwrap().len(), 1);
    assert!(live.prepare_count_tokens(&request).await.is_err());
}

#[tokio::test]
async fn exact_endpoint_fallback_keeps_the_pre_await_snapshot() {
    let current = ModelRuntime::from_config(config(
        "https://count.invalid",
        ProtocolFamily::AnthropicMessages,
        "claude-sonnet-4-6",
    ))
    .unwrap();
    let source = Arc::new(Published {
        runtime: Mutex::new(current.clone()),
        reads: AtomicUsize::new(0),
    });
    let live = current.with_catalog_source(source.clone());
    let request = LlmRequest::new("claude-sonnet-4-6")
        .with_profile("count-snapshot")
        .with_user_text("input");
    let expected = source
        .runtime
        .lock()
        .unwrap()
        .estimate_input_tokens_for_route(&request)
        .unwrap();
    let counter = Arc::new(Counter {
        replace_on_send: Some((
            source.clone(),
            Mutex::new(Some(
                ModelRuntime::from_config(config(
                    "https://next.invalid",
                    ProtocolFamily::OpenAiChat,
                    "new-model",
                ))
                .unwrap(),
            )),
        )),
        ..Default::default()
    });
    assert_eq!(
        crate::model::count_tokens::count_tokens(&live, counter, &request)
            .await
            .unwrap(),
        expected
    );
    assert_eq!(
        source.reads.load(Ordering::SeqCst),
        1,
        "exact and fallback must share a captured catalog"
    );
    assert!(
        live.prepare_count_tokens(&request).await.is_err(),
        "the next operation must see removal"
    );
}
