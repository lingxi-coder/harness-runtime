//! Production platforms inject the SDK transport directly.
use llm_runtime::services::sdk;

#[tokio::test]
async fn configured_provider_transport_supports_responses_without_a_host_bridge() {
    use sdk::Transport;
    let transport = platform_common::provider_transport().unwrap();
    let error = transport
        .connect_websocket(sdk::HttpRequest {
            method: "GET".into(),
            url: "file:///invalid".into(),
            headers: vec![],
            body: Default::default(),
            timeout: None,
        })
        .await
        .err()
        .unwrap();
    assert!(matches!(
        error,
        sdk::protocol::LlmError::InvalidRequest { .. }
    ));
}
#[test]
fn same_transport_can_be_shared_by_model_and_provider_services() {
    let transport: std::sync::Arc<dyn llm_runtime::Transport> =
        std::sync::Arc::new(platform_common::provider_transport().unwrap());
    let services = llm_runtime::ProviderServices::with_transport(
        &[],
        sdk::protocol::Region::International,
        transport.clone(),
    )
    .unwrap();
    assert!(std::ptr::eq(transport.as_ref(), services.transport()));
}
