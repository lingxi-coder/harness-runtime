use bytes::Bytes;
use futures_util::{stream, StreamExt};
use lingxi_llm_client::HttpStreamRequest;
use llm_runtime::Transport;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;
fn upload(polls: Arc<AtomicUsize>) -> HttpStreamRequest {
    HttpStreamRequest {
        method: "POST".into(),
        url: "https://example.com/files".into(),
        headers: vec![("content-type".into(), "application/octet-stream".into())],
        body: stream::iter([
            Ok(Bytes::from_static(&[0, 255])),
            Ok(Bytes::from_static(&[128, 1])),
        ])
        .inspect(move |_| {
            polls.fetch_add(1, Ordering::SeqCst);
        })
        .boxed(),
        content_length: 4,
        timeout: Some(Duration::from_secs(7)),
    }
}
#[tokio::test]
async fn default_host_upload_rejects_without_consuming_the_body() {
    struct LegacyTransport;
    impl llm_runtime::test_support::FixtureTransport for LegacyTransport {
        fn execute<'a>(
            &'a self,
            _: &'a llm_runtime::ProviderRequest,
        ) -> llm_runtime::BoxFuture<'a, Result<llm_runtime::ProviderResponse, llm_runtime::LlmError>>
        {
            panic!("must not silently buffer uploads")
        }

        fn open_stream<'a>(
            &'a self,
            _: &'a llm_runtime::ProviderRequest,
        ) -> llm_runtime::BoxFuture<'a, Result<llm_runtime::StreamingResponse, llm_runtime::LlmError>>
        {
            panic!("must not silently buffer uploads")
        }
    }
    llm_runtime::impl_fixture_transport!(LegacyTransport);
    let polls = Arc::new(AtomicUsize::new(0));
    let result = LegacyTransport.send_stream(upload(polls.clone())).await;
    assert!(matches!(
        result,
        Err(lingxi_llm_client::protocol::LlmError::UnsupportedCapability { .. })
    ));
    assert_eq!(polls.load(Ordering::SeqCst), 0);
}
