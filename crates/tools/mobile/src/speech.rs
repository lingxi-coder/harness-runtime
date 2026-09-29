//! `speech` exposes live recognition and completed speech playback.

use lingxi_core::host::audio::{AudioOperation, AudioOperationKind, AudioOperationSuccess};
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};
use tool_api::BuiltinToolContext;

use crate::audio_support::{execute_operation, operation_context, operation_supported};

/// Tool name byte-lock.
pub const TOOL_NAME: &str = "speech";
const MAX_SPEAK_TEXT_CHARS: usize = 5_550;

fn input_schema(supports_listen: bool, supports_speak: bool) -> Value {
    let mut actions = Vec::new();
    if supports_listen {
        actions.push("transcribe");
    }
    if supports_speak {
        actions.push("speak");
    }
    input_schema_for_action_rule(json!({ "type": "string", "enum": actions }))
}

fn runtime_validation_schema() -> Value {
    input_schema_for_action_rule(json!({ "type": "string" }))
}

fn input_schema_for_action_rule(action: Value) -> Value {
    json!({
        "type": "object",
        "properties": {
            "action": action,
            "text": { "type": "string", "maxLength": MAX_SPEAK_TEXT_CHARS, "description": "Text to speak (action=speak)." },
            "voice": { "type": "string", "description": "Voice id (action=speak, optional)." },
            "language": { "type": "string", "description": "BCP-47 hint for live listening or speech playback (optional)." },
            "rate": { "type": "number", "minimum": 0.5, "maximum": 2.0, "description": "Speech playback rate (action=speak, optional)." }
        },
        "required": ["action"]
    })
}

fn optional_string(input: &Value, key: &str) -> Result<Option<String>, String> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .filter(|text| !text.trim().is_empty())
            .map(|text| Some(text.to_string()))
            .ok_or_else(|| format!("`{key}` must be a non-empty string when provided")),
    }
}

fn optional_rate(input: &Value) -> Result<Option<f32>, String> {
    match input.get("rate") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let rate = value
                .as_f64()
                .ok_or_else(|| "`rate` must be a number between 0.5 and 2".to_string())?;
            if !rate.is_finite() || !(0.5..=2.0).contains(&rate) {
                return Err("`rate` must be a number between 0.5 and 2".into());
            }
            Ok(Some(rate as f32))
        }
    }
}

fn speak_text(input: &Value) -> Result<String, String> {
    let text = input
        .get("text")
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| "`speak` requires a non-empty `text`".to_string())?;
    if text.chars().count() > MAX_SPEAK_TEXT_CHARS {
        return Err(format!(
            "`text` must be at most {MAX_SPEAK_TEXT_CHARS} characters for speech playback"
        ));
    }
    Ok(text.to_string())
}

fn speech_timeout(text: &str) -> Duration {
    let chars = u64::try_from(text.chars().count()).unwrap_or(u64::MAX);
    Duration::from_secs(
        90_u64
            .saturating_add(chars.saturating_mul(200) / 1_000)
            .min(1_200),
    )
}

/// Public speech tool backed only by the app-scoped device AudioService.
#[derive(Clone)]
pub struct SpeechTool {
    ctx: BuiltinToolContext,
    schema: Value,
    validation_schema: Value,
}

impl SpeechTool {
    /// Construct from the builtin tool context.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        let (supports_listen, supports_speak) = supported_actions(&ctx);
        Self {
            ctx,
            schema: input_schema(supports_listen, supports_speak),
            validation_schema: runtime_validation_schema(),
        }
    }

    fn supported_actions(&self) -> (bool, bool) {
        supported_actions(&self.ctx)
    }
}

fn supported_actions(ctx: &BuiltinToolContext) -> (bool, bool) {
    ctx.audio
        .as_ref()
        .map(|service| {
            let capabilities = service.capabilities();
            (
                operation_supported(&capabilities, AudioOperationKind::Listen),
                operation_supported(&capabilities, AudioOperationKind::Speak),
            )
        })
        .unwrap_or((false, false))
}

#[async_trait::async_trait]
impl Tool for SpeechTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn input_schema(&self) -> &Value {
        &self.schema
    }

    fn input_validation_schema(&self) -> &Value {
        &self.validation_schema
    }

    fn input_schema_snapshot(&self) -> Option<Value> {
        let (supports_listen, supports_speak) = self.supported_actions();
        Some(input_schema(supports_listen, supports_speak))
    }

    fn input_schema_revision(&self) -> Option<String> {
        self.ctx.audio.as_ref().map(|service| {
            let capabilities = service.capabilities();
            format!(
                "{}:{}",
                capabilities.service_epoch, capabilities.support_revision
            )
        })
    }

    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        let (supports_listen, supports_speak) = self.supported_actions();
        supports_listen || supports_speak
    }

    fn max_result_size_chars(&self) -> usize {
        4096
    }

    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }

    fn is_read_only(&self, _: &Value) -> bool {
        true
    }

    async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "speech tool — native OS permission gates live audio operations".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, input: &Value, _: &DescriptionOptions) -> String {
        match input.get("action").and_then(Value::as_str) {
            Some("speak") => "Speaking the requested text".into(),
            _ => "Listening to live microphone speech".into(),
        }
    }

    async fn prompt(&self, _: &PromptOptions) -> String {
        "Listen to live speech from the microphone, or play synthesized speech.".into()
    }

    async fn validate_input(
        &self,
        input: &Value,
        _: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        let (supports_listen, supports_speak) = self.supported_actions();
        match input.get("action").and_then(Value::as_str) {
            Some("transcribe") if supports_listen => optional_string(input, "language")
                .map(|_| ())
                .map_err(ValidationError),
            Some("speak") if supports_speak => speak_text(input)
                .map(|_| ())
                .and_then(|_| optional_string(input, "voice").map(|_| ()))
                .and_then(|_| optional_string(input, "language").map(|_| ()))
                .and_then(|_| optional_rate(input).map(|_| ()))
                .map_err(ValidationError),
            Some("transcribe" | "speak") => Err(ValidationError(
                "the requested speech action is not supported on this device".into(),
            )),
            _ => Err(ValidationError(
                "`action` must be a supported `transcribe` or `speak` operation".into(),
            )),
        }
    }

    async fn call(
        &self,
        input: Value,
        use_context: ToolUseContext,
        _progress_tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let service = self.ctx.audio.as_ref().ok_or_else(|| {
            ToolError::Internal("device audio service is not available on this platform".into())
        })?;
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("transcribe");
        let (supports_listen, supports_speak) = self.supported_actions();
        let data = match action {
            "speak" if supports_speak => {
                let text = speak_text(&input).map_err(ToolError::InvalidInput)?;
                let voice = optional_string(&input, "voice").map_err(ToolError::InvalidInput)?;
                let language =
                    optional_string(&input, "language").map_err(ToolError::InvalidInput)?;
                let rate = optional_rate(&input).map_err(ToolError::InvalidInput)?;
                let context =
                    operation_context(service, &use_context, speech_timeout(&text)).await?;
                match execute_operation(
                    service,
                    &use_context,
                    context,
                    AudioOperation::Speak {
                        text,
                        language,
                        rate,
                        voice,
                    },
                )
                .await?
                {
                    AudioOperationSuccess::PlaybackCompleted { duration_ms } => {
                        json!({ "spoken": true, "duration_ms": duration_ms })
                    }
                    _ => {
                        return Err(ToolError::Internal(
                            "audio service returned an invalid speak result".into(),
                        ))
                    }
                }
            }
            "transcribe" if supports_listen => {
                let language =
                    optional_string(&input, "language").map_err(ToolError::InvalidInput)?;
                let context =
                    operation_context(service, &use_context, Duration::from_secs(60)).await?;
                match execute_operation(
                    service,
                    &use_context,
                    context,
                    AudioOperation::Listen { language },
                )
                .await?
                {
                    AudioOperationSuccess::Transcript { transcript } => json!({
                        "text": transcript.text,
                        "language": transcript.language,
                        "confidence": transcript.confidence,
                    }),
                    _ => {
                        return Err(ToolError::Internal(
                            "audio service returned an invalid listen result".into(),
                        ))
                    }
                }
            }
            _ => {
                return Err(ToolError::InvalidInput(
                    "the requested speech action is not supported on this device".into(),
                ))
            }
        };

        Ok(ToolCallResult {
            data,
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

/// Register `speech` against `reg`.
pub fn register_all(reg: &mut tool_api::ToolRegistry, ctx: tool_api::BuiltinToolContext) {
    reg.register_builtin(Arc::new(SpeechTool::new(ctx)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use lingxi_core::host::audio::{
        AudioCapabilitySnapshot, AudioError, AudioOperationContext, AudioOperationId,
        AudioOperationReadiness, AudioReadinessState, AudioService,
    };
    use lingxi_core::host::{SttTranscript, TtsAudio, VoiceRecording};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    struct FakeAudio {
        supported: Mutex<Vec<AudioOperationKind>>,
        operations: Mutex<Vec<AudioOperation>>,
        contexts: Mutex<Vec<AudioOperationContext>>,
        readiness: AudioReadinessState,
        support_revision: AtomicU64,
    }

    #[async_trait]
    impl AudioService for FakeAudio {
        fn capabilities(&self) -> AudioCapabilitySnapshot {
            let supported = self.supported.lock().unwrap().clone();
            AudioCapabilitySnapshot {
                service_epoch: 3,
                support_revision: self.support_revision.load(Ordering::SeqCst),
                supported_operations: supported.clone(),
                readiness: supported
                    .iter()
                    .copied()
                    .map(|operation| AudioOperationReadiness {
                        operation,
                        state: self.readiness,
                    })
                    .collect(),
                max_payload_bytes: 4096,
            }
        }

        async fn execute(
            &self,
            context: AudioOperationContext,
            operation: AudioOperation,
        ) -> Result<AudioOperationSuccess, AudioError> {
            self.contexts.lock().unwrap().push(context);
            self.operations.lock().unwrap().push(operation.clone());
            match operation {
                AudioOperation::Listen { .. } => Ok(AudioOperationSuccess::Transcript {
                    transcript: SttTranscript {
                        text: "live words".into(),
                        language: Some("en-US".into()),
                        confidence: Some(0.9),
                    },
                }),
                AudioOperation::Speak { .. } => {
                    Ok(AudioOperationSuccess::PlaybackCompleted { duration_ms: 85 })
                }
                AudioOperation::Synthesize { .. } => Ok(AudioOperationSuccess::Synthesized {
                    audio: TtsAudio {
                        pcm: vec![0, 0],
                        sample_rate_hz: 24_000,
                    },
                }),
                AudioOperation::StartRecording { .. } => {
                    Ok(AudioOperationSuccess::RecordingStarted {
                        handle: lingxi_core::host::audio::AudioRecordingHandle("unused".into()),
                    })
                }
                AudioOperation::StopRecording { .. } => Ok(AudioOperationSuccess::Recording {
                    recording: VoiceRecording {
                        audio_bytes: vec![],
                        mime_type: "audio/m4a".into(),
                    },
                }),
                AudioOperation::Status { .. } => Ok(AudioOperationSuccess::Status {
                    status: Default::default(),
                }),
                AudioOperation::EndOwner => Ok(AudioOperationSuccess::OwnerEnded),
            }
        }

        async fn cancel(&self, _identity: AudioOperationId) -> Result<(), AudioError> {
            Ok(())
        }
    }

    fn fake_context(
        supported: Vec<AudioOperationKind>,
        readiness: AudioReadinessState,
    ) -> (BuiltinToolContext, Arc<FakeAudio>) {
        let mut context =
            tool_api::test_support::shell_test_ctx(lingxi_core::host::process::ProcessOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                timed_out: false,
            });
        let audio = Arc::new(FakeAudio {
            supported: Mutex::new(supported),
            operations: Mutex::new(Vec::new()),
            contexts: Mutex::new(Vec::new()),
            readiness,
            support_revision: AtomicU64::new(9),
        });
        context.audio = Some(audio.clone());
        (context, audio)
    }

    fn use_context() -> ToolUseContext {
        let mut context = tool_api::test_support::fresh_ctx();
        context.origin_session_id = Some(lingxi_core::types::SessionId::new());
        context
    }

    #[tokio::test]
    async fn transcribe_always_uses_live_listen() {
        let (context, audio) =
            fake_context(vec![AudioOperationKind::Listen], AudioReadinessState::Ready);
        let tool = SpeechTool::new(context);
        let result = tool
            .call(
                json!({ "action": "transcribe", "language": "zh-CN" }),
                use_context(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .expect("live listen succeeds");
        assert_eq!(result.data["text"], "live words");
        assert_eq!(result.data["language"], "en-US");
        assert!(
            matches!(audio.operations.lock().unwrap().as_slice(), [AudioOperation::Listen { language: Some(language) }] if language == "zh-CN")
        );
    }

    #[tokio::test]
    async fn speak_waits_for_actual_playback_completion() {
        let (context, audio) =
            fake_context(vec![AudioOperationKind::Speak], AudioReadinessState::Ready);
        let tool = SpeechTool::new(context);
        let result = tool
            .call(
                json!({
                    "action": "speak",
                    "text": "hello",
                    "voice": "speaker-1",
                    "language": "en-US",
                    "rate": 1.25,
                }),
                use_context(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .expect("playback completes");
        assert_eq!(result.data["spoken"], true);
        assert_eq!(result.data["duration_ms"], 85);
        assert!(matches!(
            audio.operations.lock().unwrap().as_slice(),
            [AudioOperation::Speak { text, voice: Some(voice), language: Some(language), rate: Some(rate) }]
                if text == "hello" && voice == "speaker-1" && language == "en-US" && *rate == 1.25
        ));
    }

    #[tokio::test]
    async fn speak_deadline_scales_with_long_utterances_and_has_a_bound() {
        let (context, audio) =
            fake_context(vec![AudioOperationKind::Speak], AudioReadinessState::Ready);
        let tool = SpeechTool::new(context);
        let text = "a".repeat(2_000);

        tool.call(
            json!({ "action": "speak", "text": text }),
            use_context(),
            tool_api::test_support::fresh_tx(),
        )
        .await
        .expect("long playback completes");

        let contexts = audio.contexts.lock().unwrap();
        assert_eq!(contexts.len(), 1);
        assert_eq!(contexts[0].timeout_budget_ms, Some(490_000));

        assert_eq!(
            speech_timeout(&"a".repeat(MAX_SPEAK_TEXT_CHARS)),
            Duration::from_secs(1_200)
        );
    }

    #[tokio::test]
    async fn speak_rejects_text_that_exceeds_the_deadline_bound() {
        let (context, audio) =
            fake_context(vec![AudioOperationKind::Speak], AudioReadinessState::Ready);
        let tool = SpeechTool::new(context);
        let input = json!({
            "action": "speak",
            "text": "a".repeat(MAX_SPEAK_TEXT_CHARS + 1),
        });

        assert_eq!(
            tool.input_schema()["properties"]["text"]["maxLength"],
            MAX_SPEAK_TEXT_CHARS
        );
        assert!(tool.validate_input(&input, &use_context()).await.is_err());
        assert!(matches!(
            tool.call(input, use_context(), tool_api::test_support::fresh_tx()).await,
            Err(ToolError::InvalidInput(message)) if message.contains("at most")
        ));
        assert!(audio.operations.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn readiness_does_not_remove_supported_actions_from_the_schema() {
        let (context, _) = fake_context(vec![AudioOperationKind::Speak], AudioReadinessState::Busy);
        let tool = SpeechTool::new(context);
        assert_eq!(
            tool.input_schema()["properties"]["action"]["enum"],
            json!(["speak"])
        );
        assert!(tool
            .validate_input(&json!({ "action": "transcribe" }), &use_context())
            .await
            .is_err());
        assert!(tool
            .validate_input(&json!({ "action": "speak", "text": "hi" }), &use_context())
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn live_support_updates_change_enabled_state_and_schema_snapshot() {
        let (context, audio) = fake_context(Vec::new(), AudioReadinessState::Unavailable);
        let tool = SpeechTool::new(context);
        let static_context = ToolStaticContext::default();
        assert!(!tool.is_enabled(&static_context));

        *audio.supported.lock().unwrap() = vec![AudioOperationKind::Speak];
        audio.support_revision.store(10, Ordering::SeqCst);
        assert!(tool.is_enabled(&static_context));
        assert_eq!(tool.input_schema_revision().as_deref(), Some("3:10"));
        assert_eq!(
            tool.input_schema_snapshot().unwrap()["properties"]["action"]["enum"],
            json!(["speak"])
        );
        assert!(tool
            .validate_input(&json!({ "action": "speak", "text": "hi" }), &use_context())
            .await
            .is_ok());

        audio.supported.lock().unwrap().clear();
        assert!(!tool.is_enabled(&static_context));
        assert_eq!(
            tool.input_schema_snapshot().unwrap()["properties"]["action"]["enum"],
            json!([])
        );
    }

    #[tokio::test]
    async fn post_handshake_runtime_validation_uses_live_support_without_advertising_early() {
        let (context, audio) = fake_context(Vec::new(), AudioReadinessState::Unavailable);
        let tool = SpeechTool::new(context);
        let input = json!({ "action": "speak", "text": "hello after handshake" });

        assert_eq!(
            tool.input_schema_snapshot().unwrap()["properties"]["action"]["enum"],
            json!([]),
            "the pre-handshake capability schema must stay truthful"
        );
        assert!(
            tool.input_validation_schema()["properties"]["action"]
                .get("enum")
                .is_none(),
            "runtime shape schema must not cache the pre-handshake empty enum"
        );
        assert!(tool.validate_input(&input, &use_context()).await.is_err());

        *audio.supported.lock().unwrap() = vec![AudioOperationKind::Speak];
        audio.support_revision.store(10, Ordering::SeqCst);
        assert_eq!(
            tool.input_schema_snapshot().unwrap()["properties"]["action"]["enum"],
            json!(["speak"]),
            "the advertised schema updates to the post-handshake capability"
        );
        tool.validate_input(&input, &use_context())
            .await
            .expect("the live validator accepts speech once the service advertises it");
        let result = tool
            .call(input, use_context(), tool_api::test_support::fresh_tx())
            .await
            .expect("post-handshake speech is callable");

        assert_eq!(result.data["spoken"], true);
        assert!(matches!(
            audio.operations.lock().unwrap().as_slice(),
            [AudioOperation::Speak { text, .. }] if text == "hello after handshake"
        ));
    }

    #[tokio::test]
    async fn invalid_playback_rate_is_rejected_without_entering_the_service() {
        let (context, audio) =
            fake_context(vec![AudioOperationKind::Speak], AudioReadinessState::Ready);
        let tool = SpeechTool::new(context);
        let input = json!({ "action": "speak", "text": "hello", "rate": 3.0 });
        assert!(tool.validate_input(&input, &use_context()).await.is_err());
        assert!(matches!(
            tool.call(input, use_context(), tool_api::test_support::fresh_tx()).await,
            Err(ToolError::InvalidInput(message)) if message.contains("rate")
        ));
        assert!(audio.operations.lock().unwrap().is_empty());
    }
}
