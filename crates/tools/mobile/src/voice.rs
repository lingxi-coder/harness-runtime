//! `voice` exposes raw recording with host-managed session handles.

use lingxi_core::host::audio::{
    AudioErrorKind, AudioOperation, AudioOperationContext, AudioOperationId, AudioOperationKind,
    AudioOperationSuccess, AudioOwner, AudioRecordingHandle, AudioService,
};
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
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
pub const TOOL_NAME: &str = "voice";

fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "action": { "type": "string", "enum": ["start_recording", "stop_recording", "is_recording"] },
            "sample_rate_hz": { "type": "integer", "minimum": 8000, "maximum": 48000 },
            "format": { "type": "string" }
        },
        "required": ["action"]
    })
}

fn sample_rate_hz(input: &Value) -> Result<u32, String> {
    match input.get("sample_rate_hz") {
        None | Some(Value::Null) => Ok(16_000),
        Some(value) => {
            let rate = value
                .as_u64()
                .and_then(|rate| u32::try_from(rate).ok())
                .filter(|rate| (8_000..=48_000).contains(rate))
                .ok_or_else(|| {
                    "`sample_rate_hz` must be an integer from 8000 to 48000".to_string()
                })?;
            Ok(rate)
        }
    }
}

fn recording_format(input: &Value) -> Result<String, String> {
    match input.get("format") {
        None | Some(Value::Null) => Ok("m4a".into()),
        Some(value) => value
            .as_str()
            .filter(|format| !format.trim().is_empty() && format.len() <= 32)
            .map(str::to_string)
            .ok_or_else(|| "`format` must be a non-empty string of at most 32 bytes".into()),
    }
}

/// Public raw microphone tool backed by the app-scoped AudioService.
#[derive(Clone)]
pub struct VoiceTool {
    ctx: BuiltinToolContext,
    schema: Value,
    starts: Arc<StdMutex<HashSet<AudioOwner>>>,
}

impl VoiceTool {
    /// Construct from the builtin tool context.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self {
            ctx,
            schema: input_schema(),
            starts: Arc::new(StdMutex::new(HashSet::new())),
        }
    }

    fn supports_record(&self) -> bool {
        self.ctx.audio.as_ref().is_some_and(|audio| {
            operation_supported(&audio.capabilities(), AudioOperationKind::Record)
        })
    }
}

struct StartReservation {
    owners: Arc<StdMutex<HashSet<AudioOwner>>>,
    owner: AudioOwner,
}

impl Drop for StartReservation {
    fn drop(&mut self) {
        if let Ok(mut owners) = self.owners.lock() {
            owners.remove(&self.owner);
        }
    }
}

struct StartRollback {
    service: Arc<dyn AudioService>,
    context: AudioOperationContext,
    handle: Option<AudioRecordingHandle>,
    armed: bool,
}

impl StartRollback {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StartRollback {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let service = self.service.clone();
        let context = self.context.clone();
        let handle = self.handle.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Some(handle) = handle {
                    let mut cleanup = context.clone();
                    cleanup.identity = AudioOperationId::new(
                        context.identity.generation,
                        context.identity.service_epoch,
                    );
                    cleanup.timeout_budget_ms = Some(60_000);
                    let stopped = matches!(
                        tokio::time::timeout(
                            Duration::from_secs(60),
                            service
                                .execute(cleanup.clone(), AudioOperation::StopRecording { handle }),
                        )
                        .await,
                        Ok(Ok(AudioOperationSuccess::Recording { .. }))
                    );
                    if !stopped {
                        cleanup.identity = AudioOperationId::new(
                            context.identity.generation,
                            context.identity.service_epoch,
                        );
                        let _ = tokio::time::timeout(
                            Duration::from_secs(60),
                            service.execute(cleanup, AudioOperation::EndOwner),
                        )
                        .await;
                    }
                } else {
                    let _ = service.cancel(context.identity).await;
                }
            });
        }
    }
}

#[async_trait::async_trait]
impl Tool for VoiceTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn input_schema(&self) -> &Value {
        &self.schema
    }

    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        self.supports_record()
    }

    fn max_result_size_chars(&self) -> usize {
        4096
    }

    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }

    fn is_read_only(&self, _: &Value) -> bool {
        false
    }

    async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "voice tool — native OS microphone permission gates recording".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, input: &Value, _: &DescriptionOptions) -> String {
        match input.get("action").and_then(Value::as_str) {
            Some("stop_recording") => "Stopping the voice recording".into(),
            Some("is_recording") => "Checking recording state".into(),
            _ => "Starting a voice recording".into(),
        }
    }

    async fn prompt(&self, _: &PromptOptions) -> String {
        "Record audio from the device microphone.".into()
    }

    async fn validate_input(
        &self,
        input: &Value,
        _: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        if !self.supports_record() {
            return Err(ValidationError(
                "recording is not supported on this device".into(),
            ));
        }
        if input.get("sample_rate_hz").is_some() {
            sample_rate_hz(input).map_err(ValidationError)?;
        }
        if input.get("format").is_some() {
            recording_format(input).map_err(ValidationError)?;
        }
        match input.get("action").and_then(Value::as_str) {
            Some("start_recording") => Ok(()),
            Some("stop_recording" | "is_recording") => Ok(()),
            _ => Err(ValidationError(
                "`action` must be `start_recording`, `stop_recording`, or `is_recording`".into(),
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
        if !operation_supported(&service.capabilities(), AudioOperationKind::Record) {
            return Err(ToolError::Internal(
                "recording is not supported on this device".into(),
            ));
        }
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("start_recording");
        let sample_rate_hz = sample_rate_hz(&input).map_err(ToolError::InvalidInput)?;
        let format = recording_format(&input).map_err(ToolError::InvalidInput)?;
        let timeout = match action {
            "start_recording" => Duration::from_secs(120),
            _ => Duration::from_secs(30),
        };
        let context = operation_context(service, &use_context, timeout).await?;
        let owner = context.owner.clone();

        let data = match action {
            "start_recording" => {
                // The reservation is owner-local and held only through this
                // call. It prevents duplicate starts for one conversation
                // without holding the shared handle-map mutex across native
                // permission prompts or other long audio operations.
                let inserted = self
                    .starts
                    .lock()
                    .map_err(|_| {
                        ToolError::Internal("voice start reservation is unavailable".into())
                    })?
                    .insert(owner.clone());
                if !inserted {
                    return Err(ToolError::Internal(format!(
                        "{}: a recording start is already pending for this session",
                        AudioErrorKind::Busy
                    )));
                }
                let _reservation = StartReservation {
                    owners: self.starts.clone(),
                    owner: owner.clone(),
                };
                if self
                    .ctx
                    .audio_recording_handles
                    .lock()
                    .await
                    .contains_key(&owner)
                {
                    return Err(ToolError::Internal(format!(
                        "{}: this session already owns a recording",
                        AudioErrorKind::Busy
                    )));
                }
                let mut rollback = StartRollback {
                    service: service.clone(),
                    context: context.clone(),
                    handle: None,
                    armed: true,
                };
                match execute_operation(
                    service,
                    &use_context,
                    context,
                    AudioOperation::StartRecording {
                        sample_rate_hz,
                        format,
                    },
                )
                .await?
                {
                    AudioOperationSuccess::RecordingStarted { handle } => {
                        rollback.handle = Some(handle.clone());
                        // Keep the rollback guard armed while acquiring the
                        // short registry lock: cancellation in this delivery
                        // gap must stop the completed native recording.
                        let mut handles = self.ctx.audio_recording_handles.lock().await;
                        if handles.contains_key(&owner) {
                            return Err(ToolError::Internal(format!(
                                "{}: this session already owns a recording",
                                AudioErrorKind::Busy
                            )));
                        }
                        handles.insert(owner, handle);
                        rollback.disarm();
                        json!({ "recording": true })
                    }
                    _ => {
                        return Err(ToolError::Internal(
                            "audio service returned an invalid start result".into(),
                        ))
                    }
                }
            }
            "stop_recording" => {
                let handle = self
                    .ctx
                    .audio_recording_handles
                    .lock()
                    .await
                    .get(&owner)
                    .cloned()
                    .ok_or_else(|| {
                        ToolError::Internal(format!(
                            "{}: no recording handle belongs to this session",
                            AudioErrorKind::NotRecording
                        ))
                    })?;
                match execute_operation(
                    service,
                    &use_context,
                    context,
                    AudioOperation::StopRecording {
                        handle: handle.clone(),
                    },
                )
                .await
                {
                    Ok(AudioOperationSuccess::Recording { recording }) => {
                        let mut handles = self.ctx.audio_recording_handles.lock().await;
                        if handles.get(&owner) == Some(&handle) {
                            handles.remove(&owner);
                        }
                        json!({
                            "recording": false,
                            "audio_bytes_len": recording.audio_bytes.len(),
                            "mime_type": recording.mime_type,
                        })
                    }
                    Ok(_) => {
                        return Err(ToolError::Internal(
                            "audio service returned an invalid stop result".into(),
                        ))
                    }
                    Err(error) => {
                        let mut handles = self.ctx.audio_recording_handles.lock().await;
                        if matches!(&error, ToolError::Internal(message) if message.starts_with("not_recording:"))
                            && handles.get(&owner) == Some(&handle)
                        {
                            handles.remove(&owner);
                        }
                        return Err(error);
                    }
                }
            }
            "is_recording" => {
                let handle = self
                    .ctx
                    .audio_recording_handles
                    .lock()
                    .await
                    .get(&owner)
                    .cloned();
                match execute_operation(
                    service,
                    &use_context,
                    context,
                    AudioOperation::Status {
                        handle: handle.clone(),
                    },
                )
                .await
                {
                    Ok(AudioOperationSuccess::Status { status }) => {
                        let mut handles = self.ctx.audio_recording_handles.lock().await;
                        if !status.recording
                            && handle
                                .as_ref()
                                .is_some_and(|handle| handles.get(&owner) == Some(handle))
                        {
                            handles.remove(&owner);
                        }
                        json!({ "recording": status.recording })
                    }
                    Err(ToolError::Internal(message)) if message.starts_with("not_recording:") => {
                        let mut handles = self.ctx.audio_recording_handles.lock().await;
                        if handle
                            .as_ref()
                            .is_some_and(|handle| handles.get(&owner) == Some(handle))
                        {
                            handles.remove(&owner);
                        }
                        json!({ "recording": false })
                    }
                    Err(error) => return Err(error),
                    Ok(_) => {
                        return Err(ToolError::Internal(
                            "audio service returned an invalid status result".into(),
                        ))
                    }
                }
            }
            _ => return Err(ToolError::InvalidInput("unknown voice action".into())),
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

/// Register `voice` against `reg`.
pub fn register_all(reg: &mut tool_api::ToolRegistry, ctx: tool_api::BuiltinToolContext) {
    reg.register_builtin(Arc::new(VoiceTool::new(ctx)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use lingxi_core::host::audio::{
        AudioCapabilitySnapshot, AudioError, AudioOperationId, AudioOperationReadiness,
        AudioReadinessState, AudioRecordingHandle, AudioService, AudioStatus,
    };
    use lingxi_core::host::VoiceRecording;
    use std::collections::HashMap;
    use std::sync::atomic::Ordering;
    use std::sync::Mutex;

    struct FakeAudio {
        active: Mutex<HashMap<lingxi_core::host::audio::AudioOwner, AudioRecordingHandle>>,
        operations: Mutex<
            Vec<(
                lingxi_core::host::audio::AudioOperationContext,
                AudioOperation,
            )>,
        >,
        cancelled: Mutex<Vec<AudioOperationId>>,
        fail_status: bool,
    }

    #[async_trait]
    impl AudioService for FakeAudio {
        fn capabilities(&self) -> AudioCapabilitySnapshot {
            AudioCapabilitySnapshot {
                service_epoch: 4,
                support_revision: 2,
                supported_operations: vec![AudioOperationKind::Record],
                readiness: vec![AudioOperationReadiness {
                    operation: AudioOperationKind::Record,
                    state: AudioReadinessState::Ready,
                }],
                max_payload_bytes: 4096,
            }
        }

        async fn execute(
            &self,
            context: lingxi_core::host::audio::AudioOperationContext,
            operation: AudioOperation,
        ) -> Result<AudioOperationSuccess, AudioError> {
            self.operations
                .lock()
                .unwrap()
                .push((context.clone(), operation.clone()));
            match operation {
                AudioOperation::StartRecording { .. } => {
                    let handle =
                        AudioRecordingHandle(format!("handle-{}", context.identity.generation));
                    self.active
                        .lock()
                        .unwrap()
                        .insert(context.owner, handle.clone());
                    Ok(AudioOperationSuccess::RecordingStarted { handle })
                }
                AudioOperation::StopRecording { handle } => {
                    let mut active = self.active.lock().unwrap();
                    if active.get(&context.owner) != Some(&handle) {
                        return Err(AudioError::new(
                            AudioErrorKind::NotRecording,
                            "unknown recording handle",
                        ));
                    }
                    active.remove(&context.owner);
                    Ok(AudioOperationSuccess::Recording {
                        recording: VoiceRecording {
                            audio_bytes: vec![1, 2, 3],
                            mime_type: "audio/m4a".into(),
                        },
                    })
                }
                AudioOperation::Status { handle } => {
                    if self.fail_status {
                        return Err(AudioError::new(
                            AudioErrorKind::NativeFailure,
                            "status transport failed",
                        ));
                    }
                    let active = self.active.lock().unwrap();
                    let recording = match handle {
                        Some(handle) if active.get(&context.owner) != Some(&handle) => {
                            return Err(AudioError::new(
                                AudioErrorKind::NotRecording,
                                "stale recording handle",
                            ));
                        }
                        Some(_) => true,
                        None => active.contains_key(&context.owner),
                    };
                    Ok(AudioOperationSuccess::Status {
                        status: AudioStatus {
                            recording,
                            playing: false,
                        },
                    })
                }
                AudioOperation::EndOwner => {
                    self.active.lock().unwrap().remove(&context.owner);
                    Ok(AudioOperationSuccess::OwnerEnded)
                }
                _ => Err(AudioError::new(
                    AudioErrorKind::Unsupported,
                    "unexpected operation",
                )),
            }
        }

        async fn cancel(&self, identity: AudioOperationId) -> Result<(), AudioError> {
            self.cancelled.lock().unwrap().push(identity);
            Ok(())
        }
    }

    fn context(service: Arc<dyn AudioService>) -> BuiltinToolContext {
        let mut context = tool_api::test_support::shell_test_ctx(mobile_linux_api::ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        });
        context.audio = Some(service);
        context
    }

    fn use_context(session_id: lingxi_core::types::SessionId) -> ToolUseContext {
        let mut context = tool_api::test_support::fresh_ctx();
        context.origin_session_id = Some(session_id);
        context
    }

    #[tokio::test]
    async fn dropped_start_with_a_returned_handle_stops_the_recording() {
        let audio = Arc::new(FakeAudio {
            active: Mutex::new(HashMap::new()),
            operations: Mutex::new(Vec::new()),
            cancelled: Mutex::new(Vec::new()),
            fail_status: false,
        });
        let service: Arc<dyn AudioService> = audio.clone();
        let use_context = use_context(lingxi_core::types::SessionId::new());
        let context = operation_context(&service, &use_context, Duration::from_secs(5))
            .await
            .unwrap();
        let handle = match service
            .execute(
                context.clone(),
                AudioOperation::StartRecording {
                    sample_rate_hz: 16_000,
                    format: "m4a".into(),
                },
            )
            .await
            .unwrap()
        {
            AudioOperationSuccess::RecordingStarted { handle } => handle,
            _ => panic!("expected a recording handle"),
        };
        drop(StartRollback {
            service,
            context: context.clone(),
            handle: Some(handle),
            armed: true,
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while !audio.active.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the completed recording is stopped");
        assert!(audio
            .operations
            .lock()
            .unwrap()
            .iter()
            .any(|(_, operation)| { matches!(operation, AudioOperation::StopRecording { .. }) }));
        assert!(audio.cancelled.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn start_stop_and_status_use_the_host_scoped_handle() {
        let audio = Arc::new(FakeAudio {
            active: Mutex::new(HashMap::new()),
            operations: Mutex::new(Vec::new()),
            cancelled: Mutex::new(Vec::new()),
            fail_status: false,
        });
        let builtin = context(audio.clone());
        let handle_registry = builtin.audio_recording_handles.clone();
        let tool = VoiceTool::new(builtin);
        let session = lingxi_core::types::SessionId::new();
        let started = tool
            .call(
                json!({ "action": "start_recording" }),
                use_context(session),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(started.data["recording"], true);
        assert!(
            started.data.get("handle").is_none(),
            "native handles stay host-managed"
        );
        let owner = audio.operations.lock().unwrap()[0].0.owner.clone();
        assert!(
            handle_registry.lock().await.contains_key(&owner),
            "the start handle survives its completed call"
        );
        assert!(
            audio.cancelled.lock().unwrap().is_empty(),
            "a committed start must not be cancelled when its future completes"
        );

        let status = tool
            .call(
                json!({ "action": "is_recording" }),
                use_context(session),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(status.data["recording"], true);
        let stopped = tool
            .call(
                json!({ "action": "stop_recording" }),
                use_context(session),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(stopped.data["recording"], false);
        assert_eq!(stopped.data["audio_bytes_len"], 3);
        assert_eq!(
            audio
                .operations
                .lock()
                .unwrap()
                .iter()
                .map(|(_, op)| match op {
                    AudioOperation::StartRecording { .. } => "start",
                    AudioOperation::Status { .. } => "status",
                    AudioOperation::StopRecording { .. } => "stop",
                    _ => "other",
                })
                .collect::<Vec<_>>(),
            vec!["start", "status", "stop"],
        );
        let operations = audio.operations.lock().unwrap();
        let owner = operations[0].0.owner.clone();
        assert!(operations.iter().all(|(context, _)| context.owner == owner));
        assert_ne!(
            operations[0].0.identity, operations[2].0.identity,
            "StopRecording has a fresh operation identity"
        );
        assert!(matches!(
            &operations[1].1,
            AudioOperation::Status { handle: Some(_) }
        ));
        assert!(matches!(
            &operations[2].1,
            AudioOperation::StopRecording { .. }
        ));
    }

    struct StartRaceAudio {
        first_entered: tokio::sync::Notify,
        second_entered: tokio::sync::Notify,
        release_first: tokio::sync::Notify,
        calls: std::sync::atomic::AtomicUsize,
        active: Mutex<HashMap<lingxi_core::host::audio::AudioOwner, AudioRecordingHandle>>,
        starts: Mutex<HashMap<AudioOperationId, lingxi_core::host::audio::AudioOwner>>,
        start_ids: Mutex<Vec<AudioOperationId>>,
        cancelled: Mutex<Vec<AudioOperationId>>,
    }

    #[async_trait]
    impl AudioService for StartRaceAudio {
        fn capabilities(&self) -> AudioCapabilitySnapshot {
            AudioCapabilitySnapshot {
                service_epoch: 8,
                support_revision: 1,
                supported_operations: vec![AudioOperationKind::Record],
                readiness: vec![AudioOperationReadiness {
                    operation: AudioOperationKind::Record,
                    state: AudioReadinessState::Ready,
                }],
                max_payload_bytes: 4096,
            }
        }

        async fn execute(
            &self,
            context: lingxi_core::host::audio::AudioOperationContext,
            operation: AudioOperation,
        ) -> Result<AudioOperationSuccess, AudioError> {
            let AudioOperation::StartRecording { .. } = operation else {
                return Err(AudioError::new(
                    AudioErrorKind::Unsupported,
                    "unexpected operation",
                ));
            };
            let index = self.calls.fetch_add(1, Ordering::SeqCst);
            self.start_ids
                .lock()
                .unwrap()
                .push(context.identity.clone());
            if index != 0 {
                self.second_entered.notify_one();
                return Err(AudioError::new(AudioErrorKind::Busy, "device is busy"));
            }
            let handle = AudioRecordingHandle(context.identity.id.clone());
            self.active
                .lock()
                .unwrap()
                .insert(context.owner.clone(), handle.clone());
            self.starts
                .lock()
                .unwrap()
                .insert(context.identity.clone(), context.owner.clone());
            self.first_entered.notify_one();
            self.release_first.notified().await;
            if self.cancelled.lock().unwrap().contains(&context.identity) {
                return Err(AudioError::new(
                    AudioErrorKind::Cancelled,
                    "start cancelled",
                ));
            }
            Ok(AudioOperationSuccess::RecordingStarted { handle })
        }

        async fn cancel(&self, identity: AudioOperationId) -> Result<(), AudioError> {
            self.cancelled.lock().unwrap().push(identity.clone());
            if let Some(owner) = self.starts.lock().unwrap().remove(&identity) {
                self.active.lock().unwrap().remove(&owner);
            }
            self.release_first.notify_one();
            Ok(())
        }
    }

    #[tokio::test]
    async fn pending_start_does_not_serialize_a_different_owner_and_drop_cancels_exact_identity() {
        let audio = Arc::new(StartRaceAudio {
            first_entered: tokio::sync::Notify::new(),
            second_entered: tokio::sync::Notify::new(),
            release_first: tokio::sync::Notify::new(),
            calls: std::sync::atomic::AtomicUsize::new(0),
            active: Mutex::new(HashMap::new()),
            starts: Mutex::new(HashMap::new()),
            start_ids: Mutex::new(Vec::new()),
            cancelled: Mutex::new(Vec::new()),
        });
        let builtin_context = context(audio.clone());
        let handle_registry = builtin_context.audio_recording_handles.clone();
        let tool = Arc::new(VoiceTool::new(builtin_context));
        let first_session = lingxi_core::types::SessionId::new();
        let second_session = lingxi_core::types::SessionId::new();
        let first = tokio::spawn({
            let tool = tool.clone();
            async move {
                tool.call(
                    json!({ "action": "start_recording" }),
                    use_context(first_session),
                    tool_api::test_support::fresh_tx(),
                )
                .await
            }
        });
        tokio::time::timeout(Duration::from_secs(1), audio.first_entered.notified())
            .await
            .expect("first owner reaches the pending native start");

        let second = tokio::time::timeout(
            Duration::from_millis(250),
            tool.call(
                json!({ "action": "start_recording" }),
                use_context(second_session),
                tool_api::test_support::fresh_tx(),
            ),
        )
        .await
        .expect("second owner must reach the service without waiting on shared handle storage");
        assert!(
            matches!(second, Err(ToolError::Internal(message)) if message.starts_with("busy:"))
        );
        assert_eq!(audio.calls.load(Ordering::SeqCst), 2);

        let first_identity = audio.start_ids.lock().unwrap()[0].clone();
        first.abort();
        let _ = first.await;
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if audio.active.lock().unwrap().is_empty()
                    && audio.cancelled.lock().unwrap().contains(&first_identity)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("dropping an undelivered start cancels its exact identity and releases capture");
        assert!(
            handle_registry.lock().await.is_empty(),
            "an undelivered handle is never committed"
        );
    }

    #[tokio::test]
    async fn a_different_session_cannot_stop_another_sessions_recording() {
        let audio = Arc::new(FakeAudio {
            active: Mutex::new(HashMap::new()),
            operations: Mutex::new(Vec::new()),
            cancelled: Mutex::new(Vec::new()),
            fail_status: false,
        });
        let tool = VoiceTool::new(context(audio.clone()));
        let first_session = lingxi_core::types::SessionId::new();
        let second_session = lingxi_core::types::SessionId::new();
        tool.call(
            json!({ "action": "start_recording" }),
            use_context(first_session),
            tool_api::test_support::fresh_tx(),
        )
        .await
        .unwrap();
        let result = tool
            .call(
                json!({ "action": "stop_recording" }),
                use_context(second_session),
                tool_api::test_support::fresh_tx(),
            )
            .await;
        assert!(
            matches!(result, Err(ToolError::Internal(message)) if message.starts_with("not_recording:"))
        );
        let stopped = tool
            .call(
                json!({ "action": "stop_recording" }),
                use_context(first_session),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(stopped.data["recording"], false);
    }

    #[tokio::test]
    async fn stale_status_clears_handle_and_allows_new_recording() {
        let audio = Arc::new(FakeAudio {
            active: Mutex::new(HashMap::new()),
            operations: Mutex::new(Vec::new()),
            cancelled: Mutex::new(Vec::new()),
            fail_status: false,
        });
        let builtin = context(audio.clone());
        let registry = builtin.audio_recording_handles.clone();
        let tool = VoiceTool::new(builtin);
        let session = lingxi_core::types::SessionId::new();
        tool.call(
            json!({ "action": "start_recording" }),
            use_context(session),
            tool_api::test_support::fresh_tx(),
        )
        .await
        .unwrap();
        audio.active.lock().unwrap().clear();
        let status = tool
            .call(
                json!({ "action": "is_recording" }),
                use_context(session),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(status.data["recording"], false);
        assert!(registry.lock().await.is_empty());
        let restarted = tool
            .call(
                json!({ "action": "start_recording" }),
                use_context(session),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(restarted.data["recording"], true);
    }

    #[tokio::test]
    async fn status_failure_is_not_reported_as_idle() {
        let audio = Arc::new(FakeAudio {
            active: Mutex::new(HashMap::new()),
            operations: Mutex::new(Vec::new()),
            cancelled: Mutex::new(Vec::new()),
            fail_status: true,
        });
        let tool = VoiceTool::new(context(audio));
        let result = tool
            .call(
                json!({ "action": "is_recording" }),
                use_context(lingxi_core::types::SessionId::new()),
                tool_api::test_support::fresh_tx(),
            )
            .await;
        assert!(
            matches!(result, Err(ToolError::Internal(message)) if message.starts_with("native_failure:"))
        );
    }

    #[tokio::test]
    async fn invalid_explicit_sample_rate_never_enters_the_audio_service() {
        let audio = Arc::new(FakeAudio {
            active: Mutex::new(HashMap::new()),
            operations: Mutex::new(Vec::new()),
            cancelled: Mutex::new(Vec::new()),
            fail_status: false,
        });
        let tool = VoiceTool::new(context(audio.clone()));
        let input = json!({ "action": "start_recording", "sample_rate_hz": -1 });
        assert!(tool
            .validate_input(&input, &use_context(lingxi_core::types::SessionId::new()))
            .await
            .is_err());
        assert!(matches!(
            tool.call(input, use_context(lingxi_core::types::SessionId::new()), tool_api::test_support::fresh_tx()).await,
            Err(ToolError::InvalidInput(message)) if message.contains("sample_rate_hz")
        ));
        assert!(audio.operations.lock().unwrap().is_empty());
    }
}
