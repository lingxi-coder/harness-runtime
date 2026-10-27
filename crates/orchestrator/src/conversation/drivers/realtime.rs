//! Native realtime Agent execution through the ordinary Harness tool dispatcher.
use super::*;
use base64::Engine as _;
use futures::{stream::FuturesUnordered, StreamExt};
use lingxi_core::types::{utf16_json::Utf16JsonProjection, MessageId, ToolUseId};
use lingxi_llm_client::realtime::{
    RealtimeClose, RealtimeControl, RealtimeEvent, RealtimeEvents, RealtimeHistoryItem,
    RealtimeInput, RealtimeRole, RealtimeTranscriptDirection,
};
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
pub use tool_api::{MediaAgentCapabilityContext, MediaAgentHandoffRequest};

/// Host snapshot used to configure an SDK agent connector before dispatch.
#[derive(Clone, Debug)]
pub struct RealtimeAgentContext {
    /// Owning current Agent session.
    pub session_id: String,
    /// Exact current session provider profile.
    pub profile_name: Option<String>,
    /// Trusted profile/account namespace pinned for this realtime session.
    pub account_scope: Option<String>,
    /// Current Agent system instructions.
    pub instructions: String,
    /// Normalized text and tool history; connector imports it without a response.
    pub history: Vec<RealtimeHistoryItem>,
    /// Current permission-filtered tool catalog in function schema form.
    pub tools: Vec<serde_json::Value>,
}
/// Bounded host controls and native playback evidence.
#[derive(Debug)]
pub enum RealtimeAgentInput {
    /// Provider-neutral input, excluding model-controlled route/configuration.
    Provider(RealtimeInput),
    /// Native player drained this item; only now may its transcript enter history.
    PlaybackCompleted { item_id: Option<String> },
}

/// Bounded transport endpoints for one host-opened native realtime session.
///
/// This contract lives with the Agent orchestration layer so app hosts can
/// implement it without depending on the full Harness runtime.
pub struct RealtimeAgentSessionIo {
    /// Provider-neutral control with bounded input queues.
    pub control: RealtimeControl,
    /// Bounded normalized provider event stream.
    pub events: RealtimeEvents,
    /// Bounded host audio, image, and transcript input queue.
    pub inputs: mpsc::Receiver<RealtimeAgentInput>,
    /// Bounded native playback/event output queue.
    pub output: mpsc::Sender<RealtimeEvent>,
    /// Realtime session limits.
    pub limits: RealtimeAgentLimits,
    /// Whether the selected realtime provider accepts image inputs.
    pub image_input_supported: bool,
}

/// Host adapter that opens realtime IO for a trusted Conversation media session.
///
/// Harness calls this only after the current tool result has been committed and
/// ordinary streamed tool work has drained. Implementations resolve provider
/// config and credentials from trusted host state, never the tool request.
#[async_trait::async_trait]
pub trait MediaConversationAttachment: Send + Sync {
    /// Structural mode union used when the tool catalog is built. Current
    /// session/profile support is checked dynamically by `supports_mode`.
    fn supported_modes(&self) -> Vec<media_api::MediaSessionMode> {
        vec![media_api::MediaSessionMode::Audio]
    }

    /// Modes usable by this particular session's registered provider and
    /// settings. Implementations backed by a shared app host should resolve by
    /// this exact owner and profile, never by whichever profile was most recently set.
    fn supported_modes_for_session(
        &self,
        _context: &MediaAgentCapabilityContext,
    ) -> Vec<media_api::MediaSessionMode> {
        self.supported_modes()
    }

    /// Whether this attachment can continue a live Agent turn for the selected
    /// audio/video mode for this exact session and provider profile. Video-only
    /// requires a video-capable Agent input lane.
    fn supports_mode(
        &self,
        _context: &MediaAgentCapabilityContext,
        mode: media_api::MediaSessionMode,
    ) -> bool {
        self.supported_modes().contains(&mode)
    }

    /// Open bounded realtime transport over the already-open, owner-validated
    /// media session channel supplied by Harness.
    async fn open(
        &self,
        request: MediaAgentHandoffRequest,
        prepared: RealtimeAgentContext,
        audio: std::sync::Arc<dyn media_api::MediaAudioChannel>,
    ) -> Result<RealtimeAgentSessionIo, String>;
}

/// Hard session and resource bounds, independent from chat retry budgets.
#[derive(Clone, Copy, Debug)]
pub struct RealtimeAgentLimits {
    /// Absolute session lifetime, including tool and permission waits.
    pub max_duration: Duration,
    /// Idle lifetime; paused while a tool or permission request is pending.
    pub idle_timeout: Duration,
    /// Maximum simultaneous tool requests.
    pub max_pending_tools: usize,
    /// Maximum accumulated text bytes per transcript item.
    pub max_transcript_bytes: usize,
    /// Maximum pending transcript items awaiting playback acknowledgements.
    pub max_pending_transcripts: usize,
    /// Maximum decoded image bytes accepted from one tool or live input.
    pub max_image_bytes_per_input: usize,
    /// Maximum number of tool/live images accepted in one realtime session.
    pub max_image_inputs: usize,
    /// Aggregate decoded image bytes accepted in one realtime session.
    pub max_image_bytes_per_session: usize,
}
impl Default for RealtimeAgentLimits {
    fn default() -> Self {
        Self {
            max_duration: Duration::from_secs(30 * 60),
            idle_timeout: Duration::from_secs(2 * 60),
            max_pending_tools: 8,
            max_transcript_bytes: 1024 * 1024,
            max_pending_transcripts: 64,
            max_image_bytes_per_input: 8 * 1024 * 1024,
            max_image_inputs: 512,
            max_image_bytes_per_session: 64 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RealtimeImageInput {
    data: Vec<u8>,
    mime_type: String,
}

struct RealtimeToolDispatchResult {
    call_id: String,
    output: serde_json::Value,
    prevent: bool,
    images: Vec<RealtimeImageInput>,
}
/// A session's terminal reason. Provider reconnect/replay is never automatic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RealtimeAgentEnd {
    Closed,
    Cancelled,
    Deadline,
    Idle,
    ToolStopped,
}
fn failure(message: impl Into<String>) -> OrchestratorError {
    OrchestratorError::Internal(message.into())
}
fn source_value(source: &Utf16JsonProjection) -> Result<serde_json::Value, OrchestratorError> {
    let raw = source
        .to_json_string()
        .map_err(|error| failure(error.to_string()))?;
    serde_json::from_str(&raw).map_err(|error| {
        failure(format!(
            "realtime JSON cannot represent this source: {error}"
        ))
    })
}
fn source_text(source: &Utf16JsonProjection) -> Result<String, OrchestratorError> {
    let units = source
        .string_units("")
        .ok_or_else(|| failure("realtime text must be a string"))?;
    String::from_utf16(&units).map_err(|error| {
        failure(format!(
            "realtime text cannot represent this source: {error}"
        ))
    })
}

fn content_blocks_contain_image(
    content_blocks: &Utf16JsonProjection,
) -> Result<bool, OrchestratorError> {
    let Some(count) = content_blocks.array_len("") else {
        return Err(failure(
            "historical tool-result content blocks are not a typed array",
        ));
    };
    for index in 0..count {
        let pointer = format!("/{index}");
        if content_blocks.string_at(&format!("{pointer}/type")) == Some("image") {
            return Ok(true);
        }
        let block = content_blocks
            .subprojection(&pointer)
            .map_err(|error| failure(error.to_string()))?;
        if value_contains_inline_image_payload(&source_value(&block)?) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn value_contains_inline_image_payload(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(object) => {
            let typed_image =
                object.get("type").and_then(serde_json::Value::as_str) == Some("image");
            let has_inline_source = object
                .get("source")
                .and_then(serde_json::Value::as_object)
                .is_some_and(|source| {
                    source.get("type").and_then(serde_json::Value::as_str) == Some("base64")
                        && source
                            .get("data")
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(|data| !data.is_empty())
                });
            let has_inline_file = object
                .get("file")
                .and_then(serde_json::Value::as_object)
                .is_some_and(|file| {
                    file.get("base64")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|data| !data.is_empty())
                });
            (typed_image && (has_inline_source || has_inline_file))
                || object.values().any(value_contains_inline_image_payload)
        }
        serde_json::Value::Array(values) => values.iter().any(value_contains_inline_image_payload),
        serde_json::Value::String(source) => serde_json::from_str::<serde_json::Value>(source)
            .ok()
            .is_some_and(|parsed| value_contains_inline_image_payload(&parsed)),
        _ => false,
    }
}

fn image_inputs_from_tool_blocks(
    content_blocks: &Utf16JsonProjection,
    limits: RealtimeAgentLimits,
) -> Result<(Vec<RealtimeImageInput>, Vec<serde_json::Value>), OrchestratorError> {
    let blocks = source_value(content_blocks)?;
    let Some(blocks) = blocks.as_array() else {
        return Err(failure(
            "realtime tool-result content blocks are not an image-aware array",
        ));
    };
    let mut images = Vec::new();
    let mut safe_blocks = Vec::with_capacity(blocks.len());
    for block in blocks {
        if block.get("type").and_then(serde_json::Value::as_str) != Some("image") {
            if value_contains_inline_image_payload(block) {
                return Err(failure(
                    "realtime tool output embedded image bytes outside a typed image block",
                ));
            }
            safe_blocks.push(block.clone());
            continue;
        }
        let source = block
            .get("source")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| failure("realtime image block has no typed source"))?;
        if source.get("type").and_then(serde_json::Value::as_str) != Some("base64") {
            return Err(failure(
                "realtime image result has no owner-resolved inline bytes",
            ));
        }
        let mime_type = source
            .get("media_type")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| failure("realtime image block has no MIME type"))?;
        let encoded = source
            .get("data")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| failure("realtime image block has no encoded image bytes"))?;
        let max_encoded_len = limits
            .max_image_bytes_per_input
            .saturating_add(2)
            .checked_div(3)
            .unwrap_or(0)
            .saturating_mul(4);
        if encoded.len() > max_encoded_len {
            return Err(failure(
                "realtime tool image exceeds the configured payload limit",
            ));
        }
        let data = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| failure("realtime tool image is not valid base64"))?;
        validate_realtime_image_bytes(&data, mime_type, limits)?;
        let sniffed = tool_api::util::image_sniff::sniff_image_media_type(&data)
            .ok_or_else(|| failure("realtime tool image bytes are not a recognized image"))?;
        if sniffed != mime_type {
            return Err(failure(
                "realtime tool image MIME type does not match its bytes",
            ));
        }
        safe_blocks.push(serde_json::json!({
            "type": "image_input",
            "mime_type": sniffed,
            "byte_len": data.len(),
        }));
        images.push(RealtimeImageInput {
            data,
            mime_type: sniffed.to_owned(),
        });
    }
    Ok((images, safe_blocks))
}

fn validate_realtime_image_bytes(
    data: &[u8],
    mime_type: &str,
    limits: RealtimeAgentLimits,
) -> Result<(), OrchestratorError> {
    if data.is_empty() || data.len() > limits.max_image_bytes_per_input {
        return Err(failure(
            "realtime image input is empty or exceeds the per-image byte limit",
        ));
    }
    if !mime_type.starts_with("image/") || mime_type.len() > 64 {
        return Err(failure("realtime image MIME type is unsupported"));
    }
    let sniffed = tool_api::util::image_sniff::sniff_image_media_type(data)
        .ok_or_else(|| failure("realtime image input bytes are not a recognized image"))?;
    if sniffed != mime_type {
        return Err(failure("realtime image MIME type does not match its bytes"));
    }
    Ok(())
}

fn accept_realtime_image(
    data: &[u8],
    mime_type: &str,
    image_input_supported: bool,
    limits: RealtimeAgentLimits,
    image_count: &mut usize,
    image_bytes: &mut usize,
) -> Result<(), OrchestratorError> {
    if !image_input_supported {
        return Err(failure(
            "unsupported: selected native realtime provider does not support image input",
        ));
    }
    validate_realtime_image_bytes(data, mime_type, limits)?;
    let next_count = image_count
        .checked_add(1)
        .ok_or_else(|| failure("realtime image count overflow"))?;
    let next_bytes = image_bytes
        .checked_add(data.len())
        .ok_or_else(|| failure("realtime image byte count overflow"))?;
    if next_count > limits.max_image_inputs {
        return Err(failure("realtime image input count limit reached"));
    }
    if next_bytes > limits.max_image_bytes_per_session {
        return Err(failure("realtime image session byte limit reached"));
    }
    *image_count = next_count;
    *image_bytes = next_bytes;
    Ok(())
}

const REALTIME_HELD_MEDIA_RESOURCES: [tool_api::tool_trait::MediaResource; 2] = [
    tool_api::tool_trait::MediaResource::Microphone,
    tool_api::tool_trait::MediaResource::Speaker,
];

/// Convert model-visible history without dropping unsupported user media.
pub fn realtime_history(
    history: &[ConversationMessage],
) -> Result<Vec<RealtimeHistoryItem>, OrchestratorError> {
    let mut output = Vec::new();
    let mut tool_ids = HashMap::new();
    for message in history {
        let (role, blocks) = match message {
            ConversationMessage::System {
                content,
                api_system,
                ..
            } => {
                if api_system.is_some() {
                    output.push(RealtimeHistoryItem::Message {
                        item_id: None,
                        role: RealtimeRole::System,
                        text: source_text(content)?,
                    });
                }
                continue;
            }
            ConversationMessage::User {
                content,
                is_visible_in_transcript_only,
                ..
            } => {
                if *is_visible_in_transcript_only {
                    continue;
                }
                (RealtimeRole::User, content)
            }
            ConversationMessage::Assistant { content, .. } => (RealtimeRole::Assistant, content),
        };
        for block in blocks {
            match block {
                ContentBlock::Text { text, .. } | ContentBlock::TextJsUtf16 { text, .. } => {
                    if !text.is_empty() {
                        output.push(RealtimeHistoryItem::Message {
                            item_id: None,
                            role,
                            text: text.clone(),
                        });
                    }
                }
                ContentBlock::ToolUse {
                    id,
                    name,
                    input,
                    provider_id,
                    ..
                } => {
                    let call_id = provider_id.clone().unwrap_or_else(|| id.to_string());
                    tool_ids.insert(id.clone(), (call_id.clone(), name.clone()));
                    output.push(RealtimeHistoryItem::ToolCall {
                        item_id: None,
                        call_id,
                        name: name.clone(),
                        arguments: source_value(input)?,
                    });
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    content_blocks,
                    provider_tool_use_id,
                    ..
                } => {
                    if let Some(content_blocks) = content_blocks {
                        if content_blocks_contain_image(content_blocks)? {
                            return Err(failure(
                                "native realtime cannot import historical image tool results; start a fresh realtime session to provide images as live inputs",
                            ));
                        }
                    }
                    let content_value = source_value(content)?;
                    if value_contains_inline_image_payload(&content_value) {
                        return Err(failure(
                            "native realtime cannot import historical image bytes embedded in tool-result JSON",
                        ));
                    }
                    let (mapped, name) = tool_ids
                        .get(tool_use_id)
                        .cloned()
                        .ok_or_else(|| failure("realtime history has an unmatched tool result"))?;
                    output.push(RealtimeHistoryItem::ToolResult {
                        item_id: None,
                        call_id: provider_tool_use_id.clone().unwrap_or(mapped),
                        name: Some(name),
                        output: serde_json::json!({"content":content_value,"is_error":is_error,"content_blocks":content_blocks}),
                    });
                }
                ContentBlock::Image { .. } => {
                    return Err(failure(
                        "native realtime cannot import historical image content; start a fresh realtime session to provide images as live inputs",
                    ));
                }
                // Signed provider reasoning is not portable between conversation
                // protocols, and is never presented as ordinary transcript text.
                ContentBlock::Thinking { .. }
                | ContentBlock::RedactedThinking { .. }
                | ContentBlock::ProviderContent { .. } => {}
                _ => {
                    return Err(failure(
                        "native realtime cannot import this session's media/history block",
                    ));
                }
            }
        }
    }
    Ok(output)
}
struct RealtimeGuard {
    control: RealtimeControl,
    cancel: CancellationToken,
}
impl Drop for RealtimeGuard {
    fn drop(&mut self) {
        self.cancel.cancel();
        let control = self.control.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = tokio::time::timeout(
                    Duration::from_secs(1),
                    control.abort(RealtimeClose {
                        code: 1000,
                        reason: "host realtime ended".into(),
                    }),
                )
                .await;
            });
        }
    }
}
#[derive(Default)]
struct TranscriptLedger {
    output: HashMap<Option<String>, String>,
    heard: HashSet<Option<String>>,
    discarded: HashSet<Option<String>>,
    finalized_input: HashSet<Option<String>>,
    finalized_output: HashSet<Option<String>>,
    generated: HashSet<Option<String>>,
}
impl TranscriptLedger {
    fn interrupt(&mut self) {
        self.discarded.extend(self.output.keys().cloned());
        self.discarded.extend(self.generated.drain());
        self.discarded.extend(self.heard.iter().cloned());
        self.output.clear();
        self.heard.clear();
    }
    fn output_final(
        &mut self,
        key: Option<String>,
        text: String,
        limits: RealtimeAgentLimits,
    ) -> Result<Option<String>, OrchestratorError> {
        if self.discarded.contains(&key) || self.finalized_output.contains(&key) {
            return Ok(None);
        }
        if text.len() > limits.max_transcript_bytes {
            return Err(failure("realtime transcript exceeds its byte bound"));
        }
        if self.heard.remove(&key) {
            self.finalized_output.insert(key.clone());
            self.generated.remove(&key);
            return Ok(Some(text));
        }
        if !self.output.contains_key(&key) && self.output.len() >= limits.max_pending_transcripts {
            return Err(failure("realtime playback transcript queue is full"));
        }
        self.output.insert(key, text);
        Ok(None)
    }
    fn playback(&mut self, key: Option<String>) -> Option<String> {
        if self.discarded.contains(&key) || self.finalized_output.contains(&key) {
            return None;
        }
        let text = self.output.remove(&key);
        if text.is_none() {
            if self.generated.contains(&key) {
                self.heard.insert(key);
            }
        } else {
            self.finalized_output.insert(key.clone());
            self.generated.remove(&key);
        }
        text
    }
}
impl ConversationOrchestrator {
    /// Confirm that the exact successful `media_session_start` call named by a
    /// trusted handoff callback is present in the current Agent history.
    pub async fn media_handoff_result_committed(
        &self,
        session_id: &str,
        assistant_message_id: &MessageId,
        assistant_message: &ConversationMessage,
        tool_use_id: &ToolUseId,
        media_session_ref: &str,
    ) -> bool {
        let session = self.session.lock().await;
        if session.session_id.as_uuid().to_string() != session_id
            || assistant_message.id() != *assistant_message_id
        {
            return false;
        }
        let source_tool_use = match assistant_message {
            ConversationMessage::Assistant { content, .. } => content.iter().find(|block| {
                matches!(
                    block,
                    ContentBlock::ToolUse { id, name, .. }
                        if id == tool_use_id && name == "media_session_start"
                )
            }),
            _ => None,
        };
        let Some(source_tool_use) = source_tool_use else {
            return false;
        };
        // Streamed dispatch binds the callback to a per-block assistant row,
        // while in-memory history keeps the merged assistant message. Verify
        // the immutable source block itself instead of comparing those IDs.
        let call_committed = session.history.iter().any(|message| match message {
            ConversationMessage::Assistant { content, .. } => {
                content.iter().any(|block| block == source_tool_use)
            }
            _ => false,
        });
        if !call_committed {
            return false;
        }
        session.history.iter().any(|message| match message {
            ConversationMessage::User { content, .. } => content.iter().any(|block| {
                let ContentBlock::ToolResult {
                    tool_use_id: result_id,
                    content,
                    is_error,
                    ..
                } = block
                else {
                    return false;
                };
                if result_id != tool_use_id || is_error == &Some(true) {
                    return false;
                }
                let parsed_session_ref = content
                    .string_at("")
                    .and_then(|model_text| {
                        serde_json::from_str::<serde_json::Value>(model_text).ok()
                    })
                    .and_then(|value| {
                        value
                            .get("session_ref")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                    });
                parsed_session_ref.as_deref() == Some(media_session_ref)
            }),
            _ => false,
        })
    }

    /// Resolve the current inference route without requesting credentials or
    /// selecting a provider from model spelling. Default sessions use the same
    /// registry resolver as an ordinary Agent request.
    pub async fn current_audio_profile(&self) -> Result<String, OrchestratorError> {
        let (model, profile) = self.current_prompt_route().await;
        self.resolve_audio_profile(&model, profile)
    }
    /// Atomically bind a native caller to one session and its effective profile.
    pub async fn current_audio_binding(&self) -> Result<(String, String), OrchestratorError> {
        let session = self.session.lock().await;
        let profile = self.resolve_audio_profile(&session.model, session.model_profile.clone())?;
        Ok((session.session_id.as_uuid().to_string(), profile))
    }
    fn resolve_audio_profile(
        &self,
        model: &str,
        profile: Option<String>,
    ) -> Result<String, OrchestratorError> {
        match profile.filter(|value| !value.trim().is_empty()) {
            Some(profile) => Ok(profile),
            None => self
                .api
                .resolve_media_route(model, None)
                .map(|route| route.main.profile_name)
                .map_err(|_| {
                    failure("current inference route has no authoritative audio provider profile")
                }),
        }
    }
    /// Prepare current Agent history and tool policy for an SDK native connector.
    pub async fn prepare_realtime_agent(&self) -> Result<RealtimeAgentContext, OrchestratorError> {
        self.cancel_idle_compaction();
        let _gate = self.turn_gate.lock().await;
        let (session_id, history) = {
            let session = self.session.lock().await;
            (
                session.session_id.as_uuid().to_string(),
                session.model_context_history(),
            )
        };
        let profile_name = self.current_audio_profile().await?;
        let account_scope = format!("profile:{profile_name}");
        let tools = self
            .build_wire_tools()
            .await?
            .0
            .into_iter()
            .filter(|tool| {
                self.find_dispatchable_tool(&tool.name)
                    .is_some_and(|registered| {
                        !registered
                            .catalog_execution_metadata()
                            .conflicts_with(&REALTIME_HELD_MEDIA_RESOURCES)
                    })
            })
            .map(|tool| {
                Ok(serde_json::json!({
                    "name": tool.name,
                    "description": source_value(&tool.description)?,
                    "parameters": source_value(&tool.input_schema)?,
                }))
            })
            .collect::<Result<Vec<_>, OrchestratorError>>()?;
        Ok(RealtimeAgentContext {
            session_id,
            profile_name: Some(profile_name),
            account_scope: Some(account_scope),
            instructions: self.effective_system_prompt().await?,
            history: realtime_history(&history)?,
            tools,
        })
    }
    async fn append_realtime_text(&self, text: String, user: bool) {
        if text.trim().is_empty() {
            return;
        }
        let message = if user {
            ConversationMessage::user(MessageId::new(), text)
        } else {
            ConversationMessage::Assistant {
                id: MessageId::new(),
                content: vec![ContentBlock::Text {
                    text,
                    citations: None,
                }],
                stop_reason: Some("end_turn".into()),
                per_turn_effort: None,
                resumable: None,
            }
        };
        self.session.lock().await.history.push(message.clone());
        self.persist_message_to_jsonl(&message).await;
    }
    async fn realtime_dispatch_tool(
        &self,
        call_id: String,
        id: ToolUseId,
        name: String,
        input: serde_json::Value,
        assistant: MessageId,
        cancel: CancellationToken,
        limits: RealtimeAgentLimits,
    ) -> Result<RealtimeToolDispatchResult, OrchestratorError> {
        let input = Utf16JsonProjection::from_serde_value(&input)
            .map_err(|error| failure(error.to_string()))?;
        let tool = self
            .find_dispatchable_tool(&name)
            .ok_or_else(|| failure("realtime Agent tool is no longer available"))?;
        if tool
            .execution_metadata(&input)
            .conflicts_with(&REALTIME_HELD_MEDIA_RESOURCES)
        {
            let result = ContentBlock::ToolResult {
                tool_use_id: id,
                content: "This operation needs a microphone or speaker currently in use by native realtime.".into(),
                is_error: Some(true),
                provider_tool_use_id: Some(call_id.clone()),
                content_blocks: None,
            };
            let message = ConversationMessage::User {
                id: MessageId::new(),
                content: vec![result.clone()],
                is_meta: false,
                is_compact_summary: false,
                is_visible_in_transcript_only: false,
                api_message_override: None,
                interrupted_by_shutdown: None,
                refused_message_id: None,
                origin: None,
                turn_origin: None,
                turn_companion: None,
                usage_limit_note: None,
            };
            self.session.lock().await.history.push(message.clone());
            self.persist_message_to_jsonl(&message).await;
            return Ok(RealtimeToolDispatchResult {
                call_id,
                output: serde_json::json!({
                    "is_error": true,
                    "content": "This operation needs a microphone or speaker currently in use by native realtime."
                }),
                prevent: false,
                images: Vec::new(),
            });
        }
        let uses = vec![(id.clone(), name, input, Some(call_id.clone()))];
        let deferred = crate::turn_loop::tool_dispatch::dispatch_tool_uses_tracked_deferred(
            self,
            &uses,
            Some(cancel),
            Some(assistant),
        )
        .await?;
        let (results, prevent, injected, modifiers) =
            crate::turn_loop::tool_dispatch::finish_direct_tool_dispatch(self, deferred).await?;
        let requires_restart = !injected.is_empty() || !modifiers.is_empty();
        let mut images = Vec::new();
        let value = match results.first() {
            Some(ContentBlock::ToolResult {
                content,
                is_error,
                content_blocks,
                ..
            }) => {
                let content = source_value(content)?;
                if let Some(content_blocks) = content_blocks {
                    let (tool_images, safe_blocks) =
                        image_inputs_from_tool_blocks(content_blocks, limits)?;
                    images = tool_images;
                    serde_json::json!({
                        "content": content,
                        "is_error": is_error,
                        "content_blocks": safe_blocks
                    })
                } else {
                    if value_contains_inline_image_payload(&content) {
                        return Err(failure(
                            "realtime tool result contains image bytes without typed image blocks",
                        ));
                    }
                    serde_json::json!({"content":content,"is_error":is_error,"content_blocks":content_blocks})
                }
            }
            Some(block) => {
                let value =
                    serde_json::to_value(block).map_err(|error| failure(error.to_string()))?;
                if value_contains_inline_image_payload(&value) {
                    return Err(failure(
                        "realtime tool result contains image bytes without typed image blocks",
                    ));
                }
                value
            }
            None => serde_json::json!({"is_error":true,"content":"tool produced no result"}),
        };
        let message = ConversationMessage::User {
            id: MessageId::new(),
            content: results,
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
            api_message_override: None,
            interrupted_by_shutdown: None,
            refused_message_id: None,
            origin: None,
            turn_origin: None,
            turn_companion: None,
            usage_limit_note: None,
        };
        self.session.lock().await.history.push(message.clone());
        self.persist_message_to_jsonl(&message).await;
        self.flush_hook_attachments(&id).await;
        crate::turn_loop::append_tool_injected_messages(self, injected, None).await;
        crate::turn_loop::apply_model_context_modifiers(self, modifiers).await?;
        Ok(RealtimeToolDispatchResult {
            call_id,
            output: value,
            prevent: prevent || requires_restart,
            images,
        })
    }
    /// Drive one native session while keeping the shared Agent permission,
    /// sandbox, hook, cancellation and transcript paths authoritative.
    /// The host must already run the SDK driver and import `prepared.history`.
    pub async fn run_realtime_agent(
        self: &Arc<Self>,
        prepared: RealtimeAgentContext,
        control: RealtimeControl,
        mut events: RealtimeEvents,
        mut inputs: mpsc::Receiver<RealtimeAgentInput>,
        output: mpsc::Sender<RealtimeEvent>,
        limits: RealtimeAgentLimits,
        image_input_supported: bool,
        cancel: CancellationToken,
    ) -> Result<RealtimeAgentEnd, OrchestratorError> {
        let session_cancel = cancel.clone();
        let _guard = RealtimeGuard {
            control: control.clone(),
            cancel: session_cancel.clone(),
        };
        if !control.capabilities().agent_conversation() {
            return Err(failure(
                "provider realtime lacks required Agent history, tools or transcripts",
            ));
        }
        if limits.max_duration.is_zero()
            || limits.idle_timeout.is_zero()
            || limits.max_pending_tools == 0
            || limits.max_transcript_bytes == 0
            || limits.max_pending_transcripts == 0
            || limits.max_image_bytes_per_input == 0
            || limits.max_image_inputs == 0
            || limits.max_image_bytes_per_session == 0
        {
            return Err(failure("native realtime resource limits must be positive"));
        }
        self.cancel_idle_compaction();
        let _gate = tokio::select! {biased;_ = session_cancel.cancelled()=>return Ok(RealtimeAgentEnd::Cancelled),gate=self.turn_gate.lock()=>gate};
        {
            let session = self.session.lock().await;
            if session.session_id.as_uuid().to_string() != prepared.session_id
                || realtime_history(&session.model_context_history())? != prepared.history
            {
                return Err(failure(
                    "Agent session history changed before realtime admission",
                ));
            }
        }
        let current_profile = self.current_audio_profile().await?;
        if Some(current_profile.clone()) != prepared.profile_name
            || Some(format!("profile:{current_profile}")) != prepared.account_scope
        {
            return Err(failure(
                "Agent provider profile changed before realtime admission",
            ));
        }
        let allowed: HashSet<String> = prepared
            .tools
            .iter()
            .filter_map(|tool| {
                tool.get("name")
                    .and_then(|name| name.as_str())
                    .map(str::to_owned)
            })
            .collect();
        let deadline = Instant::now() + limits.max_duration;
        let mut idle = Instant::now() + limits.idle_timeout;
        let mut ledger = TranscriptLedger::default();
        let mut tool_tokens: HashMap<String, CancellationToken> = HashMap::new();
        let mut seen_calls = HashSet::new();
        let mut usage_turns = HashSet::new();
        let mut pending: FuturesUnordered<
            std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<RealtimeToolDispatchResult, OrchestratorError>,
                        > + Send,
                >,
            >,
        > = FuturesUnordered::new();
        let mut image_input_count = 0usize;
        let mut image_input_bytes = 0usize;
        // Each native call enters the common batch dispatcher independently.
        // Serialize those batches so mutating tools cannot overlap outside its
        // per-batch scheduler's concurrency safety boundary.
        let tool_dispatch = Arc::new(tokio::sync::Mutex::new(()));
        let result = async {
        loop {
            tokio::select! {
                biased;
                _ = session_cancel.cancelled()=>return Ok(RealtimeAgentEnd::Cancelled),
                _ = tokio::time::sleep_until(deadline)=>return Ok(RealtimeAgentEnd::Deadline),
                _ = tokio::time::sleep_until(idle), if pending.is_empty()=>return Ok(RealtimeAgentEnd::Idle),
                completed = pending.next(), if !pending.is_empty()=>{
                    let completed=completed.expect("nonempty tool futures")?;let cancelled = tool_tokens.remove(&completed.call_id).is_some_and(|token| token.is_cancelled());idle=Instant::now()+limits.idle_timeout;
                    if cancelled { continue; }
                    for image in &completed.images {
                        accept_realtime_image(&image.data, &image.mime_type, image_input_supported, limits, &mut image_input_count, &mut image_input_bytes)?;
                    }
                    control.send(RealtimeInput::ToolResult{call_id:completed.call_id,output:completed.output}).map_err(|error|failure(error.to_string()))?;
                    for image in completed.images {
                        control.send(RealtimeInput::Image { data: image.data.into(), mime_type: image.mime_type }).map_err(|error|failure(error.to_string()))?;
                    }
                    if completed.prevent{return Ok(RealtimeAgentEnd::ToolStopped);}
                    // Submit every concurrent result before requesting the next response.
                    if pending.is_empty(){control.continue_response().map_err(|error|failure(error.to_string()))?;}
                },
                input = inputs.recv()=>{
                    let Some(input)=input else{return Ok(RealtimeAgentEnd::Closed);};idle=Instant::now()+limits.idle_timeout;
                    match input {
                        RealtimeAgentInput::PlaybackCompleted{item_id}=>{if let Some(text)=ledger.playback(item_id){self.append_realtime_text(text,false).await;}},
                        RealtimeAgentInput::Provider(input)=>{
                            // Hosts cannot bypass the Agent dispatcher with forged results/history.
                            if matches!(input,RealtimeInput::ImportHistory{..}|RealtimeInput::DeleteItem{..}|RealtimeInput::ToolResult{..}|RealtimeInput::ToolResults{..}){return Err(failure("host realtime input cannot replace Agent history or tool results"));}
                            if let RealtimeInput::Image { data, mime_type } = &input {
                                accept_realtime_image(data.as_ref(), mime_type, image_input_supported, limits, &mut image_input_count, &mut image_input_bytes)?;
                            }
                            if matches!(input,RealtimeInput::Interrupt){ledger.interrupt();for token in tool_tokens.values(){CancellationToken::cancel(token);}}
                            if let RealtimeInput::Text(text)=&input{self.append_realtime_text(text.clone(),true).await;}
                            control.send(input).map_err(|error|failure(error.to_string()))?;
                        }
                    }
                },
                event = events.next()=>{
                    let Some(event)=event else{return Ok(RealtimeAgentEnd::Closed);};idle=Instant::now()+limits.idle_timeout;
                    if seen_calls.len() + usage_turns.len() + ledger.finalized_input.len() + ledger.finalized_output.len() + ledger.discarded.len() > limits.max_pending_transcripts.saturating_mul(64) { return Err(failure("realtime session history resource bound reached")); }
                    match &event {
                        RealtimeEvent::ToolCall{call_id,name,arguments}=>{
                            if !seen_calls.insert(call_id.clone()){return Err(failure("provider repeated a realtime tool call identity"));}
                            if pending.len()>=limits.max_pending_tools{return Err(failure("realtime tool queue is full"));}
                            if !allowed.contains(name)||self.find_dispatchable_tool(name).is_none(){return Err(failure("provider requested an unavailable realtime Agent tool"));}
                            let id=ToolUseId::new();let assistant=MessageId::new();let message=ConversationMessage::Assistant{id:assistant,content:vec![ContentBlock::ToolUse{id:id.clone(),name:name.clone(),input:Utf16JsonProjection::from_serde_value(arguments).map_err(|error|failure(error.to_string()))?,provider_id:Some(call_id.clone())}],stop_reason:Some("tool_use".into()),per_turn_effort:None,resumable:None};
                            self.session.lock().await.history.push(message.clone());self.persist_message_to_jsonl(&message).await;
                            let token=session_cancel.child_token();tool_tokens.insert(call_id.clone(),token.clone());let orch=self.clone();let call_id=call_id.clone();let name=name.clone();let arguments=arguments.clone();
                            let tool_dispatch = tool_dispatch.clone();
                            pending.push(Box::pin(async move {
                                let _dispatch = tool_dispatch.lock().await;
                                orch.realtime_dispatch_tool(call_id,id,name,arguments,assistant,token,limits).await
                            }));
                        },
                        RealtimeEvent::ToolCancelled{call_ids}=>{for id in call_ids{if let Some(token)=tool_tokens.get(id){token.cancel();}}},
                        RealtimeEvent::Interrupted|RealtimeEvent::UserSpeechStarted=>{ ledger.interrupt(); for token in tool_tokens.values() { token.cancel(); } },
                        RealtimeEvent::AudioDelta { item_id, .. } => {
                            if !ledger.generated.contains(item_id) && ledger.generated.len() >= limits.max_pending_transcripts { return Err(failure("realtime pending playback queue is full")); }
                            ledger.generated.insert(item_id.clone());
                        },
                        RealtimeEvent::Usage { turn_id: Some(turn_id), input_tokens, output_tokens, .. } => {
                            if usage_turns.insert(turn_id.clone()) {
                                let mut session = self.session.lock().await;
                                session.usage.0.input_tokens = session.usage.0.input_tokens.saturating_add(input_tokens.unwrap_or(0));
                                session.usage.0.output_tokens = session.usage.0.output_tokens.saturating_add(output_tokens.unwrap_or(0));
                            }
                        },
                        RealtimeEvent::Transcript{direction,text,item_id,final_chunk:true,..}=>{
                            if text.len()>limits.max_transcript_bytes{return Err(failure("realtime transcript exceeds its byte bound"));}
                            match direction {
                                RealtimeTranscriptDirection::Input=>{if ledger.finalized_input.insert(item_id.clone()){self.append_realtime_text(text.clone(),true).await;}},
                                RealtimeTranscriptDirection::Output=>{if let Some(text)=ledger.output_final(item_id.clone(),text.clone(),limits)?{self.append_realtime_text(text,false).await;}},
                            }
                        },
                        RealtimeEvent::ProviderError{message,..}|RealtimeEvent::ConnectionInterrupted{message}=>return Err(failure(format!("native realtime provider failed: {message}"))),
                        RealtimeEvent::Closed{..}=>return Ok(RealtimeAgentEnd::Closed),
                        _=>{},
                    }
                    output.try_send(event).map_err(|error|failure(format!("realtime output queue unavailable: {error}")))?;
                }
            }
        }
        }.await;
        // Close the duplex transport immediately, then let the common tool
        // cancellation policy finish its cleanup. Block-policy mutations must
        // never be force-dropped merely because the audio session ended.
        session_cancel.cancel();
        let _ = tokio::time::timeout(
            Duration::from_secs(1),
            control.abort(RealtimeClose {
                code: 1000,
                reason: "host realtime ended".into(),
            }),
        )
        .await;
        while pending.next().await.is_some() {}
        result
    }
}

#[cfg(test)]
#[path = "realtime_tests.rs"]
mod tests;
