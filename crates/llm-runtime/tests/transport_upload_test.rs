use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{stream, StreamExt};
use lingxi_llm_client::{HttpStreamRequest, Transport as SdkTransport};
use llm_runtime::{LlmTransportBridge, Transport};
use platform_api::http::{RawByteStreamWithMeta, SseStream};
use platform_api::{HttpError, HttpTransport};

#[derive(Default)]
struct StreamingHost {
    seen: Mutex<Option<platform_api::http::HttpStreamRequest>>,
}

#[async_trait]
impl HttpTransport for StreamingHost {
    async fn request(&self, _: protocol::HttpRequest) -> Result<protocol::HttpResponse, HttpError> {
        panic!("streaming upload must not use the buffered request seam")
    }

    async fn stream_sse(&self, _: protocol::HttpRequest) -> Result<SseStream, HttpError> {
        panic!("binary uploads must not use SSE")
    }

    async fn send_stream(
        &self,
        request: platform_api::http::HttpStreamRequest,
    ) -> Result<RawByteStreamWithMeta, HttpError> {
        *self.seen.lock().unwrap() = Some(request);
        Ok(RawByteStreamWithMeta {
            status: 429,
            headers: vec![("retry-after".into(), "3".into())],
            stream: stream::iter([
                Ok(vec![0, 255, 128]),
                Err(HttpError::Timeout(Duration::from_secs(2))),
            ])
            .boxed(),
        })
    }
}

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
async fn bridge_streams_binary_uploads_and_preserves_error_response_metadata() {
    let polls = Arc::new(AtomicUsize::new(0));
    let bridge = LlmTransportBridge::new(StreamingHost::default());
    let mut response = Transport::send_stream_raw(&bridge, upload(polls.clone()))
        .await
        .unwrap();
    assert_eq!(
        polls.load(Ordering::SeqCst),
        0,
        "bridge must not pre-buffer uploads"
    );
    let mut request = bridge.inner().seen.lock().unwrap().take().unwrap();
    assert_eq!(request.method, protocol::HttpMethod::Post);
    assert_eq!(request.url, "https://example.com/files");
    assert_eq!(request.headers[0].1, "application/octet-stream");
    assert_eq!(request.content_length, 4);
    assert_eq!(request.timeout, Some(Duration::from_secs(7)));
    assert_eq!(request.body.next().await.unwrap().unwrap(), [0, 255]);
    assert_eq!(polls.load(Ordering::SeqCst), 1);
    assert_eq!(request.body.next().await.unwrap().unwrap(), [128, 1]);
    assert!(request.body.next().await.is_none());
    assert_eq!(response.status, 429);
    assert_eq!(response.header("retry-after"), Some("3"));
    assert_eq!(
        response.body.next().await.unwrap().unwrap().as_ref(),
        &[0, 255, 128]
    );
    assert!(matches!(
        response.body.next().await.unwrap(),
        Err(lingxi_llm_client::protocol::LlmError::TransportTimeout { .. })
    ));
}

#[tokio::test]
async fn upstream_transport_rejects_invalid_method_without_polling_upload() {
    let polls = Arc::new(AtomicUsize::new(0));
    let bridge = LlmTransportBridge::new(StreamingHost::default());
    let mut request = upload(polls.clone());
    request.method = "BAD METHOD".into();
    let result = SdkTransport::send_stream(&bridge, request).await;
    assert!(matches!(
        result,
        Err(lingxi_llm_client::protocol::LlmError::InvalidRequest { .. })
    ));
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    assert!(bridge.inner().seen.lock().unwrap().is_none());
}

#[tokio::test]
async fn default_host_upload_rejects_without_consuming_the_body() {
    struct LegacyTransport;
    impl Transport for LegacyTransport {
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
    let polls = Arc::new(AtomicUsize::new(0));
    let result = LegacyTransport.send_stream_raw(upload(polls.clone())).await;
    assert!(matches!(
        result,
        Err(lingxi_llm_client::protocol::LlmError::UnsupportedCapability { .. })
    ));
    assert_eq!(polls.load(Ordering::SeqCst), 0);
}
