use super::*;

/// Without unified rate-limit headers the cached info stays `None`.
#[tokio::test]
async fn rate_limit_info_none_when_no_unified_headers() {
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    let adapter = make_adapter(transport);
    let _ = adapter
        .messages_create(crate::MessagesCreateRequest::new(
            "claude-sonnet-4-20250514",
            None,
            None,
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("ok");

    // No unified headers → None still.
    assert!(
        adapter.last_rate_limit_info().is_none(),
        "last_rate_limit_info must be None when no unified headers are present"
    );
}

/// The adapter override drives the real `/v1/messages/count_tokens` endpoint
/// on an Anthropic route: it sends one request to the count_tokens URL
/// carrying the `count_tokens` beta header and returns the decoded
/// `input_tokens` from the response.
#[tokio::test]
async fn count_tokens_through_adapter_hits_anthropic_endpoint_with_beta() {
    let transport = FakeTransport::always(ProviderResponse {
        status: 200,
        headers: BTreeMap::new(),
        body_json: provider_payload(serde_json::json!({ "input_tokens": 2095 })),
        request_id: None,
    });
    let adapter = make_adapter(transport.clone());

    let count = adapter
        .count_tokens(
            "claude-sonnet-4-20250514",
            None,
            Some("you are helpful"),
            Vec::new(),
            Vec::new(),
        )
        .await
        .expect("count_tokens ok");

    assert_eq!(
        count, 2095,
        "decoded input_tokens from the count_tokens response"
    );
    assert_eq!(
        transport.seen_count(),
        1,
        "exactly one count_tokens request sent"
    );

    let url = transport.seen.lock().unwrap()[0].url.clone();
    assert!(
        url.ends_with("/v1/messages/count_tokens?beta=true"),
        "must route to the count_tokens endpoint; url={url}"
    );
    let expected_beta = crate::model::betas::assemble_beta_header(
        crate::model::betas::Provider::Anthropic,
        crate::model::betas::Endpoint::CountTokens,
        &crate::model::betas::BetaContext::for_model("claude-sonnet-4-20250514"),
    );
    let expected_beta = format!(
        "{expected_beta},{}",
        lingxi_llm_client::providers::anthropic::request_policy::TOKEN_COUNTING
    );
    assert_eq!(
        transport
            .seen_headers(0)
            .get("anthropic-beta")
            .map(String::as_str),
        Some(expected_beta.as_str()),
        "countTokens appends the SDK token-counting beta after model betas"
    );
}
