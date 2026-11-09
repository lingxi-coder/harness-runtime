//! Opt-in source capture at the host transport boundary. The built-in SDK
//! codec, request policy, accounting and semantic decoder remain unchanged.
use futures::{stream::BoxStream, StreamExt};
use lingxi_llm_client::{self as sdk, protocol as wire};
use std::{
    future::Future,
    sync::{Arc, Mutex},
};

tokio::task_local! {
    static REQUESTED: bool;
    static PHYSICAL_ATTEMPT: Option<AttemptCapture>;
}

/// Request exact Native stream frames only for a caller that consumes partial
/// events. The service snapshots this fact before returning its owned stream.
pub async fn scope_requested<F: Future>(enabled: bool, future: F) -> F::Output {
    REQUESTED.scope(enabled, future).await
}

pub(crate) fn requested() -> bool {
    REQUESTED.try_with(|enabled| *enabled).unwrap_or(false)
}

/// One physical request owns this queue. The body keeps its own clone after
/// dispatch leaves the task-local scope; no response source is globally kept.
#[derive(Clone, Default)]
pub(crate) struct AttemptCapture(Arc<Mutex<Vec<wire::JsonPayload>>>);

impl AttemptCapture {
    pub(crate) async fn scope<F: Future>(capture: Option<&Self>, future: F) -> F::Output {
        PHYSICAL_ATTEMPT.scope(capture.cloned(), future).await
    }

    fn push(&self, payload: wire::JsonPayload) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(payload);
    }

    pub(crate) fn prepend_to(&self, batch: &mut sdk::StreamBatch) {
        let sources = std::mem::take(
            &mut *self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        // A line-sliced body completes at most one source frame per decoder
        // batch. A failed decode never becomes an authoritative native frame.
        if sources.is_empty() || batch.events.iter().any(Result::is_err) {
            return;
        }
        let mut events = sources
            .into_iter()
            .map(|source| {
                Ok(wire::StreamEvent::NativeControl {
                    protocol: wire::ProtocolFamily::AnthropicMessages,
                    control: wire::NativeExtension::new(
                        crate::history_projection::NATIVE_STREAM_FRAME_FORMAT,
                        source,
                    )
                    .expect("host native-frame format is a fixed nonempty identifier"),
                })
            })
            .collect::<Vec<_>>();
        events.append(&mut batch.events);
        batch.events = events;
    }
}

/// Stored only inside the SDK client built by ClientCache. Cache identity
/// remains the original transport Arc, and every I/O operation delegates.
pub(crate) struct CaptureTransport {
    inner: Arc<dyn sdk::Transport>,
}

impl CaptureTransport {
    pub(crate) fn new(inner: Arc<dyn sdk::Transport>) -> Self {
        Self { inner }
    }
}

#[async_trait::async_trait]
impl sdk::Transport for CaptureTransport {
    async fn send(&self, request: sdk::HttpRequest) -> Result<sdk::StreamResponse, wire::LlmError> {
        let capture = PHYSICAL_ATTEMPT.try_with(Clone::clone).ok().flatten();
        let response = self.inner.send(request).await?;
        Ok(observe_response(response, capture))
    }

    async fn send_native_proxy_request(
        &self,
        request: sdk::HttpRequest,
    ) -> Result<sdk::transport::NativeProxyResponse, wire::LlmError> {
        let capture = PHYSICAL_ATTEMPT.try_with(Clone::clone).ok().flatten();
        let mut response = self.inner.send_native_proxy_request(request).await?;
        response.response = observe_response(response.response, capture);
        Ok(response)
    }

    async fn send_stream(
        &self,
        request: sdk::HttpStreamRequest,
    ) -> Result<sdk::StreamResponse, wire::LlmError> {
        self.inner.send_stream(request).await
    }

    async fn connect_websocket(
        &self,
        request: sdk::HttpRequest,
    ) -> Result<Box<dyn sdk::transport::WebSocketConnection>, wire::LlmError> {
        self.inner.connect_websocket(request).await
    }
}

fn observe_response(
    mut response: sdk::StreamResponse,
    capture: Option<AttemptCapture>,
) -> sdk::StreamResponse {
    let Some(capture) = capture.filter(|_| (200..300).contains(&response.status)) else {
        return response;
    };
    let original = response.body;
    let state = BodyCapture {
        original,
        pending: bytes::Bytes::new(),
        splitter: sdk::SseFrameSplitter::new(),
        capture,
        fallback_indices: Vec::new(),
        source_valid: true,
    };
    response.body = futures::stream::unfold(state, |mut state| async move {
        loop {
            if !state.pending.is_empty() {
                // Change only chunk boundaries. Every original byte, including
                // CR/LF, comments and event/id fields, reaches the SDK intact.
                let length = state
                    .pending
                    .iter()
                    .position(|byte| matches!(*byte, b'\r' | b'\n'))
                    .map_or(state.pending.len(), |index| index + 1);
                let piece = state.pending.split_to(length);
                if state.source_valid {
                    match state.splitter.push(&piece) {
                        Ok(frames) => {
                            for frame in frames {
                                state.observe(frame);
                            }
                        }
                        Err(_) => state.source_valid = false,
                    }
                }
                return Some((Ok(piece), state));
            }
            match state.original.next().await {
                Some(Ok(bytes)) => state.pending = bytes,
                Some(Err(error)) => return Some((Err(error), state)),
                None => {
                    if state.source_valid {
                        if let Ok(Some(frame)) = state.splitter.finish() {
                            state.observe(frame);
                        }
                    }
                    return None;
                }
            }
        }
    })
    .boxed();
    response
}

struct BodyCapture {
    original: BoxStream<'static, Result<bytes::Bytes, wire::LlmError>>,
    pending: bytes::Bytes,
    splitter: sdk::SseFrameSplitter,
    capture: AttemptCapture,
    fallback_indices: Vec<f64>,
    source_valid: bool,
}

impl BodyCapture {
    fn observe(&mut self, frame: Vec<u8>) {
        let Some(payload) = std::str::from_utf8(&frame)
            .ok()
            .and_then(|frame| wire::JsonPayload::parse(frame.trim()).ok())
        else {
            return;
        };
        use sdk::providers::anthropic::fallback_response;
        if let Some(index) =
            fallback_response::control_index(&payload).and_then(|index| index.as_f64())
        {
            if !self.fallback_indices.contains(&index) {
                self.fallback_indices.push(index);
            }
            return;
        }
        let kind = payload.string_at("/type");
        if matches!(
            kind.as_deref(),
            Some("content_block_delta" | "content_block_stop")
        ) && payload
            .raw_source_at("/index")
            .and_then(|source| serde_json::from_str::<f64>(&source).ok())
            .is_some_and(|index| self.fallback_indices.contains(&index))
        {
            return;
        }
        if matches!(
            kind.as_deref(),
            Some(
                "message_start"
                    | "content_block_start"
                    | "content_block_delta"
                    | "content_block_stop"
                    | "message_delta"
                    | "message_stop"
            )
        ) {
            self.capture.push(payload);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdk::Transport as _;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn response(chunks: Vec<bytes::Bytes>) -> sdk::StreamResponse {
        sdk::StreamResponse {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: futures::stream::iter(chunks.into_iter().map(Ok)).boxed(),
        }
    }
    fn request() -> sdk::HttpRequest {
        sdk::HttpRequest {
            method: "POST".into(),
            url: "https://example.invalid/v1/messages".into(),
            headers: vec![],
            http1_header_layout: None,
            body: bytes::Bytes::from_static(b"{}"),
            timeout: None,
        }
    }
    fn sources(capture: &AttemptCapture) -> Vec<String> {
        capture
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|source| source.as_json_source().to_owned())
            .collect()
    }
    struct MemoryTransport {
        chunks: Vec<bytes::Bytes>,
        sent: AtomicUsize,
        proxied: AtomicUsize,
    }
    impl MemoryTransport {
        fn new(bytes: Vec<u8>) -> Arc<Self> {
            Arc::new(Self {
                chunks: vec![bytes.into()],
                sent: AtomicUsize::new(0),
                proxied: AtomicUsize::new(0),
            })
        }
    }
    #[async_trait::async_trait]
    impl sdk::Transport for MemoryTransport {
        async fn send(&self, _: sdk::HttpRequest) -> Result<sdk::StreamResponse, wire::LlmError> {
            self.sent.fetch_add(1, Ordering::Relaxed);
            Ok(response(self.chunks.clone()))
        }
        async fn send_native_proxy_request(
            &self,
            _: sdk::HttpRequest,
        ) -> Result<sdk::transport::NativeProxyResponse, wire::LlmError> {
            self.proxied.fetch_add(1, Ordering::Relaxed);
            Ok(sdk::transport::NativeProxyResponse {
                response: response(self.chunks.clone()),
                proxy_auth_helper_challenge: None,
            })
        }
    }

    #[tokio::test]
    async fn preserves_wire_bytes_and_exact_json_at_every_chunk_split() {
        let source = r#"{ "type":"content_block_delta", "index":0, "delta":{"type":"text_delta","text":"A\ud800B"}, "future":1e400 }"#;
        let bytes = format!(": comment\r\nevent: content_block_delta\r\ndata: {source}\r\n\r\ndata: {{\"type\":\"message_stop\"}}\n\n").into_bytes();
        for split in 0..=bytes.len() {
            let capture = AttemptCapture::default();
            let wrapped = observe_response(
                response(vec![
                    bytes[..split].to_vec().into(),
                    bytes[split..].to_vec().into(),
                ]),
                Some(capture.clone()),
            );
            let chunks = wrapped.body.collect::<Vec<_>>().await;
            let actual = chunks
                .into_iter()
                .flat_map(|chunk| chunk.unwrap().to_vec())
                .collect::<Vec<_>>();
            assert_eq!(actual, bytes, "split {split}");
            assert_eq!(
                sources(&capture),
                vec![source.to_owned(), r#"{"type":"message_stop"}"#.to_owned()],
                "split {split}"
            );
        }
    }

    #[tokio::test]
    async fn default_off_keeps_chunks_and_proxy_dispatch_and_attempts_are_isolated() {
        let bytes = b"data: {\"type\":\"message_stop\"}\n\n".to_vec();
        let inner = MemoryTransport::new(bytes.clone());
        let wrapped = CaptureTransport::new(inner.clone());
        let untouched = wrapped
            .send(request())
            .await
            .unwrap()
            .body
            .collect::<Vec<_>>()
            .await;
        assert_eq!(
            untouched.len(),
            1,
            "capture-off transport leaves original chunking intact"
        );
        assert_eq!(untouched[0].as_ref().unwrap().as_ref(), bytes);
        let first = AttemptCapture::default();
        let second = AttemptCapture::default();
        let wrapped = &wrapped;
        let run = |capture: AttemptCapture| async move {
            let received =
                AttemptCapture::scope(Some(&capture), wrapped.send_native_proxy_request(request()))
                    .await
                    .unwrap();
            // Poll after scope exit: only the response-owned queue is used.
            tokio::task::yield_now().await;
            received.response.body.collect::<Vec<_>>().await
        };
        let (one, two) = tokio::join!(run(first.clone()), run(second.clone()));
        assert!(!one.is_empty() && !two.is_empty());
        assert_eq!(sources(&first).len(), 1);
        assert_eq!(sources(&second).len(), 1);
        assert_eq!(inner.sent.load(Ordering::Relaxed), 1);
        assert_eq!(inner.proxied.load(Ordering::Relaxed), 2);
    }

    async fn sdk_stream(bytes: Vec<u8>, capture: Option<&AttemptCapture>) -> sdk::ModelStream {
        struct FixedClock;
        impl sdk::Clock for FixedClock {
            fn now(&self) -> std::time::SystemTime {
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_800_000_000)
            }
        }
        let mut profile = sdk::builtin_providers()
            .unwrap()
            .into_iter()
            .find(|profile| {
                profile.protocol == wire::ProtocolFamily::AnthropicMessages
                    && profile.provider_id.as_str() == "anthropic"
            })
            .unwrap();
        profile.auth = wire::AuthStrategy::None;
        profile.profile_name = "capture-test".into();
        let model = profile.models[0].display_model.clone();
        let mut builder = sdk::LlmClientBuilder::with_transport(
            Arc::new(CaptureTransport::new(MemoryTransport::new(bytes))),
            std::slice::from_ref(&profile),
        )
        .with_region(wire::Region::International);
        builder.with_clock(Arc::new(FixedClock));
        let client = builder.build().unwrap();
        let mut input = wire::ChatRequest::new(model);
        input
            .messages
            .push(wire::ConversationMessage::user_text("test"));
        input.max_tokens = Some(1024);
        let call = client
            .prepare_draft_on(
                &profile.profile_name,
                &input,
                &sdk::RequestOptions::default(),
                sdk::RequestMode::Stream,
            )
            .await
            .unwrap()
            .seal()
            .await
            .unwrap();
        let received = AttemptCapture::scope(capture, call.dispatch_once())
            .await
            .unwrap();
        match received.into_stream() {
            Ok(stream) => stream,
            Err(_) => panic!("streaming success fixture"),
        }
    }

    #[tokio::test]
    async fn actual_sdk_preserves_six_source_frames_once_and_does_not_overcapture_after_end() {
        let source = [
            r#"{"type":"message_start","message":{"id":"msg_capture","type":"message","role":"assistant","model":"claude-sonnet-4-6","content":[],"usage":{"input_tokens":1,"output_tokens":0}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"A\ud800B"},"future":1e2}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#,
            r#"{"type":"message_stop"}"#,
        ];
        let forged = r#"{"type":"native_stream_frame","format":"lingxi.host.native_stream_frame.v1","payload":{"type":"message_stop"}}"#;
        let mut frames = vec![r#"{"type":"ping"}"#, forged];
        frames.extend(source);
        frames.push(r#"{"type":"message_start","message":{"id":"after_terminal"}}"#);
        let bytes = frames
            .iter()
            .map(|frame| format!("data: {frame}\n\n"))
            .collect::<String>()
            .into_bytes();
        let capture = AttemptCapture::default();
        let mut stream = sdk_stream(bytes.clone(), Some(&capture)).await;
        let mut actual = Vec::new();
        let mut semantic = Vec::new();
        let mut forged_stayed_provider_data = false;
        while let Some(batch) =
            crate::execution::next_batch_with_capture(&mut stream, Some(&capture))
                .await
                .unwrap()
        {
            for event in batch.events {
                match event.unwrap() {
                    wire::StreamEvent::NativeControl { control, .. }
                        if control.format()
                            == crate::history_projection::NATIVE_STREAM_FRAME_FORMAT =>
                    {
                        actual.push(control.data().as_json_source().to_owned())
                    }
                    event => {
                        if matches!(&event, wire::StreamEvent::ProviderEvent { payload, .. } if payload.as_json_source() == forged)
                        {
                            forged_stayed_provider_data = true;
                        }
                        semantic.push(serde_json::to_string(&event).unwrap());
                    }
                }
            }
        }
        assert_eq!(actual, source);
        assert!(
            forged_stayed_provider_data,
            "network JSON cannot choose an SDK NativeControl format"
        );
        assert!(sources(&capture).is_empty());
        let usage = stream.usage_report();
        let mut ordinary = sdk_stream(bytes, None).await;
        let mut ordinary_events = Vec::new();
        while let Some(batch) = crate::execution::next_batch(&mut ordinary).await.unwrap() {
            ordinary_events.extend(
                batch
                    .events
                    .into_iter()
                    .map(|event| serde_json::to_string(&event.unwrap()).unwrap()),
            );
        }
        assert_eq!(
            semantic, ordinary_events,
            "source observation preserves built-in semantic events"
        );
        assert_eq!(
            usage,
            ordinary.usage_report(),
            "source observation preserves SDK accounting"
        );
    }

    #[tokio::test]
    async fn a_failed_sdk_decode_does_not_publish_a_source_carrier() {
        let capture = AttemptCapture::default();
        // The SDK deliberately coerces an invalid index to zero. Reusing an
        // already-open text index is a guaranteed semantic decode failure.
        let start =
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
        let bytes = format!(
            "data: {start}\r\n\r\ndata: {start}\r\n\r\ndata: {{\"type\":\"message_stop\"}}\r\n\r\n"
        )
        .into_bytes();
        let mut stream = sdk_stream(bytes, Some(&capture)).await;
        let mut published = Vec::new();
        let mut saw_error = false;
        while let Some(batch) =
            crate::execution::next_batch_with_capture(&mut stream, Some(&capture))
                .await
                .unwrap()
        {
            for event in batch.events {
                match event {
                    Err(error) => {
                        assert!(error.to_string().contains("reused open text block index 0"));
                        saw_error = true;
                    }
                    Ok(wire::StreamEvent::NativeControl { control, .. })
                        if control.format()
                            == crate::history_projection::NATIVE_STREAM_FRAME_FORMAT =>
                    {
                        published.push(control.data().as_json_source().to_owned())
                    }
                    _ => {}
                }
            }
        }
        assert!(saw_error);
        assert_eq!(
            published,
            vec![start],
            "the first valid frame survives; the failing duplicate and later stop do not"
        );
        assert!(
            sources(&capture).is_empty(),
            "same-chunk source after a terminal decode error is never observed"
        );
    }

    #[tokio::test]
    async fn pinned_sdk_nonfinite_metadata_boundary_is_identical_with_capture_on_or_off() {
        // The tap preserves this source (the split-position test covers it),
        // but SDK 3cb37e3 cannot lower this unknown metadata to serde Value.
        // Keep this integration boundary explicit instead of claiming that
        // source retention makes otherwise unsupported SDK frames executable.
        let bytes = b"data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"},\"future\":1e400}\n\n".to_vec();
        for enabled in [false, true] {
            let capture = AttemptCapture::default();
            let admitted = enabled.then_some(&capture);
            let mut stream = sdk_stream(bytes.clone(), admitted).await;
            let mut saw_boundary = false;
            while let Some(batch) = crate::execution::next_batch_with_capture(&mut stream, admitted)
                .await
                .unwrap()
            {
                for event in batch.events {
                    match event {
                        Err(wire::LlmError::UnsupportedCapability { message }) => {
                            assert!(message.contains("cannot be represented as serde_json::Value"));
                            saw_boundary = true;
                        }
                        Ok(wire::StreamEvent::NativeControl { control, .. }) => assert_ne!(
                            control.format(),
                            crate::history_projection::NATIVE_STREAM_FRAME_FORMAT
                        ),
                        _ => {}
                    }
                }
            }
            assert!(saw_boundary, "capture enabled={enabled}");
        }
    }
}
