//! The `llm.chat` operation of the `window.lingxi.v2` bridge.
//!
//! An app-initiated model call spends the USER's quota, so the ladder is
//! stricter than the device ops': the page may not pick a model (it always
//! rides the live `/model` selection), may not ask for tools, and may not
//! run two calls at once. Every call brackets itself with an
//! `AppLlmActivityChanged` pair so the client can show "this app is calling
//! AI" without inspecting the payload.
//!
//! Attachments should arrive by HANDLE (`mediaId`), not inline. `llm.chat`
//! has an 8 MiB long-context lane, but base64 would still expand and copy a
//! capture through JS, WebKit, Swift/Kotlin, and Rust. The bytes are already
//! engine-side (a device op produced them), so a handle skips that round trip.
//! Small app-generated images (a canvas export) may still be sent inline.

use super::{AgentOutputStream, BridgeFailure, LocalAppsHostBroker};
use crate::mobile::local_apps_llm::{ChatMessage, ChatPart, ChatRequest, ChatRole};
use base64::Engine as _;
use client::protocol::events::ClientEvent;
use client::protocol::local_apps::{AppCapabilityKindDto, AppEventDto};
use futures_util::StreamExt;
use llm_runtime::{ContentDelta, LlmEvent};
use local_apps::AppCapability;
use platform_api::OutputStream;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

const MAX_CHAT_MESSAGES: usize = 20;
const MAX_SYSTEM_BYTES: usize = 8 * 1024;
const MAX_TOKENS_MIN: u32 = 1;
const MAX_TOKENS_MAX: u32 = 4_096;
const MAX_TOKENS_DEFAULT: u32 = 1_024;
/// Cap on the answer handed back to the page.
const MAX_CHAT_TEXT_BYTES: usize = 64 * 1024;
/// Cap on ONE request's attachments, decoded. Roughly five default-preset
/// photos; providers reject far less than this anyway.
const MAX_ATTACHMENT_BYTES: usize = 8 * 1024 * 1024;
const MAX_ATTACHMENTS: usize = 8;
const CHAT_TIMEOUT: Duration = Duration::from_secs(120);

/// Holds an app's single in-flight `llm.chat` slot and its "calling AI"
/// indicator, and gives BOTH back when the call ends.
///
/// Straight-line cleanup after the await is not enough: `llm_chat_value`'s
/// future is dropped whenever the connection is torn down mid-call (a
/// reconnect, the local-app view closing, a `Task` cancellation on iOS) while
/// `LocalAppsHostBroker` outlives it in the process-wide profile cache. Nothing
/// else ever clears `llm_inflight` — no `stop_runtime`, no app delete — so a
/// leaked slot meant every later `llm.chat` from that app answered `llm_busy`,
/// and the indicator stayed lit, for the life of the process.
struct LlmInflightGuard {
    app_id: String,
    slots: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    event_sink: std::sync::Arc<dyn client::adapter::ClientEventSink>,
    armed: bool,
}

impl LlmInflightGuard {
    /// The ordered release for the paths that actually return: the slot is
    /// freed and `active:false` is AWAITED, so the indicator is out before the
    /// caller sees its answer. Disarms `Drop`.
    async fn release(&mut self) {
        if !std::mem::take(&mut self.armed) {
            return;
        }
        self.free_slot();
        self.event_sink
            .emit(ClientEvent::AppEvent {
                event: AppEventDto::AppLlmActivityChanged {
                    app_id: self.app_id.clone(),
                    active: false,
                },
            })
            .await;
    }

    fn free_slot(&self) {
        if let Ok(mut slots) = self.slots.lock() {
            slots.remove(&self.app_id);
        }
    }
}

impl Drop for LlmInflightGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.free_slot();
        // Drop cannot await, so the indicator reset is fire-and-forget — and
        // only when a runtime is still up, since dropping during shutdown must
        // not panic inside a destructor.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let event_sink = self.event_sink.clone();
            let app_id = self.app_id.clone();
            handle.spawn(async move {
                event_sink
                    .emit(ClientEvent::AppEvent {
                        event: AppEventDto::AppLlmActivityChanged {
                            app_id,
                            active: false,
                        },
                    })
                    .await;
            });
        }
    }
}

const REASON_LLM: &str = "应用请求调用你配置的 AI 模型来实现应用内功能。调用走你当前选择的模型与密钥，会消耗你的模型用量/费用。";

fn invalid(message: impl Into<String>) -> BridgeFailure {
    BridgeFailure::coded("llm_request_invalid", message.into())
}

/// Truncate to at most `limit` BYTES without splitting a character.
fn truncate_on_char_boundary(text: &str, limit: usize) -> (String, bool) {
    if text.len() <= limit {
        return (text.to_string(), false);
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

/// Media kinds an app may attach. Audio is deliberately absent — see
/// `transcribe_speech_value`.
fn attachment_part(media_type: &str, base64_body: String) -> Result<ChatPart, BridgeFailure> {
    if media_type.starts_with("image/") {
        Ok(ChatPart::Image {
            media_type: media_type.to_string(),
            base64: base64_body,
        })
    } else if media_type == "application/pdf" {
        Ok(ChatPart::Document {
            media_type: media_type.to_string(),
            base64: base64_body,
        })
    } else if media_type.starts_with("audio/") {
        Err(BridgeFailure::coded(
            "llm_media_unsupported",
            "audio cannot be sent to the model on this stack; call \
             device.transcribeSpeech and send the transcript as text",
        ))
    } else {
        Err(BridgeFailure::coded(
            "llm_media_unsupported",
            format!("{media_type} attachments are not supported"),
        ))
    }
}

impl LocalAppsHostBroker {
    /// Parse the page's `llm.chat` payload into a validated request.
    ///
    /// Runs BEFORE any capability prompt: a malformed call must not make the
    /// user answer a permission sheet for a request that was never going to
    /// run.
    fn parse_chat_request(
        &self,
        app_id: &str,
        payload: &Value,
        streaming: bool,
    ) -> Result<ChatRequest, BridgeFailure> {
        if !streaming && payload.get("stream").and_then(Value::as_bool) == Some(true) {
            return Err(BridgeFailure::coded(
                "llm_stream_unsupported",
                "streaming answers need a host-to-page push channel that does not exist yet; \
                 omit `stream` to receive the whole answer at once",
            ));
        }
        let system = match payload.get("system") {
            None | Some(Value::Null) => None,
            Some(value) => {
                let system = value
                    .as_str()
                    .ok_or_else(|| invalid("system must be a string"))?;
                if system.len() > MAX_SYSTEM_BYTES {
                    return Err(invalid(format!(
                        "system prompt is {} bytes (limit {MAX_SYSTEM_BYTES})",
                        system.len()
                    )));
                }
                Some(system.to_string())
            }
        };
        let raw_messages = payload
            .get("messages")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("messages must be an array"))?;
        if raw_messages.is_empty() || raw_messages.len() > MAX_CHAT_MESSAGES {
            return Err(invalid(format!(
                "messages must hold 1..={MAX_CHAT_MESSAGES} turns, got {}",
                raw_messages.len()
            )));
        }
        let mut attachment_bytes = 0usize;
        let mut attachment_count = 0usize;
        let mut messages = Vec::with_capacity(raw_messages.len());
        for raw in raw_messages {
            let role = match raw.get("role").and_then(Value::as_str) {
                Some("user") => ChatRole::User,
                Some("assistant") => ChatRole::Assistant,
                // No `system` turn: the system prompt is its own field, and
                // accepting one here would let a page smuggle a second one
                // mid-conversation.
                other => {
                    return Err(invalid(format!(
                        "message role must be user|assistant, got {other:?}"
                    )));
                }
            };
            let content = match raw.get("content") {
                // Shorthand: a bare string is one text part.
                Some(Value::String(text)) => vec![ChatPart::Text(text.clone())],
                Some(Value::Array(parts)) => {
                    let mut collected = Vec::with_capacity(parts.len());
                    for part in parts {
                        collected.push(self.parse_chat_part(
                            app_id,
                            part,
                            &mut attachment_bytes,
                            &mut attachment_count,
                        )?);
                    }
                    collected
                }
                _ => return Err(invalid("message content must be a string or an array")),
            };
            if content.is_empty() {
                return Err(invalid("a message must carry at least one part"));
            }
            messages.push(ChatMessage { role, content });
        }
        let max_tokens = match payload.get("maxTokens") {
            None | Some(Value::Null) => MAX_TOKENS_DEFAULT,
            Some(value) => u32::try_from(
                value
                    .as_u64()
                    .ok_or_else(|| invalid("maxTokens must be a positive integer"))?,
            )
            .unwrap_or(u32::MAX),
        }
        .clamp(MAX_TOKENS_MIN, MAX_TOKENS_MAX);
        let temperature = match payload.get("temperature") {
            None | Some(Value::Null) => None,
            Some(value) => {
                let temperature = value
                    .as_f64()
                    .ok_or_else(|| invalid("temperature must be a number"))?;
                if !(0.0..=1.0).contains(&temperature) {
                    return Err(invalid("temperature must be within 0.0..=1.0"));
                }
                Some(temperature as f32)
            }
        };
        Ok(ChatRequest {
            system,
            messages,
            max_tokens,
            temperature,
            cache_scope: Some(app_id.to_string()),
        })
    }

    fn parse_chat_part(
        &self,
        app_id: &str,
        part: &Value,
        attachment_bytes: &mut usize,
        attachment_count: &mut usize,
    ) -> Result<ChatPart, BridgeFailure> {
        match part.get("type").and_then(Value::as_str) {
            Some("text") => Ok(ChatPart::Text(
                part.get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("a text part needs `text`"))?
                    .to_string(),
            )),
            Some("image" | "document" | "media") => {
                *attachment_count += 1;
                if *attachment_count > MAX_ATTACHMENTS {
                    return Err(invalid(format!(
                        "a request may attach at most {MAX_ATTACHMENTS} files"
                    )));
                }
                // Preferred form: a handle to something a device op captured.
                if let Some(media_id) = part.get("mediaId").and_then(Value::as_str) {
                    let entry = self.media_entry(app_id, media_id).ok_or_else(|| {
                        invalid(format!(
                            "mediaId {media_id:?} is unknown or has expired; capture it again"
                        ))
                    })?;
                    *attachment_bytes += entry.bytes.len();
                    if *attachment_bytes > MAX_ATTACHMENT_BYTES {
                        return Err(invalid(format!(
                            "attachments exceed {MAX_ATTACHMENT_BYTES} bytes"
                        )));
                    }
                    let encoded = base64::engine::general_purpose::STANDARD.encode(&*entry.bytes);
                    return attachment_part(&entry.media_type, encoded);
                }
                // Inline form, for small app-generated images. The bridge's
                // bounded LLM lane is the outer encoded-size limit.
                let media_type = part
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("an inline attachment needs `mimeType`"))?;
                let base64_body = part
                    .get("base64")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("an attachment needs `mediaId` or `base64`"))?;
                let decoded_len = base64::engine::general_purpose::STANDARD
                    .decode(base64_body)
                    .map_err(|error| invalid(format!("attachment base64 is malformed: {error}")))?
                    .len();
                *attachment_bytes += decoded_len;
                if *attachment_bytes > MAX_ATTACHMENT_BYTES {
                    return Err(invalid(format!(
                        "attachments exceed {MAX_ATTACHMENT_BYTES} bytes"
                    )));
                }
                attachment_part(media_type, base64_body.to_string())
            }
            other => Err(invalid(format!(
                "part type must be text|image|document, got {other:?}"
            ))),
        }
    }

    async fn emit_llm_activity(&self, app_id: &str, active: bool) {
        self.event_sink
            .emit(ClientEvent::AppEvent {
                event: AppEventDto::AppLlmActivityChanged {
                    app_id: app_id.to_string(),
                    active,
                },
            })
            .await;
    }

    pub(super) async fn llm_chat_value(
        &self,
        app_id: &str,
        payload: &Value,
    ) -> Result<Value, BridgeFailure> {
        let request = self.parse_chat_request(app_id, payload, false)?;
        self.authorize_declared_capability(
            app_id,
            AppCapability::Llm,
            AppCapabilityKindDto::Llm,
            REASON_LLM,
        )
        .await?;
        let llm = self
            .llm
            .get()
            .ok_or_else(|| BridgeFailure::coded("llm_unavailable", "no model is attached"))?
            .current();

        // Take the app's single in-flight slot AFTER authorization, so a
        // pending permission sheet cannot make a later call look busy.
        if !self
            .llm_inflight
            .lock()
            .expect("llm inflight set poisoned")
            .insert(app_id.to_string())
        {
            return Err(BridgeFailure::coded(
                "llm_busy",
                "this app already has a model call in flight",
            ));
        }
        // Armed from here on, so the slot is released even on the exits the
        // compiler cannot see: this future is dropped whenever the connection
        // is torn down mid-call, while the broker outlives it in the
        // process-wide profile cache.
        let mut inflight = LlmInflightGuard {
            app_id: app_id.to_string(),
            slots: self.llm_inflight.clone(),
            event_sink: self.event_sink.clone(),
            armed: true,
        };
        self.emit_llm_activity(app_id, true).await;
        let outcome = tokio::time::timeout(CHAT_TIMEOUT, llm.chat(request)).await;
        // The ordered release on the paths that do return: the "calling AI"
        // indicator goes out before the caller sees its answer.
        inflight.release().await;

        let outcome = match outcome {
            Err(_) => return Err(BridgeFailure::coded("timeout", "the model call timed out")),
            Ok(Err(error)) => {
                return Err(BridgeFailure::coded("llm_unavailable", error.to_string()));
            }
            Ok(Ok(outcome)) => outcome,
        };
        let stopped_at_budget = outcome.stop_reason.as_deref() == Some("max_tokens");
        if outcome.text.trim().is_empty() && stopped_at_budget {
            // A reasoning model can spend the whole budget thinking and
            // never write an answer. That is a budget defect, not a reply —
            // handing the page an empty string would look like the model had
            // nothing to say.
            return Err(BridgeFailure::coded(
                "llm_truncated",
                "the model ran out of output budget before writing an answer; retry with a \
                 larger maxTokens",
            ));
        }
        let (text, cut) = truncate_on_char_boundary(&outcome.text, MAX_CHAT_TEXT_BYTES);
        Ok(json!({
            "text": text,
            "stopReason": outcome.stop_reason,
            "truncated": cut || stopped_at_budget,
        }))
    }

    pub(super) async fn llm_stream_value(
        &self,
        app_id: &str,
        request_id: &str,
        payload: &Value,
    ) -> Result<Value, BridgeFailure> {
        let request = self.parse_chat_request(app_id, payload, true)?;
        self.authorize_declared_capability(
            app_id,
            AppCapability::Llm,
            AppCapabilityKindDto::Llm,
            REASON_LLM,
        )
        .await?;
        let llm = self
            .llm
            .get()
            .ok_or_else(|| BridgeFailure::coded("llm_unavailable", "no model is attached"))?
            .current();
        if !self
            .llm_inflight
            .lock()
            .expect("llm inflight set poisoned")
            .insert(app_id.to_string())
        {
            return Err(BridgeFailure::coded(
                "llm_busy",
                "this app already has a model call in flight",
            ));
        }

        let mut inflight = LlmInflightGuard {
            app_id: app_id.to_string(),
            slots: self.llm_inflight.clone(),
            event_sink: self.event_sink.clone(),
            armed: true,
        };
        let stream_id = self.request_id("llm-stream");
        let output = Arc::new(AgentOutputStream::new(
            self.event_sink.clone(),
            app_id,
            request_id,
            Some(stream_id.clone()),
        ));
        self.emit_llm_activity(app_id, true).await;
        output.started().await;

        let streamed = tokio::time::timeout(CHAT_TIMEOUT, async {
            let mut events = llm
                .stream(request)
                .await
                .map_err(|error| error.to_string())?;
            let mut stop_reason = None;
            while let Some(event) = events.next().await {
                match event.map_err(|error| error.to_string())? {
                    LlmEvent::ContentBlockDelta {
                        delta: ContentDelta::TextDelta { text },
                        ..
                    } => output.emit_text(&text).await,
                    LlmEvent::MessageDelta { delta, .. } => {
                        stop_reason = delta.stop_reason.or(stop_reason);
                    }
                    LlmEvent::Completed { response } => {
                        stop_reason = response.stop_reason.clone().or(stop_reason);
                    }
                    LlmEvent::WebSearch { .. }
                    | LlmEvent::MessageStart { .. }
                    | LlmEvent::ContentBlockStart { .. }
                    | LlmEvent::ContentBlockDelta { .. }
                    | LlmEvent::ContentBlockStop { .. }
                    | LlmEvent::MessageStop => {}
                }
            }
            Ok::<_, String>((output.text_snapshot().await, stop_reason))
        })
        .await;
        inflight.release().await;

        match streamed {
            Err(_) => {
                output.error("timeout", "the model stream timed out").await;
                Err(BridgeFailure::coded(
                    "timeout",
                    "the model stream timed out",
                ))
            }
            Ok(Err(error)) => {
                output.error("llm_unavailable", error.clone()).await;
                Err(BridgeFailure::coded("llm_unavailable", error))
            }
            Ok(Ok((text, stop_reason))) => {
                output.completed().await;
                let (text, truncated) = truncate_on_char_boundary(&text, MAX_CHAT_TEXT_BYTES);
                Ok(json!({
                    "streamId": stream_id,
                    "text": text,
                    "stopReason": stop_reason,
                    "truncated": truncated,
                }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::mobile::local_apps_host::LocalAppsHostBroker;
    use crate::mobile::local_apps_llm::{
        ChatOutcome, ChatPart, ChatRequest, LocalAppsLlm, LocalAppsModel, LocalAppsModelStream,
    };
    use crate::mobile::local_apps_profile::SharedLlm;
    use async_trait::async_trait;
    use base64::Engine as _;
    use client::adapter::{ClientEventSink, MockSink};
    use client::protocol::events::ClientEvent;
    use client::protocol::local_apps::{
        AppAuthorizationDecisionDto, AppBridgeOperationDto, AppBridgeRequestDto, AppEventDto,
    };
    use futures_util::stream;
    use llm_runtime::{ContentDelta, LlmEvent, MessageDeltaPayload};
    use local_apps::error::AppError;
    use local_apps::test_support::FixedClock;
    use local_apps::{
        load_manifest, load_permissions, save_manifest, save_permissions, AppCapability, AppLayout,
        AppService, NoopAppEventObserver,
    };
    use serde_json::{json, Value};
    use std::sync::Arc;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::time::timeout;

    /// A model whose `chat` answers from a script, recording what it saw.
    struct ChatModel {
        outcome: std::sync::Mutex<Vec<Result<ChatOutcome, AppError>>>,
        stream_events: std::sync::Mutex<Option<Vec<Result<LlmEvent, AppError>>>>,
        seen: std::sync::Mutex<Vec<ChatRequest>>,
        /// When set, `chat` never returns — for the timeout test.
        hang: bool,
    }

    impl ChatModel {
        fn answering(text: &str, stop_reason: Option<&str>) -> Arc<Self> {
            Arc::new(Self {
                outcome: std::sync::Mutex::new(vec![Ok(ChatOutcome {
                    text: text.to_string(),
                    stop_reason: stop_reason.map(str::to_string),
                })]),
                stream_events: std::sync::Mutex::new(None),
                seen: std::sync::Mutex::new(Vec::new()),
                hang: false,
            })
        }

        fn hanging() -> Arc<Self> {
            Arc::new(Self {
                outcome: std::sync::Mutex::new(Vec::new()),
                stream_events: std::sync::Mutex::new(None),
                seen: std::sync::Mutex::new(Vec::new()),
                hang: true,
            })
        }

        fn streaming(events: Vec<Result<LlmEvent, AppError>>) -> Arc<Self> {
            Arc::new(Self {
                outcome: std::sync::Mutex::new(Vec::new()),
                stream_events: std::sync::Mutex::new(Some(events)),
                seen: std::sync::Mutex::new(Vec::new()),
                hang: false,
            })
        }
    }

    #[async_trait]
    impl LocalAppsModel for ChatModel {
        async fn chat(&self, request: ChatRequest) -> Result<ChatOutcome, AppError> {
            self.seen.lock().expect("lock").push(request);
            if self.hang {
                std::future::pending::<()>().await;
            }
            let mut outcome = self.outcome.lock().expect("lock");
            if outcome.is_empty() {
                return Err(AppError::Io("scripted chat exhausted".into()));
            }
            outcome.remove(0)
        }

        async fn stream(&self, _request: ChatRequest) -> Result<LocalAppsModelStream, AppError> {
            let events = self
                .stream_events
                .lock()
                .expect("lock")
                .take()
                .ok_or_else(|| AppError::Io("scripted stream exhausted".into()))?;
            Ok(Box::pin(stream::iter(events)))
        }
    }

    struct Harness {
        _root: TempDir,
        broker: Arc<LocalAppsHostBroker>,
        sink: Arc<MockSink>,
        app_id: String,
        layout: AppLayout,
    }

    /// A camera that always hands back the same JPEG bytes.
    struct StubCamera(Vec<u8>);

    #[async_trait]
    impl platform_api::CameraControl for StubCamera {
        async fn capture_photo(
            &self,
            _opts: platform_api::CapturePhotoOpts,
        ) -> Result<platform_api::CapturedImage, platform_api::CameraError> {
            Ok(platform_api::CapturedImage {
                jpeg_bytes: self.0.clone(),
                width: 1280,
                height: 960,
            })
        }

        async fn pick_from_library(
            &self,
        ) -> Result<platform_api::CapturedImage, platform_api::CameraError> {
            self.capture_photo(platform_api::CapturePhotoOpts {
                position: platform_api::CameraPosition::Back,
                allow_editing: false,
            })
            .await
        }
    }

    async fn harness(model: Arc<ChatModel>) -> Harness {
        harness_with_devices(
            model,
            crate::mobile::local_apps_device::DeviceCapabilities::default(),
        )
        .await
    }

    async fn harness_with_devices(
        model: Arc<ChatModel>,
        devices: crate::mobile::local_apps_device::DeviceCapabilities,
    ) -> Harness {
        let h = build_harness(model).await;
        assert!(h
            .broker
            .attach_device(Arc::new(
                crate::mobile::local_apps_device::SharedDeviceCapabilities::new(devices)
            ))
            .is_ok());
        h
    }

    async fn build_harness(model: Arc<ChatModel>) -> Harness {
        let root = TempDir::new().expect("tempdir");
        let service = Arc::new(
            AppService::load(
                root.path(),
                Arc::new(FixedClock::new(1)),
                Arc::new(NoopAppEventObserver),
            )
            .await
            .expect("load app service"),
        );
        let sink = MockSink::arc();
        let broker = LocalAppsHostBroker::new(
            root.path().to_path_buf(),
            sink.clone() as Arc<dyn ClientEventSink>,
            None,
            false,
            None,
        );
        assert!(broker.attach_service(service.clone()).is_ok());
        assert!(broker
            .attach_llm(Arc::new(SharedLlm::new(Arc::new(LocalAppsLlm::new(model)))))
            .is_ok());
        let record = service
            .create_app(Some("Chatty"), "an llm test app", None)
            .await
            .expect("create app");
        let layout = AppLayout::new(root.path().to_path_buf(), record.id.clone()).expect("layout");
        Harness {
            _root: root,
            broker,
            sink,
            app_id: record.id,
            layout,
        }
    }

    fn declare_and_grant(h: &Harness) {
        let mut manifest = load_manifest(&h.layout).expect("manifest");
        manifest.capabilities.push(AppCapability::Llm);
        save_manifest(&h.layout, &manifest).expect("declare");
        let mut permissions = load_permissions(&h.layout).expect("permissions");
        permissions.grant(AppCapability::Llm);
        save_permissions(&h.layout, &permissions).expect("grant");
    }

    async fn chat(h: &Harness, payload: Value) -> (bool, Value, Option<String>, Option<String>) {
        chat_as(h, "req-1", payload).await
    }

    async fn chat_as(
        h: &Harness,
        request_id: &str,
        payload: Value,
    ) -> (bool, Value, Option<String>, Option<String>) {
        h.broker
            .execute_bridge(AppBridgeRequestDto {
                request_id: request_id.to_string(),
                app_id: h.app_id.clone(),
                operation: AppBridgeOperationDto::LlmChat,
                payload_json: Some(payload.to_string()),
            })
            .await;
        let response = h
            .sink
            .events()
            .await
            .into_iter()
            .rev()
            .find_map(|event| match event {
                ClientEvent::AppEvent {
                    event: AppEventDto::AppBridgeResponse { response },
                } if response.request_id == request_id => Some(response),
                _ => None,
            })
            .expect("a bridge response event");
        let result = response
            .result_json
            .as_deref()
            .map(|body| serde_json::from_str(body).expect("result json"))
            .unwrap_or(Value::Null);
        (response.ok, result, response.error, response.error_code)
    }

    /// Every `AppLlmActivityChanged` flag emitted so far, in order.
    async fn activity_flags(h: &Harness) -> Vec<bool> {
        h.sink
            .events()
            .await
            .into_iter()
            .filter_map(|event| match event {
                ClientEvent::AppEvent {
                    event: AppEventDto::AppLlmActivityChanged { active, .. },
                } => Some(active),
                _ => None,
            })
            .collect()
    }

    fn one_turn() -> Value {
        json!({"messages": [{"role": "user", "content": "帮我起个标题"}]})
    }

    #[tokio::test]
    async fn long_context_has_a_bounded_eight_mebibyte_lane() {
        let model = ChatModel::answering("ok", Some("end_turn"));
        let h = harness(model.clone()).await;
        declare_and_grant(&h);

        let long_but_valid = "x".repeat(70 * 1024);
        let (ok, _, error, code) = chat(
            &h,
            json!({"messages": [{"role": "user", "content": long_but_valid}]}),
        )
        .await;
        assert!(ok, "{error:?} {code:?}");
        assert_eq!(
            model.seen.lock().expect("lock")[0].messages[0].content,
            vec![ChatPart::Text("x".repeat(70 * 1024))]
        );

        let too_large = "x".repeat(8 * 1024 * 1024);
        let (ok, _, _, code) = chat_as(
            &h,
            "too-large",
            json!({"messages": [{"role": "user", "content": too_large}]}),
        )
        .await;
        assert!(!ok);
        assert_eq!(code.as_deref(), Some("payload_too_large"));
    }

    #[tokio::test]
    async fn an_undeclared_llm_capability_is_refused_without_prompting() {
        let h = harness(ChatModel::answering("不该到这里", None)).await;

        let (ok, _, _, code) = timeout(Duration::from_secs(2), chat(&h, one_turn()))
            .await
            .expect("the refusal must not wait on a prompt");
        assert!(!ok);
        assert_eq!(code.as_deref(), Some("capability_not_declared"));
        assert!(
            activity_flags(&h).await.is_empty(),
            "a refused call must not announce activity"
        );
    }

    #[tokio::test]
    async fn a_granted_call_answers_and_brackets_itself_with_activity_events() {
        let model = ChatModel::answering("秋日手账", Some("end_turn"));
        let h = harness(model.clone()).await;
        declare_and_grant(&h);

        let (ok, result, error, code) = chat(
            &h,
            json!({
                "system": "你是这个应用的写作助手",
                "messages": [{"role": "user", "content": "帮我起个标题"}],
                "maxTokens": 256,
                "temperature": 0.4
            }),
        )
        .await;
        assert!(ok, "{error:?} {code:?}");
        assert_eq!(result["text"], "秋日手账");
        assert_eq!(result["stopReason"], "end_turn");
        assert_eq!(result["truncated"], false);
        assert_eq!(
            activity_flags(&h).await,
            vec![true, false],
            "the indicator must be switched on and back off exactly once"
        );

        let seen = &model.seen.lock().expect("lock")[0];
        assert_eq!(seen.max_tokens, 256);
        assert_eq!(seen.temperature, Some(0.4));
        assert_eq!(seen.system.as_deref(), Some("你是这个应用的写作助手"));
        assert_eq!(seen.cache_scope.as_deref(), Some(h.app_id.as_str()));
    }

    #[tokio::test]
    async fn a_denied_prompt_maps_to_permission_denied_and_still_clears_activity() {
        let h = harness(ChatModel::answering("不该到这里", None)).await;
        let mut manifest = load_manifest(&h.layout).expect("manifest");
        manifest.capabilities.push(AppCapability::Llm);
        save_manifest(&h.layout, &manifest).expect("declare");

        let resolver = {
            let sink = h.sink.clone();
            let broker = h.broker.clone();
            tokio::spawn(async move {
                loop {
                    for event in sink.events().await {
                        if let ClientEvent::AppEvent {
                            event: AppEventDto::AppCapabilityRequested { request },
                        } = event
                        {
                            assert!(
                                request.reason.contains("用量"),
                                "the first-use prompt must say the call spends the user's \
                                 model quota: {}",
                                request.reason
                            );
                            assert!(
                                broker
                                    .resolve_capability(
                                        &request.request_id,
                                        AppAuthorizationDecisionDto::Deny,
                                    )
                                    .await
                            );
                            return;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
        };

        let (ok, _, _, code) = timeout(Duration::from_secs(5), chat(&h, one_turn()))
            .await
            .expect("the denial resolves promptly");
        assert!(!ok);
        assert_eq!(code.as_deref(), Some("permission_denied"));
        assert!(
            activity_flags(&h).await.is_empty(),
            "a denied call never ran, so it must not announce activity"
        );
        resolver.await.expect("resolver completes");
    }

    #[tokio::test]
    async fn an_oversized_answer_is_truncated_on_a_char_boundary() {
        // 30k CJK characters = 90 KB, past the 64 KiB cap; cutting mid-rune
        // would produce invalid UTF-8 in the envelope.
        let long = "字".repeat(30_000);
        let h = harness(ChatModel::answering(&long, Some("end_turn"))).await;
        declare_and_grant(&h);

        let (ok, result, error, code) = chat(&h, one_turn()).await;
        assert!(ok, "{error:?} {code:?}");
        let text = result["text"].as_str().expect("text");
        assert!(text.len() <= 64 * 1024);
        assert!(text.chars().all(|c| c == '字'), "cut mid-rune");
        assert_eq!(result["truncated"], true);
    }

    #[tokio::test]
    async fn a_budget_spent_entirely_on_thinking_is_an_error_not_an_empty_answer() {
        let h = harness(ChatModel::answering("", Some("max_tokens"))).await;
        declare_and_grant(&h);

        let (ok, _, _, code) = chat(&h, one_turn()).await;
        assert!(!ok);
        assert_eq!(
            code.as_deref(),
            Some("llm_truncated"),
            "an empty answer that stopped at max_tokens is a budget defect, not a reply"
        );
    }

    #[tokio::test]
    async fn a_second_concurrent_call_from_the_same_app_is_refused() {
        let h = harness(ChatModel::hanging()).await;
        declare_and_grant(&h);

        let first = {
            let broker = h.broker.clone();
            let app_id = h.app_id.clone();
            tokio::spawn(async move {
                broker
                    .execute_bridge(AppBridgeRequestDto {
                        request_id: "req-hang".into(),
                        app_id,
                        operation: AppBridgeOperationDto::LlmChat,
                        payload_json: Some(one_turn().to_string()),
                    })
                    .await;
            })
        };
        // Wait until the first call has actually taken the slot.
        loop {
            if activity_flags(&h).await.first() == Some(&true) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let (ok, _, _, code) = timeout(Duration::from_secs(2), chat_as(&h, "req-2", one_turn()))
            .await
            .expect("the busy refusal is immediate");
        assert!(!ok);
        assert_eq!(code.as_deref(), Some("llm_busy"));
        first.abort();
    }

    #[tokio::test]
    async fn streaming_is_refused_with_a_dedicated_code() {
        let h = harness(ChatModel::answering("不该到这里", None)).await;
        declare_and_grant(&h);

        let (ok, _, _, code) = chat(
            &h,
            json!({"messages": [{"role": "user", "content": "写点什么"}], "stream": true}),
        )
        .await;
        assert!(!ok);
        assert_eq!(
            code.as_deref(),
            Some("llm_stream_unsupported"),
            "the envelope reserves `stream` for a future push channel; until it \
             exists the request must fail loudly rather than silently answer whole"
        );
    }

    #[tokio::test]
    async fn llm_stream_emits_ordered_text_frames_and_final_response() {
        let model = ChatModel::streaming(vec![
            Ok(LlmEvent::ContentBlockDelta {
                index: 0,
                delta: ContentDelta::TextDelta {
                    text: "第一段".into(),
                },
            }),
            Ok(LlmEvent::ContentBlockDelta {
                index: 0,
                delta: ContentDelta::TextDelta {
                    text: "第二段".into(),
                },
            }),
            Ok(LlmEvent::MessageDelta {
                delta: MessageDeltaPayload {
                    stop_reason: Some("end_turn".into()),
                    stop_details: None,
                },
                usage: None,
            }),
            Ok(LlmEvent::MessageStop),
        ]);
        let h = harness(model).await;
        declare_and_grant(&h);
        let request_id = "stream-request";
        h.broker
            .execute_bridge(AppBridgeRequestDto {
                request_id: request_id.into(),
                app_id: h.app_id.clone(),
                operation: AppBridgeOperationDto::LlmStream,
                payload_json: Some(
                    json!({
                        "messages": [{"role": "user", "content": "流式回答"}],
                        "maxTokens": 128
                    })
                    .to_string(),
                ),
            })
            .await;

        let events = h.sink.events().await;
        let mut data = Vec::new();
        let mut completed_seq = None;
        let response = events
            .into_iter()
            .find_map(|event| match event {
                ClientEvent::AppEvent {
                    event: AppEventDto::AppBridgeStreamFrame { frame, .. },
                } => {
                    match frame {
                        client::protocol::local_apps::AppBridgeStreamFrameDto::Data {
                            seq,
                            data_json,
                            ..
                        } => data.push((seq, data_json)),
                        client::protocol::local_apps::AppBridgeStreamFrameDto::Completed {
                            seq,
                            ..
                        } => completed_seq = Some(seq),
                        _ => {}
                    }
                    None
                }
                ClientEvent::AppEvent {
                    event: AppEventDto::AppBridgeResponse { response },
                } if response.request_id == request_id => Some(response),
                _ => None,
            })
            .expect("stream response");
        assert!(
            response.ok,
            "{:?} {:?}",
            response.error, response.error_code
        );
        assert_eq!(data.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(), [0, 1]);
        assert_eq!(completed_seq, Some(2));
        assert_eq!(
            data.into_iter()
                .map(|(_, body)| serde_json::from_str::<Value>(&body).unwrap()["text"].clone())
                .collect::<Vec<_>>(),
            [json!("第一段"), json!("第二段")]
        );
        let result: Value =
            serde_json::from_str(response.result_json.as_deref().unwrap()).expect("stream result");
        assert_eq!(result["text"], "第一段第二段");
        assert_eq!(result["stopReason"], "end_turn");
        assert_eq!(activity_flags(&h).await, [true, false]);
    }

    #[tokio::test]
    async fn a_malformed_request_is_refused_before_any_prompt() {
        let h = harness(ChatModel::answering("不该到这里", None)).await;
        declare_and_grant(&h);

        for payload in [
            json!({"messages": []}),
            json!({"messages": [{"role": "system", "content": "越权"}]}),
            json!({"messages": (0..21).map(|i| json!({"role": "user", "content": format!("{i}")})).collect::<Vec<_>>()}),
            json!({"messages": [{"role": "user", "content": "ok"}], "temperature": 3.0}),
        ] {
            let (ok, _, _, code) = chat(&h, payload.clone()).await;
            assert!(!ok, "{payload} must be refused");
            assert_eq!(code.as_deref(), Some("llm_request_invalid"), "{payload}");
        }
        assert!(activity_flags(&h).await.is_empty());
    }

    #[tokio::test]
    async fn the_token_budget_is_clamped_rather_than_trusted() {
        let model = ChatModel::answering("好", Some("end_turn"));
        let h = harness(model.clone()).await;
        declare_and_grant(&h);

        let (ok, _, _, _) = chat(
            &h,
            json!({"messages": [{"role": "user", "content": "写点什么"}], "maxTokens": 1_000_000}),
        )
        .await;
        assert!(ok);
        assert_eq!(model.seen.lock().expect("lock")[0].max_tokens, 4_096);
    }

    /// The composition the whole media design exists for: photograph
    /// something, then ask the model about it. The photo must reach the
    /// provider as a real image part — and it should travel by HANDLE so its
    /// bytes do not make an unnecessary base64 round trip through the page.
    #[tokio::test]
    async fn a_captured_photo_can_be_attached_to_a_chat_by_handle() {
        let jpeg = vec![7u8; 120 * 1024];
        let model = ChatModel::answering("这是一只猫", Some("end_turn"));
        let h = harness_with_devices(
            model.clone(),
            crate::mobile::local_apps_device::DeviceCapabilities {
                camera: Some(Arc::new(StubCamera(jpeg.clone()))),
                ..crate::mobile::local_apps_device::DeviceCapabilities::default()
            },
        )
        .await;
        declare_and_grant(&h);
        let mut manifest = load_manifest(&h.layout).expect("manifest");
        manifest.capabilities.push(AppCapability::Camera);
        save_manifest(&h.layout, &manifest).expect("declare camera");
        let mut permissions = load_permissions(&h.layout).expect("permissions");
        permissions.grant(AppCapability::Camera);
        save_permissions(&h.layout, &permissions).expect("grant camera");

        h.broker
            .execute_bridge(AppBridgeRequestDto {
                request_id: "cap-1".into(),
                app_id: h.app_id.clone(),
                operation: AppBridgeOperationDto::CapturePhoto,
                payload_json: Some("{}".into()),
            })
            .await;
        let capture = h
            .sink
            .events()
            .await
            .into_iter()
            .rev()
            .find_map(|event| match event {
                ClientEvent::AppEvent {
                    event: AppEventDto::AppBridgeResponse { response },
                } if response.request_id == "cap-1" => Some(response),
                _ => None,
            })
            .expect("capture response");
        assert!(capture.ok, "{:?}", capture.error);
        let capture: Value =
            serde_json::from_str(&capture.result_json.expect("result")).expect("json");
        let media_id = capture["mediaId"].as_str().expect("mediaId").to_string();
        assert!(
            capture["base64"].as_str().expect("base64").len() > 64 * 1024,
            "this fixture must be large enough to prove the handle avoids a meaningful \
             base64 round trip"
        );

        let (ok, result, error, code) = chat_as(
            &h,
            "chat-1",
            json!({
                "messages": [{
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "这是什么？"},
                        {"type": "image", "mediaId": media_id}
                    ]
                }]
            }),
        )
        .await;
        assert!(ok, "{error:?} {code:?}");
        assert_eq!(result["text"], "这是一只猫");

        let seen = &model.seen.lock().expect("lock")[0];
        assert_eq!(seen.messages[0].content.len(), 2);
        assert_eq!(
            seen.messages[0].content[0],
            ChatPart::Text("这是什么？".into())
        );
        match &seen.messages[0].content[1] {
            ChatPart::Image { media_type, base64 } => {
                assert_eq!(media_type, "image/jpeg");
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(base64)
                    .expect("decodes");
                assert_eq!(decoded, jpeg, "the model must see the captured bytes");
            }
            other => panic!("the photo must reach the provider as an image block: {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_unknown_or_foreign_media_handle_is_refused() {
        let h = harness(ChatModel::answering("不该到这里", None)).await;
        declare_and_grant(&h);

        let (ok, _, error, code) = chat(
            &h,
            json!({"messages": [{
                "role": "user",
                "content": [{"type": "image", "mediaId": "media-999"}]
            }]}),
        )
        .await;
        assert!(!ok);
        assert_eq!(code.as_deref(), Some("llm_request_invalid"));
        assert!(
            error.unwrap_or_default().contains("expired"),
            "the message must tell the app to capture again"
        );
    }

    /// Audio is the one attachment kind this stack cannot honour, so it must
    /// fail loudly and name the way that does work — a silently dropped
    /// recording would look like the model ignored what it heard.
    #[tokio::test]
    async fn an_audio_attachment_is_refused_and_points_at_transcription() {
        let h = harness(ChatModel::answering("不该到这里", None)).await;
        declare_and_grant(&h);

        let (ok, _, error, code) = chat(
            &h,
            json!({"messages": [{
                "role": "user",
                "content": [{
                    "type": "media",
                    "mimeType": "audio/m4a",
                    "base64": "AAAA"
                }]
            }]}),
        )
        .await;
        assert!(!ok);
        assert_eq!(code.as_deref(), Some("llm_media_unsupported"));
        assert!(
            error.unwrap_or_default().contains("transcribeSpeech"),
            "the refusal must name the operation that does work"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_hung_provider_call_times_out_and_releases_the_slot() {
        let h = harness(ChatModel::hanging()).await;
        declare_and_grant(&h);

        let (ok, _, _, code) = chat(&h, one_turn()).await;
        assert!(!ok);
        assert_eq!(code.as_deref(), Some("timeout"));
        assert_eq!(
            activity_flags(&h).await,
            vec![true, false],
            "a timed-out call must still clear the indicator"
        );
    }
}
