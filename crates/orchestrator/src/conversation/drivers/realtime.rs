//! Native realtime Agent execution through the ordinary Harness tool dispatcher.
use super::*;
use base64::Engine as _;
use futures::{stream::FuturesUnordered, StreamExt};
use lingxi_core::types::{utf16_json::Utf16JsonProjection, MessageId, ToolUseId};
use lingxi_llm_client::realtime::{
    RealtimeClose, RealtimeControl, RealtimeEvent, RealtimeEvents, RealtimeHistoryItem,
    RealtimeInput, RealtimeOrderedEvent, RealtimeRole, RealtimeTranscriptDirection,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
pub use tool_api::{MediaAgentCapabilityContext, MediaAgentHandoffRequest};

/// Host snapshot used to configure an SDK agent connector before dispatch.
#[derive(Clone, Debug)]
pub struct RealtimeAgentContext {
    /// Owning current Agent session.
    pub session_id: String,
    /// Session's logical provider profile when this snapshot was captured.
    pub session_profile_name: String,
    /// Logical audio profile selected by the host before account resolution.
    pub audio_profile_name: String,
    /// Logical audio profile at preparation; the host replaces it with the
    /// selected account's exact execution profile before admission.
    pub profile_name: Option<String>,
    /// Unbound at preparation; the host supplies the selected account scope.
    pub account_scope: Option<String>,
    /// Conversation behavior admitted when the instructions were assembled.
    pub conversation_mode: lingxi_core::host::session_context::SessionModeSnapshot,
    /// Effective skill-instruction epoch used to project this history.
    pub conversation_profile_epoch: u64,
    /// Host-issued engine attachment selected by media handoff admission.
    pub attachment_id: Option<media_api::MediaAttachmentId>,
    /// Current Agent system instructions.
    pub instructions: String,
    /// Normalized text and tool history; connector imports it without a response.
    pub history: Vec<RealtimeHistoryItem>,
    /// Current permission-filtered tool catalog in function schema form.
    pub tools: Vec<serde_json::Value>,
}
/// Bounded host controls and native playback evidence.
#[derive(Debug, Clone)]
pub enum RealtimeAgentInput {
    /// Provider-neutral input, excluding model-controlled route/configuration.
    Provider(RealtimeInput),
    /// Native player drained this item; only now may its transcript enter history.
    PlaybackCompleted { item_id: Option<String> },
}

/// Events from the provider and lifecycle evidence owned by the Agent loop.
/// A provider event cannot construct either of the local lifecycle variants.
#[derive(Debug, Clone, PartialEq)]
pub enum RealtimeAgentOutput {
    Provider(RealtimeEvent),
    /// A locally identified input and its Agent work have begun.
    AgentResponseStarted {
        turn_id: String,
    },
    /// The input's Agent work, including tool continuations, has completed.
    AgentTurnCompleted {
        turn_id: String,
    },
}

#[derive(Clone, Copy)]
enum ExplicitCancelTailStage {
    ToolCancelled,
    Interrupted,
    Raw,
}

struct ExplicitCancelTail {
    stage: ExplicitCancelTailStage,
    response_id: Option<String>,
    belongs_to_current: bool,
}

enum FencedLocalInput {
    Audio,
    CommitAudio,
    Text(String),
}

struct PendingLocalInput {
    id: u64,
    input: FencedLocalInput,
}

// One large input can arrive as the queue reaches its high-water mark. Reserve
// space for it, then let the bounded input channel apply backpressure until the
// previous owner and its tools drain. All data-bearing inputs count, not only
// PCM; image limits are also enforced before an image enters the queue.
const MAX_DEFERRED_AUDIO_CHUNK_BYTES: usize = 1024 * 1024;
const MAX_DEFERRED_ITEM_BYTES: usize = 8 * 1024 * 1024 + 1024;
const MAX_DEFERRED_PAYLOAD_BYTES: usize = MAX_DEFERRED_ITEM_BYTES + MAX_DEFERRED_AUDIO_CHUNK_BYTES;

fn deferred_input_bytes(input: &RealtimeInput) -> Result<usize, OrchestratorError> {
    let size = match input {
        RealtimeInput::Audio { data, .. } => {
            if data.len() > MAX_DEFERRED_AUDIO_CHUNK_BYTES {
                return Err(failure("realtime deferred audio chunk is too large"));
            }
            Some(data.len())
        }
        RealtimeInput::Image { data, mime_type } => data.len().checked_add(mime_type.len()),
        RealtimeInput::Text(text) => Some(text.len()),
        RealtimeInput::RetrieveItem { item_id }
        | RealtimeInput::DeleteItem { item_id }
        | RealtimeInput::TruncateAudio { item_id, .. } => Some(item_id.len()),
        RealtimeInput::ImportHistory { .. }
        | RealtimeInput::ToolResult { .. }
        | RealtimeInput::ToolResults { .. } => {
            return Err(failure(
                "host realtime input cannot replace Agent history or tool results",
            ))
        }
        _ => Some(0),
    };
    let size = size.ok_or_else(|| failure("realtime deferred input size overflow"))?;
    if size > MAX_DEFERRED_ITEM_BYTES {
        return Err(failure("realtime deferred input is too large"));
    }
    Ok(size)
}

/// Associate provider transcription with locally admitted audio, including
/// completion events that arrive after the Agent turn has finished.
#[derive(Default)]
struct AudioInputAdmissions {
    pending_openai_commits: usize,
    openai_items: HashSet<String>,
    openai_tainted_items: HashSet<String>,
    early_openai_finals: HashMap<String, RealtimeEvent>,
    streaming_audio: bool,
    gemini_current: Option<Option<String>>,
    gemini_provisional_turn: Option<String>,
    gemini_turns: HashSet<String>,
    gemini_tainted_turns: HashSet<String>,
}

impl AudioInputAdmissions {
    fn admit_audio(
        &mut self,
        boundary: RealtimeCompletionBoundary,
        limit: usize,
    ) -> Result<(), OrchestratorError> {
        match boundary {
            RealtimeCompletionBoundary::ExplicitResponse => {
                if self.pending_openai_commits >= limit {
                    return Err(failure("realtime pending audio commit queue is full"));
                }
                self.pending_openai_commits += 1;
            }
            RealtimeCompletionBoundary::Gemini38 => {
                self.gemini_current = Some(None);
                if let Some(turn_id) = self.gemini_provisional_turn.take() {
                    self.bind_gemini_turn(&turn_id, true);
                }
            }
        }
        self.streaming_audio = false;
        Ok(())
    }

    fn admit_text(&mut self) {
        self.gemini_current = None;
        self.clear_audio();
    }

    fn clear_audio(&mut self) {
        self.streaming_audio = false;
        if let Some(turn_id) = self.gemini_provisional_turn.take() {
            self.gemini_tainted_turns.insert(turn_id);
        }
    }

    fn bind_gemini_turn(&mut self, turn_id: &str, local_turn_active: bool) {
        let Some(number) = turn_id.strip_prefix("gemini-turn-") else {
            return;
        };
        if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
            return;
        }
        if self.gemini_turns.contains(turn_id) || self.gemini_tainted_turns.contains(turn_id) {
            return;
        }
        if local_turn_active {
            if let Some(current) = self.gemini_current.as_mut() {
                if current.is_none() {
                    *current = Some(turn_id.to_owned());
                    self.gemini_turns.insert(turn_id.to_owned());
                }
            }
        }
    }

    fn openai_committed(&mut self, item_id: &str) -> Option<RealtimeEvent> {
        if item_id.is_empty()
            || self.openai_items.contains(item_id)
            || self.openai_tainted_items.contains(item_id)
        {
            return None;
        }
        if self.pending_openai_commits == 0 {
            self.openai_tainted_items.insert(item_id.to_owned());
            return None;
        }
        self.pending_openai_commits -= 1;
        self.openai_items.insert(item_id.to_owned());
        let early = self
            .early_openai_finals
            .remove(item_id)
            .filter(|_| !self.openai_tainted_items.contains(item_id));
        // Unmatched events cannot borrow authorization from a future commit.
        if self.pending_openai_commits == 0 {
            self.early_openai_finals.clear();
        }
        early
    }

    fn transcript(
        &mut self,
        boundary: RealtimeCompletionBoundary,
        item_id: Option<&str>,
        turn_id: Option<&str>,
        text: &str,
        final_chunk: bool,
        local_turn_active: bool,
        limit: usize,
        event: &RealtimeEvent,
    ) -> Result<(Option<String>, bool), OrchestratorError> {
        let Some(item_id) = item_id.filter(|id| !id.is_empty()) else {
            return Ok((None, false));
        };
        match boundary {
            RealtimeCompletionBoundary::ExplicitResponse => {
                if self.openai_tainted_items.contains(item_id) {
                    return Ok((None, false));
                }
                if self.openai_items.contains(item_id) {
                    return Ok((final_chunk.then(|| text.to_owned()), true));
                }
                if !self.streaming_audio && self.pending_openai_commits == 0 {
                    self.openai_tainted_items.insert(item_id.to_owned());
                    return Ok((None, false));
                }
                if final_chunk && self.pending_openai_commits > 0 {
                    if !self.early_openai_finals.contains_key(item_id)
                        && self.early_openai_finals.len() >= limit
                    {
                        return Err(failure("realtime pending input transcripts are full"));
                    }
                    self.early_openai_finals
                        .entry(item_id.to_owned())
                        .or_insert_with(|| event.clone());
                }
                Ok((None, !final_chunk && self.streaming_audio))
            }
            RealtimeCompletionBoundary::Gemini38 => {
                let Some(turn_id) = turn_id else {
                    return Ok((None, false));
                };
                let Some(number) = turn_id.strip_prefix("gemini-turn-") else {
                    return Ok((None, false));
                };
                if item_id != format!("gemini-input-{number}") {
                    return Ok((None, false));
                }
                if self.gemini_tainted_turns.contains(turn_id) {
                    return Ok((None, false));
                }
                self.bind_gemini_turn(turn_id, local_turn_active);
                if self.gemini_turns.contains(turn_id) {
                    return Ok((final_chunk.then(|| text.to_owned()), true));
                }
                if self.streaming_audio {
                    let provisional = self
                        .gemini_provisional_turn
                        .get_or_insert_with(|| turn_id.to_owned());
                    return Ok((None, !final_chunk && provisional == turn_id));
                }
                // The codec accumulates interim text until turn completion. A
                // later commit cannot make an earlier unowned fragment safe.
                self.gemini_tainted_turns.insert(turn_id.to_owned());
                Ok((None, false))
            }
        }
    }

    fn retained_identity_count(&self) -> usize {
        self.openai_items.len()
            + self.openai_tainted_items.len()
            + self.early_openai_finals.len()
            + self.gemini_turns.len()
            + self.gemini_tainted_turns.len()
    }
}

#[derive(Default)]
struct RealtimeTurnTracker {
    next_id: u64,
    current: Option<String>,
    response_started: bool,
    response_done: bool,
    response_had_tool: bool,
    continuation_requested: bool,
    provider_id: Option<String>,
    completed_provider_ids: HashSet<String>,
    completed_anonymous: bool,
    cancelled_terminal: bool,
    interrupted: bool,
    google_candidate: Option<(bool, bool)>,
    google_projection_seen: bool,
}

impl RealtimeTurnTracker {
    fn begin(&mut self) -> Result<Option<String>, OrchestratorError> {
        if self.current.is_some() {
            return Ok(None);
        }
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| failure("realtime turn identity exhausted"))?;
        let id = format!("agent-{}", self.next_id);
        self.current = Some(id.clone());
        self.response_started = false;
        self.response_done = false;
        self.response_had_tool = false;
        self.continuation_requested = false;
        self.provider_id = None;
        self.cancelled_terminal = false;
        self.interrupted = false;
        self.google_candidate = None;
        self.google_projection_seen = false;
        Ok(Some(id))
    }

    fn response_started(
        &mut self,
        provider_id: &Option<String>,
        pending_tools: bool,
    ) -> Result<(), OrchestratorError> {
        if provider_id
            .as_ref()
            .is_some_and(|id| self.completed_provider_ids.contains(id))
        {
            return Err(failure(
                "realtime provider reused a completed response identity",
            ));
        }
        if self.continuation_requested && !pending_tools {
            if self.response_started && self.response_had_tool && !self.response_done {
                return Err(failure(
                    "realtime continuation started before its prior response completed",
                ));
            }
            self.response_had_tool = false;
            self.continuation_requested = false;
        }
        self.response_started = true;
        self.response_done = false;
        self.provider_id = provider_id.clone();
        Ok(())
    }

    fn tool_called(&mut self) {
        self.response_had_tool = true;
    }

    fn continued(&mut self) {
        self.continuation_requested = true;
    }

    fn completed(
        &mut self,
        provider_id: &Option<String>,
        interrupted: bool,
        pending_tools: bool,
    ) -> Result<Option<String>, OrchestratorError> {
        if !self.response_started {
            return Ok(None);
        }
        if let Some(id) = provider_id {
            if self.completed_provider_ids.contains(id) {
                return Ok(None);
            }
            if self
                .provider_id
                .as_ref()
                .is_some_and(|expected| expected != id)
            {
                return Ok(None);
            }
            self.completed_provider_ids.insert(id.clone());
        } else {
            if self.provider_id.is_some() || self.completed_anonymous {
                return Err(failure(
                    "realtime provider completion has ambiguous response identity",
                ));
            }
            self.completed_anonymous = true;
        }
        self.response_done = true;
        if interrupted && pending_tools {
            self.cancelled_terminal = true;
        }
        if interrupted && !pending_tools {
            return Ok(self.finish());
        }
        if pending_tools || self.response_had_tool {
            return Ok(None);
        }
        Ok(self.finish())
    }

    fn finish(&mut self) -> Option<String> {
        let id = self.current.take()?;
        self.response_started = false;
        self.response_done = false;
        self.response_had_tool = false;
        self.continuation_requested = false;
        self.provider_id = None;
        self.cancelled_terminal = false;
        self.interrupted = false;
        self.google_candidate = None;
        self.google_projection_seen = false;
        Some(id)
    }

    fn cancelled_tools_drained(&mut self) -> Option<String> {
        if self.cancelled_terminal {
            self.finish()
        } else {
            None
        }
    }
}

/// Bounded transport endpoints for one host-opened native realtime session.
///
/// This contract lives with the Agent orchestration layer so app hosts can
/// implement it without depending on the full Harness runtime.
pub struct RealtimeAgentSessionIo {
    /// Logical audio profile selected by the trusted host.
    pub audio_profile_name: String,
    /// Exact provider profile selected by the trusted host for this session.
    pub provider_profile: String,
    /// Exact account scope selected by the trusted host for this session.
    pub provider_account_scope: String,
    /// Provider-neutral control with bounded input queues.
    pub control: RealtimeControl,
    /// Bounded normalized provider event stream.
    pub events: RealtimeEvents,
    /// Bounded host audio, image, and transcript input queue.
    pub inputs: mpsc::Receiver<RealtimeAgentInput>,
    /// Bounded native playback/event output queue.
    pub output: mpsc::Sender<RealtimeAgentOutput>,
    /// Realtime session limits.
    pub limits: RealtimeAgentLimits,
    /// Whether the selected realtime provider accepts image inputs.
    pub image_input_supported: bool,
}

/// A host-created child attachment and its private provider-route lifecycle.
/// Only trusted host assembly receives this value; media tools receive the
/// child's scoped MediaService, not this attachment capability.
pub struct ScheduledMediaChild {
    /// Independent attachment minted by the current parent attachment.
    pub attachment: media_runtime::MediaAttachmentHandle,
    /// Host-specific binding and cleanup of this child's provider route.
    pub route: std::sync::Arc<dyn ScheduledMediaChildRoute>,
}

/// Nonblocking check that a parent engine still owns its selected session and
/// profile. A busy or closed parent returns false, leaving child media unavailable.
pub type ScheduledMediaParentCheck = std::sync::Arc<dyn Fn() -> bool + Send + Sync>;

/// Host-only provider route for a short-lived scheduled child attachment.
/// Registration must validate both attachment lifecycles and the exact trusted
/// parent/child session and profile context. Cleanup is valid only after the
/// child MediaService drain has been confirmed.
pub trait ScheduledMediaChildRoute: Send + Sync {
    /// Bind the child's current session to an already authorized parent route.
    fn bind_session(
        &self,
        parent_session_id: &str,
        child_session_id: &str,
        profile_name: &str,
        account_scope: &str,
        parent_is_current: ScheduledMediaParentCheck,
    ) -> Result<(), String>;

    /// Remove only this child's route after its media drain is confirmed.
    fn release_after_media_drain(&self) -> Result<(), String>;
}

/// Host adapter that opens realtime IO for a trusted Conversation media session.
///
/// Harness calls this only after the current tool result has been committed and
/// ordinary streamed tool work has drained. Implementations resolve provider
/// config and credentials from trusted host state, never the tool request.
#[async_trait::async_trait]
pub trait MediaConversationAttachment: Send + Sync {
    /// Create an independent scheduled child and host-only route binding.
    /// Implementations must fork from the supplied current parent handle.
    /// Hosts without a scheduled route factory leave child cloud audio unavailable.
    fn fork_scheduled_child(
        &self,
        _parent: &media_runtime::MediaAttachmentHandle,
    ) -> Result<Option<ScheduledMediaChild>, String> {
        Ok(None)
    }

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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RealtimeCompletionBoundary {
    /// OpenAI response IDs and a separate response after each tool batch.
    ExplicitResponse,
    /// Gemini 3.8 Live requires the same-frame interactionStatus to be IDLE.
    Gemini38,
}

#[derive(Clone, Copy, Debug)]
pub struct RealtimeAgentLimits {
    /// Contract selected from the trusted, pinned provider and model route.
    pub completion_boundary: RealtimeCompletionBoundary,
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
            completion_boundary: RealtimeCompletionBoundary::ExplicitResponse,
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
// Model-facing realtime history skips transcript-only rows and nonportable
// reasoning. Use the same projection when recording context metadata.
fn realtime_manifest_message(message: &ConversationMessage) -> Option<ConversationMessage> {
    let mut message = message.clone();
    match &mut message {
        ConversationMessage::System { api_system, .. } => {
            api_system.as_ref()?;
        }
        ConversationMessage::User {
            is_visible_in_transcript_only: true,
            ..
        } => return None,
        ConversationMessage::User { content, .. }
        | ConversationMessage::Assistant { content, .. } => {
            content.retain(|block| match block {
                ContentBlock::Text { text, .. } | ContentBlock::TextJsUtf16 { text, .. } => {
                    !text.is_empty()
                }
                ContentBlock::ToolUse { .. } | ContentBlock::ToolResult { .. } => true,
                _ => false,
            });
            if content.is_empty() {
                return None;
            }
        }
    }
    Some(message)
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
    /// Fail-closed, nonblocking snapshot for host-owned scheduled media routes.
    /// This never waits behind a session transition or provider update.
    pub fn try_current_audio_binding(&self) -> Result<(String, String), OrchestratorError> {
        let session = self
            .session
            .try_lock()
            .map_err(|_| failure("current audio binding is busy"))?;
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
        let gate = Arc::new(self.turn_gate.clone().lock_owned().await);
        // A native handoff is automatic continuation, not a human retry of
        // paused storage. Leave recovery to the next human admission.
        self.check_storage_pause().await?;
        // Realtime preparation is a query boundary: a profile chosen during a
        // previous turn must be admitted before capturing history or prompts.
        self.admit_conversation_profile(&gate).await?;
        self.refresh_effective_skill_projection().await?;
        let conversation_mode = self.conversation_profile().await;
        let conversation_profile_epoch = self.profile_epoch().await;
        let (session_id, history) = {
            let session = self.session.lock().await;
            (
                session.session_id.as_uuid().to_string(),
                session.model_context_history(),
            )
        };
        let profile_name = self.current_audio_profile().await?;
        let account_scope = lingxi_core::host::provider_accounts::UNBOUND_ACCOUNT_SCOPE.to_owned();
        let mut wire_tools = self
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
            .collect::<Vec<_>>();
        let tools = wire_tools
            .iter_mut()
            .map(|tool| {
                // Realtime sends full function schemas without these ordinary
                // provider-specific flags; its manifest should reflect that.
                tool.strict = false;
                tool.defer_loading = false;
                Ok(serde_json::json!({
                    "name": tool.name.as_str(),
                    "description": source_value(&tool.description)?,
                    "parameters": source_value(&tool.input_schema)?,
                }))
            })
            .collect::<Result<Vec<_>, OrchestratorError>>()?;
        let realtime_history = realtime_history(&history)?;
        let manifest_history = history
            .iter()
            .filter_map(realtime_manifest_message)
            .collect::<Vec<_>>();
        let system = self.provider_system_prompt().await?;
        let instructions = Self::system_prompt_source_text(&system)
            .string_at("")
            .map(str::to_owned)
            .ok_or_else(|| failure("realtime system instructions are not a string"))?;
        self.record_context_projection(Some(&system), &manifest_history, &wire_tools)
            .await;
        self.check_storage_pause().await?;
        Ok(RealtimeAgentContext {
            session_id,
            session_profile_name: profile_name.clone(),
            audio_profile_name: profile_name.clone(),
            profile_name: Some(profile_name),
            account_scope: Some(account_scope),
            conversation_mode,
            conversation_profile_epoch,
            attachment_id: None,
            instructions,
            history: realtime_history,
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
        events: RealtimeEvents,
        mut inputs: mpsc::Receiver<RealtimeAgentInput>,
        output: mpsc::Sender<RealtimeAgentOutput>,
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
        let gate = tokio::select! {biased;_ = session_cancel.cancelled()=>return Ok(RealtimeAgentEnd::Cancelled),gate=self.turn_gate.clone().lock_owned()=>Arc::new(gate)};
        self.check_storage_pause().await?;
        // Connector creation happens outside the gate. Reject a context
        // captured for another fixed conversation profile.
        // The guard closes already-opened realtime IO on this error path.
        self.admit_conversation_profile(&gate).await?;
        self.refresh_effective_skill_projection().await?;
        if self.conversation_profile().await != prepared.conversation_mode
            || self.profile_epoch().await != prepared.conversation_profile_epoch
        {
            return Err(failure(
                "Agent conversation mode changed before realtime admission",
            ));
        }
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
        if current_profile != prepared.session_profile_name {
            return Err(failure(
                "Agent provider profile changed before realtime admission",
            ));
        }
        if !self
            .api
            .audio_account_binding_is_current(
                &prepared.audio_profile_name,
                prepared.profile_name.as_deref().unwrap_or_default(),
                prepared.account_scope.as_deref().unwrap_or_default(),
            )
            .await
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
        let mut turn = RealtimeTurnTracker::default();
        let mut audio_admissions = AudioInputAdmissions::default();
        let mut ordered_events = events.into_ordered();
        let mut next_input_id = 0_u64;
        let mut pending_local_input: Option<PendingLocalInput> = None;
        let mut deferred_inputs = VecDeque::new();
        let mut deferred_payload_bytes = 0_usize;
        let mut replay_deferred = false;
        let mut explicit_cancel_tail: Option<ExplicitCancelTail> = None;
        let mut deferred_terminal: Option<String> = None;
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
        macro_rules! emit_agent_output {
            ($event:expr) => {{
                tokio::select! {
                    biased;
                    _ = session_cancel.cancelled() => return Ok(RealtimeAgentEnd::Cancelled),
                    _ = tokio::time::sleep_until(deadline) => return Ok(RealtimeAgentEnd::Deadline),
                    sent = output.send($event) => sent.map_err(|error| failure(format!("realtime output queue unavailable: {error}")))?,
                }
            }};
        }
        loop {
            // Stop native work after any failed transcript append. The outer
            // cleanup still settles tools already accepted by this owner.
            self.check_storage_pause().await?;
            // After an interrupted owner settles, replay its queued successor
            // before polling later provider events. Cancellation/deadline still
            // take precedence and close the entire transport.
            if turn.current.is_none() && pending.is_empty() && explicit_cancel_tail.is_none() && !deferred_inputs.is_empty() { replay_deferred = true; }
            if replay_deferred && deferred_inputs.is_empty() { replay_deferred = false; }
            let deferred = if replay_deferred && pending_local_input.is_none() { deferred_inputs.front().cloned() } else { None };
            let replaying = deferred.is_some();
            let input_capacity = replaying || (deferred_inputs.len() < limits.max_pending_transcripts
                && deferred_payload_bytes <= MAX_DEFERRED_PAYLOAD_BYTES - MAX_DEFERRED_ITEM_BYTES);
            tokio::select! {
                biased;
                _ = session_cancel.cancelled()=>return Ok(RealtimeAgentEnd::Cancelled),
                _ = tokio::time::sleep_until(deadline)=>return Ok(RealtimeAgentEnd::Deadline),
                _ = tokio::time::sleep_until(idle), if pending.is_empty()=>return Ok(RealtimeAgentEnd::Idle),
                completed = pending.next(), if !pending.is_empty()=>{
                    let completed=completed.expect("nonempty tool futures")?;let cancelled = tool_tokens.remove(&completed.call_id).is_some_and(|token| token.is_cancelled());idle=Instant::now()+limits.idle_timeout;
                    if cancelled {
                        if pending.is_empty() {
                            if let Some(turn_id) = turn.cancelled_tools_drained() {
                                if explicit_cancel_tail.is_some() { deferred_terminal = Some(turn_id); }
                                else { emit_agent_output!(RealtimeAgentOutput::AgentTurnCompleted { turn_id }); }
                            }
                        }
                        continue;
                    }
                    for image in &completed.images {
                        accept_realtime_image(&image.data, &image.mime_type, image_input_supported, limits, &mut image_input_count, &mut image_input_bytes)?;
                    }
                    control.send(RealtimeInput::ToolResult{call_id:completed.call_id,output:completed.output}).map_err(|error|failure(error.to_string()))?;
                    for image in completed.images {
                        control.send(RealtimeInput::Image { data: image.data.into(), mime_type: image.mime_type }).map_err(|error|failure(error.to_string()))?;
                    }
                    if completed.prevent{return Ok(RealtimeAgentEnd::ToolStopped);}
                    // Submit every concurrent result before requesting the next response.
                    if pending.is_empty(){self.check_storage_pause().await?;control.continue_response().map_err(|error|failure(error.to_string()))?;turn.continued();}
                },
                input = async { match deferred { Some(input) => Some(input), None => inputs.recv().await } }, if pending_local_input.is_none() && input_capacity=>{
                    let Some(input)=input else{return Ok(RealtimeAgentEnd::Closed);};idle=Instant::now()+limits.idle_timeout;
                    if replaying {
                        let consumed = deferred_inputs.pop_front().expect("deferred input was peeked");
                        if let RealtimeAgentInput::Provider(input) = consumed {
                            deferred_payload_bytes -= deferred_input_bytes(&input)?;
                        }
                        if deferred_inputs.is_empty() { replay_deferred = false; }
                    }
                    match input {
                        RealtimeAgentInput::PlaybackCompleted{item_id}=>{if let Some(text)=ledger.playback(item_id){self.append_realtime_text(text,false).await;}},
                        RealtimeAgentInput::Provider(input)=>{
                            // Storage can pause while this select is waiting.
                            self.check_storage_pause().await?;
                            // Hosts cannot bypass the Agent dispatcher with forged results/history.
                            if matches!(input,RealtimeInput::ImportHistory{..}|RealtimeInput::DeleteItem{..}|RealtimeInput::ToolResult{..}|RealtimeInput::ToolResults{..}){return Err(failure("host realtime input cannot replace Agent history or tool results"));}
                            let starts_turn = matches!(&input, RealtimeInput::CommitAudio | RealtimeInput::Text(_));
                            let streams_audio = matches!(&input, RealtimeInput::Audio { .. });
                            // Capture can hand off one final chunk after its commit
                            // was queued. A new input during this turn must first
                            // interrupt it; that late chunk belongs to neither one.
                            if !replaying && streams_audio && turn.current.is_some()
                                && !turn.interrupted && deferred_inputs.is_empty() { continue; }
                            if replaying && starts_turn && turn.current.is_some() { return Err(failure("more than one realtime input was queued while interrupting")); }
                            if !replaying {
                                if let RealtimeInput::Image { data, mime_type } = &input {
                                    accept_realtime_image(data.as_ref(), mime_type, image_input_supported, limits, &mut image_input_count, &mut image_input_bytes)?;
                                }
                            }
                            if !replaying && (!deferred_inputs.is_empty() || ((starts_turn || streams_audio) && (turn.current.is_some() || explicit_cancel_tail.is_some()))) {
                                if deferred_inputs.is_empty() && turn.current.is_some() && !turn.interrupted { return Err(failure("a realtime Agent input is already unsettled")); }
                                if deferred_inputs.len() >= limits.max_pending_transcripts { return Err(failure("realtime deferred input queue is full")); }
                                deferred_payload_bytes = deferred_payload_bytes.checked_add(deferred_input_bytes(&input)?).ok_or_else(|| failure("realtime deferred input size overflow"))?;
                                if deferred_payload_bytes > MAX_DEFERRED_PAYLOAD_BYTES { return Err(failure("realtime deferred input queue is full")); }
                                deferred_inputs.push_back(RealtimeAgentInput::Provider(input));
                                continue;
                            }
                            if matches!(input,RealtimeInput::Interrupt) {
                                ledger.interrupt();
                                if turn.current.is_none() { continue; }
                                turn.interrupted=true;
                                for token in tool_tokens.values(){CancellationToken::cancel(token);}
                                // A completed provider response has nothing left to
                                // cancel; the interrupted Agent still owns its
                                // tools until their cancellation cleanup drains.
                                if turn.response_done && !turn.continuation_requested {
                                    if pending.is_empty() {
                                        if let Some(turn_id) = turn.finish() {
                                            if explicit_cancel_tail.is_some() { deferred_terminal = Some(turn_id); }
                                            else { emit_agent_output!(RealtimeAgentOutput::AgentTurnCompleted { turn_id }); }
                                        }
                                    } else { turn.cancelled_terminal=true; }
                                    continue;
                                }
                            }
                            let commits_audio = matches!(&input, RealtimeInput::CommitAudio);
                            let clears_audio = matches!(&input, RealtimeInput::ClearAudio);
                            if commits_audio && limits.completion_boundary == RealtimeCompletionBoundary::ExplicitResponse
                                && audio_admissions.pending_openai_commits >= limits.max_pending_transcripts {
                                return Err(failure("realtime pending audio commit queue is full"));
                            }
                            let fenced = match &input {
                                RealtimeInput::Audio { .. } => Some(FencedLocalInput::Audio),
                                RealtimeInput::CommitAudio => Some(FencedLocalInput::CommitAudio),
                                RealtimeInput::Text(text) => Some(FencedLocalInput::Text(text.clone())),
                                _ => None,
                            };
                            if let Some(fenced) = fenced {
                                next_input_id = next_input_id.checked_add(1).ok_or_else(|| failure("realtime input identity exhausted"))?;
                                control.send_with_fence(input, next_input_id).map_err(|error|failure(error.to_string()))?;
                                pending_local_input = Some(PendingLocalInput { id: next_input_id, input: fenced });
                                continue;
                            }
                            control.send(input).map_err(|error|failure(error.to_string()))?;
                            if clears_audio { audio_admissions.clear_audio(); }
                        }
                    }
                },
                event = ordered_events.next()=>{
                    let Some(event)=event else{return Ok(RealtimeAgentEnd::Closed);};idle=Instant::now()+limits.idle_timeout;
                    let event = match event {
                        RealtimeOrderedEvent::Provider(event) => event,
                        RealtimeOrderedEvent::InputSent { id } => {
                            let pending_input = pending_local_input.take().ok_or_else(|| failure("unexpected realtime input fence"))?;
                            if pending_input.id != id { return Err(failure("realtime input fence identity mismatch")); }
                            match pending_input.input {
                                FencedLocalInput::Audio => audio_admissions.streaming_audio = true,
                                FencedLocalInput::CommitAudio => {
                                    audio_admissions.admit_audio(limits.completion_boundary, limits.max_pending_transcripts)?;
                                    if let Some(turn_id) = turn.begin()? {
                                        emit_agent_output!(RealtimeAgentOutput::AgentResponseStarted { turn_id });
                                    }
                                },
                                FencedLocalInput::Text(text) => {
                                    audio_admissions.admit_text();
                                    let turn_id = turn.begin()?;
                                    self.append_realtime_text(text, true).await;
                                    if let Some(turn_id) = turn_id {
                                        emit_agent_output!(RealtimeAgentOutput::AgentResponseStarted { turn_id });
                                    }
                                },
                            }
                            continue;
                        }
                    };
                    if seen_calls.len() + usage_turns.len() + turn.completed_provider_ids.len() + ledger.finalized_input.len() + ledger.finalized_output.len() + ledger.discarded.len() + audio_admissions.retained_identity_count() > limits.max_pending_transcripts.saturating_mul(64) { return Err(failure("realtime session history resource bound reached")); }
                    let mut finished_cancel_tail = false;
                    let mut suppress_cancel_projection = false;
                    if let Some(tail) = explicit_cancel_tail.as_mut() {
                        match (tail.stage, &event) {
                            (ExplicitCancelTailStage::ToolCancelled, RealtimeEvent::Usage { .. }) => {},
                            (ExplicitCancelTailStage::ToolCancelled, RealtimeEvent::ToolCancelled { .. }) => {
                                tail.stage = ExplicitCancelTailStage::Interrupted;
                                suppress_cancel_projection = !tail.belongs_to_current;
                            },
                            (ExplicitCancelTailStage::Interrupted, RealtimeEvent::Interrupted) => {
                                tail.stage = ExplicitCancelTailStage::Raw;
                                suppress_cancel_projection = !tail.belongs_to_current;
                            },
                            (ExplicitCancelTailStage::Raw, RealtimeEvent::ProviderEvent { name, native }) if name == "response.done" => {
                                let id = native.get("response_id").and_then(serde_json::Value::as_str).or_else(|| native.pointer("/response/id").and_then(serde_json::Value::as_str));
                                if id != tail.response_id.as_deref() || native.pointer("/response/status").and_then(serde_json::Value::as_str) != Some("cancelled") { return Err(failure("OpenAI cancellation frame identity is inconsistent")); }
                                finished_cancel_tail = true;
                            },
                            _ => return Err(failure("OpenAI cancellation frame has an incomplete event tail")),
                        }
                    }
                    if suppress_cancel_projection { continue; }
                    if turn.current.is_none() && matches!(&event, RealtimeEvent::TurnStarted { .. } | RealtimeEvent::ToolCall { .. } | RealtimeEvent::AudioDelta { .. } | RealtimeEvent::TextDelta { .. } | RealtimeEvent::Transcript { direction: RealtimeTranscriptDirection::Output, .. }) {
                        return Err(failure("realtime provider output arrived without an admitted local input"));
                    }
                    if turn.current.is_none() && matches!(&event, RealtimeEvent::TurnCompleted { .. } | RealtimeEvent::ToolCancelled { .. } | RealtimeEvent::Interrupted | RealtimeEvent::UserSpeechStarted)
                        && !(explicit_cancel_tail.as_ref().is_some_and(|tail| tail.belongs_to_current) && matches!(&event, RealtimeEvent::ToolCancelled { .. } | RealtimeEvent::Interrupted))
                        && !(limits.completion_boundary == RealtimeCompletionBoundary::ExplicitResponse && matches!(&event, RealtimeEvent::TurnCompleted { status: Some(status), .. } if status == "cancelled")) {
                        continue;
                    }
                    let mut terminal = if finished_cancel_tail { explicit_cancel_tail = None; deferred_terminal.take() } else { None };
                    let mut forward_event = true;
                    match &event {
                        RealtimeEvent::TurnStarted { turn_id } if limits.completion_boundary == RealtimeCompletionBoundary::ExplicitResponse => turn.response_started(turn_id, !pending.is_empty())?,
                        RealtimeEvent::ToolCall{call_id,name,arguments}=>{
                            self.check_storage_pause().await?;
                            if !seen_calls.insert(call_id.clone()){return Err(failure("provider repeated a realtime tool call identity"));}
                            if pending.len()>=limits.max_pending_tools{return Err(failure("realtime tool queue is full"));}
                            if !allowed.contains(name)||self.find_dispatchable_tool(name).is_none(){return Err(failure("provider requested an unavailable realtime Agent tool"));}
                            turn.tool_called();
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
                        RealtimeEvent::Interrupted|RealtimeEvent::UserSpeechStarted=>{
                            turn.interrupted=true; ledger.interrupt(); for token in tool_tokens.values() { token.cancel(); }
                            if turn.response_done && !turn.continuation_requested {
                                if pending.is_empty() { terminal = turn.finish(); }
                                else { turn.cancelled_terminal=true; }
                            }
                        },
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
                        RealtimeEvent::Transcript{direction,text,item_id,turn_id,final_chunk,..}=>{
                            if *final_chunk && text.len()>limits.max_transcript_bytes{return Err(failure("realtime transcript exceeds its byte bound"));}
                            match direction {
                                RealtimeTranscriptDirection::Input=>{
                                    let (text, forward) = audio_admissions.transcript(
                                        limits.completion_boundary, item_id.as_deref(), turn_id.as_deref(),
                                        text, *final_chunk, turn.current.is_some(), limits.max_pending_transcripts, &event,
                                    )?;
                                    forward_event = forward;
                                    if let Some(text) = text {
                                        if ledger.finalized_input.insert(item_id.clone()){self.append_realtime_text(text,true).await;}
                                    }
                                },
                                RealtimeTranscriptDirection::Output if *final_chunk=>{if let Some(text)=ledger.output_final(item_id.clone(),text.clone(),limits)?{self.append_realtime_text(text,false).await;}},
                                RealtimeTranscriptDirection::Output=>{},
                            }
                        },
                        RealtimeEvent::TurnCompleted { turn_id, status } => {
                            if limits.completion_boundary == RealtimeCompletionBoundary::Gemini38 {
                                if let Some(id) = turn_id { audio_admissions.bind_gemini_turn(id, turn.current.is_some()); }
                            }
                            let stale_response_id = turn_id.as_ref().is_some_and(|id| turn.completed_provider_ids.contains(id) || turn.provider_id.as_ref().is_some_and(|expected| expected != id));
                            if limits.completion_boundary == RealtimeCompletionBoundary::ExplicitResponse && status.as_deref() == Some("cancelled") {
                                let belongs = turn.current.is_some() && turn.response_started && !stale_response_id;
                                explicit_cancel_tail = Some(ExplicitCancelTail { stage: ExplicitCancelTailStage::ToolCancelled, response_id: turn_id.clone(), belongs_to_current: belongs });
                                if !belongs { continue; }
                            }
                            if limits.completion_boundary == RealtimeCompletionBoundary::ExplicitResponse && stale_response_id { continue; }
                            let interrupted = turn.interrupted || matches!(status.as_deref(), Some("cancelled" | "interrupted"));
                            if interrupted { for token in tool_tokens.values() { token.cancel(); } }
                            match limits.completion_boundary {
                                RealtimeCompletionBoundary::ExplicitResponse => terminal = turn.completed(turn_id, interrupted, !pending.is_empty())?,
                                RealtimeCompletionBoundary::Gemini38 => {
                                    if turn.google_candidate.is_some() { return Err(failure("Gemini completion lacks its matching provider frame")); }
                                    turn.google_candidate = Some((!pending.is_empty(), interrupted));
                                    turn.google_projection_seen = false;
                                }
                            }
                        },
                        RealtimeEvent::ProviderEvent { name, native } if limits.completion_boundary == RealtimeCompletionBoundary::ExplicitResponse && name == "input_audio_buffer.committed" => {
                            if let Some(item_id) = native.get("item_id").and_then(serde_json::Value::as_str) {
                                if let Some(buffered) = audio_admissions.openai_committed(item_id) {
                                    if let RealtimeEvent::Transcript { text, .. } = &buffered {
                                        if ledger.finalized_input.insert(Some(item_id.to_owned())) { self.append_realtime_text(text.clone(), true).await; }
                                    }
                                    emit_agent_output!(RealtimeAgentOutput::Provider(buffered));
                                }
                            }
                        },
                        RealtimeEvent::ProviderEvent { name, native } if limits.completion_boundary == RealtimeCompletionBoundary::Gemini38 && turn.google_candidate.is_some() => {
                            // The pinned codec may insert this projection from the
                            // same frame before it forwards the full serverContent.
                            if name == "generationComplete" && native == &serde_json::Value::Bool(true) {
                                if turn.google_projection_seen { return Err(failure("Gemini completion has repeated provider projections")); }
                                turn.google_projection_seen = true;
                                emit_agent_output!(RealtimeAgentOutput::Provider(event.clone()));
                                continue;
                            }
                            let Some(content) = (name == "serverContent").then(|| native.get("serverContent")).flatten() else { return Err(failure("Gemini completion lacks its matching provider frame")); };
                            if content.get("turnComplete").and_then(serde_json::Value::as_bool) != Some(true) { return Err(failure("Gemini completion frame is inconsistent")); }
                            let (was_pending, interrupted) = turn.google_candidate.take().expect("checked candidate");
                            let interrupted = interrupted || turn.interrupted;
                            match content.get("interactionStatus").and_then(serde_json::Value::as_str) {
                                Some("IN_PROGRESS") => { turn.response_done=false; },
                                Some("IDLE") => {
                                    turn.response_done=true;
                                    if interrupted && (was_pending || !pending.is_empty()) { turn.cancelled_terminal = true; }
                                    if pending.is_empty() && (interrupted || (!was_pending && (!turn.response_had_tool || turn.continuation_requested))) { terminal = turn.finish(); }
                                },
                                _ => return Err(failure("Gemini completion cannot confirm interactionStatus IDLE")),
                            }
                        },
                        RealtimeEvent::ProviderError{message,..}|RealtimeEvent::ConnectionInterrupted{message}=>return Err(failure(format!("native realtime provider failed: {message}"))),
                        RealtimeEvent::Closed{..}=>return Ok(RealtimeAgentEnd::Closed),
                        _=>{},
                    }
                    if forward_event { emit_agent_output!(RealtimeAgentOutput::Provider(event)); }
                    if let Some(turn_id) = terminal {
                        if explicit_cancel_tail.is_some() { deferred_terminal = Some(turn_id); }
                        else { emit_agent_output!(RealtimeAgentOutput::AgentTurnCompleted { turn_id }); }
                    }
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

#[cfg(test)]
#[path = "realtime_storage_tests.rs"]
mod storage_tests;
