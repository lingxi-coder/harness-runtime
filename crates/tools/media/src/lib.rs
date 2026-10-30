//! Cross-platform model-facing media tools backed by the host's typed
//! `MediaService` capability.

#![forbid(unsafe_code)]
#![allow(clippy::too_many_lines)]

use async_trait::async_trait;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use media_api::{
    CaptureOptions, MediaAsset, MediaContent, MediaError, MediaErrorKind, MediaEventBatch,
    MediaEventKind, MediaKind, MediaMetadata, MediaOperation, MediaOperationContext,
    MediaOperationId, MediaOperationKind, MediaOwner, MediaReadinessState, MediaRef,
    MediaResourceRef, MediaResourceState, MediaResult, MediaSessionIntent, MediaSessionMode,
    MediaSessionRef, MediaSessionRequest, MediaSessionUpdate, MediaSourceKind, MediaSourceRef,
    MediaSourceUsage, RecordOptions, RecordingRef, TranscriptionInput, MAX_MEDIA_EVENT_BATCH_SIZE,
};
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tool_api::context::{MediaAgentCapabilityContext, MediaAgentHandoffHandle, ToolUseContext};
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, MediaResource, PromptOptions, Tool, ToolCallResult, ToolError,
    ToolExecutionMetadata, ToolInputRepairMetadata, ToolInputRepairSchema,
    ToolInputSchemaProvenance, ToolStaticContext, ValidationError,
};
use tool_api::BuiltinToolContext;

/// Canonical public media tool names, in registration order.
pub const TOOL_NAMES: [&str; 13] = [
    "media_sources",
    "media_status",
    "media_capture",
    "media_record",
    "media_transcribe",
    "media_synthesize",
    "media_play",
    "media_speak",
    "media_session_start",
    "media_session_update",
    "media_observe",
    "media_wait",
    "media_stop",
];

/// Bundled cross-platform product workflow for the media tool family.
pub mod skill {
    /// Canonical skill name exposed through the command registry.
    pub const NAME: &str = "media";
    /// Skill instructions shared by desktop and mobile hosts.
    pub const MARKDOWN: &str = include_str!("../skill/SKILL.md");
}

static NEXT_MEDIA_GENERATION: AtomicU64 = AtomicU64::new(1);
const MAX_SPEECH_TEXT_CHARS: usize = 5_550;
const MAX_MEDIA_TIMEOUT_MS: u64 = 120_000;
const DEFAULT_WAIT_TIMEOUT_MS: u64 = 30_000;

#[derive(Clone, Copy, PartialEq, Eq)]
struct SourceKindInfo {
    source_kind: MediaSourceKind,
    media_kind: Option<MediaKind>,
}

#[derive(Clone)]
struct MediaTool {
    ctx: BuiltinToolContext,
    kind: MediaOperationKind,
    name: &'static str,
    schema: Utf16JsonProjection,
    source_kinds: Arc<StdMutex<HashMap<(u64, String), Option<SourceKindInfo>>>>,
}

impl MediaTool {
    #[cfg(test)]
    fn new(ctx: BuiltinToolContext, kind: MediaOperationKind, name: &'static str) -> Self {
        Self::with_source_kinds(ctx, kind, name, Arc::new(StdMutex::new(HashMap::new())))
    }

    fn with_source_kinds(
        ctx: BuiltinToolContext,
        kind: MediaOperationKind,
        name: &'static str,
        source_kinds: Arc<StdMutex<HashMap<(u64, String), Option<SourceKindInfo>>>>,
    ) -> Self {
        let conversation_modes = supported_conversation_modes(ctx.media_agent_handoff.as_ref());
        let schema = generated_projection(&schema_for(kind, &conversation_modes));
        Self {
            ctx,
            kind,
            name,
            schema,
            source_kinds,
        }
    }

    fn service(&self) -> Option<&Arc<dyn media_api::MediaService>> {
        self.ctx.media.as_ref()
    }

    fn supported(&self) -> bool {
        self.service()
            .is_some_and(|service| service.capabilities().supports(self.kind))
    }

    fn media_capability_context(
        &self,
        options: &PromptOptions,
    ) -> Option<MediaAgentCapabilityContext> {
        let session_id = options.session_id.as_ref()?.as_uuid().to_string();
        let model = options.model.clone()?;
        let profile_name = options.model_profile.clone();
        Some(MediaAgentCapabilityContext {
            session_id,
            model,
            account_scope: profile_name
                .as_ref()
                .map(|profile| format!("profile:{profile}")),
            profile_name,
            attachment_id: self.service().and_then(|service| service.attachment_id()),
        })
    }

    fn prompt_conversation_modes(&self, options: &PromptOptions) -> Vec<MediaSessionMode> {
        if options.is_scheduled_turn {
            return Vec::new();
        }
        let Some(handle) = self.ctx.media_agent_handoff.as_ref() else {
            return Vec::new();
        };
        let Some(context) = self.media_capability_context(options) else {
            return Vec::new();
        };
        handle.supported_modes_for_context(&context)
    }

    async fn validate_conversation_mode(
        &self,
        operation: &MediaOperation,
        invocation: &ToolUseContext,
    ) -> Result<Option<tool_api::context::MediaAgentCapabilityContext>, String> {
        let MediaOperation::SessionStart { request } = operation else {
            return Ok(None);
        };
        if request.intent != MediaSessionIntent::Conversation {
            return Ok(None);
        }
        if invocation.agent_id.is_some()
            || invocation.origin_session_id.is_some()
            || invocation.depth != 0
            || invocation.is_scheduled_turn
        {
            return Err(
                "conversation media sessions are available only to an interactive main Agent"
                    .into(),
            );
        }
        let mode = request.mode;
        let capability_context = invocation
            .media_agent_capability_context(
                self.service().and_then(|service| service.attachment_id()),
            )
            .await
            .ok_or_else(|| "conversation intent requires a trusted session context".to_owned())?;
        if self
            .ctx
            .media_agent_handoff
            .as_ref()
            .is_some_and(|handoff| handoff.supports_mode(&capability_context, mode))
        {
            return Ok(Some(capability_context));
        }
        Err(format!(
            "conversation intent is unavailable for `{}` media mode on this host",
            session_mode_name(mode)
        ))
    }
}

fn supported_conversation_modes(
    handle: Option<&Arc<dyn MediaAgentHandoffHandle>>,
) -> Vec<MediaSessionMode> {
    handle.map_or_else(Vec::new, |handle| handle.supported_modes())
}

fn session_mode_name(mode: MediaSessionMode) -> &'static str {
    match mode {
        MediaSessionMode::Audio => "audio",
        MediaSessionMode::Video => "video",
        MediaSessionMode::AudioVideo => "audio_video",
    }
}

fn schema_for(kind: MediaOperationKind, conversation_modes: &[MediaSessionMode]) -> Value {
    let target = json!({
        "type": "object",
        "properties": {
            "kind": {"type":"string", "enum":["source","recording","session","media","operation"]},
            "ref": {"type":"string", "minLength":1},
            "generation":{"type":"integer","minimum":0},
            "service_epoch":{"type":"integer","minimum":0},
            "incarnation":{"type":"string","format":"uuid"}
        },
        "oneOf": [
            {
                "properties":{"kind":{"const":"operation"}},
                "required":["kind","ref","generation","service_epoch","incarnation"]
            },
            {
                "properties":{"kind":{"enum":["source","recording","session","media"]}},
                "required":["kind","ref"]
            }
        ],
        "required": ["kind","ref"],
        "additionalProperties": false
    });
    let source_filter = json!({
        "type":"object",
        "properties": {
            "kind":{"type":"string","enum":["audio","image","video"]},
            "usage":{"type":"string","enum":["capture","record","transcribe","play","speak","session","read_image"]}
        },
        "additionalProperties":false
    });
    let mut schema = match kind {
        MediaOperationKind::Sources => json!({
            "type":"object", "properties": {
                "filter": source_filter
            }, "additionalProperties":false
        }),
        MediaOperationKind::Status => json!({
            "type":"object", "properties": {"target":target}, "additionalProperties":false
        }),
        MediaOperationKind::Capture => json!({
            "type":"object", "properties": {
                "source_ref":{"type":"string","minLength":1},
                "max_duration_ms":{"type":"integer","minimum":1,"maximum":3600000},
                "max_dimension_px":{"type":"integer","minimum":1,"maximum":2000},
                "jpeg_quality":{"type":"number","minimum":0,"maximum":1}
            }, "required":["source_ref"], "additionalProperties":false
        }),
        MediaOperationKind::Record => json!({
            "type":"object", "properties": {
                "source_ref":{"type":"string","minLength":1},
                "max_duration_ms":{"type":"integer","minimum":1,"maximum":3600000},
                "mime_type":{"type":"string","minLength":1,"maxLength":64}
            }, "required":["source_ref"], "additionalProperties":false
        }),
        MediaOperationKind::Transcribe => json!({
            "type":"object", "properties": {
                "source":{"type":"object","properties":{
                    "kind":{"type":"string","enum":["live","stored"]},
                    "source_ref":{"type":"string","minLength":1},
                    "media_ref":{"type":"string","minLength":1}
                },"required":["kind"],
                "oneOf":[
                    {"properties":{"kind":{"const":"live"}},"required":["kind","source_ref"]},
                    {"properties":{"kind":{"const":"stored"}},"required":["kind","media_ref"]}
                ],
                "additionalProperties":false},
                "language":{"type":"string","minLength":1,"maxLength":35}
            }, "required":["source"], "additionalProperties":false
        }),
        MediaOperationKind::Synthesize | MediaOperationKind::Speak => speech_schema(),
        MediaOperationKind::Play => json!({
            "type":"object", "properties": {
                "media_ref":{"type":"string","minLength":1},
                "source_ref":{"type":"string","minLength":1}
            }, "required":["media_ref"], "additionalProperties":false
        }),
        MediaOperationKind::SessionStart => {
            let intents = if conversation_modes.is_empty() {
                vec!["observation"]
            } else {
                vec!["observation", "conversation"]
            };
            let mut operation_schema = json!({
                "type":"object", "properties": {
                    "intent":{"type":"string","enum":intents},
                    "mode":{"type":"string","enum":["audio","video","audio_video"]},
                    "audio_source_ref":{"type":"string","minLength":1},
                    "video_source_ref":{"type":"string","minLength":1}
                }, "required":["intent","mode"], "additionalProperties":false
            });
            operation_schema["oneOf"] = json!([
                {
                    "properties":{"mode":{"const":"audio"}},
                    "required":["mode","audio_source_ref"]
                },
                {
                    "properties":{"mode":{"const":"video"}},
                    "required":["mode","video_source_ref"]
                },
                {
                    "properties":{"mode":{"const":"audio_video"}},
                    "required":["mode","audio_source_ref","video_source_ref"]
                }
            ]);
            if !conversation_modes.is_empty() {
                let supported_modes: Vec<_> = conversation_modes
                    .iter()
                    .copied()
                    .map(session_mode_name)
                    .collect();
                operation_schema["allOf"] = json!([{
                    "if": {
                        "properties": {"intent":{"const":"conversation"}},
                        "required": ["intent"]
                    },
                    "then": {
                        "properties": {"mode":{"enum":supported_modes}},
                        "required": ["mode"]
                    }
                }]);
            }
            operation_schema
        }
        MediaOperationKind::SessionUpdate => json!({
            "type":"object", "properties": {
                "session_ref":{"type":"string","minLength":1},
                "update":{"type":"object","properties":{
                    "type":{"type":"string","enum":["set_enabled","select_source","set_playback_gain"]},
                    "kind":{"type":"string","enum":["audio","video"]},
                    "enabled":{"type":"boolean"},
                    "source_ref":{"type":"string","minLength":1},
                    "gain":{"type":"number","minimum":0,"maximum":1}
                },"required":["type"],
                "oneOf":[
                    {"properties":{"type":{"const":"set_enabled"}},"required":["type","kind","enabled"]},
                    {"properties":{"type":{"const":"select_source"}},"required":["type","kind","source_ref"]},
                    {"properties":{"type":{"const":"set_playback_gain"}},"required":["type","gain"]}
                ],
                "additionalProperties":false}
            }, "required":["session_ref","update"], "additionalProperties":false
        }),
        MediaOperationKind::Observe => observation_schema(false, target.clone()),
        MediaOperationKind::Wait => observation_schema(true, target.clone()),
        MediaOperationKind::Stop => json!({
            "type":"object", "properties": {
                "target": {
                    "type":"object", "properties": {
                        "kind":{"type":"string","enum":["recording","session","operation"]},
                        "ref":{"type":"string","minLength":1},
                        "generation":{"type":"integer","minimum":0},
                        "service_epoch":{"type":"integer","minimum":0},
                        "incarnation":{"type":"string","format":"uuid"}
                    },
                    "oneOf":[
                        {"properties":{"kind":{"const":"operation"}},"required":["kind","ref","generation","service_epoch","incarnation"]},
                        {"properties":{"kind":{"enum":["recording","session"]}},"required":["kind","ref"]}
                    ],
                    "required":["kind","ref"], "additionalProperties":false
                }
            }, "required":["target"], "additionalProperties":false
        }),
    };
    schema["additionalProperties"] = json!(false);
    schema
}

fn speech_schema() -> Value {
    json!({
        "type":"object", "properties":{
            "text":{"type":"string","minLength":1,"maxLength":MAX_SPEECH_TEXT_CHARS},
            "language":{"type":"string","minLength":1,"maxLength":35},
            "voice":{"type":"string","minLength":1,"maxLength":128},
            "rate":{"type":"number","minimum":0.5,"maximum":2}
        },"required":["text"],"additionalProperties":false
    })
}

fn observation_schema(wait: bool, target: Value) -> Value {
    let mut properties = Map::new();
    properties.insert("target".into(), target);
    properties.insert(
        "after_sequence".into(),
        json!({"type":"integer","minimum":0}),
    );
    properties.insert(
        "limit".into(),
        json!({"type":"integer","minimum":1,"maximum":MAX_MEDIA_EVENT_BATCH_SIZE}),
    );
    if wait {
        properties.insert(
            "timeout_ms".into(),
            json!({"type":"integer","minimum":1,"maximum":MAX_MEDIA_TIMEOUT_MS}),
        );
    }
    json!({
        "type":"object", "properties":properties,
        "required": if wait { vec!["target"] } else { vec!["target"] },
        "additionalProperties":false
    })
}

fn generated_projection(value: &Value) -> Utf16JsonProjection {
    Utf16JsonProjection::from_serde_value(value)
        .expect("locally generated media-tool JSON is a valid projection")
}

fn schema_repair() -> ToolInputRepairMetadata {
    ToolInputRepairMetadata::Known {
        schema: ToolInputRepairSchema::Advertised,
        schema_provenance: ToolInputSchemaProvenance::GeneratedBuiltin,
        exempt_top_level_keys: None,
    }
}

fn string_at<'a>(input: &'a Utf16JsonProjection, key: &str) -> Option<&'a str> {
    input.get_property(key).and_then(|value| value.as_str())
}

fn nested(input: &Utf16JsonProjection, key: &str) -> Result<Utf16JsonProjection, String> {
    input
        .subprojection(&format!("/{key}"))
        .map_err(|error| format!("`{key}` must be an object: {error}"))
}

fn required_string(input: &Utf16JsonProjection, key: &str) -> Result<String, String> {
    string_at(input, key)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("`{key}` must be a non-empty string"))
}

fn required_u64(input: &Utf16JsonProjection, key: &str) -> Result<u64, String> {
    optional_u64(input, key, 0, u64::MAX)?.ok_or_else(|| format!("`{key}` is required"))
}

fn optional_string(input: &Utf16JsonProjection, key: &str) -> Result<Option<String>, String> {
    match input.get_property(key) {
        None => Ok(None),
        Some(value) if value.is_null() => Ok(None),
        Some(value) => value
            .as_str()
            .filter(|value| !value.trim().is_empty())
            .map(|value| Some(value.to_owned()))
            .ok_or_else(|| format!("`{key}` must be a non-empty string when provided")),
    }
}

fn optional_u64(
    input: &Utf16JsonProjection,
    key: &str,
    min: u64,
    max: u64,
) -> Result<Option<u64>, String> {
    match input.get_property(key) {
        None => Ok(None),
        Some(value) if value.is_null() => Ok(None),
        Some(value) => {
            let number = value
                .as_number()
                .filter(|number| number.is_finite() && number.fract() == 0.0)
                .and_then(|number| u64::try_from(number as u128).ok())
                .filter(|number| (min..=max).contains(number))
                .ok_or_else(|| format!("`{key}` must be an integer from {min} through {max}"))?;
            Ok(Some(number))
        }
    }
}

fn optional_u32(
    input: &Utf16JsonProjection,
    key: &str,
    min: u32,
    max: u32,
) -> Result<Option<u32>, String> {
    optional_u64(input, key, u64::from(min), u64::from(max))?
        .map(|number| u32::try_from(number).map_err(|_| format!("`{key}` is out of range")))
        .transpose()
}

fn optional_f32(
    input: &Utf16JsonProjection,
    key: &str,
    min: f32,
    max: f32,
) -> Result<Option<f32>, String> {
    match input.get_property(key) {
        None => Ok(None),
        Some(value) if value.is_null() => Ok(None),
        Some(value) => {
            let number = value
                .as_number()
                .filter(|number| number.is_finite())
                .map(|number| number as f32)
                .filter(|number| (min..=max).contains(number))
                .ok_or_else(|| format!("`{key}` must be between {min} and {max}"))?;
            Ok(Some(number))
        }
    }
}

fn media_ref(value: String) -> MediaRef {
    MediaRef(value)
}

fn source_ref(value: String) -> MediaSourceRef {
    MediaSourceRef(value.into())
}

fn recording_ref(value: String) -> RecordingRef {
    RecordingRef(value.into())
}

fn session_ref(value: String) -> MediaSessionRef {
    MediaSessionRef(value.into())
}

fn parse_kind(value: &str) -> Result<MediaKind, String> {
    match value {
        "audio" => Ok(MediaKind::Audio),
        "image" => Ok(MediaKind::Image),
        "video" => Ok(MediaKind::Video),
        _ => Err("media `kind` must be `audio`, `image`, or `video`".into()),
    }
}

fn parse_usage(value: &str) -> Result<MediaSourceUsage, String> {
    match value {
        "capture" => Ok(MediaSourceUsage::Capture),
        "record" => Ok(MediaSourceUsage::Record),
        "transcribe" => Ok(MediaSourceUsage::Transcribe),
        "play" => Ok(MediaSourceUsage::Play),
        "speak" => Ok(MediaSourceUsage::Speak),
        "session" => Ok(MediaSourceUsage::Session),
        "read_image" => Ok(MediaSourceUsage::ReadImage),
        _ => Err("`usage` is not a supported media source use".into()),
    }
}

fn parse_media_target(target: &Utf16JsonProjection) -> Result<MediaResourceRef, String> {
    let kind = string_at(target, "kind").ok_or("`target.kind` must be a string")?;
    let reference = required_string(target, "ref")?;
    match kind {
        "source" => Ok(MediaResourceRef::Source(source_ref(reference))),
        "recording" => Ok(MediaResourceRef::Recording(recording_ref(reference))),
        "session" => Ok(MediaResourceRef::Session(session_ref(reference))),
        "media" => Ok(MediaResourceRef::Media(media_ref(reference))),
        "operation" => {
            let incarnation = required_string(target, "incarnation")?;
            let incarnation = media_api::MediaOperationIncarnation::from_string(&incarnation)
                .ok_or("`target.incarnation` is invalid")?;
            Ok(MediaResourceRef::Operation {
                identity: MediaOperationId {
                    id: reference,
                    generation: required_u64(target, "generation")?,
                    service_epoch: required_u64(target, "service_epoch")?,
                },
                incarnation,
            })
        }
        _ => Err("`target.kind` must name a typed media resource".into()),
    }
}

fn parse_stop_target(target: &Utf16JsonProjection) -> Result<MediaResourceRef, String> {
    match parse_media_target(target)? {
        target @ MediaResourceRef::Recording(_)
        | target @ MediaResourceRef::Session(_)
        | target @ MediaResourceRef::Operation { .. } => Ok(target),
        _ => Err("`media_stop` accepts only a recording, session, or operation reference".into()),
    }
}

fn parse_cursor(
    input: &Utf16JsonProjection,
) -> Result<Option<media_api::MediaEventCursor>, String> {
    optional_u64(input, "after_sequence", 0, u64::MAX)?
        .map(|sequence| Ok(media_api::MediaEventCursor { sequence }))
        .transpose()
}

fn parse_operation(
    kind: MediaOperationKind,
    input: &Utf16JsonProjection,
) -> Result<MediaOperation, String> {
    match kind {
        MediaOperationKind::Sources => {
            let (media_kind, usage) = match input.get_property("filter") {
                None => (None, None),
                Some(filter) if filter.is_null() => (None, None),
                Some(_) => {
                    let filter = nested(input, "filter")?;
                    let media_kind = optional_string(&filter, "kind")?
                        .map(|value| parse_kind(&value))
                        .transpose()?;
                    let usage = optional_string(&filter, "usage")?
                        .map(|value| parse_usage(&value))
                        .transpose()?;
                    (media_kind, usage)
                }
            };
            Ok(MediaOperation::Sources {
                kind: media_kind,
                usage,
            })
        }
        MediaOperationKind::Status => {
            let target = match input.get_property("target") {
                None => None,
                Some(value) if value.is_null() => None,
                Some(_) => Some(parse_media_target(&nested(input, "target")?)?),
            };
            Ok(MediaOperation::Status { target })
        }
        MediaOperationKind::Capture => Ok(MediaOperation::Capture {
            source: source_ref(required_string(input, "source_ref")?),
            options: CaptureOptions {
                max_duration_ms: optional_u64(input, "max_duration_ms", 1, 3_600_000)?,
                max_dimension_px: optional_u32(input, "max_dimension_px", 1, 2_000)?,
                jpeg_quality: optional_f32(input, "jpeg_quality", 0.0, 1.0)?,
            },
        }),
        MediaOperationKind::Record => Ok(MediaOperation::Record {
            source: source_ref(required_string(input, "source_ref")?),
            options: RecordOptions {
                max_duration_ms: optional_u64(input, "max_duration_ms", 1, 3_600_000)?,
                mime_type: optional_string(input, "mime_type")?,
            },
        }),
        MediaOperationKind::Transcribe => {
            let source = nested(input, "source")?;
            let source_kind = string_at(&source, "kind").ok_or("`source.kind` is required")?;
            let selection = match source_kind {
                "live" => TranscriptionInput::LiveSource(source_ref(required_string(
                    &source,
                    "source_ref",
                )?)),
                "stored" => TranscriptionInput::StoredMedia(media_ref(required_string(
                    &source,
                    "media_ref",
                )?)),
                _ => return Err("`source.kind` must be `live` or `stored`".into()),
            };
            Ok(MediaOperation::Transcribe {
                input: selection,
                language: optional_string(input, "language")?,
            })
        }
        MediaOperationKind::Synthesize | MediaOperationKind::Speak => {
            let text = required_string(input, "text")?;
            if text.chars().count() > MAX_SPEECH_TEXT_CHARS {
                return Err(format!(
                    "`text` must be at most {MAX_SPEECH_TEXT_CHARS} characters"
                ));
            }
            let language = optional_string(input, "language")?;
            let voice = optional_string(input, "voice")?;
            let rate = optional_f32(input, "rate", 0.5, 2.0)?;
            if kind == MediaOperationKind::Synthesize {
                Ok(MediaOperation::Synthesize {
                    text,
                    language,
                    rate,
                    voice,
                })
            } else {
                Ok(MediaOperation::Speak {
                    text,
                    language,
                    rate,
                    voice,
                })
            }
        }
        MediaOperationKind::Play => Ok(MediaOperation::Play {
            media: media_ref(required_string(input, "media_ref")?),
            source: optional_string(input, "source_ref")?.map(source_ref),
        }),
        MediaOperationKind::SessionStart => {
            let intent = match string_at(input, "intent").ok_or("`intent` is required")? {
                "observation" => MediaSessionIntent::Observation,
                "conversation" => MediaSessionIntent::Conversation,
                _ => return Err("`intent` must be `observation` or `conversation`".into()),
            };
            let mode = match string_at(input, "mode").ok_or("`mode` is required")? {
                "audio" => MediaSessionMode::Audio,
                "video" => MediaSessionMode::Video,
                "audio_video" => MediaSessionMode::AudioVideo,
                _ => return Err("`mode` must be `audio`, `video`, or `audio_video`".into()),
            };
            let audio_source = optional_string(input, "audio_source_ref")?.map(source_ref);
            let video_source = optional_string(input, "video_source_ref")?.map(source_ref);
            if matches!(mode, MediaSessionMode::Audio | MediaSessionMode::AudioVideo)
                && audio_source.is_none()
            {
                return Err("`audio_source_ref` is required for an audio session".into());
            }
            if matches!(mode, MediaSessionMode::Video | MediaSessionMode::AudioVideo)
                && video_source.is_none()
            {
                return Err("`video_source_ref` is required for a video session".into());
            }
            Ok(MediaOperation::SessionStart {
                request: MediaSessionRequest {
                    intent,
                    mode,
                    audio_source,
                    video_source,
                },
            })
        }
        MediaOperationKind::SessionUpdate => {
            let update_value = nested(input, "update")?;
            let update_kind =
                string_at(&update_value, "type").ok_or("`update.type` is required")?;
            let update = match update_kind {
                "set_enabled" => {
                    let kind = match string_at(&update_value, "kind") {
                        Some("audio") => MediaKind::Audio,
                        Some("video") => MediaKind::Video,
                        _ => return Err("`update.kind` must be `audio` or `video`".into()),
                    };
                    let enabled = update_value
                        .get_property("enabled")
                        .and_then(|value| value.as_bool())
                        .ok_or("`update.enabled` must be a boolean")?;
                    MediaSessionUpdate::SetEnabled { kind, enabled }
                }
                "select_source" => {
                    let kind = match string_at(&update_value, "kind") {
                        Some("audio") => MediaKind::Audio,
                        Some("video") => MediaKind::Video,
                        _ => return Err("`update.kind` must be `audio` or `video`".into()),
                    };
                    MediaSessionUpdate::SelectSource {
                        kind,
                        source: source_ref(required_string(&update_value, "source_ref")?),
                    }
                }
                "set_playback_gain" => {
                    let gain = optional_f32(&update_value, "gain", 0.0, 1.0)?
                        .ok_or("`update.gain` is required")?;
                    MediaSessionUpdate::SetPlaybackGain { gain }
                }
                _ => return Err("`update.type` is unsupported".into()),
            };
            Ok(MediaOperation::SessionUpdate {
                session: session_ref(required_string(input, "session_ref")?),
                update,
            })
        }
        MediaOperationKind::Observe | MediaOperationKind::Wait => {
            let target = parse_media_target(&nested(input, "target")?)?;
            let after = parse_cursor(input)?;
            let limit = optional_u64(input, "limit", 1, u64::from(MAX_MEDIA_EVENT_BATCH_SIZE))?
                .unwrap_or(32) as u16;
            if kind == MediaOperationKind::Observe {
                Ok(MediaOperation::Observe {
                    target,
                    after,
                    limit,
                })
            } else {
                let timeout_ms = optional_u64(input, "timeout_ms", 1, MAX_MEDIA_TIMEOUT_MS)?
                    .unwrap_or(DEFAULT_WAIT_TIMEOUT_MS);
                Ok(MediaOperation::Wait {
                    target,
                    after,
                    timeout_ms,
                    limit,
                })
            }
        }
        MediaOperationKind::Stop => Ok(MediaOperation::Stop {
            target: parse_stop_target(&nested(input, "target")?)?,
        }),
    }
}

async fn operation_context(
    service: &Arc<dyn media_api::MediaService>,
    use_context: &ToolUseContext,
    timeout: Duration,
) -> Result<MediaOperationContext, ToolError> {
    let owner = if let Some(session_id) = use_context.origin_session_id.as_ref() {
        MediaOwner::Session {
            session_id: session_id.as_uuid().to_string(),
        }
    } else if let Some(session) = use_context.session.as_ref() {
        MediaOwner::Session {
            session_id: session.lock().await.session_id.as_uuid().to_string(),
        }
    } else {
        return Err(ToolError::InvalidInput(
            "media call has no trusted session owner".into(),
        ));
    };
    let capabilities = service.capabilities_for(&owner);
    let generation = NEXT_MEDIA_GENERATION.fetch_add(1, Ordering::Relaxed).max(1);
    let identity = MediaOperationId::new(generation, capabilities.service_epoch);
    let timeout_budget_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
    let initiator = media_api::MediaInitiator {
        agent_id: use_context.agent_id.as_ref().map(ToString::to_string),
        tool_use_id: use_context.tool_use_id.as_ref().map(ToString::to_string),
        request_id: None,
    };
    Ok(MediaOperationContext::new(identity, owner)
        .with_initiator(initiator)
        .with_timeout_budget_ms(timeout_budget_ms)
        .with_max_payload_bytes(capabilities.max_payload_bytes))
}

fn map_media_error(error: MediaError) -> ToolError {
    match error.kind {
        MediaErrorKind::PermissionDenied => ToolError::PermissionDenied(error.message.into()),
        MediaErrorKind::Cancelled => ToolError::Aborted,
        MediaErrorKind::InvalidRequest => ToolError::InvalidInput(error.message.into()),
        kind => ToolError::Internal(format!("{kind}: {}", error.message)),
    }
}

fn readiness_error(state: MediaReadinessState) -> Option<MediaErrorKind> {
    match state {
        MediaReadinessState::Busy => Some(MediaErrorKind::Busy),
        MediaReadinessState::Unavailable => Some(MediaErrorKind::Unavailable),
        MediaReadinessState::MissingModel => Some(MediaErrorKind::ModelMissing),
        MediaReadinessState::Ready | MediaReadinessState::NeedsPermission => None,
    }
}

async fn execute_operation(
    service: &Arc<dyn media_api::MediaService>,
    use_context: &ToolUseContext,
    context: MediaOperationContext,
    operation: MediaOperation,
) -> Result<MediaResult, ToolError> {
    let kind = operation.kind();
    let capabilities = service.capabilities_for(&context.owner);
    if !capabilities.supports(kind) {
        return Err(map_media_error(MediaError::new(
            MediaErrorKind::Unsupported,
            "media operation is not supported by this host",
        )));
    }
    if let Some(state) = capabilities.readiness_for(kind) {
        if let Some(error_kind) = readiness_error(state) {
            return Err(map_media_error(MediaError::new(
                error_kind,
                "media operation is not ready on this host",
            )));
        }
    }
    if use_context
        .cancel
        .as_ref()
        .is_some_and(|cancel| cancel.is_cancelled())
    {
        return Err(ToolError::Aborted);
    }
    let budget = context.timeout_budget_ms.unwrap_or(30_000);
    if budget == 0 {
        return Err(map_media_error(MediaError::new(
            MediaErrorKind::Timeout,
            "media operation has no remaining time budget",
        )));
    }
    let cancel_context = context.clone();
    let deadline = Duration::from_millis(budget);
    let call = service.execute(context, operation);
    let result = if let Some(cancel) = use_context.cancel.as_ref() {
        tokio::select! {
            biased;
            result = call => result.map_err(map_media_error),
            _ = tokio::time::sleep(deadline) => {
                let _ = service.cancel(cancel_context.clone()).await;
                return Err(map_media_error(MediaError::new(MediaErrorKind::Timeout, "media operation exceeded its deadline")));
            }
            _ = cancel.cancelled() => {
                let _ = service.cancel(cancel_context.clone()).await;
                return Err(ToolError::Aborted);
            }
        }
    } else {
        match tokio::time::timeout(deadline, call).await {
            Ok(result) => result.map_err(map_media_error),
            Err(_) => {
                let _ = service.cancel(cancel_context).await;
                return Err(map_media_error(MediaError::new(
                    MediaErrorKind::Timeout,
                    "media operation exceeded its deadline",
                )));
            }
        }
    }?;
    Ok(result)
}

async fn read_media(
    service: &Arc<dyn media_api::MediaService>,
    use_context: &ToolUseContext,
    owner: MediaOwner,
    asset: &MediaAsset,
    timeout: Duration,
) -> Result<(MediaContent, MediaOperationContext), ToolError> {
    let capabilities = service.capabilities_for(&owner);
    let generation = NEXT_MEDIA_GENERATION.fetch_add(1, Ordering::Relaxed).max(1);
    let initiator = media_api::MediaInitiator {
        agent_id: use_context.agent_id.as_ref().map(ToString::to_string),
        tool_use_id: use_context.tool_use_id.as_ref().map(ToString::to_string),
        request_id: None,
    };
    let context = MediaOperationContext::new(
        MediaOperationId::new(generation, capabilities.service_epoch),
        owner,
    )
    .with_initiator(initiator)
    .with_timeout_budget_ms(u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX))
    .with_max_payload_bytes(capabilities.max_payload_bytes);
    let cancel_context = context.clone();
    let future = service.read_media(context.clone(), asset.reference.clone());
    let content = if let Some(cancel) = use_context.cancel.as_ref() {
        tokio::select! {
            biased;
            result = future => result.map_err(map_media_error)?,
            _ = tokio::time::sleep(timeout) => {
                let _ = service.cancel(cancel_context.clone()).await;
                return Err(map_media_error(MediaError::new(MediaErrorKind::Timeout, "media read exceeded its deadline")));
            }
            _ = cancel.cancelled() => {
                let _ = service.cancel(cancel_context.clone()).await;
                return Err(ToolError::Aborted);
            }
        }
    } else {
        match tokio::time::timeout(timeout, future).await {
            Ok(result) => result.map_err(map_media_error)?,
            Err(_) => {
                let _ = service.cancel(cancel_context).await;
                return Err(map_media_error(MediaError::new(
                    MediaErrorKind::Timeout,
                    "media read exceeded its deadline",
                )));
            }
        }
    };
    content
        .validate(context.max_payload_bytes)
        .map_err(map_media_error)?;
    if content.reference != asset.reference {
        return Err(ToolError::Internal(
            "media service returned bytes for a different asset reference".into(),
        ));
    }
    if content.metadata.kind != asset.metadata.kind
        || content.metadata.mime_type != asset.metadata.mime_type
        || content.metadata.byte_len != asset.metadata.byte_len
    {
        return Err(ToolError::Internal(
            "media service returned content inconsistent with its asset metadata".into(),
        ));
    }
    Ok((content, context))
}

fn image_result(
    asset: &MediaAsset,
    content: MediaContent,
    context: &MediaOperationContext,
    captured_at_unix_ms: Option<u64>,
) -> Result<ToolCallResult, ToolError> {
    if content.metadata.kind != MediaKind::Image
        || !content.metadata.mime_type.starts_with("image/")
    {
        return Err(ToolError::Internal(
            "media service returned non-image content for a visual observation".into(),
        ));
    }
    let original_size = content.bytes.len();
    let processed =
        tool_api::util::image_budget::process_image(content.bytes).map_err(ToolError::Internal)?;
    let (width, height) = processed.dimensions;
    let mut file = json!({
        "base64": processed.base64,
        "type": processed.media_type,
        "originalSize": original_size,
    });
    if let Some((original_width, original_height, display_width, display_height)) =
        processed.resized
    {
        file["dimensions"] = json!({
            "originalWidth": original_width,
            "originalHeight": original_height,
            "displayWidth": display_width,
            "displayHeight": display_height,
        });
    }
    Ok(ToolCallResult {
        data: generated_projection(&json!({
            "type":"image",
            "file":file,
            "media_ref":asset.reference.0,
            "width":width,
            "height":height,
            "mime_type":content.metadata.mime_type,
            "originalSize":original_size,
            "captured_at_unix_ms":captured_at_unix_ms
                .or(content.metadata.captured_at_unix_ms)
                .or(asset.metadata.captured_at_unix_ms),
            "operation_ref":operation_ref_json(context)
        })),
        model_content: Some("[Image attached for visual analysis.]".into()),
        new_messages: Vec::new(),
        context_modifier: None,
        mcp_meta: None,
        is_error: false,
        subagent_hooks_fired: false,
    })
}

fn plain_result(data: Value) -> ToolCallResult {
    ToolCallResult::from_data(generated_projection(&data))
}

fn source_kind_name(kind: MediaSourceKind) -> &'static str {
    match kind {
        MediaSourceKind::Microphone => "microphone",
        MediaSourceKind::Camera => "camera",
        MediaSourceKind::Screen => "screen",
        MediaSourceKind::Speaker => "speaker",
        MediaSourceKind::Library => "library",
        MediaSourceKind::Remote => "remote",
    }
}

fn media_kind_name(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Audio => "audio",
        MediaKind::Image => "image",
        MediaKind::Video => "video",
    }
}

fn usage_name(usage: MediaSourceUsage) -> &'static str {
    match usage {
        MediaSourceUsage::Capture => "capture",
        MediaSourceUsage::Record => "record",
        MediaSourceUsage::Transcribe => "transcribe",
        MediaSourceUsage::Play => "play",
        MediaSourceUsage::Speak => "speak",
        MediaSourceUsage::Session => "session",
        MediaSourceUsage::ReadImage => "read_image",
    }
}

fn operation_name(kind: MediaOperationKind) -> &'static str {
    match kind {
        MediaOperationKind::Sources => "media_sources",
        MediaOperationKind::Status => "media_status",
        MediaOperationKind::Capture => "media_capture",
        MediaOperationKind::Record => "media_record",
        MediaOperationKind::Transcribe => "media_transcribe",
        MediaOperationKind::Synthesize => "media_synthesize",
        MediaOperationKind::Play => "media_play",
        MediaOperationKind::Speak => "media_speak",
        MediaOperationKind::SessionStart => "media_session_start",
        MediaOperationKind::SessionUpdate => "media_session_update",
        MediaOperationKind::Observe => "media_observe",
        MediaOperationKind::Wait => "media_wait",
        MediaOperationKind::Stop => "media_stop",
    }
}

fn resource_json(resource: &MediaResourceRef) -> Value {
    match resource {
        MediaResourceRef::Source(source) => json!({"kind":"source","ref":source.0.0}),
        MediaResourceRef::Recording(recording) => json!({"kind":"recording","ref":recording.0.0}),
        MediaResourceRef::Session(session) => json!({"kind":"session","ref":session.0.0}),
        MediaResourceRef::Media(media) => json!({"kind":"media","ref":media.0}),
        MediaResourceRef::Operation {
            identity,
            incarnation,
        } => operation_ref_value(identity, *incarnation),
    }
}

fn operation_ref_value(
    operation: &MediaOperationId,
    incarnation: media_api::MediaOperationIncarnation,
) -> Value {
    json!({
        "kind":"operation",
        "ref":operation.id,
        "generation":operation.generation,
        "service_epoch":operation.service_epoch,
        "incarnation":incarnation.as_string()
    })
}

fn operation_ref_json(context: &MediaOperationContext) -> Value {
    operation_ref_value(&context.identity, context.operation_incarnation())
}

fn operation_id_value(operation: &MediaOperationId) -> Value {
    json!({
        "id":operation.id,
        "generation":operation.generation,
        "service_epoch":operation.service_epoch
    })
}

fn resource_state_name(state: MediaResourceState) -> &'static str {
    match state {
        MediaResourceState::Ready => "ready",
        MediaResourceState::Active => "active",
        MediaResourceState::Stopping => "stopping",
        MediaResourceState::Completed => "completed",
        MediaResourceState::Failed => "failed",
    }
}

fn metadata_json(metadata: &MediaMetadata) -> Value {
    json!({
        "kind":media_kind_name(metadata.kind),
        "mime_type":metadata.mime_type,
        "byte_len":metadata.byte_len,
        "captured_at_unix_ms":metadata.captured_at_unix_ms,
        "duration_ms":metadata.duration_ms,
        "width_px":metadata.width_px,
        "height_px":metadata.height_px,
        "sample_rate_hz":metadata.sample_rate_hz,
        "channels":metadata.channels
    })
}

fn asset_json(asset: &MediaAsset) -> Value {
    json!({"media_ref":asset.reference.0,"metadata":metadata_json(&asset.metadata)})
}

fn plain_operation_result(mut data: Value, context: &MediaOperationContext) -> ToolCallResult {
    if let Value::Object(object) = &mut data {
        object.insert("operation_ref".to_owned(), operation_ref_json(context));
    }
    plain_result(data)
}

fn handoff_cleanup_failure_result(
    handoff_error: &str,
    cleanup_error: &str,
    session: &MediaSessionRef,
    context: &MediaOperationContext,
) -> ToolCallResult {
    let recovery_target = json!({
        "kind":"session",
        "ref":session.0.0
    });
    let target_text = recovery_target.to_string();
    let mut result = plain_operation_result(
        json!({
            "error":"could not schedule the realtime media handoff",
            "handoff_error":handoff_error,
            "cleanup_error":cleanup_error,
            "session_ref":session.0.0,
            "recovery":{
                "tool":"media_stop",
                "target":recovery_target
            }
        }),
        context,
    );
    result.model_content = Some(format!(
        "Could not schedule the realtime media handoff: {handoff_error}. Cleanup also failed: {cleanup_error}. The media session may still be active. Retry `media_stop` with target {target_text}."
    ).into());
    result.is_error = true;
    result
}

fn event_json(event: &media_api::MediaEvent) -> Value {
    let mut value = json!({
        "sequence":event.sequence,
        "occurred_at_unix_ms":event.occurred_at_unix_ms,
        "target":resource_json(&event.target),
            "operation_id":event.operation_id.as_ref().map(operation_id_value)
    });
    let detail = match &event.kind {
        MediaEventKind::StateChanged { resource, state } => json!({
            "type":"state_changed","resource":resource_json(resource),"state":resource_state_name(*state)
        }),
        MediaEventKind::Frame { source, asset } => json!({
            "type":"frame","source_ref":source.0.0,"asset":asset_json(asset)
        }),
        MediaEventKind::Transcript { transcript } => json!({
            "type":"transcript","text":transcript.text,"language":transcript.language,"confidence":transcript.confidence
        }),
        MediaEventKind::Error { kind, message } => json!({
            "type":"error","kind":kind.to_string(),"message":message
        }),
    };
    value["event"] = detail;
    value
}

fn event_batch_json(batch: &MediaEventBatch) -> Value {
    json!({
        "target":resource_json(&batch.target),
        "events":batch.events.iter().map(event_json).collect::<Vec<_>>(),
        "next_sequence":batch.next_cursor.map(|cursor| cursor.sequence),
        "dropped_before_cursor":batch.dropped_before_cursor,
        "event_count":batch.events.len()
    })
}

fn parse_timeout(kind: MediaOperationKind, input: &Utf16JsonProjection) -> Duration {
    let millis = if kind == MediaOperationKind::Wait {
        optional_u64(input, "timeout_ms", 1, MAX_MEDIA_TIMEOUT_MS)
            .ok()
            .flatten()
            .unwrap_or(DEFAULT_WAIT_TIMEOUT_MS)
            .saturating_add(5_000)
    } else {
        match kind {
            MediaOperationKind::Capture => 120_000,
            MediaOperationKind::Transcribe | MediaOperationKind::Synthesize => 180_000,
            MediaOperationKind::Play | MediaOperationKind::Speak => 1_200_000,
            MediaOperationKind::Stop => 60_000,
            _ => 30_000,
        }
    };
    Duration::from_millis(millis)
}

fn execution_metadata(
    kind: MediaOperationKind,
    input: &Utf16JsonProjection,
    source_kinds: &StdMutex<HashMap<(u64, String), Option<SourceKindInfo>>>,
    service_epoch: Option<u64>,
) -> ToolExecutionMetadata {
    let operation = parse_operation(kind, input);
    let mut resources = Vec::new();
    if operation.is_err() {
        match kind {
            MediaOperationKind::Capture => {
                resources.extend([MediaResource::Microphone, MediaResource::Camera]);
            }
            MediaOperationKind::Record => {
                resources.extend([MediaResource::Microphone, MediaResource::Camera]);
            }
            MediaOperationKind::Transcribe => resources.push(MediaResource::Microphone),
            MediaOperationKind::Play | MediaOperationKind::Speak => {
                resources.push(MediaResource::Speaker);
            }
            MediaOperationKind::SessionStart | MediaOperationKind::SessionUpdate => {
                resources.extend([
                    MediaResource::Microphone,
                    MediaResource::Camera,
                    MediaResource::Speaker,
                ]);
            }
            MediaOperationKind::Observe | MediaOperationKind::Wait => {
                resources.extend([MediaResource::Microphone, MediaResource::Camera]);
            }
            MediaOperationKind::Sources
            | MediaOperationKind::Status
            | MediaOperationKind::Synthesize
            | MediaOperationKind::Stop => {}
        }
    }
    match operation {
        Ok(MediaOperation::Capture { source, .. }) => {
            let source_kind = service_epoch.and_then(|epoch| {
                source_kinds
                    .lock()
                    .ok()
                    .and_then(|known| known.get(&(epoch, source.0 .0.clone())).copied().flatten())
            });
            push_source_resource(&mut resources, source_kind);
        }
        Ok(MediaOperation::Record { source, .. }) => {
            let source_kind = service_epoch.and_then(|epoch| {
                source_kinds
                    .lock()
                    .ok()
                    .and_then(|known| known.get(&(epoch, source.0 .0.clone())).copied().flatten())
            });
            push_source_resource(&mut resources, source_kind);
        }
        Ok(MediaOperation::Transcribe {
            input: TranscriptionInput::LiveSource(_),
            ..
        }) => {
            resources.push(MediaResource::Microphone);
        }
        Ok(MediaOperation::Play { .. } | MediaOperation::Speak { .. }) => {
            resources.push(MediaResource::Speaker);
        }
        Ok(MediaOperation::SessionStart { request }) => {
            if request.audio_source.is_some() {
                resources.push(MediaResource::Microphone);
            }
            if matches!(request.intent, MediaSessionIntent::Conversation) {
                resources.push(MediaResource::Speaker);
            }
            if request.video_source.is_some() {
                resources.push(MediaResource::Camera);
            }
        }
        Ok(MediaOperation::SessionUpdate { update, .. }) => match update {
            MediaSessionUpdate::SetEnabled {
                kind: MediaKind::Audio,
                enabled: true,
            }
            | MediaSessionUpdate::SelectSource {
                kind: MediaKind::Audio,
                ..
            } => resources.push(MediaResource::Microphone),
            MediaSessionUpdate::SetEnabled {
                kind: MediaKind::Video,
                enabled: true,
            }
            | MediaSessionUpdate::SelectSource {
                kind: MediaKind::Video,
                ..
            } => resources.push(MediaResource::Camera),
            MediaSessionUpdate::SetPlaybackGain { .. } => resources.push(MediaResource::Speaker),
            _ => {}
        },
        Ok(MediaOperation::Observe {
            target: MediaResourceRef::Source(source),
            ..
        }) => {
            let source_kind = service_epoch.and_then(|epoch| {
                source_kinds
                    .lock()
                    .ok()
                    .and_then(|known| known.get(&(epoch, source.0 .0.clone())).copied().flatten())
            });
            push_source_resource(&mut resources, source_kind);
        }
        // These resources expose already-buffered events. Observe does not
        // acquire or hold a microphone/camera lease.
        Ok(MediaOperation::Observe {
            target:
                MediaResourceRef::Recording(_)
                | MediaResourceRef::Session(_)
                | MediaResourceRef::Operation { .. },
            ..
        }) => {}
        _ => {}
    }
    resources.sort_by_key(|resource| match resource {
        MediaResource::Microphone => 0,
        MediaResource::Camera => 1,
        MediaResource::Speaker => 2,
    });
    resources.dedup();
    ToolExecutionMetadata {
        exclusive_media_resources: resources,
    }
}

fn push_source_resource(resources: &mut Vec<MediaResource>, source: Option<SourceKindInfo>) {
    match source.map(|source| (source.source_kind, source.media_kind)) {
        Some((MediaSourceKind::Microphone, _)) => resources.push(MediaResource::Microphone),
        Some((MediaSourceKind::Camera | MediaSourceKind::Screen, _))
        | Some((MediaSourceKind::Library, Some(MediaKind::Image | MediaKind::Video))) => {
            resources.push(MediaResource::Camera);
        }
        Some((MediaSourceKind::Speaker, _)) => resources.push(MediaResource::Speaker),
        // A stored audio item or remote source does not consume a local device.
        Some((MediaSourceKind::Library | MediaSourceKind::Remote, Some(MediaKind::Audio))) => {}
        Some((MediaSourceKind::Remote, Some(MediaKind::Image | MediaKind::Video))) => {}
        // Unknown modality, stale, or not-yet-discovered sources cannot be
        // safely narrowed from an opaque reference.
        Some((MediaSourceKind::Library | MediaSourceKind::Remote, None)) | None => {
            resources.extend([MediaResource::Microphone, MediaResource::Camera]);
        }
    }
}

#[async_trait]
impl Tool for MediaTool {
    fn name(&self) -> &str {
        self.name
    }

    fn input_schema(&self) -> &Utf16JsonProjection {
        &self.schema
    }

    fn input_schema_snapshot(&self) -> Option<Utf16JsonProjection> {
        (self.kind == MediaOperationKind::SessionStart).then(|| {
            let modes = supported_conversation_modes(self.ctx.media_agent_handoff.as_ref());
            generated_projection(&schema_for(self.kind, &modes))
        })
    }

    fn input_schema_snapshot_for_prompt(
        &self,
        options: &PromptOptions,
    ) -> Option<Utf16JsonProjection> {
        (self.kind == MediaOperationKind::SessionStart).then(|| {
            generated_projection(&schema_for(
                self.kind,
                &self.prompt_conversation_modes(options),
            ))
        })
    }

    fn input_repair_metadata(&self) -> ToolInputRepairMetadata {
        schema_repair()
    }

    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        self.supported()
    }

    fn input_schema_revision(&self) -> Option<String> {
        self.service().map(|service| {
            let capabilities = service.capabilities();
            let conversation_modes =
                supported_conversation_modes(self.ctx.media_agent_handoff.as_ref());
            format!(
                "{}:{}:{:?}",
                capabilities.service_epoch, capabilities.support_revision, conversation_modes
            )
        })
    }

    fn input_schema_revision_for_prompt(&self, options: &PromptOptions) -> Option<String> {
        self.service().map(|service| {
            let capabilities = service.capabilities();
            let conversation_modes = if self.kind == MediaOperationKind::SessionStart {
                self.prompt_conversation_modes(options)
            } else {
                Vec::new()
            };
            format!(
                "{}:{}:{:?}",
                capabilities.service_epoch, capabilities.support_revision, conversation_modes
            )
        })
    }

    fn max_result_size_chars(&self) -> usize {
        4_096
    }

    fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
        matches!(
            self.kind,
            MediaOperationKind::Sources | MediaOperationKind::Status
        )
    }

    fn is_read_only(&self, _: &Utf16JsonProjection) -> bool {
        matches!(
            self.kind,
            MediaOperationKind::Sources
                | MediaOperationKind::Status
                | MediaOperationKind::Transcribe
                | MediaOperationKind::Observe
                | MediaOperationKind::Wait
        )
    }

    fn execution_metadata(&self, input: &Utf16JsonProjection) -> ToolExecutionMetadata {
        execution_metadata(
            self.kind,
            input,
            &self.source_kinds,
            self.service()
                .map(|service| service.capabilities().service_epoch),
        )
    }

    fn catalog_execution_metadata(&self) -> ToolExecutionMetadata {
        // Capture and record accept camera or microphone sources. Keep both in
        // the live catalog and narrow them after `media_sources` reports the
        // selected source; unknown refs are handled conservatively at dispatch.
        let resources = match self.kind {
            MediaOperationKind::Play | MediaOperationKind::Speak => vec![MediaResource::Speaker],
            _ => Vec::new(),
        };
        ToolExecutionMetadata {
            exclusive_media_resources: resources,
        }
    }

    async fn check_permissions(
        &self,
        _: &Utf16JsonProjection,
        _: &ToolUseContext,
    ) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "The host media service handles native OS permission prompts for the requested operation.".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, _input: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
        let action = operation_name(self.kind);
        match self.kind {
            MediaOperationKind::Capture => "Capturing a selected media source".into(),
            MediaOperationKind::Transcribe => {
                "Transcribing the selected live or stored audio".into()
            }
            MediaOperationKind::SessionStart => "Starting a host-configured media session".into(),
            MediaOperationKind::Observe => "Observing current events from a media target".into(),
            MediaOperationKind::Wait => "Waiting for events from a media target".into(),
            _ => format!("Running {action}"),
        }
    }

    async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
        format!(
            "Use `{}` with typed references returned by `media_sources` or earlier media operations. Follow its current schema; the host owns permissions, resource routing, and media bytes.",
            self.name
        )
        .into()
    }

    async fn validate_input(
        &self,
        input: &Utf16JsonProjection,
        use_context: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        let operation =
            parse_operation(self.kind, input).map_err(|message| ValidationError(message.into()))?;
        self.validate_conversation_mode(&operation, use_context)
            .await
            .map(|_| ())
            .map_err(|message| ValidationError(message.into()))
    }

    async fn call(
        &self,
        input: Utf16JsonProjection,
        use_context: ToolUseContext,
        _progress_tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let operation = parse_operation(self.kind, &input)
            .map_err(|message| ToolError::InvalidInput(message.into()))?;
        let capability_context = self
            .validate_conversation_mode(&operation, &use_context)
            .await
            .map_err(|message| ToolError::InteractionRequired(message.into()))?;
        let service = self.service().ok_or_else(|| {
            ToolError::Internal("media service is unavailable on this host".into())
        })?;
        let timeout = parse_timeout(self.kind, &input);
        let context = operation_context(service, &use_context, timeout).await?;
        let owner = context.owner.clone();
        let result =
            execute_operation(service, &use_context, context.clone(), operation.clone()).await?;
        match (self.kind, result) {
            (MediaOperationKind::Sources, MediaResult::Sources { sources }) => {
                if let Ok(mut known) = self.source_kinds.lock() {
                    for source in &sources {
                        let key = (
                            context.identity.service_epoch,
                            source.reference.0 .0.clone(),
                        );
                        let info = SourceKindInfo {
                            source_kind: source.kind,
                            media_kind: source.media_kind,
                        };
                        match known.get_mut(&key) {
                            Some(previous) if *previous != Some(info) => *previous = None,
                            Some(_) => {}
                            None => {
                                known.insert(key, Some(info));
                            }
                        }
                    }
                }
                Ok(plain_operation_result(
                    json!({
                        "sources":sources.iter().map(|source| json!({
                            "source_ref":source.reference.0.0,
                            "kind":source_kind_name(source.kind),
                            "media_kind":source.media_kind.map(media_kind_name),
                            "label":source.label,
                            "usages":source.usages.iter().copied().map(usage_name).collect::<Vec<_>>(),
                            "supported_operations":source.supported_operations.iter().copied().map(operation_name).collect::<Vec<_>>(),
                            "is_default":source.is_default
                        })).collect::<Vec<_>>()
                    }),
                    &context,
                ))
            }
            (MediaOperationKind::Status, MediaResult::Status { resources }) => {
                Ok(plain_operation_result(
                    json!({
                        "resources":resources.iter().map(|resource| json!({
                            "target":resource_json(&resource.resource),
                            "state":resource_state_name(resource.state),
                            "started_at_unix_ms":resource.started_at_unix_ms,
                            "updated_at_unix_ms":resource.updated_at_unix_ms
                        })).collect::<Vec<_>>()
                    }),
                    &context,
                ))
            }
            (MediaOperationKind::Capture, MediaResult::Captured { asset }) => {
                if asset.metadata.kind == MediaKind::Image {
                    let (content, _) =
                        read_media(service, &use_context, owner, &asset, timeout).await?;
                    let captured_at = asset.metadata.captured_at_unix_ms;
                    image_result(&asset, content, &context, captured_at)
                } else {
                    Ok(plain_operation_result(
                        json!({"captured":true,"asset":asset_json(&asset)}),
                        &context,
                    ))
                }
            }
            (MediaOperationKind::Record, MediaResult::RecordingStarted { recording }) => {
                Ok(plain_operation_result(
                    json!({
                        "recording_ref":recording.0.0,
                        "recording":true,
                        "status":"active"
                    }),
                    &context,
                ))
            }
            (MediaOperationKind::Transcribe, MediaResult::Transcript { transcript }) => {
                Ok(plain_operation_result(
                    json!({
                        "text":transcript.text,
                        "language":transcript.language,
                        "confidence":transcript.confidence
                    }),
                    &context,
                ))
            }
            (MediaOperationKind::Synthesize, MediaResult::Synthesized { asset }) => {
                Ok(plain_operation_result(
                    json!({"synthesized":true,"asset":asset_json(&asset)}),
                    &context,
                ))
            }
            (MediaOperationKind::Play, MediaResult::PlaybackCompleted { duration_ms }) => Ok(
                plain_operation_result(json!({"played":true,"duration_ms":duration_ms}), &context),
            ),
            (MediaOperationKind::Speak, MediaResult::SpeechCompleted { duration_ms }) => Ok(
                plain_operation_result(json!({"spoken":true,"duration_ms":duration_ms}), &context),
            ),
            (MediaOperationKind::SessionStart, MediaResult::SessionStarted { session, status }) => {
                let conversation_requested = matches!(
                    &operation,
                    MediaOperation::SessionStart {
                        request: MediaSessionRequest {
                            intent: MediaSessionIntent::Conversation,
                            ..
                        }
                    }
                );
                if conversation_requested
                    && !matches!(
                        status.state,
                        media_api::MediaSessionState::Failed | media_api::MediaSessionState::Ended
                    )
                {
                    let request = tool_api::context::MediaAgentHandoffRequest {
                        operation_id: context.identity.clone(),
                        session: session.clone(),
                        mode: match &operation {
                            MediaOperation::SessionStart { request } => request.mode,
                            _ => unreachable!("session-start result has a session-start operation"),
                        },
                    };
                    let handoff_result = match self.ctx.media_agent_handoff.as_ref() {
                        None => Err(
                            "this host cannot hand a conversation media session to the live Agent"
                                .to_owned(),
                        ),
                        Some(handoff) => {
                            match use_context
                                .media_agent_capability_context(
                                    service.attachment_id(),
                                )
                                .await
                            {
                                Some(current_context)
                                    if capability_context.as_ref().is_some_and(|initial| {
                                        initial == &current_context
                                    }) && handoff.supports_mode(&current_context, request.mode) =>
                                {
                                    handoff.request(&use_context, &current_context, request)
                                }
                                _ => Err(
                                    "the current Agent provider no longer supports this media conversation mode"
                                        .to_owned(),
                                ),
                            }
                        }
                    };
                    if let Err(message) = handoff_result {
                        let mut cleanup_use_context = use_context.clone();
                        cleanup_use_context.cancel = None;
                        let cleanup_result: Result<(), ToolError> = if service
                            .capabilities_for(&owner)
                            .supports(MediaOperationKind::Stop)
                        {
                            match tokio::time::timeout(Duration::from_secs(30), async {
                                let cleanup = operation_context(
                                    service,
                                    &cleanup_use_context,
                                    Duration::from_secs(30),
                                )
                                .await?;
                                match execute_operation(
                                    service,
                                    &cleanup_use_context,
                                    cleanup,
                                    MediaOperation::Stop {
                                        target: MediaResourceRef::Session(session.clone()),
                                    },
                                )
                                .await?
                                {
                                    MediaResult::Stopped {
                                        resource: MediaResourceRef::Session(stopped),
                                    } if stopped.0 .0 == session.0 .0 => Ok(()),
                                    _ => Err(ToolError::Internal(
                                        "media service did not confirm stopping the opened session"
                                            .into(),
                                    )),
                                }
                            })
                            .await
                            {
                                Ok(result) => result,
                                Err(_) => Err(ToolError::Timeout { timeout_ms: 30_000 }),
                            }
                        } else {
                            Err(ToolError::InteractionRequired(
                                "media_stop is unsupported on this host".into(),
                            ))
                        };
                        if let Err(cleanup_error) = cleanup_result {
                            let cleanup_error = cleanup_error.model_facing_message();
                            return Ok(handoff_cleanup_failure_result(
                                &message,
                                cleanup_error
                                    .string_at("")
                                    .unwrap_or("media cleanup failed"),
                                &session,
                                &context,
                            ));
                        }
                        return Err(ToolError::InteractionRequired(format!(
                            "could not schedule the realtime media handoff: {message}"
                        )));
                    }
                }
                Ok(plain_operation_result(
                    json!({
                        "session_ref":session.0.0,
                        "status":format!("{:?}",status.state).to_lowercase(),
                        "audio_enabled":status.audio_enabled,
                        "video_enabled":status.video_enabled
                    }),
                    &context,
                ))
            }
            (
                MediaOperationKind::SessionUpdate,
                MediaResult::SessionUpdated { session, status },
            ) => Ok(plain_operation_result(
                json!({
                    "session_ref":session.0.0,
                    "status":format!("{:?}",status.state).to_lowercase(),
                    "audio_enabled":status.audio_enabled,
                    "video_enabled":status.video_enabled
                }),
                &context,
            )),
            (
                MediaOperationKind::Observe | MediaOperationKind::Wait,
                MediaResult::Events { batch },
            ) => {
                let mut data = event_batch_json(&batch);
                // Deliver a real, budgeted pixel payload for the newest frame.
                // Other event summaries remain in the JSON result.
                let latest_frame = batch
                    .events
                    .iter()
                    .rev()
                    .find_map(|event| match &event.kind {
                        MediaEventKind::Frame { asset, .. }
                            if asset.metadata.kind == MediaKind::Image =>
                        {
                            Some((event, asset))
                        }
                        _ => None,
                    });
                if let Some((event, asset)) = latest_frame {
                    let (content, _) =
                        read_media(service, &use_context, owner, asset, timeout).await?;
                    let image =
                        image_result(asset, content, &context, Some(event.occurred_at_unix_ms))?;
                    let image_json = image
                        .data
                        .to_json_string()
                        .map_err(|error| ToolError::Internal(error.to_string()))?;
                    let image_value: Value = serde_json::from_str(&image_json)
                        .map_err(|error| ToolError::Internal(error.to_string()))?;
                    if let Value::Object(image_fields) = image_value {
                        for (key, value) in image_fields {
                            data[&key] = value;
                        }
                    }
                    data["operation_ref"] = operation_ref_json(&context);
                    Ok(ToolCallResult {
                        data: generated_projection(&data),
                        model_content: image.model_content,
                        new_messages: Vec::new(),
                        context_modifier: None,
                        mcp_meta: None,
                        is_error: false,
                        subagent_hooks_fired: false,
                    })
                } else {
                    Ok(plain_operation_result(data, &context))
                }
            }
            (MediaOperationKind::Stop, MediaResult::RecordingStopped { recording, asset }) => {
                Ok(plain_operation_result(
                    json!({
                        "recording_ref":recording.0.0,
                        "recording":false,
                        "status":"completed",
                        "asset":asset_json(&asset)
                    }),
                    &context,
                ))
            }
            (MediaOperationKind::Stop, MediaResult::Stopped { resource }) => {
                Ok(plain_operation_result(
                    json!({"stopped":true,"target":resource_json(&resource)}),
                    &context,
                ))
            }
            _ => Err(ToolError::Internal(format!(
                "media service returned an invalid result for `{}`",
                self.name
            ))),
        }
    }
}

fn all_operation_kinds() -> [(MediaOperationKind, &'static str); 13] {
    [
        (MediaOperationKind::Sources, TOOL_NAMES[0]),
        (MediaOperationKind::Status, TOOL_NAMES[1]),
        (MediaOperationKind::Capture, TOOL_NAMES[2]),
        (MediaOperationKind::Record, TOOL_NAMES[3]),
        (MediaOperationKind::Transcribe, TOOL_NAMES[4]),
        (MediaOperationKind::Synthesize, TOOL_NAMES[5]),
        (MediaOperationKind::Play, TOOL_NAMES[6]),
        (MediaOperationKind::Speak, TOOL_NAMES[7]),
        (MediaOperationKind::SessionStart, TOOL_NAMES[8]),
        (MediaOperationKind::SessionUpdate, TOOL_NAMES[9]),
        (MediaOperationKind::Observe, TOOL_NAMES[10]),
        (MediaOperationKind::Wait, TOOL_NAMES[11]),
        (MediaOperationKind::Stop, TOOL_NAMES[12]),
    ]
}

/// Register every media operation when a host service exists.
///
/// Capability and readiness can change after the runtime is composed, so
/// `is_enabled` filters the live registry snapshot instead of freezing the
/// initial capability list at registration time. Readiness is not an
/// availability condition: supported tools remain visible while permission is
/// needed, a model is missing, or a device resource is temporarily busy.
pub fn register_all(reg: &mut tool_api::ToolRegistry, ctx: BuiltinToolContext) {
    if ctx.media.is_none() {
        return;
    }
    let source_kinds = Arc::new(StdMutex::new(HashMap::new()));
    for (kind, name) in all_operation_kinds() {
        reg.register_builtin(Arc::new(MediaTool::with_source_kinds(
            ctx.clone(),
            kind,
            name,
            source_kinds.clone(),
        )));
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod schema_consistency_tests;

#[cfg(test)]
mod prompt_context_tests;
