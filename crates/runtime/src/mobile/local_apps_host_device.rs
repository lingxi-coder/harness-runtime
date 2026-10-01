//! Device operations of the `window.lingxi.v2` bridge (`device.*`).
//!
//! Every operation runs the same ladder: parse+clamp the page payload →
//! [`LocalAppsHostBroker::authorize_declared_capability`] (manifest-declared,
//! then persisted → session → prompt) → dispatch into the live
//! [`crate::mobile::local_apps_device::SharedDeviceCapabilities`] handle → envelope
//! the result as JSON with media returned as base64 (the page turns it into
//! a Blob URL; see `lib/lingxi-bridge.js`). Media responses are capped at
//! [`MAX_DEVICE_MEDIA_RESULT_BYTES`] — `evaluateJavaScript` delivers the
//! envelope as one string, and multi-MB strings are where the WebView hurts.

use super::{BridgeFailure, LocalAppsHostBroker};
use base64::Engine as _;
use device_api::{
    AudioError, AudioErrorKind, AudioInitiator, AudioOperation, AudioOperationContext,
    AudioOperationId, AudioOperationKind, AudioOperationSuccess, AudioOwner, AudioRecordingHandle,
    AudioService,
};
use device_api::{
    CalendarError, CalendarEvent, CalendarQuery, CameraError, CameraPosition, CapturePhotoOpts,
    ClipboardError, ContactsError, ContactsQuery, DeepLinkError, DeviceStatusError, HapticError,
    HapticStyle, LocationError, NotificationError, NotificationRequest, ShareError, SharePayload,
    ShareResult, VoiceRecording,
};
use local_app_contracts::approvals::CapabilityKind;
use local_apps::{load_permissions, AppCapability};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Notify};
use tokio::time::Instant;

/// Cap on the base64 body of one media response. A default-preset photo is
/// ~400-700 KB base64 and a 5-minute 16 kHz AAC mono recording ~1.6 MB, both
/// comfortably inside; past ~8 MB `evaluateJavaScript` delivery visibly
/// stalls the page, so 4 MiB is the contract.
pub(super) const MAX_DEVICE_MEDIA_RESULT_BYTES: usize = 4 * 1024 * 1024;

const PHOTO_MIN_DIMENSION: u32 = 256;
const PHOTO_MAX_DIMENSION: u32 = 2048;
const PHOTO_DEFAULT_DIMENSION: u32 = 1280;
const PHOTO_MIN_QUALITY: f32 = 0.5;
const PHOTO_MAX_QUALITY: f32 = 0.92;
const PHOTO_DEFAULT_QUALITY: f32 = 0.8;
const RECORD_MIN_DURATION_MS: u64 = 1_000;
const RECORD_MAX_DURATION_MS: u64 = 300_000;
const RECORD_DEFAULT_DURATION_MS: u64 = 120_000;
/// How long a watchdog-finished recording waits for the page to collect it.
const FINISHED_RECORDING_TTL: Duration = Duration::from_secs(60);
const LOCATION_TIMEOUT: Duration = Duration::from_secs(30);
const RECORD_SAMPLE_RATE_HZ: u32 = 16_000;
const RECORD_FORMAT: &str = "m4a";
const LOCAL_APP_AUDIO_START_TIMEOUT: Duration = Duration::from_secs(120);
const LOCAL_APP_AUDIO_TIMEOUT: Duration = Duration::from_secs(60);
static NEXT_LOCAL_APP_AUDIO_GENERATION: AtomicU64 = AtomicU64::new(1);
const NOTIFICATION_TITLE_MAX_CHARS: usize = 100;
const NOTIFICATION_BODY_MAX_CHARS: usize = 500;
const NOTIFICATION_TAG_MAX_LEN: usize = 64;
const CLIPBOARD_TEXT_MAX_CHARS: usize = 100_000;
const SHARE_TEXT_MAX_CHARS: usize = 20_000;
const SHARE_URL_MAX_CHARS: usize = 4_096;
const TTS_TEXT_MAX_CHARS: usize = 10_000;
const DEEP_LINK_MAX_CHARS: usize = 4_096;
const CALENDAR_MAX_RANGE_MS: u64 = 366 * 24 * 60 * 60 * 1_000;
const CALENDAR_MAX_LIMIT: u32 = 100;
const CONTACTS_QUERY_MAX_CHARS: usize = 200;
const CONTACTS_MAX_LIMIT: u32 = 50;

// First-use prompt reasons, one per capability (reach the sheet verbatim
// through `CapabilityRequest.reason`).
const REASON_CAMERA: &str = "应用请求使用相机拍摄一张照片。";
const REASON_PHOTO_LIBRARY: &str = "应用请求从相册选择一张图片。";
const REASON_MICROPHONE: &str = "应用请求使用麦克风录音。";
const REASON_LOCATION: &str = "应用请求获取一次当前位置。";
const REASON_NOTIFICATIONS: &str = "应用请求发送本地通知。";
const REASON_TRANSCRIBE: &str = "应用请求使用麦克风把你说的话转写成文字。";
const REASON_CLIPBOARD: &str = "应用请求读取或写入系统剪贴板。";
const REASON_SHARE: &str = "应用请求打开系统分享面板。";
const REASON_TTS: &str = "应用请求将文字转换为语音。";
const REASON_HAPTICS: &str = "应用请求触发一次短促的触觉反馈。";
const REASON_DEEP_LINK: &str = "应用请求打开一个外部链接。";
const REASON_CALENDAR: &str = "应用请求读取你指定时间范围内的日历事件。";
const REASON_CONTACTS: &str = "应用请求搜索你的联系人信息。";
const REASON_MEDIA: &str = "应用请求读取它自己刚刚获取的媒体内容。";

/// The single in-flight `device.recordAudio*` session.
pub(super) struct ActiveRecording {
    app_id: String,
    runtime_generation: u64,
    invocation: local_apps::InvocationContext,
    handle: AudioRecordingHandle,
    started: Instant,
    watchdog: tokio::task::JoinHandle<()>,
    replacing: bool,
    stopping: bool,
    finished: Option<FinishedRecording>,
    /// Pin the app-scoped service used to start this handle across profile
    /// swaps, just as the old recorder path pinned its native recorder.
    audio: Arc<dyn AudioService>,
}

#[derive(Clone)]
struct RecordingReplacement {
    app_id: String,
    runtime_generation: u64,
    invocation: local_apps::InvocationContext,
    handle: AudioRecordingHandle,
    audio: Arc<dyn AudioService>,
}

#[derive(Clone)]
struct ManualRecordingStop {
    app_id: String,
    runtime_generation: u64,
    handle: AudioRecordingHandle,
    context: AudioOperationContext,
    audio: Arc<dyn AudioService>,
    duration_ms: u64,
}

struct RecordingReplacementGuard {
    recording: Arc<Mutex<Option<ActiveRecording>>>,
    replacement: RecordingReplacement,
    stopped: bool,
    armed: bool,
}

impl RecordingReplacementGuard {
    fn new(
        recording: Arc<Mutex<Option<ActiveRecording>>>,
        replacement: RecordingReplacement,
    ) -> Self {
        Self {
            recording,
            replacement,
            stopped: false,
            armed: true,
        }
    }

    fn mark_stopped(&mut self) {
        self.stopped = true;
    }

    async fn finish(&mut self) {
        let stopped = self.stopped;
        let mut slot = self.recording.lock().await;
        finish_recording_replacement_slot(&mut slot, &self.replacement, stopped);
        self.armed = false;
    }
}

impl Drop for RecordingReplacementGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(mut slot) = self.recording.try_lock() {
            finish_recording_replacement_slot(&mut slot, &self.replacement, self.stopped);
            return;
        }

        let recording = self.recording.clone();
        let replacement = self.replacement.clone();
        let stopped = self.stopped;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let mut slot = recording.lock().await;
                finish_recording_replacement_slot(&mut slot, &replacement, stopped);
            });
        }
    }
}

fn finish_recording_replacement_slot(
    slot: &mut Option<ActiveRecording>,
    replacement: &RecordingReplacement,
    stopped: bool,
) {
    let still_registered = slot.as_ref().is_some_and(|active| {
        active.app_id == replacement.app_id
            && active.runtime_generation == replacement.runtime_generation
            && active.handle == replacement.handle
    });
    if !still_registered {
        return;
    }
    if stopped {
        if let Some(active) = slot.take() {
            active.watchdog.abort();
        }
    } else if let Some(active) = slot.as_mut() {
        active.replacing = false;
    }
}

fn manual_stop_matches(active: &ActiveRecording, stop: &ManualRecordingStop) -> bool {
    active.app_id == stop.app_id
        && active.runtime_generation == stop.runtime_generation
        && active.handle == stop.handle
}

async fn execute_manual_recording_stop(
    recording: Arc<Mutex<Option<ActiveRecording>>>,
    stop: ManualRecordingStop,
) -> Result<(), BridgeFailure> {
    let result = execute_local_audio(
        &stop.audio,
        stop.context.clone(),
        AudioOperation::StopRecording {
            handle: stop.handle.clone(),
        },
    )
    .await;
    let mut slot = recording.lock().await;
    let Some(active) = slot
        .as_mut()
        .filter(|active| manual_stop_matches(active, &stop) && active.stopping)
    else {
        return Err(BridgeFailure::coded(
            "cancelled",
            "Local App recording changed while it was being stopped",
        ));
    };

    match result {
        Ok(AudioOperationSuccess::Recording {
            recording: native_recording,
        }) => {
            let finished_at = Instant::now();
            active.finished = Some(FinishedRecording {
                recording: native_recording,
                duration_ms: stop.duration_ms,
                at: finished_at,
                auto_stopped: false,
            });
            active.stopping = false;
            active.watchdog.abort();
            active.watchdog = spawn_finished_recording_expiry(
                recording.clone(),
                stop.app_id,
                stop.runtime_generation,
                stop.handle,
                finished_at,
            );
            Ok(())
        }
        Ok(_) => {
            active.stopping = false;
            Err(BridgeFailure::coded(
                "native_failure",
                "audio service returned an invalid recording-stop result",
            ))
        }
        Err(error) => {
            active.stopping = false;
            Err(error)
        }
    }
}

async fn clear_manual_recording_stop(
    recording: &Mutex<Option<ActiveRecording>>,
    stop: &ManualRecordingStop,
) {
    let mut slot = recording.lock().await;
    if let Some(active) = slot
        .as_mut()
        .filter(|active| manual_stop_matches(active, stop))
    {
        active.stopping = false;
    }
}

fn spawn_finished_recording_expiry(
    recording: Arc<Mutex<Option<ActiveRecording>>>,
    app_id: String,
    runtime_generation: u64,
    handle: AudioRecordingHandle,
    finished_at: Instant,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tokio::time::sleep_until(finished_at + FINISHED_RECORDING_TTL).await;
        let mut slot = recording.lock().await;
        let expired = slot.as_ref().is_some_and(|active| {
            active.app_id == app_id
                && active.runtime_generation == runtime_generation
                && active.handle == handle
                && active
                    .finished
                    .as_ref()
                    .is_some_and(|finished| finished.at == finished_at)
        });
        if expired {
            // Dropping this task's own JoinHandle detaches the nearly-finished
            // task; it returns immediately after releasing the registry lock.
            drop(slot.take());
        }
    })
}

/// A native start that has not yet delivered its host-owned handle. Retained
/// separately from `ActiveRecording` so runtime teardown can cancel the exact
/// operation while the native permission prompt or device start is pending.
pub(super) struct PendingRecordingStart {
    app_id: String,
    runtime_generation: u64,
    context: AudioOperationContext,
    audio: Arc<dyn AudioService>,
    scope: Arc<LocalAppAudioScope>,
}

/// Cancellation scope for every audio request admitted by one Local App
/// runtime generation. It is intentionally host-local and generation-scoped:
/// cancelling it never tombstones the stable AudioOwner used by the service.
pub(super) struct LocalAppAudioScope {
    cancelled: AtomicBool,
    cancellation: Notify,
    state: std::sync::Mutex<()>,
}

impl LocalAppAudioScope {
    fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            cancellation: Notify::new(),
            state: std::sync::Mutex::new(()),
        }
    }

    fn cancelled() -> Self {
        let scope = Self::new();
        scope.cancel();
        scope
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    fn cancel(&self) {
        let _state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.cancelled.store(true, Ordering::SeqCst);
        self.cancellation.notify_waiters();
    }

    fn commit_if_active<T>(&self, commit: impl FnOnce() -> T) -> Option<T> {
        let _state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.is_cancelled() {
            None
        } else {
            Some(commit())
        }
    }

    async fn cancelled_signal(&self) {
        loop {
            let notified = self.cancellation.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

/// Registry plus a per-app high-water mark. Runtime generations are minted
/// from the host's monotonic request counter, so an ended generation cannot
/// be re-created by a late bridge continuation after its runtime has stopped.
#[derive(Default)]
pub(super) struct LocalAppAudioScopeRegistry {
    scopes: std::collections::HashMap<(String, u64), Arc<LocalAppAudioScope>>,
    cancelled_through: std::collections::HashMap<String, u64>,
}

struct PendingStartGuard {
    pending: Arc<Mutex<Option<PendingRecordingStart>>>,
    identity: AudioOperationId,
    handle: Option<AudioRecordingHandle>,
    armed: bool,
}

impl PendingStartGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingStartGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let pending = self.pending.clone();
        let identity = self.identity.clone();
        let handle = self.handle.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let start = {
                    let guard = pending.lock().await;
                    guard
                        .as_ref()
                        .filter(|start| start.context.identity == identity)
                        .map(|start| {
                            (
                                start.context.clone(),
                                start.audio.clone(),
                                start.app_id.clone(),
                                start.runtime_generation,
                            )
                        })
                };
                if let Some((context, audio, app_id, runtime_generation)) = start {
                    cleanup_uncommitted_recording(
                        pending,
                        context,
                        audio,
                        app_id,
                        runtime_generation,
                        handle,
                    )
                    .await;
                }
            });
        }
    }
}

async fn cleanup_uncommitted_recording(
    pending: Arc<Mutex<Option<PendingRecordingStart>>>,
    start_context: AudioOperationContext,
    audio: Arc<dyn AudioService>,
    app_id: String,
    runtime_generation: u64,
    handle: Option<AudioRecordingHandle>,
) {
    let request_id = start_context
        .initiator
        .as_ref()
        .and_then(|initiator| initiator.request_id.as_deref())
        .unwrap_or("orphaned-recording")
        .to_string();
    loop {
        if handle.is_none()
            && !pending
                .lock()
                .await
                .as_ref()
                .is_some_and(|start| start.context.identity == start_context.identity)
        {
            return;
        }
        if handle.is_none() {
            let _ = tokio::time::timeout(
                LOCAL_APP_AUDIO_TIMEOUT,
                audio.cancel(start_context.identity.clone()),
            )
            .await;
        }
        let stopped = if let Some(handle) = &handle {
            let context = local_audio_context_for_owner(
                &audio,
                &app_id,
                runtime_generation,
                &request_id,
                LOCAL_APP_AUDIO_TIMEOUT,
            );
            matches!(
                execute_local_audio(
                    &audio,
                    context,
                    AudioOperation::StopRecording {
                        handle: handle.clone(),
                    },
                )
                .await,
                Ok(AudioOperationSuccess::Recording { .. })
            )
        } else {
            false
        };
        let released = if stopped {
            true
        } else {
            let context = local_audio_context_for_owner(
                &audio,
                &app_id,
                runtime_generation,
                &request_id,
                LOCAL_APP_AUDIO_TIMEOUT,
            );
            let ended = matches!(
                execute_local_audio(&audio, context, AudioOperation::EndOwner).await,
                Ok(AudioOperationSuccess::OwnerEnded)
            );
            ended
        };
        if released {
            let mut guard = pending.lock().await;
            if guard
                .as_ref()
                .is_some_and(|start| start.context.identity == start_context.identity)
            {
                *guard = None;
            }
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// A recording the duration watchdog already stopped, parked until the page
/// collects it (or [`FINISHED_RECORDING_TTL`] expires).
struct FinishedRecording {
    recording: VoiceRecording,
    duration_ms: u64,
    at: Instant,
    auto_stopped: bool,
}

fn invalid(message: impl Into<String>) -> BridgeFailure {
    BridgeFailure::coded("invalid_request", message.into())
}

fn unavailable(what: &str) -> BridgeFailure {
    BridgeFailure::coded(
        "capability_unavailable",
        format!("{what} is not available on this device/build"),
    )
}

fn map_camera_error(error: CameraError) -> BridgeFailure {
    let message = error.to_string();
    match error {
        CameraError::PermissionDenied => BridgeFailure::coded("permission_denied", message),
        CameraError::Cancelled => BridgeFailure::coded("cancelled", message),
        CameraError::DeviceUnavailable => BridgeFailure::coded("device_unavailable", message),
        CameraError::Other(_) => BridgeFailure::from(message),
    }
}

fn map_audio_error(error: AudioError) -> BridgeFailure {
    let (code, message) = match error.kind {
        AudioErrorKind::PermissionDenied => ("permission_denied", error.message),
        AudioErrorKind::Busy => ("audio_session_busy", error.message),
        AudioErrorKind::Cancelled => ("cancelled", error.message),
        AudioErrorKind::Timeout => ("timeout", error.message),
        AudioErrorKind::NoSpeech => ("no_speech", error.message),
        AudioErrorKind::NotRecording => ("not_recording", error.message),
        AudioErrorKind::Unavailable => ("device_unavailable", error.message),
        AudioErrorKind::Unsupported => ("unsupported", error.message),
        AudioErrorKind::ModelMissing => ("model_missing", error.message),
        AudioErrorKind::VoiceMissing => ("voice_missing", error.message),
        AudioErrorKind::InvalidRequest => ("invalid_request", error.message),
        AudioErrorKind::SynthesisFailed => ("synthesis_failed", error.message),
        AudioErrorKind::NativeFailure => ("native_failure", error.message),
        AudioErrorKind::MediaTooLarge => ("media_too_large", error.message),
    };
    BridgeFailure::coded(code, message)
}

fn local_audio_context(
    audio: &Arc<dyn AudioService>,
    invocation: &local_apps::InvocationContext,
    runtime_generation: u64,
    timeout: Duration,
) -> Result<AudioOperationContext, BridgeFailure> {
    invocation
        .validate()
        .map_err(|error| invalid(format!("invalid Local App invocation context: {error}")))?;
    Ok(local_audio_context_for_owner(
        audio,
        &invocation.app_id,
        runtime_generation,
        &invocation.request_id,
        timeout,
    ))
}

fn local_audio_context_for_owner(
    audio: &Arc<dyn AudioService>,
    app_id: &str,
    runtime_generation: u64,
    request_id: &str,
    timeout: Duration,
) -> AudioOperationContext {
    let capabilities = audio.capabilities();
    let generation = NEXT_LOCAL_APP_AUDIO_GENERATION
        .fetch_add(1, Ordering::Relaxed)
        .max(1);
    let identity = AudioOperationId::new(generation, capabilities.service_epoch);
    let timeout_budget_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
    let initiator = AudioInitiator {
        agent_id: None,
        tool_use_id: None,
        request_id: Some(request_id.to_string()),
    };
    AudioOperationContext {
        identity,
        owner: AudioOwner::LocalApp {
            app_id: app_id.to_string(),
            runtime_generation,
        },
        initiator: Some(initiator),
        timeout_budget_ms: Some(timeout_budget_ms),
        max_payload_bytes: capabilities.max_payload_bytes,
    }
}

async fn execute_local_audio(
    audio: &Arc<dyn AudioService>,
    context: AudioOperationContext,
    operation: AudioOperation,
) -> Result<AudioOperationSuccess, BridgeFailure> {
    let timeout = Duration::from_millis(
        context
            .timeout_budget_ms
            .unwrap_or(LOCAL_APP_AUDIO_TIMEOUT.as_millis() as u64),
    );
    tokio::time::timeout(timeout, audio.execute(context, operation))
        .await
        .map_err(|_| BridgeFailure::coded("timeout", "Local App audio operation timed out"))?
        .map_err(map_audio_error)
}

async fn execute_local_audio_scoped(
    audio: &Arc<dyn AudioService>,
    context: AudioOperationContext,
    operation: AudioOperation,
    scope: &Arc<LocalAppAudioScope>,
) -> Result<AudioOperationSuccess, BridgeFailure> {
    if scope.is_cancelled() {
        return Err(BridgeFailure::coded(
            "cancelled",
            "Local App runtime ended before the audio operation started",
        ));
    }
    tokio::select! {
        biased;
        _ = scope.cancelled_signal() => Err(BridgeFailure::coded(
            "cancelled",
            "Local App runtime ended while the audio operation was running",
        )),
        result = execute_local_audio(audio, context, operation) => result,
    }
}

fn map_location_error(error: LocationError) -> BridgeFailure {
    let message = error.to_string();
    match error {
        LocationError::PermissionDenied => BridgeFailure::coded("permission_denied", message),
        LocationError::Unavailable => BridgeFailure::coded("device_unavailable", message),
        LocationError::Timeout => BridgeFailure::coded("timeout", message),
        LocationError::Other(_) => BridgeFailure::from(message),
    }
}

fn map_notification_error(error: NotificationError) -> BridgeFailure {
    let message = error.to_string();
    match error {
        NotificationError::PermissionDenied => BridgeFailure::coded("permission_denied", message),
        NotificationError::Other(_) => BridgeFailure::from(message),
    }
}

/// Base64-encode media, enforcing [`MAX_DEVICE_MEDIA_RESULT_BYTES`].
fn encode_media(bytes: &[u8]) -> Result<String, BridgeFailure> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    if encoded.len() > MAX_DEVICE_MEDIA_RESULT_BYTES {
        return Err(BridgeFailure::coded(
            "media_too_large",
            format!(
                "media payload is {} base64 bytes (limit {MAX_DEVICE_MEDIA_RESULT_BYTES})",
                encoded.len()
            ),
        ));
    }
    Ok(encoded)
}

/// Parse the page's photo scaling knobs, clamped to the contract ranges.
fn photo_scaling(payload: &Value) -> Result<(u32, f32), BridgeFailure> {
    let max_dimension = match payload.get("maxDimension") {
        None | Some(Value::Null) => PHOTO_DEFAULT_DIMENSION,
        Some(value) => u32::try_from(
            value
                .as_u64()
                .ok_or_else(|| invalid("maxDimension must be a positive integer"))?,
        )
        .unwrap_or(u32::MAX),
    }
    .clamp(PHOTO_MIN_DIMENSION, PHOTO_MAX_DIMENSION);
    let quality = match payload.get("quality") {
        None | Some(Value::Null) => PHOTO_DEFAULT_QUALITY,
        Some(value) => value
            .as_f64()
            .ok_or_else(|| invalid("quality must be a number"))? as f32,
    }
    .clamp(PHOTO_MIN_QUALITY, PHOTO_MAX_QUALITY);
    Ok((max_dimension, quality))
}

fn map_clipboard_error(error: ClipboardError) -> BridgeFailure {
    let message = error.to_string();
    match error {
        ClipboardError::Unsupported => BridgeFailure::coded("unsupported", message),
        ClipboardError::Other(_) => BridgeFailure::from(message),
    }
}

fn map_share_error(error: ShareError) -> BridgeFailure {
    let message = error.to_string();
    match error {
        ShareError::Unsupported => BridgeFailure::coded("unsupported", message),
        ShareError::Other(_) => BridgeFailure::from(message),
    }
}

fn map_device_status_error(error: DeviceStatusError) -> BridgeFailure {
    let message = error.to_string();
    match error {
        DeviceStatusError::Unavailable => BridgeFailure::coded("device_unavailable", message),
        DeviceStatusError::Other(_) => BridgeFailure::from(message),
    }
}

fn map_haptic_error(error: HapticError) -> BridgeFailure {
    let message = error.to_string();
    match error {
        HapticError::Unavailable => BridgeFailure::coded("device_unavailable", message),
        HapticError::Other(_) => BridgeFailure::from(message),
    }
}

fn map_deep_link_error(error: DeepLinkError) -> BridgeFailure {
    let message = error.to_string();
    match error {
        DeepLinkError::Unavailable => BridgeFailure::coded("device_unavailable", message),
        DeepLinkError::Rejected(_) => BridgeFailure::coded("rejected", message),
        DeepLinkError::Other(_) => BridgeFailure::from(message),
    }
}

fn map_calendar_error(error: CalendarError) -> BridgeFailure {
    let message = error.to_string();
    match error {
        CalendarError::Unavailable => BridgeFailure::coded("device_unavailable", message),
        CalendarError::PermissionDenied => BridgeFailure::coded("permission_denied", message),
        CalendarError::Invalid(_) => BridgeFailure::coded("invalid_request", message),
        CalendarError::Other(_) => BridgeFailure::from(message),
    }
}

fn map_contacts_error(error: ContactsError) -> BridgeFailure {
    let message = error.to_string();
    match error {
        ContactsError::Unavailable => BridgeFailure::coded("device_unavailable", message),
        ContactsError::PermissionDenied => BridgeFailure::coded("permission_denied", message),
        ContactsError::Invalid(_) => BridgeFailure::coded("invalid_request", message),
        ContactsError::Other(_) => BridgeFailure::from(message),
    }
}

fn calendar_query(payload: &Value) -> Result<CalendarQuery, BridgeFailure> {
    let start_ms = payload
        .get("startMs")
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid("startMs must be a non-negative integer"))?;
    let end_ms = payload
        .get("endMs")
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid("endMs must be a non-negative integer"))?;
    if end_ms <= start_ms || end_ms - start_ms > CALENDAR_MAX_RANGE_MS {
        return Err(invalid(format!(
            "calendar range must be 1..={CALENDAR_MAX_RANGE_MS} milliseconds"
        )));
    }
    let limit = payload
        .get("limit")
        .and_then(Value::as_u64)
        .map(|value| u32::try_from(value).unwrap_or(u32::MAX))
        .unwrap_or(50)
        .clamp(1, CALENDAR_MAX_LIMIT);
    Ok(CalendarQuery {
        start_ms,
        end_ms,
        limit,
    })
}

fn contacts_query(payload: &Value) -> Result<ContactsQuery, BridgeFailure> {
    let query = payload
        .get("query")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("query is required"))?
        .trim()
        .to_string();
    if query.is_empty() || query.chars().count() > CONTACTS_QUERY_MAX_CHARS {
        return Err(invalid(format!(
            "query must be 1..={CONTACTS_QUERY_MAX_CHARS} characters"
        )));
    }
    let limit = payload
        .get("limit")
        .and_then(Value::as_u64)
        .map(|value| u32::try_from(value).unwrap_or(u32::MAX))
        .unwrap_or(20)
        .clamp(1, CONTACTS_MAX_LIMIT);
    Ok(ContactsQuery { query, limit })
}

fn haptic_style(value: &Value) -> Result<(HapticStyle, &'static str), BridgeFailure> {
    let raw = value
        .get("style")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("style is required"))?;
    let style = match raw {
        "light" => (HapticStyle::Light, "light"),
        "medium" => (HapticStyle::Medium, "medium"),
        "heavy" => (HapticStyle::Heavy, "heavy"),
        "success" => (HapticStyle::Success, "success"),
        "warning" => (HapticStyle::Warning, "warning"),
        "error" => (HapticStyle::Error, "error"),
        _ => {
            return Err(invalid(
                "style must be light|medium|heavy|success|warning|error",
            ))
        }
    };
    Ok(style)
}

fn validated_deep_link(value: &Value) -> Result<String, BridgeFailure> {
    let raw = value
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("url is required"))?;
    if raw.is_empty() || raw.chars().count() > DEEP_LINK_MAX_CHARS {
        return Err(invalid(format!(
            "url must be 1..={DEEP_LINK_MAX_CHARS} characters"
        )));
    }
    let parsed =
        reqwest::Url::parse(raw).map_err(|error| invalid(format!("invalid URL: {error}")))?;
    if parsed.username() != "" || parsed.password().is_some() {
        return Err(invalid("deep links must not contain username or password"));
    }
    match parsed.scheme() {
        "http" | "https" => {
            if parsed.host_str().is_none() {
                return Err(invalid("http(s) deep links require a host"));
            }
        }
        "mailto" | "tel" => {}
        _ => return Err(invalid("deep link scheme is not allowed")),
    }
    Ok(parsed.to_string())
}

fn valid_notification_tag(tag: &str) -> bool {
    let bytes = tag.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= NOTIFICATION_TAG_MAX_LEN
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'-')
}

impl LocalAppsHostBroker {
    fn devices(
        &self,
    ) -> Result<crate::mobile::local_apps_device::DeviceCapabilities, BridgeFailure> {
        self.device
            .get()
            .map(|cell| cell.current())
            .ok_or_else(|| unavailable("the device capability set"))
    }

    /// Build a trusted context for an app flow step that calls a device
    /// capability. The step id is host-minted by the flow executor and is
    /// carried to AudioService as request attribution; the app/runtime owner
    /// generation still comes from the live host registry.
    pub(super) async fn flow_audio_invocation(
        &self,
        app_id: &str,
        flow_id: &str,
        request_id: &str,
        capability: local_apps::CapabilityId,
    ) -> Result<(local_apps::InvocationContext, u64), BridgeFailure> {
        let layout = self.layout(app_id).map_err(BridgeFailure::from)?;
        let permissions =
            load_permissions(&layout).map_err(|error| BridgeFailure::from(error.to_string()))?;
        let (runtime_generation, _) = self
            .runtime_identity(app_id)
            .await
            .map_err(BridgeFailure::from)?
            .ok_or_else(|| {
                BridgeFailure::coded("runtime_stopped", "Local App runtime is not running")
            })?;
        let invocation = local_apps::InvocationContext {
            app_id: app_id.to_string(),
            app_instance_id: format!("flow-{flow_id}"),
            request_id: request_id.to_string(),
            turn_id: None,
            origin: local_apps::InvocationOrigin::ConversationAgent,
            grant_epoch: permissions.grant_epoch,
            capability_instance: Some(format!("{app_id}:{}", capability.as_str())),
            call_chain: Vec::new(),
        };
        invocation.validate().map_err(|error| {
            invalid(format!(
                "invalid Local App flow invocation context: {error}"
            ))
        })?;
        Ok((invocation, runtime_generation))
    }

    /// Resolve the current host-owned app runtime generation only after the
    /// caller has built and validated its invocation context. A page cannot
    /// claim an owner generation in payload JSON.
    pub(super) async fn audio_runtime_generation(
        &self,
        invocation: &local_apps::InvocationContext,
    ) -> Result<u64, BridgeFailure> {
        invocation
            .validate()
            .map_err(|error| invalid(format!("invalid Local App invocation context: {error}")))?;
        let (generation, _) = self
            .runtime_identity(&invocation.app_id)
            .await
            .map_err(BridgeFailure::from)?
            .ok_or_else(|| {
                BridgeFailure::coded("runtime_stopped", "Local App runtime is not running")
            })?;
        Ok(generation)
    }

    fn local_app_audio_scope(
        &self,
        app_id: &str,
        runtime_generation: u64,
    ) -> Arc<LocalAppAudioScope> {
        let mut registry = self
            .audio_runtime_scopes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if registry
            .cancelled_through
            .get(app_id)
            .is_some_and(|cancelled_through| runtime_generation <= *cancelled_through)
        {
            return Arc::new(LocalAppAudioScope::cancelled());
        }
        registry
            .scopes
            .entry((app_id.to_string(), runtime_generation))
            .or_insert_with(|| Arc::new(LocalAppAudioScope::new()))
            .clone()
    }

    pub(super) fn cancel_local_app_audio_scope(&self, app_id: &str, runtime_generation: u64) {
        let mut registry = self
            .audio_runtime_scopes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry
            .cancelled_through
            .entry(app_id.to_string())
            .and_modify(|cancelled_through| {
                *cancelled_through = (*cancelled_through).max(runtime_generation)
            })
            .or_insert(runtime_generation);
        registry.scopes.retain(|(scope_app_id, generation), scope| {
            if scope_app_id == app_id && *generation <= runtime_generation {
                scope.cancel();
                false
            } else {
                true
            }
        });
    }

    async fn ensure_local_app_audio_scope_active(
        &self,
        invocation: &local_apps::InvocationContext,
        runtime_generation: u64,
        scope: &Arc<LocalAppAudioScope>,
    ) -> Result<(), BridgeFailure> {
        if scope.is_cancelled() {
            return Err(BridgeFailure::coded(
                "cancelled",
                "Local App runtime ended before the audio operation started",
            ));
        }
        if !matches!(
            self.audio_runtime_generation(invocation).await,
            Ok(generation) if generation == runtime_generation
        ) {
            self.cancel_local_app_audio_scope(&invocation.app_id, runtime_generation);
            return Err(BridgeFailure::coded(
                "cancelled",
                "Local App runtime ended before the audio operation started",
            ));
        }
        if scope.is_cancelled() {
            return Err(BridgeFailure::coded(
                "cancelled",
                "Local App runtime ended before the audio operation started",
            ));
        }
        Ok(())
    }

    /// Retain one capture and build the JSON envelope for it.
    ///
    /// The envelope carries BOTH the base64 (so the page can render it right
    /// away as a Blob URL) and a `mediaId` handle (so `llm.chat` can attach
    /// it without pushing megabytes back through the WebView request path).
    fn media_envelope(
        &self,
        app_id: &str,
        media_type: &str,
        bytes: Vec<u8>,
        extra: Value,
    ) -> Result<Value, BridgeFailure> {
        let base64_body = encode_media(&bytes)?;
        let handle = self.media.put(
            app_id,
            self.request_id("media"),
            crate::mobile::local_apps_device::MediaEntry {
                media_type: media_type.to_string(),
                bytes: std::sync::Arc::new(bytes),
            },
        );
        let mut envelope = json!({
            "mimeType": media_type,
            "base64": base64_body,
            "mediaId": handle,
        });
        if let (Some(target), Some(extra)) = (envelope.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                target.insert(key.clone(), value.clone());
            }
        }
        Ok(envelope)
    }

    /// Look up a retained capture for `llm.chat`.
    pub(super) fn media_entry(
        &self,
        app_id: &str,
        handle: &str,
    ) -> Option<crate::mobile::local_apps_device::MediaEntry> {
        self.media.get(app_id, handle)
    }

    pub(super) fn clear_media(&self, app_id: &str) {
        self.media.clear_app(app_id);
    }

    pub(super) async fn capture_photo_value(
        &self,
        app_id: &str,
        payload: &Value,
    ) -> Result<Value, BridgeFailure> {
        self.authorize_declared_capability(
            app_id,
            AppCapability::Camera,
            CapabilityKind::Camera,
            REASON_CAMERA,
        )
        .await?;
        let camera = self
            .devices()?
            .camera
            .ok_or_else(|| unavailable("the camera"))?;
        let (max_dimension, quality) = photo_scaling(payload)?;
        let position = match payload.get("camera").and_then(Value::as_str) {
            None | Some("back") => CameraPosition::Back,
            Some("front") => CameraPosition::Front,
            Some(other) => {
                return Err(invalid(format!("camera must be front|back, got {other:?}")));
            }
        };
        let allow_editing = payload
            .get("allowEditing")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let image = camera
            .capture_photo_sized(
                CapturePhotoOpts {
                    position,
                    allow_editing,
                },
                max_dimension,
                quality,
            )
            .await
            .map_err(map_camera_error)?;
        self.media_envelope(
            app_id,
            "image/jpeg",
            image.jpeg_bytes,
            json!({ "width": image.width, "height": image.height }),
        )
    }

    pub(super) async fn pick_image_value(
        &self,
        app_id: &str,
        payload: &Value,
    ) -> Result<Value, BridgeFailure> {
        self.authorize_declared_capability(
            app_id,
            AppCapability::PhotoLibrary,
            CapabilityKind::PhotoLibrary,
            REASON_PHOTO_LIBRARY,
        )
        .await?;
        let camera = self
            .devices()?
            .camera
            .ok_or_else(|| unavailable("the photo library"))?;
        let (max_dimension, quality) = photo_scaling(payload)?;
        let image = camera
            .pick_from_library_sized(max_dimension, quality)
            .await
            .map_err(map_camera_error)?;
        self.media_envelope(
            app_id,
            "image/jpeg",
            image.jpeg_bytes,
            json!({ "width": image.width, "height": image.height }),
        )
    }

    pub(super) async fn record_audio_start_value(
        &self,
        invocation: &local_apps::InvocationContext,
        runtime_generation: u64,
        payload: &Value,
    ) -> Result<Value, BridgeFailure> {
        let app_id = invocation.app_id.as_str();
        let scope = self.local_app_audio_scope(app_id, runtime_generation);
        self.ensure_local_app_audio_scope_active(invocation, runtime_generation, &scope)
            .await?;
        self.authorize_declared_capability(
            app_id,
            AppCapability::Microphone,
            CapabilityKind::Microphone,
            REASON_MICROPHONE,
        )
        .await?;
        // Authorization may have waited on a native prompt while this runtime
        // was closed or replaced. Never admit its late approval into audio.
        self.ensure_local_app_audio_scope_active(invocation, runtime_generation, &scope)
            .await?;
        let audio = self
            .devices()?
            .audio
            .ok_or_else(|| unavailable("the device audio service"))?;
        if !audio
            .capabilities()
            .supported_operations
            .contains(&device_api::audio::AudioOperationKind::Record)
        {
            return Err(BridgeFailure::coded(
                "unsupported",
                "raw recording is not supported on this device",
            ));
        }
        let max_duration_ms = match payload.get("maxDurationMs") {
            None | Some(Value::Null) => RECORD_DEFAULT_DURATION_MS,
            Some(value) => value
                .as_u64()
                .ok_or_else(|| invalid("maxDurationMs must be a positive integer"))?,
        }
        .clamp(RECORD_MIN_DURATION_MS, RECORD_MAX_DURATION_MS);

        // Serializes STARTS only, and never blocks another start on the OS
        // permission prompt. Runtime teardown uses the separately stored
        // pending operation identity to cancel this exact start.
        let _start_gate = match self.recording_start.try_lock() {
            Ok(gate) => gate,
            Err(_) => {
                return Err(BridgeFailure::coded(
                    "audio_session_busy",
                    "another recording is already starting",
                ));
            }
        };

        // Short critical section: reserve an unfinished same-app recording
        // for replacement while keeping its cleanup handle and watchdog
        // registered until the native stop succeeds.
        let orphan = {
            let mut guard = self.recording.lock().await;
            if let Some(active) = guard.as_ref() {
                // A foreign session blocks a start while it is still running
                // AND while it is parked but still collectable: the reclaim
                // below discards the orphan's bytes outright, so exempting a
                // parked recording here would let one app silently destroy
                // audio another app is still entitled to fetch — with no
                // error on either side.
                let collectable = active
                    .finished
                    .as_ref()
                    .is_some_and(|finished| finished.at.elapsed() <= FINISHED_RECORDING_TTL);
                if active.app_id != app_id && (active.finished.is_none() || collectable) {
                    return Err(BridgeFailure::coded(
                        "audio_session_busy",
                        "another app currently holds the recorder",
                    ));
                }
            }
            if guard.as_ref().is_some_and(|active| active.replacing) {
                return Err(BridgeFailure::coded(
                    "audio_session_busy",
                    "another recording replacement is already in progress",
                ));
            }
            if guard.as_ref().is_some_and(|active| active.stopping) {
                return Err(BridgeFailure::coded(
                    "audio_session_busy",
                    "the active recording is already stopping",
                ));
            }
            if guard
                .as_ref()
                .is_some_and(|active| active.finished.is_some())
            {
                if let Some(finished) = guard.take() {
                    finished.watchdog.abort();
                }
                None
            } else if let Some(active) = guard.as_mut() {
                active.replacing = true;
                Some(RecordingReplacement {
                    app_id: active.app_id.clone(),
                    runtime_generation: active.runtime_generation,
                    invocation: active.invocation.clone(),
                    handle: active.handle.clone(),
                    audio: active.audio.clone(),
                })
            } else {
                None
            }
        };

        // A same-app restart or an expired parked recording is reclaimed.
        // Release an unfinished capture through the exact pinned service,
        // owner generation, and handle that created it. Keep it addressable if
        // native stop fails so a later stop or runtime teardown can retry.
        let mut replaced_active = false;
        if let Some(orphan) = orphan {
            let mut replacement_guard =
                RecordingReplacementGuard::new(self.recording.clone(), orphan.clone());
            let context = match local_audio_context(
                &orphan.audio,
                &orphan.invocation,
                orphan.runtime_generation,
                LOCAL_APP_AUDIO_TIMEOUT,
            ) {
                Ok(context) => context,
                Err(error) => {
                    replacement_guard.finish().await;
                    return Err(error);
                }
            };
            let stop_result = execute_local_audio(
                &orphan.audio,
                context,
                AudioOperation::StopRecording {
                    handle: orphan.handle.clone(),
                },
            )
            .await;
            match stop_result {
                Ok(AudioOperationSuccess::Recording { .. }) => {
                    replacement_guard.mark_stopped();
                    replacement_guard.finish().await;
                    replaced_active = true;
                }
                Ok(_) => {
                    replacement_guard.finish().await;
                    return Err(BridgeFailure::coded(
                        "native_failure",
                        "audio service returned an invalid recording-stop result",
                    ));
                }
                Err(error) => {
                    replacement_guard.finish().await;
                    return Err(error);
                }
            }
        }

        self.ensure_local_app_audio_scope_active(invocation, runtime_generation, &scope)
            .await?;

        let start_context = local_audio_context(
            &audio,
            invocation,
            runtime_generation,
            LOCAL_APP_AUDIO_START_TIMEOUT,
        )?;
        let pending = PendingRecordingStart {
            app_id: app_id.to_string(),
            runtime_generation,
            context: start_context.clone(),
            audio: audio.clone(),
            scope: scope.clone(),
        };
        {
            let mut slot = self.recording_pending.lock().await;
            if slot.as_ref().is_some_and(|start| {
                start.context.identity.service_epoch != start_context.identity.service_epoch
            }) {
                *slot = None;
            }
            if slot.is_some() {
                return Err(BridgeFailure::coded(
                    "audio_session_busy",
                    "another recording is already starting",
                ));
            }
            *slot = Some(pending);
        }
        let mut pending_guard = PendingStartGuard {
            pending: self.recording_pending.clone(),
            identity: start_context.identity.clone(),
            handle: None,
            armed: true,
        };
        let start_result = execute_local_audio_scoped(
            &audio,
            start_context.clone(),
            AudioOperation::StartRecording {
                sample_rate_hz: RECORD_SAMPLE_RATE_HZ,
                format: RECORD_FORMAT.into(),
            },
            &scope,
        )
        .await?;
        let handle = match start_result {
            AudioOperationSuccess::RecordingStarted { handle } => handle,
            _ => {
                return Err(BridgeFailure::coded(
                    "native_failure",
                    "audio service returned an invalid recording-start result",
                ));
            }
        };
        pending_guard.handle = Some(handle.clone());

        // A permission prompt can outlive the Local App runtime that initiated
        // it. If teardown won the race, release the returned exact handle before
        // publishing it to the host recording table.
        if !matches!(
            self.audio_runtime_generation(invocation).await,
            Ok(generation) if generation == runtime_generation
        ) {
            pending_guard.disarm();
            tokio::spawn(cleanup_uncommitted_recording(
                self.recording_pending.clone(),
                start_context.clone(),
                audio.clone(),
                app_id.to_string(),
                runtime_generation,
                Some(handle),
            ));
            return Err(BridgeFailure::coded(
                "cancelled",
                "Local App runtime ended while microphone capture was starting",
            ));
        }

        let watchdog = {
            let recording_cell = self.recording.clone();
            let audio = audio.clone();
            let app = app_id.to_string();
            let invocation = invocation.clone();
            let handle = handle.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(max_duration_ms)).await;
                loop {
                    let mut guard = recording_cell.lock().await;
                    let Some(active) = guard.as_mut() else { return };
                    if active.app_id != app
                        || active.runtime_generation != runtime_generation
                        || active.handle != handle
                        || active.finished.is_some()
                    {
                        return;
                    }
                    if active.replacing || active.stopping {
                        drop(guard);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                    let duration_ms =
                        u64::try_from(active.started.elapsed().as_millis()).unwrap_or(u64::MAX);
                    let context = match local_audio_context(
                        &audio,
                        &invocation,
                        runtime_generation,
                        LOCAL_APP_AUDIO_TIMEOUT,
                    ) {
                        Ok(context) => context,
                        Err(_) => {
                            *guard = None;
                            return;
                        }
                    };
                    active.stopping = true;
                    drop(guard);
                    let result = execute_local_audio(
                        &audio,
                        context,
                        AudioOperation::StopRecording {
                            handle: handle.clone(),
                        },
                    )
                    .await;
                    let mut guard = recording_cell.lock().await;
                    let Some(active) = guard.as_mut().filter(|active| {
                        active.app_id == app
                            && active.runtime_generation == runtime_generation
                            && active.handle == handle
                            && active.stopping
                    }) else {
                        return;
                    };
                    match result {
                        Ok(AudioOperationSuccess::Recording { recording }) => {
                            let finished_at = Instant::now();
                            active.stopping = false;
                            active.finished = Some(FinishedRecording {
                                recording,
                                duration_ms,
                                at: finished_at,
                                auto_stopped: true,
                            });
                            active.watchdog = spawn_finished_recording_expiry(
                                recording_cell.clone(),
                                app.clone(),
                                runtime_generation,
                                handle.clone(),
                                finished_at,
                            );
                        }
                        other => {
                            tracing::warn!(
                                app_id = %app,
                                runtime_generation,
                                result = ?other,
                                "watchdog could not stop Local App recording"
                            );
                            drop(guard);
                            let released = match local_audio_context(
                                &audio,
                                &invocation,
                                runtime_generation,
                                LOCAL_APP_AUDIO_TIMEOUT,
                            ) {
                                Ok(context) => {
                                    matches!(
                                        execute_local_audio(
                                            &audio,
                                            context,
                                            AudioOperation::EndOwner,
                                        )
                                        .await,
                                        Ok(AudioOperationSuccess::OwnerEnded)
                                    )
                                }
                                Err(_) => false,
                            };
                            let mut guard = recording_cell.lock().await;
                            let Some(active) = guard.as_mut().filter(|active| {
                                active.app_id == app
                                    && active.runtime_generation == runtime_generation
                                    && active.handle == handle
                                    && active.stopping
                            }) else {
                                return;
                            };
                            if released {
                                *guard = None;
                            } else {
                                active.stopping = false;
                                drop(guard);
                                tokio::time::sleep(Duration::from_millis(250)).await;
                                continue;
                            }
                        }
                    }
                    return;
                }
            })
        };
        let mut watchdog = Some(watchdog);

        // Acquire both host registries before committing. The pending guard
        // remains armed through each await, so cancellation before both locks
        // are held still cancels the exact native start identity. Once both
        // are held, the handle insertion and pending-reservation removal are
        // synchronous and form one host-side commit.
        let mut recording = self.recording.lock().await;
        let mut pending = self.recording_pending.lock().await;
        let pending_matches = pending
            .as_ref()
            .is_some_and(|start| start.context.identity == start_context.identity);
        let runtime_matches = matches!(
            self.audio_runtime_generation(invocation).await,
            Ok(generation) if generation == runtime_generation
        );
        let commit_valid = pending_matches && runtime_matches && recording.is_none();
        let committed = if commit_valid {
            scope.commit_if_active(|| {
                *recording = Some(ActiveRecording {
                    app_id: app_id.to_string(),
                    runtime_generation,
                    invocation: invocation.clone(),
                    handle: handle.clone(),
                    started: Instant::now(),
                    watchdog: watchdog.take().expect("watchdog is present before commit"),
                    replacing: false,
                    stopping: false,
                    finished: None,
                    audio: audio.clone(),
                });
                *pending = None;
            })
        } else {
            None
        };
        if committed.is_none() {
            drop(pending);
            drop(recording);
            if let Some(watchdog) = watchdog.take() {
                watchdog.abort();
            }
            pending_guard.disarm();
            tokio::spawn(cleanup_uncommitted_recording(
                self.recording_pending.clone(),
                start_context.clone(),
                audio.clone(),
                app_id.to_string(),
                runtime_generation,
                Some(handle),
            ));
            return Err(BridgeFailure::coded(
                "cancelled",
                "Local App runtime ended while microphone capture was starting",
            ));
        }
        drop(pending);
        drop(recording);
        pending_guard.disarm();
        if scope.is_cancelled()
            || !matches!(
                self.audio_runtime_generation(invocation).await,
                Ok(generation) if generation == runtime_generation
            )
        {
            self.force_stop_recording(app_id, runtime_generation).await;
            return Err(BridgeFailure::coded(
                "cancelled",
                "Local App runtime ended while microphone capture was starting",
            ));
        }
        Ok(json!({
            "started": true,
            "maxDurationMs": max_duration_ms,
            "replacedActive": replaced_active,
        }))
    }

    // Stopping deliberately re-checks NO permission: the grant gated the
    // capture; refusing to RELEASE the microphone because an allow-once
    // grant was consumed would keep it hot instead.
    pub(super) async fn record_audio_stop_value(
        &self,
        invocation: &local_apps::InvocationContext,
        runtime_generation: u64,
    ) -> Result<Value, BridgeFailure> {
        let app_id = invocation.app_id.as_str();
        let (finished, stop) = {
            let mut guard = self.recording.lock().await;
            let Some(active) = guard.as_ref() else {
                return Err(BridgeFailure::coded(
                    "not_recording",
                    "no recording is active",
                ));
            };
            if active.app_id != app_id || active.runtime_generation != runtime_generation {
                return Err(BridgeFailure::coded(
                    "not_recording",
                    "another app owns the active recording",
                ));
            }
            if active.replacing {
                return Err(BridgeFailure::coded(
                    "audio_session_busy",
                    "the active recording is being replaced",
                ));
            }
            if active.stopping {
                return Err(BridgeFailure::coded(
                    "audio_session_busy",
                    "the active recording is already stopping",
                ));
            }

            if active.finished.is_some() {
                let active = guard.take().expect("checked above");
                active.watchdog.abort();
                (active.finished, None)
            } else {
                let duration_ms =
                    u64::try_from(active.started.elapsed().as_millis()).unwrap_or(u64::MAX);
                let context = local_audio_context(
                    &active.audio,
                    invocation,
                    runtime_generation,
                    LOCAL_APP_AUDIO_TIMEOUT,
                )?;
                let stop = ManualRecordingStop {
                    app_id: active.app_id.clone(),
                    runtime_generation: active.runtime_generation,
                    handle: active.handle.clone(),
                    context,
                    audio: active.audio.clone(),
                    duration_ms,
                };
                guard
                    .as_mut()
                    .expect("recording remained registered")
                    .stopping = true;
                (None, Some(stop))
            }
        };

        if let Some(finished) = finished {
            if finished.at.elapsed() > FINISHED_RECORDING_TTL {
                return Err(BridgeFailure::coded(
                    "not_recording",
                    "the recording expired uncollected",
                ));
            }
            return self.media_envelope(
                app_id,
                &finished.recording.mime_type,
                finished.recording.audio_bytes,
                json!({
                    "durationMs": finished.duration_ms,
                    "autoStopped": finished.auto_stopped,
                }),
            );
        }

        let stop = stop.expect("unfinished recording schedules a native stop");
        let worker = tokio::spawn(execute_manual_recording_stop(
            self.recording.clone(),
            stop.clone(),
        ));
        match worker.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(error),
            Err(error) => {
                clear_manual_recording_stop(&self.recording, &stop).await;
                return Err(BridgeFailure::coded(
                    "native_failure",
                    format!("recording stop task failed: {error}"),
                ));
            }
        }

        let mut guard = self.recording.lock().await;
        let active = guard
            .as_ref()
            .filter(|active| manual_stop_matches(active, &stop));
        if !active.is_some_and(|active| active.finished.is_some()) {
            return Err(BridgeFailure::coded(
                "not_recording",
                "the recording ended before its media result could be collected",
            ));
        }
        let active = guard
            .take()
            .expect("finished recording remained registered");
        active.watchdog.abort();
        let finished = active.finished.expect("checked above");
        self.media_envelope(
            app_id,
            &finished.recording.mime_type,
            finished.recording.audio_bytes,
            json!({ "durationMs": finished.duration_ms, "autoStopped": false }),
        )
    }

    /// Reclaim the recorder when `app_id`'s runtime stops (user stop, quota
    /// eviction, process exit) so the native audio-session lease never
    /// outlives the page that opened it.
    pub(super) async fn force_stop_recording(&self, app_id: &str, runtime_generation: u64) {
        self.cancel_local_app_audio_scope(app_id, runtime_generation);
        let active = {
            let guard = self.recording.lock().await;
            guard
                .as_ref()
                .filter(|active| {
                    active.app_id == app_id && active.runtime_generation == runtime_generation
                })
                .map(|active| {
                    (
                        active.audio.clone(),
                        active.invocation.clone(),
                        active.handle.clone(),
                    )
                })
        };
        let active_owned = active.is_some();
        let pending = {
            let slot = self.recording_pending.lock().await;
            slot.as_ref()
                .filter(|start| {
                    start.app_id == app_id && start.runtime_generation == runtime_generation
                })
                .map(|start| PendingRecordingStart {
                    app_id: start.app_id.clone(),
                    runtime_generation: start.runtime_generation,
                    context: start.context.clone(),
                    audio: start.audio.clone(),
                    scope: start.scope.clone(),
                })
        };

        if let Some((audio, invocation, handle)) = active {
            let context = match local_audio_context(
                &audio,
                &invocation,
                runtime_generation,
                LOCAL_APP_AUDIO_TIMEOUT,
            ) {
                Ok(context) => context,
                Err(_) => return,
            };
            if let Err(error) = execute_local_audio(&audio, context, AudioOperation::EndOwner).await
            {
                tracing::warn!(
                    app_id,
                    runtime_generation,
                    error = %error.message,
                    "could not end Local App audio owner during runtime teardown"
                );
                return;
            }
            let mut guard = self.recording.lock().await;
            if guard.as_ref().is_some_and(|active| {
                active.app_id == app_id
                    && active.runtime_generation == runtime_generation
                    && active.handle == handle
            }) {
                if let Some(active) = guard.take() {
                    active.watchdog.abort();
                }
            }
        }

        if let Some(pending) = pending {
            pending.scope.cancel();
            let identity = pending.context.identity.clone();
            let _ = pending.audio.cancel(identity.clone()).await;
            let request_id = pending
                .context
                .initiator
                .as_ref()
                .and_then(|initiator| initiator.request_id.as_deref())
                .unwrap_or("runtime-teardown");
            let context = local_audio_context_for_owner(
                &pending.audio,
                app_id,
                runtime_generation,
                request_id,
                LOCAL_APP_AUDIO_TIMEOUT,
            );
            if let Err(error) =
                execute_local_audio(&pending.audio, context, AudioOperation::EndOwner).await
            {
                tracing::warn!(
                    app_id,
                    runtime_generation,
                    error = %error.message,
                    "could not cancel pending Local App audio start during runtime teardown"
                );
            }
        } else if !active_owned {
            // Runtime teardown also cancels short owner-scoped operations even
            // when no recording handle was installed.
            if let Ok(devices) = self.devices() {
                if let Some(audio) = devices.audio {
                    let context = local_audio_context_for_owner(
                        &audio,
                        app_id,
                        runtime_generation,
                        "runtime-teardown",
                        LOCAL_APP_AUDIO_TIMEOUT,
                    );
                    let _ = execute_local_audio(&audio, context, AudioOperation::EndOwner).await;
                }
            }
        }
    }

    /// Listen once and return what was said.
    ///
    /// Live `Listen` opens the microphone for one utterance. A completed
    /// Local App recording stays a media result and is never reused as an
    /// implicit recognition fallback; an app that wants voice input requests
    /// a live transcript here and sends that text.
    ///
    /// Rides `Microphone`: it is the same hardware and the same user-visible
    /// risk, so a second capability would be a distinction without a
    /// difference.
    pub(super) async fn transcribe_speech_value(
        &self,
        invocation: &local_apps::InvocationContext,
        runtime_generation: u64,
        payload: &Value,
    ) -> Result<Value, BridgeFailure> {
        let app_id = invocation.app_id.as_str();
        let scope = self.local_app_audio_scope(app_id, runtime_generation);
        self.ensure_local_app_audio_scope_active(invocation, runtime_generation, &scope)
            .await?;
        self.authorize_declared_capability(
            app_id,
            AppCapability::Microphone,
            CapabilityKind::Microphone,
            REASON_TRANSCRIBE,
        )
        .await?;
        self.ensure_local_app_audio_scope_active(invocation, runtime_generation, &scope)
            .await?;
        let audio = self
            .devices()?
            .audio
            .ok_or_else(|| unavailable("the device audio service"))?;
        if !audio
            .capabilities()
            .supported_operations
            .contains(&AudioOperationKind::Listen)
        {
            return Err(BridgeFailure::coded(
                "unsupported",
                "live speech recognition is not supported on this device",
            ));
        }
        let language = match payload.get("language") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_str()
                    .ok_or_else(|| invalid("language must be a BCP-47 string"))?
                    .to_string(),
            ),
        };
        let context = local_audio_context(
            &audio,
            invocation,
            runtime_generation,
            LOCAL_APP_AUDIO_TIMEOUT,
        )?;
        let transcript = match execute_local_audio_scoped(
            &audio,
            context,
            AudioOperation::Listen { language },
            &scope,
        )
        .await?
        {
            AudioOperationSuccess::Transcript { transcript } => transcript,
            _ => {
                return Err(BridgeFailure::coded(
                    "native_failure",
                    "audio service returned an invalid listen result",
                ));
            }
        };
        if scope.is_cancelled()
            || !matches!(
                self.audio_runtime_generation(invocation).await,
                Ok(generation) if generation == runtime_generation
            )
        {
            return Err(BridgeFailure::coded(
                "cancelled",
                "Local App runtime ended while speech recognition was running",
            ));
        }
        Ok(json!({
            "text": transcript.text,
            "language": transcript.language,
            "confidence": transcript.confidence,
        }))
    }

    pub(super) async fn get_location_value(&self, app_id: &str) -> Result<Value, BridgeFailure> {
        self.authorize_declared_capability(
            app_id,
            AppCapability::Location,
            CapabilityKind::Location,
            REASON_LOCATION,
        )
        .await?;
        let location = self
            .devices()?
            .location
            .ok_or_else(|| unavailable("location services"))?;
        let fix = tokio::time::timeout(LOCATION_TIMEOUT, location.current_location())
            .await
            .map_err(|_| BridgeFailure::coded("timeout", "the location request timed out"))?
            .map_err(map_location_error)?;
        Ok(json!({
            "latitude": fix.latitude,
            "longitude": fix.longitude,
            "accuracyM": fix.accuracy_m,
            "timestampMs": fix.timestamp_ms,
        }))
    }

    pub(super) async fn post_notification_value(
        &self,
        app_id: &str,
        payload: &Value,
    ) -> Result<Value, BridgeFailure> {
        self.authorize_declared_capability(
            app_id,
            AppCapability::Notifications,
            CapabilityKind::Notifications,
            REASON_NOTIFICATIONS,
        )
        .await?;
        let notifications = self
            .devices()?
            .notifications
            .ok_or_else(|| unavailable("notifications"))?;
        let title = payload
            .get("title")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("title is required"))?;
        if title.trim().is_empty() || title.chars().count() > NOTIFICATION_TITLE_MAX_CHARS {
            return Err(invalid(format!(
                "title must be 1..={NOTIFICATION_TITLE_MAX_CHARS} characters"
            )));
        }
        let body = payload
            .get("body")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("body is required"))?;
        if body.trim().is_empty() || body.chars().count() > NOTIFICATION_BODY_MAX_CHARS {
            return Err(invalid(format!(
                "body must be 1..={NOTIFICATION_BODY_MAX_CHARS} characters"
            )));
        }
        let page_tag = match payload.get("tag") {
            None | Some(Value::Null) => None,
            Some(value) => {
                let tag = value
                    .as_str()
                    .ok_or_else(|| invalid("tag must be a string"))?;
                if !valid_notification_tag(tag) {
                    return Err(invalid("tag must match ^[a-z0-9][a-z0-9_-]{0,63}$"));
                }
                Some(tag.to_string())
            }
        };
        // The app-scoped prefix is applied HERE, before the native layer, so
        // no app can address (and replace) another app's — or the
        // assistant's — notifications.
        // The page's own tag is what comes back, NOT the composed identifier.
        // The composed form contains `.`, which this very function's grammar
        // rejects — echoing it would hand the page a value it cannot pass
        // back, breaking exactly the replace/dedupe round-trip a `tag` is for.
        // (An app that supplied no tag gets its minted one back and can
        // replace with it, because the same prefix is re-derived here.)
        let echoed_tag = page_tag.unwrap_or_else(|| self.request_id("n"));
        notifications
            .notify(NotificationRequest {
                title: title.to_string(),
                body: body.to_string(),
                tag: Some(format!("local-app.{app_id}.{echoed_tag}")),
            })
            .await
            .map_err(map_notification_error)?;
        Ok(json!({ "posted": true, "tag": echoed_tag }))
    }

    pub(super) async fn clipboard_get_text_value(
        &self,
        app_id: &str,
    ) -> Result<Value, BridgeFailure> {
        self.authorize_declared_capability(
            app_id,
            AppCapability::Clipboard,
            CapabilityKind::Clipboard,
            REASON_CLIPBOARD,
        )
        .await?;
        let clipboard = self
            .devices()?
            .clipboard
            .ok_or_else(|| unavailable("the clipboard"))?;
        Ok(json!({
            "text": clipboard.get_text().await.map_err(map_clipboard_error)?,
        }))
    }

    pub(super) async fn clipboard_set_text_value(
        &self,
        app_id: &str,
        payload: &Value,
    ) -> Result<Value, BridgeFailure> {
        self.authorize_declared_capability(
            app_id,
            AppCapability::Clipboard,
            CapabilityKind::Clipboard,
            REASON_CLIPBOARD,
        )
        .await?;
        let text = payload
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("text is required"))?;
        if text.chars().count() > CLIPBOARD_TEXT_MAX_CHARS {
            return Err(invalid(format!(
                "text must be at most {CLIPBOARD_TEXT_MAX_CHARS} characters"
            )));
        }
        let clipboard = self
            .devices()?
            .clipboard
            .ok_or_else(|| unavailable("the clipboard"))?;
        clipboard
            .set_text(text.to_string())
            .await
            .map_err(map_clipboard_error)?;
        Ok(json!({ "written": true }))
    }

    pub(super) async fn share_value(
        &self,
        app_id: &str,
        payload: &Value,
    ) -> Result<Value, BridgeFailure> {
        self.authorize_declared_capability(
            app_id,
            AppCapability::Share,
            CapabilityKind::Share,
            REASON_SHARE,
        )
        .await?;
        let text = match payload.get("text") {
            None | Some(Value::Null) => None,
            Some(value) => {
                let text = value
                    .as_str()
                    .ok_or_else(|| invalid("text must be a string"))?;
                if text.chars().count() > SHARE_TEXT_MAX_CHARS {
                    return Err(invalid(format!(
                        "text must be at most {SHARE_TEXT_MAX_CHARS} characters"
                    )));
                }
                Some(text.to_string())
            }
        };
        let url = match payload.get("url") {
            None | Some(Value::Null) => None,
            Some(value) => {
                let url = value
                    .as_str()
                    .ok_or_else(|| invalid("url must be a string"))?;
                if url.chars().count() > SHARE_URL_MAX_CHARS {
                    return Err(invalid(format!(
                        "url must be at most {SHARE_URL_MAX_CHARS} characters"
                    )));
                }
                Some(url.to_string())
            }
        };
        let image_bytes = match payload.get("mediaId") {
            None | Some(Value::Null) => None,
            Some(value) => {
                let media_id = value
                    .as_str()
                    .ok_or_else(|| invalid("mediaId must be a string"))?;
                let entry = self.media_entry(app_id, media_id).ok_or_else(|| {
                    BridgeFailure::coded(
                        "media_not_found",
                        format!("mediaId {media_id:?} is unknown or has expired"),
                    )
                })?;
                if !entry.media_type.starts_with("image/") {
                    return Err(invalid("mediaId must refer to an image"));
                }
                Some((*entry.bytes).clone())
            }
        };
        if text.as_deref().is_none_or(str::is_empty)
            && url.as_deref().is_none_or(str::is_empty)
            && image_bytes.is_none()
        {
            return Err(invalid("share requires text, url, or image mediaId"));
        }
        let share = self
            .devices()?
            .share
            .ok_or_else(|| unavailable("the system share sheet"))?;
        let result = share
            .share(SharePayload {
                text,
                image_bytes,
                url,
            })
            .await
            .map_err(map_share_error)?;
        Ok(match result {
            ShareResult::Success => json!({ "shared": true, "cancelled": false }),
            ShareResult::Cancelled => json!({ "shared": false, "cancelled": true }),
        })
    }

    pub(super) async fn synthesize_speech_value(
        &self,
        invocation: &local_apps::InvocationContext,
        runtime_generation: u64,
        payload: &Value,
    ) -> Result<Value, BridgeFailure> {
        let app_id = invocation.app_id.as_str();
        let scope = self.local_app_audio_scope(app_id, runtime_generation);
        self.ensure_local_app_audio_scope_active(invocation, runtime_generation, &scope)
            .await?;
        self.authorize_declared_capability(
            app_id,
            AppCapability::TextToSpeech,
            CapabilityKind::TextToSpeech,
            REASON_TTS,
        )
        .await?;
        self.ensure_local_app_audio_scope_active(invocation, runtime_generation, &scope)
            .await?;
        let text = payload
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("text is required"))?;
        if text.trim().is_empty() || text.chars().count() > TTS_TEXT_MAX_CHARS {
            return Err(invalid(format!(
                "text must be 1..={TTS_TEXT_MAX_CHARS} characters"
            )));
        }
        let voice = match payload.get("voice") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_str()
                    .ok_or_else(|| invalid("voice must be a string"))?
                    .to_string(),
            ),
        };
        let language = match payload.get("language") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_str()
                    .ok_or_else(|| invalid("language must be a BCP-47 string"))?
                    .to_string(),
            ),
        };
        let rate = match payload.get("rate") {
            None | Some(Value::Null) => None,
            Some(value) => {
                let rate = value
                    .as_f64()
                    .ok_or_else(|| invalid("rate must be a number between 0.5 and 2"))?;
                if !rate.is_finite() || !(0.5..=2.0).contains(&rate) {
                    return Err(invalid("rate must be a number between 0.5 and 2"));
                }
                Some(rate as f32)
            }
        };
        let audio = self
            .devices()?
            .audio
            .ok_or_else(|| unavailable("the device audio service"))?;
        if !audio
            .capabilities()
            .supported_operations
            .contains(&AudioOperationKind::Synthesize)
        {
            return Err(BridgeFailure::coded(
                "unsupported",
                "speech synthesis is not supported on this device",
            ));
        }
        let context = local_audio_context(
            &audio,
            invocation,
            runtime_generation,
            LOCAL_APP_AUDIO_TIMEOUT,
        )?;
        let audio = match execute_local_audio_scoped(
            &audio,
            context,
            AudioOperation::Synthesize {
                text: text.to_string(),
                language,
                rate,
                voice,
            },
            &scope,
        )
        .await?
        {
            AudioOperationSuccess::Synthesized { audio } => audio,
            _ => {
                return Err(BridgeFailure::coded(
                    "native_failure",
                    "audio service returned an invalid synthesis result",
                ));
            }
        };
        if audio.pcm.is_empty() || audio.pcm.len() % 2 != 0 || audio.sample_rate_hz == 0 {
            return Err(BridgeFailure::coded(
                "native_failure",
                "audio service returned invalid PCM16 mono audio",
            ));
        }
        if scope.is_cancelled()
            || !matches!(
                self.audio_runtime_generation(invocation).await,
                Ok(generation) if generation == runtime_generation
            )
        {
            return Err(BridgeFailure::coded(
                "cancelled",
                "Local App runtime ended while speech synthesis was running",
            ));
        }
        self.media_envelope(
            app_id,
            "audio/pcm",
            audio.pcm,
            json!({ "sampleRateHz": audio.sample_rate_hz }),
        )
    }

    pub(super) async fn device_status_value(&self, app_id: &str) -> Result<Value, BridgeFailure> {
        self.ensure_declared_capability(app_id, AppCapability::DeviceStatus)?;
        let provider = self
            .devices()?
            .device_status
            .ok_or_else(|| unavailable("device status"))?;
        serde_json::to_value(provider.status().await.map_err(map_device_status_error)?)
            .map_err(|error| BridgeFailure::from(format!("serialize device status: {error}")))
    }

    pub(super) async fn haptics_value(
        &self,
        app_id: &str,
        payload: &Value,
    ) -> Result<Value, BridgeFailure> {
        self.authorize_declared_capability(
            app_id,
            AppCapability::Haptics,
            CapabilityKind::Haptics,
            REASON_HAPTICS,
        )
        .await?;
        let (style, wire_style) = haptic_style(payload)?;
        let haptics = self
            .devices()?
            .haptics
            .ok_or_else(|| unavailable("haptics"))?;
        haptics.trigger(style).await.map_err(map_haptic_error)?;
        Ok(json!({ "triggered": true, "style": wire_style }))
    }

    pub(super) async fn deep_link_value(
        &self,
        app_id: &str,
        payload: &Value,
    ) -> Result<Value, BridgeFailure> {
        self.authorize_declared_capability(
            app_id,
            AppCapability::DeepLink,
            CapabilityKind::DeepLink,
            REASON_DEEP_LINK,
        )
        .await?;
        let url = validated_deep_link(payload)?;
        let opener = self
            .devices()?
            .deep_link
            .ok_or_else(|| unavailable("deep links"))?;
        opener
            .open(url.clone())
            .await
            .map_err(map_deep_link_error)?;
        Ok(json!({ "opened": true, "url": url }))
    }

    pub(super) async fn calendar_list_events_value(
        &self,
        app_id: &str,
        payload: &Value,
    ) -> Result<Value, BridgeFailure> {
        self.authorize_declared_capability(
            app_id,
            AppCapability::Calendar,
            CapabilityKind::Calendar,
            REASON_CALENDAR,
        )
        .await?;
        let query = calendar_query(payload)?;
        let calendar = self
            .devices()?
            .calendar
            .ok_or_else(|| unavailable("calendar"))?;
        let events: Vec<CalendarEvent> = calendar
            .list_events(query)
            .await
            .map_err(map_calendar_error)?;
        serde_json::to_value(events)
            .map_err(|error| BridgeFailure::from(format!("serialize calendar events: {error}")))
    }

    pub(super) async fn contacts_search_value(
        &self,
        app_id: &str,
        payload: &Value,
    ) -> Result<Value, BridgeFailure> {
        self.authorize_declared_capability(
            app_id,
            AppCapability::Contacts,
            CapabilityKind::Contacts,
            REASON_CONTACTS,
        )
        .await?;
        let query = contacts_query(payload)?;
        let contacts = self
            .devices()?
            .contacts
            .ok_or_else(|| unavailable("contacts"))?
            .search(query)
            .await
            .map_err(map_contacts_error)?;
        serde_json::to_value(contacts)
            .map_err(|error| BridgeFailure::from(format!("serialize contacts: {error}")))
    }

    pub(super) async fn media_value(
        &self,
        app_id: &str,
        payload: &Value,
    ) -> Result<Value, BridgeFailure> {
        self.authorize_declared_capability(
            app_id,
            AppCapability::Media,
            CapabilityKind::Media,
            REASON_MEDIA,
        )
        .await?;
        let handle = payload
            .get("mediaId")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("mediaId is required"))?;
        if handle.is_empty() || handle.len() > 128 {
            return Err(invalid("mediaId must be 1..=128 bytes"));
        }
        let entry = self
            .media_entry(app_id, handle)
            .ok_or_else(|| BridgeFailure::coded("not_found", "mediaId is unknown or expired"))?;
        let base64 = encode_media(&entry.bytes)?;
        Ok(json!({
            "mimeType": entry.media_type,
            "base64": base64,
            "mediaId": handle,
            "bytes": entry.bytes.len(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        execute_manual_recording_stop, local_audio_context, local_audio_context_for_owner,
        LocalAppAudioScope, ManualRecordingStop, PendingRecordingStart, PendingStartGuard,
        RecordingReplacement, RecordingReplacementGuard, FINISHED_RECORDING_TTL,
        LOCAL_APP_AUDIO_TIMEOUT, RECORD_FORMAT, RECORD_SAMPLE_RATE_HZ,
    };
    use crate::mobile::local_apps_device::{DeviceCapabilities, SharedDeviceCapabilities};
    use crate::mobile::local_apps_host::LocalAppsHostBroker;
    use async_trait::async_trait;
    use base64::Engine as _;
    use client::adapter::{ClientEventSink, MockSink};
    use client::protocol::events::ClientEvent;
    use client::protocol::local_apps::AppEventDto;
    use device_api::{
        AudioCapabilitySnapshot, AudioError, AudioErrorKind, AudioOperation, AudioOperationContext,
        AudioOperationId, AudioOperationKind, AudioOperationReadiness, AudioOperationSuccess,
        AudioOwner, AudioReadinessState, AudioRecordingHandle, AudioService, AudioStatus,
    };
    use device_api::{
        CalendarError, CalendarEvent, CalendarProvider, CalendarQuery, CameraControl, CameraError,
        CapturePhotoOpts, CapturedImage, Clipboard, ClipboardError, Contact, ContactsError,
        ContactsProvider, ContactsQuery, LocationError, LocationFix, LocationProvider,
        NotificationError, NotificationRequest, NotificationService, ShareError, SharePayload,
        ShareResult, SharingService, SttTranscript, TtsAudio, VoiceRecording,
    };
    use local_app_contracts::approvals::AuthorizationDecision;
    use local_app_contracts::bridge::{BridgeOperation, BridgeRequest};
    use local_apps::test_support::FixedClock;
    use local_apps::{
        load_manifest, load_permissions, save_manifest, save_permissions, AppCapability,
        AppDependencyRecord, AppDependencyState, AppLayout, AppRuntimeProfile, AppService,
        AppSurface, NoopAppEventObserver, APPS_SCHEMA_VERSION,
    };
    use serde_json::{json, Value};
    use sha2::{Digest, Sha256};
    use std::collections::{HashMap, HashSet};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::sync::Mutex;
    use tokio::time::timeout;

    // ---- fakes ------------------------------------------------------------

    #[derive(Default)]
    struct FakeCamera {
        /// (max_dimension, jpeg_quality) the bridge handed us, per call.
        sized_calls: StdMutex<Vec<(u32, f32)>>,
        /// Bytes the next capture/pick returns.
        bytes: StdMutex<Vec<u8>>,
        error: StdMutex<Option<CameraError>>,
    }

    impl FakeCamera {
        fn with_bytes(bytes: Vec<u8>) -> Arc<Self> {
            let fake = Self::default();
            *fake.bytes.lock().unwrap() = bytes;
            Arc::new(fake)
        }

        fn failing(error: CameraError) -> Arc<Self> {
            let fake = Self::default();
            *fake.error.lock().unwrap() = Some(error);
            Arc::new(fake)
        }

        fn image(&self) -> Result<CapturedImage, CameraError> {
            if let Some(error) = self.error.lock().unwrap().clone() {
                return Err(error);
            }
            Ok(CapturedImage {
                jpeg_bytes: self.bytes.lock().unwrap().clone(),
                width: 640,
                height: 480,
            })
        }
    }

    #[async_trait]
    impl CameraControl for FakeCamera {
        async fn capture_photo(
            &self,
            _opts: CapturePhotoOpts,
        ) -> Result<CapturedImage, CameraError> {
            self.image()
        }

        async fn pick_from_library(&self) -> Result<CapturedImage, CameraError> {
            self.image()
        }

        async fn capture_photo_sized(
            &self,
            _opts: CapturePhotoOpts,
            max_dimension: u32,
            jpeg_quality: f32,
        ) -> Result<CapturedImage, CameraError> {
            self.sized_calls
                .lock()
                .unwrap()
                .push((max_dimension, jpeg_quality));
            self.image()
        }

        async fn pick_from_library_sized(
            &self,
            max_dimension: u32,
            jpeg_quality: f32,
        ) -> Result<CapturedImage, CameraError> {
            self.sized_calls
                .lock()
                .unwrap()
                .push((max_dimension, jpeg_quality));
            self.image()
        }
    }

    #[derive(Default)]
    struct FakeClipboard {
        text: StdMutex<Option<String>>,
    }

    #[async_trait]
    impl Clipboard for FakeClipboard {
        async fn set_text(&self, text: String) -> Result<(), ClipboardError> {
            *self.text.lock().unwrap() = Some(text);
            Ok(())
        }

        async fn get_text(&self) -> Result<Option<String>, ClipboardError> {
            Ok(self.text.lock().unwrap().clone())
        }
    }

    #[derive(Default)]
    struct FakeShare {
        payloads: StdMutex<Vec<SharePayload>>,
    }

    #[async_trait]
    impl SharingService for FakeShare {
        async fn share(&self, payload: SharePayload) -> Result<ShareResult, ShareError> {
            self.payloads.lock().unwrap().push(payload);
            Ok(ShareResult::Success)
        }
    }

    #[derive(Default)]
    struct FakeAudio {
        active: StdMutex<HashMap<AudioOwner, AudioRecordingHandle>>,
        starts: StdMutex<HashMap<AudioOperationId, AudioOwner>>,
        cancelled: StdMutex<HashSet<AudioOperationId>>,
        cancel_wakeup: tokio::sync::Notify,
        operations: StdMutex<Vec<(AudioOperationContext, AudioOperation)>>,
        stopped: AtomicBool,
        fail_next_stop: StdMutex<Option<AudioErrorKind>>,
        start_gate: Option<Arc<tokio::sync::Notify>>,
        start_entered: Option<Arc<tokio::sync::Notify>>,
        stop_gate: Option<Arc<tokio::sync::Notify>>,
        stop_entered: Option<Arc<tokio::sync::Notify>>,
        stop_succeeded: Option<Arc<tokio::sync::Notify>>,
        cancel_gate: Option<Arc<tokio::sync::Notify>>,
        cancel_entered: Option<Arc<tokio::sync::Notify>>,
        synthesize_gate: Option<Arc<tokio::sync::Notify>>,
        synthesize_entered: Option<Arc<tokio::sync::Notify>>,
    }

    #[async_trait]
    impl AudioService for FakeAudio {
        fn capabilities(&self) -> AudioCapabilitySnapshot {
            let supported_operations = vec![
                AudioOperationKind::Record,
                AudioOperationKind::Listen,
                AudioOperationKind::Synthesize,
            ];
            AudioCapabilitySnapshot {
                service_epoch: 7,
                support_revision: 1,
                readiness: supported_operations
                    .iter()
                    .copied()
                    .map(|operation| AudioOperationReadiness {
                        operation,
                        state: AudioReadinessState::Ready,
                    })
                    .collect(),
                supported_operations,
                max_payload_bytes: 4 * 1024 * 1024,
            }
        }

        async fn execute(
            &self,
            context: AudioOperationContext,
            operation: AudioOperation,
        ) -> Result<AudioOperationSuccess, AudioError> {
            self.operations
                .lock()
                .unwrap()
                .push((context.clone(), operation.clone()));
            match operation {
                AudioOperation::StartRecording { .. } => {
                    let handle = AudioRecordingHandle(context.identity.id.clone());
                    self.active
                        .lock()
                        .unwrap()
                        .insert(context.owner.clone(), handle.clone());
                    self.starts
                        .lock()
                        .unwrap()
                        .insert(context.identity.clone(), context.owner.clone());
                    if let Some(entered) = &self.start_entered {
                        entered.notify_one();
                    }
                    if let Some(gate) = &self.start_gate {
                        tokio::select! {
                            _ = gate.notified() => {},
                            _ = self.cancel_wakeup.notified() => {},
                        }
                        if self.cancelled.lock().unwrap().contains(&context.identity) {
                            return Err(AudioError::new(
                                AudioErrorKind::Cancelled,
                                "start cancelled",
                            ));
                        }
                    }
                    Ok(AudioOperationSuccess::RecordingStarted { handle })
                }
                AudioOperation::StopRecording { handle } => {
                    if let Some(entered) = &self.stop_entered {
                        entered.notify_one();
                    }
                    if let Some(gate) = &self.stop_gate {
                        gate.notified().await;
                    }
                    if let Some(kind) = self.fail_next_stop.lock().unwrap().take() {
                        return Err(AudioError::new(kind, "injected recording stop failure"));
                    }
                    let mut active = self.active.lock().unwrap();
                    if active.get(&context.owner) != Some(&handle) {
                        return Err(AudioError::new(
                            AudioErrorKind::NotRecording,
                            "unknown recording handle",
                        ));
                    }
                    active.remove(&context.owner);
                    self.stopped.store(true, Ordering::SeqCst);
                    if let Some(succeeded) = &self.stop_succeeded {
                        succeeded.notify_one();
                    }
                    Ok(AudioOperationSuccess::Recording {
                        recording: VoiceRecording {
                            audio_bytes: vec![7, 7, 7],
                            mime_type: "audio/m4a".into(),
                        },
                    })
                }
                AudioOperation::Listen { .. } => Ok(AudioOperationSuccess::Transcript {
                    transcript: SttTranscript {
                        text: "明天下午三点开会".into(),
                        language: Some("zh-CN".into()),
                        confidence: Some(0.9),
                    },
                }),
                AudioOperation::Synthesize { .. } => {
                    if let Some(entered) = &self.synthesize_entered {
                        entered.notify_one();
                    }
                    if let Some(gate) = &self.synthesize_gate {
                        gate.notified().await;
                    }
                    Ok(AudioOperationSuccess::Synthesized {
                        audio: TtsAudio {
                            pcm: vec![0, 0, 1, 0],
                            sample_rate_hz: 24_000,
                        },
                    })
                }
                AudioOperation::Status { handle } => {
                    let active = self.active.lock().unwrap();
                    let recording = handle
                        .as_ref()
                        .is_some_and(|handle| active.get(&context.owner) == Some(handle));
                    Ok(AudioOperationSuccess::Status {
                        status: AudioStatus {
                            recording,
                            playing: false,
                        },
                    })
                }
                AudioOperation::EndOwner => {
                    self.active.lock().unwrap().remove(&context.owner);
                    self.stopped.store(true, Ordering::SeqCst);
                    Ok(AudioOperationSuccess::OwnerEnded)
                }
                AudioOperation::Speak { .. } => {
                    Ok(AudioOperationSuccess::PlaybackCompleted { duration_ms: 50 })
                }
            }
        }

        async fn cancel(&self, identity: AudioOperationId) -> Result<(), AudioError> {
            if let Some(entered) = &self.cancel_entered {
                entered.notify_one();
            }
            if let Some(gate) = &self.cancel_gate {
                gate.notified().await;
            }
            self.cancelled.lock().unwrap().insert(identity.clone());
            if let Some(owner) = self.starts.lock().unwrap().remove(&identity) {
                self.active.lock().unwrap().remove(&owner);
            }
            self.cancel_wakeup.notify_one();
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeCalendar {
        queries: StdMutex<Vec<CalendarQuery>>,
        events: Vec<CalendarEvent>,
    }

    #[async_trait]
    impl CalendarProvider for FakeCalendar {
        async fn list_events(
            &self,
            query: CalendarQuery,
        ) -> Result<Vec<CalendarEvent>, CalendarError> {
            self.queries.lock().unwrap().push(query);
            Ok(self.events.clone())
        }
    }

    #[derive(Default)]
    struct FakeContacts {
        queries: StdMutex<Vec<ContactsQuery>>,
        contacts: Vec<Contact>,
    }

    #[async_trait]
    impl ContactsProvider for FakeContacts {
        async fn search(&self, query: ContactsQuery) -> Result<Vec<Contact>, ContactsError> {
            self.queries.lock().unwrap().push(query);
            Ok(self.contacts.clone())
        }
    }

    struct FakeLocation {
        hang: bool,
        error: Option<LocationError>,
    }

    #[async_trait]
    impl LocationProvider for FakeLocation {
        async fn current_location(&self) -> Result<LocationFix, LocationError> {
            if self.hang {
                std::future::pending::<()>().await;
            }
            if let Some(error) = self.error.clone() {
                return Err(error);
            }
            Ok(LocationFix {
                latitude: 31.2304,
                longitude: 121.4737,
                accuracy_m: Some(65.0),
                timestamp_ms: 1_753_000_000_000,
            })
        }
    }

    #[derive(Default)]
    struct FakeNotifications {
        requests: StdMutex<Vec<NotificationRequest>>,
    }

    #[async_trait]
    impl NotificationService for FakeNotifications {
        async fn notify(&self, req: NotificationRequest) -> Result<(), NotificationError> {
            self.requests.lock().unwrap().push(req);
            Ok(())
        }
    }

    // ---- harness ----------------------------------------------------------

    struct Harness {
        _root: TempDir,
        broker: Arc<LocalAppsHostBroker>,
        sink: Arc<MockSink>,
        service: Arc<AppService>,
        app_id: String,
        layout: AppLayout,
    }

    async fn harness(devices: DeviceCapabilities) -> Harness {
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
        let broker = crate::mobile::local_apps_wire::broker_with_client_sink(
            root.path().to_path_buf(),
            sink.clone() as Arc<dyn ClientEventSink>,
            None,
            false,
            None,
        );
        assert!(broker.attach_service(service.clone()).is_ok());
        assert!(broker
            .attach_device(Arc::new(SharedDeviceCapabilities::new(devices)))
            .is_ok());
        let record = service
            .create_app(Some("Device"), "a device test app", None)
            .await
            .expect("create app");
        let layout = AppLayout::new(root.path().to_path_buf(), record.id.clone()).expect("layout");
        let static_dist = root
            .path()
            .join(layout.build_rel(false))
            .join(crate::mobile::local_apps_build::VITE_OUTPUT_DIR);
        std::fs::create_dir_all(&static_dist).expect("static dist");
        std::fs::write(static_dist.join("index.html"), "<html>ok</html>").expect("index");
        let record = prepare_launchable_runtime_fixture(&service, record, &layout).await;
        Harness {
            _root: root,
            broker,
            sink,
            service,
            app_id: record.id,
            layout,
        }
    }

    async fn prepare_launchable_runtime_fixture(
        service: &Arc<AppService>,
        record: local_apps::AppRecord,
        layout: &AppLayout,
    ) -> local_apps::AppRecord {
        let binding = crate::mobile::local_app_runtime_profiles::current_binding_for_family(
            AppRuntimeProfile::ReactDom,
        )
        .expect("react dom binding");
        let workspace = layout.root().join(layout.workspace_rel());
        crate::mobile::local_apps_build::scaffold_workspace_initialized(
            layout,
            crate::mobile::local_apps_build::LocalAppBuildTarget::ReactDomR4,
            true,
        )
        .expect("scaffold workspace");
        let scaffold =
            crate::mobile::local_app_runtime_profiles::scaffold_artifacts_for_binding(&binding)
                .expect("runtime profile scaffold");
        for (relative, bytes) in &scaffold.files {
            let path = workspace.join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create scaffold parent");
            }
            std::fs::write(path, bytes).expect("write scaffold file");
        }

        let requested_bytes = std::fs::read(
            workspace.join(crate::mobile::local_app_runtime_profiles::REQUESTED_FILE_REL),
        )
        .expect("read requested dependencies");
        let package_bytes = std::fs::read(
            workspace.join(crate::mobile::local_app_runtime_profiles::EFFECTIVE_PACKAGE_FILE_REL),
        )
        .expect("read effective package");
        let lockfile_bytes = std::fs::read(
            workspace.join(crate::mobile::local_app_runtime_profiles::LOCKFILE_FILE_REL),
        )
        .expect("read lockfile");
        let sbom_bytes = br#"{
  "spdxVersion": "SPDX-2.3",
  "SPDXID": "SPDXRef-DOCUMENT",
  "name": "device-op-fixture",
  "dataLicense": "CC0-1.0",
  "documentNamespace": "https://example.invalid/spdx/device-op-fixture"
}
"#;
        let snapshot = crate::mobile::local_app_runtime_profiles::snapshot_artifacts_for_binding(
            &binding,
            crate::mobile::local_app_runtime_profiles::hash_bytes(&requested_bytes),
            crate::mobile::local_app_runtime_profiles::hash_bytes(&package_bytes),
            crate::mobile::local_app_runtime_profiles::hash_bytes(&lockfile_bytes),
            crate::mobile::local_app_runtime_profiles::hash_bytes(b"device-op-fixture-tree"),
            sbom_bytes,
        )
        .expect("dependency snapshot");
        for (relative, bytes) in &snapshot.files {
            let path = workspace.join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create snapshot parent");
            }
            std::fs::write(path, bytes).expect("write snapshot file");
        }

        let mut manifest = load_manifest(layout).expect("manifest");
        manifest.surface = Some(AppSurface::Dom);
        manifest.template_origin = Some(local_apps::AppTemplateOrigin {
            plugin_id: local_apps::AppTemplateOrigin::BUILTIN_PLUGIN_ID.into(),
            plugin_version: "builtin".into(),
            template_id: format!(
                "{}-r{}",
                binding.family.as_str().replace('_', "-"),
                binding.revision
            ),
            template_sha256: binding.contract_sha256.clone(),
        });
        manifest.runtime_profile = Some(binding);
        manifest.dependency_snapshot = Some(snapshot.snapshot);
        save_manifest(layout, &manifest).expect("save runtime manifest");
        local_apps::storage::save_dependency_record(
            layout.root(),
            &AppDependencyRecord {
                schema_version: APPS_SCHEMA_VERSION,
                app_id: record.id.clone(),
                state: AppDependencyState::Ready,
                lockfile_sha256: manifest
                    .dependency_snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.lockfile_sha256.clone()),
                toolchain_key: manifest
                    .dependency_snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.toolchain_key.clone()),
                install_attempts: 1,
                last_error: None,
                updated_at_ms: record.updated_at_ms,
            },
        )
        .expect("save dependency record");

        let build_root = layout.root().join(layout.build_rel(false));
        let output_root = build_root.join(crate::mobile::local_apps_build::VITE_OUTPUT_DIR);
        std::fs::create_dir_all(&output_root).expect("create fixture output");
        std::fs::write(output_root.join("index.html"), "<html>ok</html>")
            .expect("write fixture output");
        let output_sha256 = digest_tree(&output_root);
        let build_receipt = json!({
            "version": 3,
            "buildId": output_sha256,
            "buildKey": "device-op-fixture",
            "runtimeContractSha256": manifest.runtime_contract_hash().expect("runtime hash"),
            "dependencySnapshotSha256": manifest
                .dependency_snapshot_hash()
                .expect("dependency hash"),
            "outputSha256": output_sha256,
        });
        std::fs::write(
            build_root.join("build.json"),
            serde_json::to_vec_pretty(&build_receipt).expect("serialize build receipt"),
        )
        .expect("write build receipt");
        service
            .commit_scaffold(&record.id, &record.name, &record.brief, None, None)
            .await
            .expect("commit formed fixture")
    }

    fn digest_tree(root: &std::path::Path) -> String {
        let mut files = Vec::new();
        collect_tree_files(root, &mut files);
        files.sort();
        let mut hasher = Sha256::new();
        for path in files {
            let relative = path.strip_prefix(root).expect("relative output path");
            hasher.update(relative.to_string_lossy().replace('\\', "/").as_bytes());
            hasher.update([0]);
            hasher.update(std::fs::read(&path).expect("read output file"));
            hasher.update([0]);
        }
        format!("{:x}", hasher.finalize())
    }

    fn collect_tree_files(current: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
        let metadata = std::fs::symlink_metadata(current).expect("inspect output path");
        assert!(
            !metadata.file_type().is_symlink(),
            "build output must not contain symlinks"
        );
        if metadata.is_dir() {
            for entry in std::fs::read_dir(current).expect("read output directory") {
                let entry = entry.expect("read output entry");
                collect_tree_files(&entry.path(), files);
            }
        } else if metadata.is_file() {
            files.push(current.to_path_buf());
        } else {
            panic!("build output must be regular files");
        }
    }

    fn declare(h: &Harness, capability: AppCapability) {
        let mut manifest = load_manifest(&h.layout).expect("manifest");
        manifest.capabilities.push(capability);
        save_manifest(&h.layout, &manifest).expect("declare");
    }

    fn grant(h: &Harness, capability: AppCapability) {
        let mut permissions = load_permissions(&h.layout).expect("permissions");
        permissions.grant(capability);
        save_permissions(&h.layout, &permissions).expect("grant");
    }

    fn declare_and_grant(h: &Harness, capability: AppCapability) {
        declare(h, capability);
        grant(h, capability);
    }

    async fn start_runtime(h: &Harness) {
        let started = h
            .broker
            .manage_runtime_value(json!({"app_id": h.app_id, "action": "start"}))
            .await
            .expect("runtime starts");
        assert_eq!(started["state"], "running");
    }

    async fn execute(
        h: &Harness,
        operation: BridgeOperation,
        payload: Value,
    ) -> (bool, Value, Option<String>, Option<String>) {
        h.broker
            .execute_bridge(BridgeRequest {
                request_id: "req-1".into(),
                app_id: h.app_id.clone(),
                operation,
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
                } => Some(response),
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

    // ---- capture / pick ---------------------------------------------------

    #[tokio::test]
    async fn capture_photo_returns_the_downscaled_jpeg_as_base64() {
        let camera = FakeCamera::with_bytes(vec![1, 2, 3]);
        let h = harness(DeviceCapabilities {
            camera: Some(camera.clone()),
            ..DeviceCapabilities::default()
        })
        .await;
        declare_and_grant(&h, AppCapability::Camera);

        let (ok, result, error, code) = execute(&h, BridgeOperation::CapturePhoto, json!({})).await;
        assert!(ok, "{error:?} {code:?}");
        assert_eq!(result["mimeType"], "image/jpeg");
        assert_eq!(result["width"], 640);
        assert_eq!(result["height"], 480);
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(result["base64"].as_str().expect("base64"))
            .expect("decodes");
        assert_eq!(bytes, vec![1, 2, 3]);
        assert_eq!(
            camera.sized_calls.lock().unwrap().as_slice(),
            &[(1280, 0.8)],
            "the defaults reach the native scaler"
        );
    }

    #[tokio::test]
    async fn capture_photo_clamps_dimension_and_quality() {
        let camera = FakeCamera::with_bytes(vec![1]);
        let h = harness(DeviceCapabilities {
            camera: Some(camera.clone()),
            ..DeviceCapabilities::default()
        })
        .await;
        declare_and_grant(&h, AppCapability::Camera);

        let (ok, _, _, _) = execute(
            &h,
            BridgeOperation::CapturePhoto,
            json!({"maxDimension": 99_999, "quality": 0.1}),
        )
        .await;
        assert!(ok);
        assert_eq!(
            camera.sized_calls.lock().unwrap().as_slice(),
            &[(2048, 0.5)]
        );
    }

    #[tokio::test]
    async fn an_oversized_media_result_is_refused() {
        let camera = FakeCamera::with_bytes(vec![0u8; 5 * 1024 * 1024]);
        let h = harness(DeviceCapabilities {
            camera: Some(camera),
            ..DeviceCapabilities::default()
        })
        .await;
        declare_and_grant(&h, AppCapability::Camera);

        let (ok, _, _, code) = execute(&h, BridgeOperation::CapturePhoto, json!({})).await;
        assert!(!ok);
        assert_eq!(code.as_deref(), Some("media_too_large"));
    }

    #[tokio::test]
    async fn pick_image_requires_its_own_photo_library_capability() {
        let camera = FakeCamera::with_bytes(vec![1]);
        let h = harness(DeviceCapabilities {
            camera: Some(camera),
            ..DeviceCapabilities::default()
        })
        .await;
        // Camera declared+granted — but PICK rides PhotoLibrary, a different
        // OS authorization surface, so it must still be refused.
        declare_and_grant(&h, AppCapability::Camera);

        let (ok, _, _, code) = execute(&h, BridgeOperation::PickImage, json!({})).await;
        assert!(!ok);
        assert_eq!(code.as_deref(), Some("capability_not_declared"));
    }

    #[tokio::test]
    async fn a_missing_device_handle_fails_typed() {
        let h = harness(DeviceCapabilities::default()).await;
        declare_and_grant(&h, AppCapability::Camera);

        let (ok, _, _, code) = execute(&h, BridgeOperation::CapturePhoto, json!({})).await;
        assert!(!ok);
        assert_eq!(code.as_deref(), Some("capability_unavailable"));
    }

    #[tokio::test]
    async fn calendar_events_are_bounded_and_require_calendar_capability() {
        let calendar = Arc::new(FakeCalendar {
            events: vec![CalendarEvent {
                id: "event-1".into(),
                title: "设计评审".into(),
                start_ms: 1_000,
                end_ms: 2_000,
                all_day: false,
                location: Some("会议室".into()),
                notes: None,
                calendar: Some("工作".into()),
            }],
            ..FakeCalendar::default()
        });
        let h = harness(DeviceCapabilities {
            calendar: Some(calendar.clone()),
            ..DeviceCapabilities::default()
        })
        .await;

        let (ok, _, _, code) = execute(
            &h,
            BridgeOperation::CalendarListEvents,
            json!({"startMs": 0, "endMs": 86_400_000}),
        )
        .await;
        assert!(!ok);
        assert_eq!(code.as_deref(), Some("capability_not_declared"));

        declare_and_grant(&h, AppCapability::Calendar);
        let (ok, result, error, code) = execute(
            &h,
            BridgeOperation::CalendarListEvents,
            json!({"startMs": 0, "endMs": 86_400_000, "limit": 10_000}),
        )
        .await;
        assert!(ok, "{error:?} {code:?}");
        assert_eq!(result[0]["title"], "设计评审");
        assert_eq!(calendar.queries.lock().unwrap()[0].limit, 100);
    }

    #[tokio::test]
    async fn contacts_search_is_trimmed_and_bounded() {
        let contacts = Arc::new(FakeContacts {
            contacts: vec![Contact {
                id: "contact-1".into(),
                display_name: "林夕".into(),
                phones: vec!["13800000000".into()],
                emails: vec!["lingxi@example.com".into()],
            }],
            ..FakeContacts::default()
        });
        let h = harness(DeviceCapabilities {
            contacts: Some(contacts.clone()),
            ..DeviceCapabilities::default()
        })
        .await;
        declare_and_grant(&h, AppCapability::Contacts);

        let (ok, result, error, code) = execute(
            &h,
            BridgeOperation::ContactsSearch,
            json!({"query": "  林夕 ", "limit": 500}),
        )
        .await;
        assert!(ok, "{error:?} {code:?}");
        assert_eq!(result[0]["display_name"], "林夕");
        let query = &contacts.queries.lock().unwrap()[0];
        assert_eq!(query.query, "林夕");
        assert_eq!(query.limit, 50);
    }

    #[tokio::test]
    async fn media_get_retrieves_only_the_app_owned_handle() {
        let camera = FakeCamera::with_bytes(vec![4, 5, 6]);
        let h = harness(DeviceCapabilities {
            camera: Some(camera),
            ..DeviceCapabilities::default()
        })
        .await;
        declare_and_grant(&h, AppCapability::Camera);
        let (ok, capture, error, code) =
            execute(&h, BridgeOperation::CapturePhoto, json!({})).await;
        assert!(ok, "{error:?} {code:?}");
        let media_id = capture["mediaId"].as_str().expect("media id").to_string();

        declare_and_grant(&h, AppCapability::Media);
        let (ok, media, error, code) =
            execute(&h, BridgeOperation::MediaGet, json!({"mediaId": media_id})).await;
        assert!(ok, "{error:?} {code:?}");
        assert_eq!(media["mimeType"], "image/jpeg");
        assert_eq!(media["bytes"], 3);

        let (ok, _, _, code) = execute(
            &h,
            BridgeOperation::MediaGet,
            json!({"mediaId": "other-app-handle"}),
        )
        .await;
        assert!(!ok);
        assert_eq!(code.as_deref(), Some("not_found"));
    }

    #[tokio::test]
    async fn an_os_level_denial_maps_to_permission_denied() {
        let camera = FakeCamera::failing(CameraError::PermissionDenied);
        let h = harness(DeviceCapabilities {
            camera: Some(camera),
            ..DeviceCapabilities::default()
        })
        .await;
        declare_and_grant(&h, AppCapability::Camera);

        let (ok, _, _, code) = execute(&h, BridgeOperation::CapturePhoto, json!({})).await;
        assert!(!ok);
        assert_eq!(code.as_deref(), Some("permission_denied"));
    }

    #[tokio::test]
    async fn a_first_use_prompt_allows_once_and_proceeds() {
        let camera = FakeCamera::with_bytes(vec![5]);
        let h = harness(DeviceCapabilities {
            camera: Some(camera),
            ..DeviceCapabilities::default()
        })
        .await;
        declare(&h, AppCapability::Camera);

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
                                broker
                                    .resolve_capability(
                                        &request.request_id,
                                        AuthorizationDecision::AllowOnce,
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

        let (ok, result, error, code) = timeout(
            Duration::from_secs(5),
            execute(&h, BridgeOperation::CapturePhoto, json!({})),
        )
        .await
        .expect("prompt resolves");
        assert!(ok, "{error:?} {code:?}");
        assert!(result["base64"].is_string());
        resolver.await.expect("resolver completes");
    }

    // ---- recording --------------------------------------------------------

    #[tokio::test]
    async fn record_stop_returns_the_recording_with_duration() {
        let audio = Arc::new(FakeAudio::default());
        let h = harness(DeviceCapabilities {
            audio: Some(audio.clone()),
            ..DeviceCapabilities::default()
        })
        .await;
        declare_and_grant(&h, AppCapability::Microphone);
        start_runtime(&h).await;

        let (ok, started, _, _) = execute(&h, BridgeOperation::RecordAudioStart, json!({})).await;
        assert!(ok);
        assert_eq!(started["started"], true);
        assert_eq!(started["maxDurationMs"], 120_000);

        let (ok, result, error, code) =
            execute(&h, BridgeOperation::RecordAudioStop, json!({})).await;
        assert!(ok, "{error:?} {code:?}");
        assert_eq!(result["mimeType"], "audio/m4a");
        assert!(result["durationMs"].is_u64());
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(result["base64"].as_str().expect("base64"))
            .expect("decodes");
        assert_eq!(bytes, vec![7, 7, 7]);
        assert!(audio.stopped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn rejected_recording_start_stops_its_returned_handle() {
        let start_gate = Arc::new(tokio::sync::Notify::new());
        let start_entered = Arc::new(tokio::sync::Notify::new());
        let audio = Arc::new(FakeAudio {
            start_gate: Some(start_gate.clone()),
            start_entered: Some(start_entered.clone()),
            ..FakeAudio::default()
        });
        let h = harness(DeviceCapabilities {
            audio: Some(audio.clone()),
            ..DeviceCapabilities::default()
        })
        .await;
        declare_and_grant(&h, AppCapability::Microphone);
        start_runtime(&h).await;

        let starting = {
            let broker = h.broker.clone();
            let app_id = h.app_id.clone();
            tokio::spawn(async move {
                broker
                    .execute_bridge(BridgeRequest {
                        request_id: "rejected-start".into(),
                        app_id,
                        operation: BridgeOperation::RecordAudioStart,
                        payload_json: Some("{}".into()),
                    })
                    .await
            })
        };
        timeout(Duration::from_secs(2), start_entered.notified())
            .await
            .expect("native recording start is pending");
        h.broker.recording_pending.lock().await.take();
        start_gate.notify_one();
        timeout(Duration::from_secs(2), starting)
            .await
            .expect("rejected start settles")
            .expect("bridge task joins");

        timeout(Duration::from_secs(2), async {
            loop {
                if audio.active.lock().unwrap().is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the uncommitted native handle is stopped");
        assert!(audio
            .operations
            .lock()
            .unwrap()
            .iter()
            .any(|(_, operation)| { matches!(operation, AudioOperation::StopRecording { .. }) }));
    }

    #[tokio::test]
    async fn dropped_start_with_a_returned_handle_stops_the_recording() {
        let fake = Arc::new(FakeAudio::default());
        let audio: Arc<dyn AudioService> = fake.clone();
        let context = local_audio_context_for_owner(
            &audio,
            "cancelled-app",
            17,
            "dropped-start",
            LOCAL_APP_AUDIO_TIMEOUT,
        );
        let handle = match audio
            .execute(
                context.clone(),
                AudioOperation::StartRecording {
                    sample_rate_hz: RECORD_SAMPLE_RATE_HZ,
                    format: RECORD_FORMAT.into(),
                },
            )
            .await
            .unwrap()
        {
            AudioOperationSuccess::RecordingStarted { handle } => handle,
            _ => panic!("expected a recording handle"),
        };
        let pending = Arc::new(Mutex::new(Some(PendingRecordingStart {
            app_id: "cancelled-app".into(),
            runtime_generation: 17,
            context: context.clone(),
            audio,
            scope: Arc::new(LocalAppAudioScope::new()),
        })));
        drop(PendingStartGuard {
            pending: pending.clone(),
            identity: context.identity,
            handle: Some(handle),
            armed: true,
        });
        timeout(Duration::from_secs(2), async {
            loop {
                if pending.lock().await.is_none() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("completed native capture is released");
        assert!(fake.active.lock().unwrap().is_empty());
        assert!(fake
            .operations
            .lock()
            .unwrap()
            .iter()
            .any(|(_, operation)| { matches!(operation, AudioOperation::StopRecording { .. }) }));
    }

    #[tokio::test]
    async fn a_new_audio_service_epoch_releases_a_stale_pending_start() {
        let fake = Arc::new(FakeAudio::default());
        let audio: Arc<dyn AudioService> = fake.clone();
        let h = harness(DeviceCapabilities {
            audio: Some(audio.clone()),
            ..DeviceCapabilities::default()
        })
        .await;
        declare_and_grant(&h, AppCapability::Microphone);
        start_runtime(&h).await;
        let mut stale = local_audio_context_for_owner(
            &audio,
            &h.app_id,
            1,
            "disconnected-start",
            LOCAL_APP_AUDIO_TIMEOUT,
        );
        stale.identity.service_epoch -= 1;
        *h.broker.recording_pending.lock().await = Some(PendingRecordingStart {
            app_id: h.app_id.clone(),
            runtime_generation: 1,
            context: stale,
            audio,
            scope: Arc::new(LocalAppAudioScope::new()),
        });
        let (ok, _, error, code) = execute(&h, BridgeOperation::RecordAudioStart, json!({})).await;
        assert!(
            ok,
            "new epoch should not inherit a stale busy reservation: {error:?} {code:?}"
        );
    }

    #[tokio::test]
    async fn cancelled_pending_start_does_not_hold_registry_during_native_cancel() {
        let start_gate = Arc::new(tokio::sync::Notify::new());
        let start_entered = Arc::new(tokio::sync::Notify::new());
        let cancel_gate = Arc::new(tokio::sync::Notify::new());
        let cancel_entered = Arc::new(tokio::sync::Notify::new());
        let audio = Arc::new(FakeAudio {
            start_gate: Some(start_gate.clone()),
            start_entered: Some(start_entered.clone()),
            cancel_gate: Some(cancel_gate.clone()),
            cancel_entered: Some(cancel_entered.clone()),
            ..FakeAudio::default()
        });
        let h = harness(DeviceCapabilities {
            audio: Some(audio),
            ..DeviceCapabilities::default()
        })
        .await;
        declare_and_grant(&h, AppCapability::Microphone);
        start_runtime(&h).await;

        let starting = {
            let broker = h.broker.clone();
            let app_id = h.app_id.clone();
            tokio::spawn(async move {
                broker
                    .execute_bridge(BridgeRequest {
                        request_id: "cancelled-pending-start".into(),
                        app_id,
                        operation: BridgeOperation::RecordAudioStart,
                        payload_json: Some("{}".into()),
                    })
                    .await
            })
        };
        timeout(Duration::from_secs(2), start_entered.notified())
            .await
            .expect("native start is pending");
        starting.abort();
        let _ = starting.await;
        timeout(Duration::from_secs(2), cancel_entered.notified())
            .await
            .expect("native cancel is pending");
        let _ = timeout(
            Duration::from_millis(100),
            h.broker.recording_pending.lock(),
        )
        .await
        .expect("pending registry remains accessible during native cancellation");
        cancel_gate.notify_one();
        start_gate.notify_one();
        timeout(Duration::from_secs(2), async {
            loop {
                if h.broker.recording_pending.lock().await.is_none() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("cancelled start eventually releases its reservation");
    }

    #[tokio::test]
    async fn cancelled_manual_stop_keeps_the_native_result_for_the_next_stop_call() {
        let stop_gate = Arc::new(tokio::sync::Notify::new());
        let stop_entered = Arc::new(tokio::sync::Notify::new());
        let stop_succeeded = Arc::new(tokio::sync::Notify::new());
        let audio = Arc::new(FakeAudio {
            stop_gate: Some(stop_gate.clone()),
            stop_entered: Some(stop_entered.clone()),
            stop_succeeded: Some(stop_succeeded.clone()),
            ..FakeAudio::default()
        });
        let h = harness(DeviceCapabilities {
            audio: Some(audio.clone()),
            ..DeviceCapabilities::default()
        })
        .await;
        declare_and_grant(&h, AppCapability::Microphone);
        start_runtime(&h).await;

        let (ok, _, error, code) = execute(&h, BridgeOperation::RecordAudioStart, json!({})).await;
        assert!(ok, "{error:?} {code:?}");
        let stopping = {
            let broker = h.broker.clone();
            let app_id = h.app_id.clone();
            tokio::spawn(async move {
                broker
                    .execute_bridge(BridgeRequest {
                        request_id: "cancelled-manual-stop".into(),
                        app_id,
                        operation: BridgeOperation::RecordAudioStop,
                        payload_json: Some("{}".into()),
                    })
                    .await;
            })
        };
        timeout(Duration::from_secs(2), stop_entered.notified())
            .await
            .expect("manual stop reaches the audio service");

        // Hold the host registry after the native stop starts so it can finish
        // while the caller is cancelled, before its result is cached there.
        let registry = h.broker.recording.lock().await;
        stop_gate.notify_one();
        timeout(Duration::from_secs(2), stop_succeeded.notified())
            .await
            .expect("native StopRecording succeeds");
        assert!(audio.active.lock().unwrap().is_empty());

        stopping.abort();
        assert!(stopping
            .await
            .expect_err("outer manual-stop request is cancelled")
            .is_cancelled());
        drop(registry);

        timeout(Duration::from_secs(2), async {
            loop {
                if h.broker
                    .recording
                    .lock()
                    .await
                    .as_ref()
                    .is_some_and(|active| active.finished.is_some() && !active.stopping)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("detached stop task caches the native result");

        let (ok, result, error, code) =
            execute(&h, BridgeOperation::RecordAudioStop, json!({})).await;
        assert!(
            ok,
            "cached stop result remains collectible: {error:?} {code:?}"
        );
        assert_eq!(result["autoStopped"], false);
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(result["base64"].as_str().expect("base64"))
            .expect("decodes");
        assert_eq!(bytes, vec![7, 7, 7]);
        assert!(h.broker.recording.lock().await.is_none());
    }

    #[tokio::test]
    async fn runtime_teardown_during_manual_stop_cannot_restore_the_recording() {
        let stop_gate = Arc::new(tokio::sync::Notify::new());
        let stop_entered = Arc::new(tokio::sync::Notify::new());
        let audio = Arc::new(FakeAudio {
            stop_gate: Some(stop_gate.clone()),
            stop_entered: Some(stop_entered.clone()),
            ..FakeAudio::default()
        });
        let h = harness(DeviceCapabilities {
            audio: Some(audio.clone()),
            ..DeviceCapabilities::default()
        })
        .await;
        declare_and_grant(&h, AppCapability::Microphone);
        start_runtime(&h).await;

        let (ok, _, error, code) = execute(&h, BridgeOperation::RecordAudioStart, json!({})).await;
        assert!(ok, "{error:?} {code:?}");

        let stopping = {
            let broker = h.broker.clone();
            let app_id = h.app_id.clone();
            tokio::spawn(async move {
                broker
                    .execute_bridge(BridgeRequest {
                        request_id: "manual-stop-racing-runtime-teardown".into(),
                        app_id,
                        operation: BridgeOperation::RecordAudioStop,
                        payload_json: Some("{}".into()),
                    })
                    .await
            })
        };
        timeout(Duration::from_secs(2), stop_entered.notified())
            .await
            .expect("manual stop reaches the audio service");

        h.broker
            .manage_runtime_value(json!({"app_id": h.app_id, "action": "stop"}))
            .await
            .expect("runtime teardown ends the audio owner");
        assert!(audio.active.lock().unwrap().is_empty());
        assert!(h.broker.recording.lock().await.is_none());

        // The detached native stop can finish after teardown. Its generation
        // and handle no longer match the registry, so it must not resurrect it.
        stop_gate.notify_one();
        let _ = timeout(Duration::from_secs(2), stopping)
            .await
            .expect("in-flight stop settles after teardown")
            .expect("manual-stop request task joins");

        assert!(audio.active.lock().unwrap().is_empty());
        assert!(h.broker.recording.lock().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn an_uncollected_manual_stop_result_expires_from_the_registry() {
        let audio = Arc::new(FakeAudio::default());
        let h = harness(DeviceCapabilities {
            audio: Some(audio.clone()),
            ..DeviceCapabilities::default()
        })
        .await;
        declare_and_grant(&h, AppCapability::Microphone);
        start_runtime(&h).await;

        let (ok, _, error, code) = execute(&h, BridgeOperation::RecordAudioStart, json!({})).await;
        assert!(ok, "{error:?} {code:?}");
        let stop = {
            let mut slot = h.broker.recording.lock().await;
            let active = slot.as_mut().expect("host recording handle");
            active.stopping = true;
            ManualRecordingStop {
                app_id: active.app_id.clone(),
                runtime_generation: active.runtime_generation,
                handle: active.handle.clone(),
                context: local_audio_context(
                    &active.audio,
                    &active.invocation,
                    active.runtime_generation,
                    LOCAL_APP_AUDIO_TIMEOUT,
                )
                .expect("valid recording stop context"),
                audio: active.audio.clone(),
                duration_ms: u64::try_from(active.started.elapsed().as_millis())
                    .unwrap_or(u64::MAX),
            }
        };

        execute_manual_recording_stop(h.broker.recording.clone(), stop)
            .await
            .expect("native manual stop completes");
        assert!(matches!(
            h.broker.recording.lock().await.as_ref(),
            Some(active) if active.finished.is_some()
        ));

        tokio::time::advance(FINISHED_RECORDING_TTL + Duration::from_millis(1)).await;
        tokio::task::yield_now().await;

        assert!(h.broker.recording.lock().await.is_none());
        assert!(audio.active.lock().unwrap().is_empty());
    }

    /// The first mic use of an app's life begins with an OS permission
    /// alert, and the user may leave it on screen indefinitely. Nothing that
    /// RECLAIMS the recorder may sit behind that: a runtime stop awaits
    /// `force_stop_recording`, so holding the state lock across the native
    /// start would hang an app teardown on an unanswered system dialog.
    #[tokio::test]
    async fn a_start_waiting_on_the_os_permission_alert_does_not_block_a_reclaim() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let entered = Arc::new(tokio::sync::Notify::new());
        let audio = Arc::new(FakeAudio {
            start_gate: Some(gate.clone()),
            start_entered: Some(entered.clone()),
            ..FakeAudio::default()
        });
        let h = harness(DeviceCapabilities {
            audio: Some(audio.clone()),
            ..DeviceCapabilities::default()
        })
        .await;
        declare_and_grant(&h, AppCapability::Microphone);
        start_runtime(&h).await;

        let starting = {
            let broker = h.broker.clone();
            let app_id = h.app_id.clone();
            tokio::spawn(async move {
                broker
                    .execute_bridge(BridgeRequest {
                        request_id: "start-1".into(),
                        app_id,
                        operation: BridgeOperation::RecordAudioStart,
                        payload_json: Some("{}".into()),
                    })
                    .await;
            })
        };
        // Let the start reach the (blocked) native call.
        timeout(Duration::from_secs(2), entered.notified())
            .await
            .expect("start reaches the native permission gate");
        let start_identity = audio
            .operations
            .lock()
            .unwrap()
            .iter()
            .find_map(|(context, operation)| {
                matches!(operation, AudioOperation::StartRecording { .. })
                    .then(|| context.identity.clone())
            })
            .expect("native start identity");
        assert!(
            !starting.is_finished(),
            "the fixture start must still be pending"
        );

        // The reclaim path must answer while that alert is still up.
        let (runtime_generation, _) = h
            .broker
            .runtime_identity(&h.app_id)
            .await
            .expect("runtime lookup")
            .expect("runtime is active");
        timeout(
            Duration::from_millis(500),
            h.broker
                .manage_runtime_value(json!({"app_id": h.app_id, "action": "stop"})),
        )
        .await
        .expect("runtime stop must not block behind the permission prompt")
        .expect("runtime stop succeeds");
        assert_ne!(runtime_generation, 0);
        assert!(
            audio.active.lock().unwrap().is_empty(),
            "targeted cancellation releases a capture created before delivery"
        );
        gate.notify_one();
        starting.await.expect("the start task completes");
        assert!(
            audio.cancelled.lock().unwrap().contains(&start_identity),
            "runtime teardown cancels the exact undelivered start identity"
        );
        assert!(
            h.broker.recording.lock().await.is_none(),
            "a cancelled pending start is never committed"
        );
    }

    #[tokio::test]
    async fn a_stop_without_a_recording_is_typed() {
        let audio = Arc::new(FakeAudio::default());
        let h = harness(DeviceCapabilities {
            audio: Some(audio),
            ..DeviceCapabilities::default()
        })
        .await;
        declare_and_grant(&h, AppCapability::Microphone);
        start_runtime(&h).await;

        let (ok, _, _, code) = execute(&h, BridgeOperation::RecordAudioStop, json!({})).await;
        assert!(!ok);
        assert_eq!(code.as_deref(), Some("not_recording"));
    }

    #[tokio::test]
    async fn a_recording_held_by_another_app_is_busy() {
        let audio = Arc::new(FakeAudio::default());
        let h = harness(DeviceCapabilities {
            audio: Some(audio),
            ..DeviceCapabilities::default()
        })
        .await;
        declare_and_grant(&h, AppCapability::Microphone);
        start_runtime(&h).await;

        let (ok, _, _, _) = execute(&h, BridgeOperation::RecordAudioStart, json!({})).await;
        assert!(ok);

        // A second app on the same broker. Declared+granted so only the
        // cross-app hold can refuse it.
        let record = h
            .service
            .create_app(Some("Second"), "second app", None)
            .await
            .expect("second app");
        let layout = AppLayout::new(h.layout.root().to_path_buf(), record.id.clone())
            .expect("second layout");
        let mut manifest = load_manifest(&layout).expect("second manifest");
        manifest.capabilities.push(AppCapability::Microphone);
        save_manifest(&layout, &manifest).expect("second declare");
        let mut permissions = load_permissions(&layout).expect("second permissions");
        permissions.grant(AppCapability::Microphone);
        save_permissions(&layout, &permissions).expect("second grant");
        let record = prepare_launchable_runtime_fixture(&h.service, record, &layout).await;
        let second_started = h
            .broker
            .manage_runtime_value(json!({"app_id": record.id, "action": "start"}))
            .await
            .expect("second runtime starts");
        assert_eq!(second_started["state"], "running");
        h.broker
            .execute_bridge(BridgeRequest {
                request_id: "req-b".into(),
                app_id: record.id,
                operation: BridgeOperation::RecordAudioStart,
                payload_json: Some("{}".into()),
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
                } if response.request_id == "req-b" => Some(response),
                _ => None,
            })
            .expect("second response");
        assert!(!response.ok);
        assert_eq!(response.error_code.as_deref(), Some("audio_session_busy"));
    }

    #[tokio::test]
    async fn a_same_app_restart_replaces_the_orphaned_recording() {
        let audio = Arc::new(FakeAudio::default());
        let h = harness(DeviceCapabilities {
            audio: Some(audio.clone()),
            ..DeviceCapabilities::default()
        })
        .await;
        declare_and_grant(&h, AppCapability::Microphone);
        start_runtime(&h).await;

        let (ok, _, _, _) = execute(&h, BridgeOperation::RecordAudioStart, json!({})).await;
        assert!(ok);
        // A reloaded page starts again: the orphan is reclaimed, not fatal.
        let (ok, restarted, error, code) =
            execute(&h, BridgeOperation::RecordAudioStart, json!({})).await;
        assert!(ok, "{error:?} {code:?}");
        assert_eq!(restarted["replacedActive"], true);
        // The replaced session's bytes are discarded; the new one still stops.
        let (ok, _, _, _) = execute(&h, BridgeOperation::RecordAudioStop, json!({})).await;
        assert!(ok);
    }

    #[tokio::test]
    async fn a_failed_replacement_stop_keeps_the_old_recording_available_for_cleanup() {
        for (kind, expected_code) in [
            (AudioErrorKind::Timeout, "timeout"),
            (AudioErrorKind::NativeFailure, "native_failure"),
        ] {
            let audio = Arc::new(FakeAudio::default());
            let h = harness(DeviceCapabilities {
                audio: Some(audio.clone()),
                ..DeviceCapabilities::default()
            })
            .await;
            declare_and_grant(&h, AppCapability::Microphone);
            start_runtime(&h).await;

            let (ok, _, error, code) =
                execute(&h, BridgeOperation::RecordAudioStart, json!({})).await;
            assert!(ok, "{error:?} {code:?}");
            let old_handle = audio
                .active
                .lock()
                .unwrap()
                .values()
                .next()
                .expect("native recording handle")
                .clone();
            *audio.fail_next_stop.lock().unwrap() = Some(kind);

            let (ok, _, error, code) =
                execute(&h, BridgeOperation::RecordAudioStart, json!({})).await;
            assert!(!ok, "replacement should report the failed native stop");
            assert_eq!(code.as_deref(), Some(expected_code), "{error:?}");

            assert!(
                matches!(
                    h.broker.recording.lock().await.as_ref(),
                    Some(active) if active.handle == old_handle && !active.replacing
                ),
                "failed replacement must leave the old host cleanup handle registered"
            );
            assert!(audio
                .active
                .lock()
                .unwrap()
                .values()
                .any(|handle| handle == &old_handle));

            let (ok, _, error, code) =
                execute(&h, BridgeOperation::RecordAudioStop, json!({})).await;
            assert!(
                ok,
                "the preserved handle remains stoppable: {error:?} {code:?}"
            );
            assert!(audio.active.lock().unwrap().is_empty());
            assert!(h.broker.recording.lock().await.is_none());
        }
    }

    #[tokio::test]
    async fn a_cancelled_replacement_stop_releases_its_reservation_and_keeps_the_handle() {
        let stop_gate = Arc::new(tokio::sync::Notify::new());
        let stop_entered = Arc::new(tokio::sync::Notify::new());
        let audio = Arc::new(FakeAudio {
            stop_gate: Some(stop_gate.clone()),
            stop_entered: Some(stop_entered.clone()),
            ..FakeAudio::default()
        });
        let h = harness(DeviceCapabilities {
            audio: Some(audio.clone()),
            ..DeviceCapabilities::default()
        })
        .await;
        declare_and_grant(&h, AppCapability::Microphone);
        start_runtime(&h).await;

        let (ok, _, error, code) = execute(&h, BridgeOperation::RecordAudioStart, json!({})).await;
        assert!(ok, "{error:?} {code:?}");
        let old_handle = audio
            .active
            .lock()
            .unwrap()
            .values()
            .next()
            .expect("native recording handle")
            .clone();

        let replacement = {
            let broker = h.broker.clone();
            let app_id = h.app_id.clone();
            tokio::spawn(async move {
                broker
                    .execute_bridge(BridgeRequest {
                        request_id: "cancelled-replacement".into(),
                        app_id,
                        operation: BridgeOperation::RecordAudioStart,
                        payload_json: Some("{}".into()),
                    })
                    .await;
            })
        };
        timeout(Duration::from_secs(2), stop_entered.notified())
            .await
            .expect("replacement reaches native stop");
        assert!(matches!(
            h.broker.recording.lock().await.as_ref(),
            Some(active) if active.handle == old_handle && active.replacing
        ));

        replacement.abort();
        assert!(replacement
            .await
            .expect_err("replacement is cancelled")
            .is_cancelled());
        assert!(
            matches!(
                h.broker.recording.lock().await.as_ref(),
                Some(active) if active.handle == old_handle && !active.replacing
            ),
            "dropping a replacement request must release its reservation"
        );

        stop_gate.notify_one();
        let (ok, _, error, code) = execute(&h, BridgeOperation::RecordAudioStop, json!({})).await;
        assert!(
            ok,
            "the original handle remains stoppable: {error:?} {code:?}"
        );
        assert!(audio.active.lock().unwrap().is_empty());
        assert!(h.broker.recording.lock().await.is_none());
    }

    #[tokio::test]
    async fn a_cancelled_successful_replacement_commit_removes_the_stopped_handle() {
        let audio = Arc::new(FakeAudio::default());
        let h = harness(DeviceCapabilities {
            audio: Some(audio.clone()),
            ..DeviceCapabilities::default()
        })
        .await;
        declare_and_grant(&h, AppCapability::Microphone);
        start_runtime(&h).await;

        let (ok, _, error, code) = execute(&h, BridgeOperation::RecordAudioStart, json!({})).await;
        assert!(ok, "{error:?} {code:?}");

        let mut slot = h.broker.recording.lock().await;
        let active = slot.as_mut().expect("host recording handle");
        active.replacing = true;
        let old_handle = active.handle.clone();
        let replacement = RecordingReplacement {
            app_id: active.app_id.clone(),
            runtime_generation: active.runtime_generation,
            invocation: active.invocation.clone(),
            handle: active.handle.clone(),
            audio: active.audio.clone(),
        };
        audio.active.lock().unwrap().clear();
        audio.stopped.store(true, Ordering::SeqCst);

        let mut guard = RecordingReplacementGuard::new(h.broker.recording.clone(), replacement);
        guard.mark_stopped();
        let cleanup = tokio::spawn(async move { guard.finish().await });
        tokio::task::yield_now().await;
        assert!(
            !cleanup.is_finished(),
            "registry cleanup is waiting on its lock"
        );

        cleanup.abort();
        assert!(cleanup
            .await
            .expect_err("commit cleanup is cancelled")
            .is_cancelled());
        assert!(matches!(
            slot.as_ref(),
            Some(active) if active.handle == old_handle && active.replacing
        ));
        drop(slot);

        timeout(Duration::from_secs(2), async {
            loop {
                if h.broker.recording.lock().await.is_none() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("drop cleanup removes the already-stopped handle");
        assert!(audio.active.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn the_watchdog_auto_stops_and_caches_the_recording() {
        let audio = Arc::new(FakeAudio::default());
        let h = harness(DeviceCapabilities {
            audio: Some(audio.clone()),
            ..DeviceCapabilities::default()
        })
        .await;
        declare_and_grant(&h, AppCapability::Microphone);
        start_runtime(&h).await;

        let (ok, _, _, _) = execute(
            &h,
            BridgeOperation::RecordAudioStart,
            json!({"maxDurationMs": 1_000}),
        )
        .await;
        assert!(ok);

        tokio::time::sleep(Duration::from_millis(1_500)).await;
        assert!(
            audio.stopped.load(Ordering::SeqCst),
            "the watchdog must stop the native recorder at the duration cap"
        );

        let (ok, result, error, code) =
            execute(&h, BridgeOperation::RecordAudioStop, json!({})).await;
        assert!(ok, "{error:?} {code:?}");
        assert_eq!(result["autoStopped"], true);
        assert!(result["base64"].is_string());
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_releases_the_owner_when_recording_stop_fails() {
        let audio = Arc::new(FakeAudio::default());
        let h = harness(DeviceCapabilities {
            audio: Some(audio.clone()),
            ..DeviceCapabilities::default()
        })
        .await;
        declare_and_grant(&h, AppCapability::Microphone);
        start_runtime(&h).await;
        let (ok, _, error, code) = execute(
            &h,
            BridgeOperation::RecordAudioStart,
            json!({"maxDurationMs": 1_000}),
        )
        .await;
        assert!(ok, "{error:?} {code:?}");
        *audio.fail_next_stop.lock().unwrap() = Some(AudioErrorKind::NativeFailure);

        tokio::time::sleep(Duration::from_millis(1_500)).await;
        assert!(audio.active.lock().unwrap().is_empty());
        assert!(audio.stopped.load(Ordering::SeqCst));
        assert!(h.broker.recording.lock().await.is_none());
        assert!(audio
            .operations
            .lock()
            .unwrap()
            .iter()
            .any(|(_, operation)| { matches!(operation, AudioOperation::EndOwner) }));
    }

    #[tokio::test]
    async fn a_blocked_watchdog_stop_does_not_hold_the_recording_registry() {
        let stop_gate = Arc::new(tokio::sync::Notify::new());
        let stop_entered = Arc::new(tokio::sync::Notify::new());
        let audio = Arc::new(FakeAudio {
            stop_gate: Some(stop_gate.clone()),
            stop_entered: Some(stop_entered.clone()),
            ..FakeAudio::default()
        });
        let h = harness(DeviceCapabilities {
            audio: Some(audio.clone()),
            ..DeviceCapabilities::default()
        })
        .await;
        declare_and_grant(&h, AppCapability::Microphone);
        start_runtime(&h).await;
        let (ok, _, error, code) = execute(
            &h,
            BridgeOperation::RecordAudioStart,
            json!({"maxDurationMs": 1_000}),
        )
        .await;
        assert!(ok, "{error:?} {code:?}");

        timeout(Duration::from_secs(2), stop_entered.notified())
            .await
            .expect("watchdog reaches native stop");
        let registry = timeout(Duration::from_millis(200), h.broker.recording.lock())
            .await
            .expect("native stop must not hold the recording registry");
        drop(registry);

        let generation = h
            .broker
            .runtime_identity(&h.app_id)
            .await
            .expect("runtime lookup")
            .expect("runtime is active")
            .0;
        timeout(
            Duration::from_secs(2),
            h.broker.force_stop_recording(&h.app_id, generation),
        )
        .await
        .expect("runtime teardown must pass the blocked watchdog");
        stop_gate.notify_one();
        tokio::task::yield_now().await;
        assert!(h.broker.recording.lock().await.is_none());
    }

    #[tokio::test]
    async fn stopping_the_runtime_reclaims_an_active_recording() {
        let audio = Arc::new(FakeAudio::default());
        let h = harness(DeviceCapabilities {
            audio: Some(audio.clone()),
            ..DeviceCapabilities::default()
        })
        .await;
        declare_and_grant(&h, AppCapability::Microphone);

        start_runtime(&h).await;
        let (ok, _, _, _) = execute(&h, BridgeOperation::RecordAudioStart, json!({})).await;
        assert!(ok);

        h.broker
            .manage_runtime_value(json!({"app_id": h.app_id, "action": "stop"}))
            .await
            .expect("runtime stops");
        assert!(
            audio.stopped.load(Ordering::SeqCst),
            "a runtime stop must release the recorder (and its audio-session lease)"
        );
        assert!(audio.active.lock().unwrap().is_empty());
        assert!(h.broker.recording.lock().await.is_none());
    }

    #[tokio::test]
    async fn stale_runtime_teardown_cannot_end_a_newer_capture_generation() {
        let audio = Arc::new(FakeAudio::default());
        let h = harness(DeviceCapabilities {
            audio: Some(audio.clone()),
            ..DeviceCapabilities::default()
        })
        .await;
        declare_and_grant(&h, AppCapability::Microphone);

        start_runtime(&h).await;
        let (old_generation, _) = h
            .broker
            .runtime_identity(&h.app_id)
            .await
            .expect("runtime lookup")
            .expect("old runtime is active");
        let (ok, _, error, code) = execute(&h, BridgeOperation::RecordAudioStart, json!({})).await;
        assert!(ok, "{error:?} {code:?}");
        h.broker
            .manage_runtime_value(json!({"app_id": h.app_id, "action": "stop"}))
            .await
            .expect("old runtime stops");
        assert!(audio.active.lock().unwrap().is_empty());

        start_runtime(&h).await;
        let (new_generation, _) = h
            .broker
            .runtime_identity(&h.app_id)
            .await
            .expect("runtime lookup")
            .expect("new runtime is active");
        assert_ne!(old_generation, new_generation);
        let (ok, _, error, code) = execute(&h, BridgeOperation::RecordAudioStart, json!({})).await;
        assert!(ok, "{error:?} {code:?}");

        h.broker
            .force_stop_recording(&h.app_id, old_generation)
            .await;
        assert!(
            audio
                .active
                .lock()
                .unwrap()
                .contains_key(&AudioOwner::LocalApp {
                    app_id: h.app_id.clone(),
                    runtime_generation: new_generation,
                }),
            "an old generation's teardown must target only its owner"
        );
        assert!(matches!(
            h.broker.recording.lock().await.as_ref(),
            Some(active) if active.runtime_generation == new_generation
        ));

        h.broker
            .manage_runtime_value(json!({"app_id": h.app_id, "action": "stop"}))
            .await
            .expect("new runtime stops");
    }

    // ---- location / notifications -----------------------------------------

    /// The audio path that actually exists on this stack: listen, transcribe,
    /// hand back text the app can send to the model.
    #[tokio::test]
    async fn transcribe_speech_returns_text_under_the_microphone_capability() {
        let audio = Arc::new(FakeAudio::default());
        let h = harness(DeviceCapabilities {
            audio: Some(audio.clone()),
            ..DeviceCapabilities::default()
        })
        .await;
        declare_and_grant(&h, AppCapability::Microphone);
        start_runtime(&h).await;

        let (ok, result, error, code) = execute(
            &h,
            BridgeOperation::TranscribeSpeech,
            json!({"language": "zh-CN"}),
        )
        .await;
        assert!(ok, "{error:?} {code:?}");
        assert_eq!(result["text"], "明天下午三点开会");
        assert_eq!(result["language"], "zh-CN");
        assert!(matches!(
            audio.operations.lock().unwrap().last().map(|(_, operation)| operation),
            Some(AudioOperation::Listen { language: Some(language) }) if language == "zh-CN"
        ));
        assert!(matches!(
            &audio.operations.lock().unwrap().last().expect("listen operation").0.owner,
            AudioOwner::LocalApp { app_id, runtime_generation } if app_id == &h.app_id && *runtime_generation > 0
        ));
    }

    #[tokio::test]
    async fn runtime_teardown_during_audio_authorization_prevents_late_native_admission() {
        let audio = Arc::new(FakeAudio::default());
        let h = harness(DeviceCapabilities {
            audio: Some(audio.clone()),
            ..DeviceCapabilities::default()
        })
        .await;
        declare(&h, AppCapability::Microphone);
        declare(&h, AppCapability::TextToSpeech);
        start_runtime(&h).await;

        let operations = [
            (
                BridgeOperation::RecordAudioStart,
                json!({}),
                "recordAudioStart",
            ),
            (
                BridgeOperation::TranscribeSpeech,
                json!({ "language": "en-US" }),
                "transcribeSpeech",
            ),
            (
                BridgeOperation::SynthesizeSpeech,
                json!({ "text": "late approval" }),
                "synthesizeSpeech",
            ),
        ];

        for (index, (operation, payload, label)) in operations.into_iter().enumerate() {
            let previous_requests = h
                .sink
                .events()
                .await
                .into_iter()
                .filter_map(|event| match event {
                    ClientEvent::AppEvent {
                        event: AppEventDto::AppCapabilityRequested { request },
                    } => Some(request.request_id),
                    _ => None,
                })
                .collect::<HashSet<_>>();
            let request_id = format!("audio-auth-race-{index}");
            let pending = {
                let broker = h.broker.clone();
                let app_id = h.app_id.clone();
                let payload_json = Some(payload.to_string());
                let bridge_request_id = request_id.clone();
                tokio::spawn(async move {
                    broker
                        .execute_bridge(BridgeRequest {
                            request_id: bridge_request_id,
                            app_id,
                            operation,
                            payload_json,
                        })
                        .await;
                })
            };

            let capability_request_id = timeout(Duration::from_secs(2), async {
                loop {
                    for event in h.sink.events().await {
                        if let ClientEvent::AppEvent {
                            event: AppEventDto::AppCapabilityRequested { request },
                        } = event
                        {
                            if request.app_id == h.app_id
                                && !previous_requests.contains(&request.request_id)
                            {
                                return request.request_id;
                            }
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("{label} must wait for capability approval"));

            let (old_generation, _) = h
                .broker
                .runtime_identity(&h.app_id)
                .await
                .expect("runtime lookup")
                .expect("runtime is active");
            h.broker
                .manage_runtime_value(json!({"app_id": h.app_id, "action": "stop"}))
                .await
                .expect("runtime stops during authorization");
            start_runtime(&h).await;
            let (new_generation, _) = h
                .broker
                .runtime_identity(&h.app_id)
                .await
                .expect("runtime lookup")
                .expect("replacement runtime is active");
            assert_ne!(old_generation, new_generation);

            assert!(
                h.broker
                    .resolve_capability(&capability_request_id, AuthorizationDecision::AllowOnce,)
                    .await
            );
            timeout(Duration::from_secs(2), pending)
                .await
                .expect("late authorization completes")
                .expect("bridge task completes");

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
                .expect("bridge response");
            assert!(!response.ok, "a stopped runtime cannot use late approval");
            assert_eq!(response.error_code.as_deref(), Some("cancelled"));
        }

        assert!(
            audio
                .operations
                .lock()
                .unwrap()
                .iter()
                .all(|(_, operation)| {
                    !matches!(
                        operation,
                        AudioOperation::StartRecording { .. }
                            | AudioOperation::Listen { .. }
                            | AudioOperation::Synthesize { .. }
                    )
                }),
            "no capture, listen, or synth request enters the service after teardown"
        );
        h.broker
            .manage_runtime_value(json!({"app_id": h.app_id, "action": "stop"}))
            .await
            .expect("replacement runtime stops");
    }

    #[tokio::test]
    async fn transcribe_speech_is_refused_when_the_microphone_is_undeclared() {
        let h = harness(DeviceCapabilities {
            audio: Some(Arc::new(FakeAudio::default())),
            ..DeviceCapabilities::default()
        })
        .await;
        start_runtime(&h).await;

        let (ok, _, _, code) = execute(&h, BridgeOperation::TranscribeSpeech, json!({})).await;
        assert!(!ok);
        assert_eq!(code.as_deref(), Some("capability_not_declared"));
    }

    /// A capture is addressable right after it is taken, and dies with the
    /// page that took it.
    #[tokio::test]
    async fn a_capture_publishes_a_media_handle_that_a_runtime_stop_clears() {
        let camera = FakeCamera::with_bytes(vec![4, 5, 6]);
        let h = harness(DeviceCapabilities {
            camera: Some(camera),
            ..DeviceCapabilities::default()
        })
        .await;
        declare_and_grant(&h, AppCapability::Camera);

        let (ok, result, _, _) = execute(&h, BridgeOperation::CapturePhoto, json!({})).await;
        assert!(ok);
        let media_id = result["mediaId"].as_str().expect("mediaId").to_string();
        let entry = h
            .broker
            .media_entry(&h.app_id, &media_id)
            .expect("the capture is addressable");
        assert_eq!(*entry.bytes, vec![4, 5, 6]);
        assert_eq!(entry.media_type, "image/jpeg");

        h.broker
            .manage_runtime_value(json!({"app_id": h.app_id, "action": "start"}))
            .await
            .expect("start");
        h.broker
            .manage_runtime_value(json!({"app_id": h.app_id, "action": "stop"}))
            .await
            .expect("stop");
        assert!(
            h.broker.media_entry(&h.app_id, &media_id).is_none(),
            "handles are a live page's hand-off buffer, not storage"
        );
    }

    /// "Allow for this session" must not outlive the session. The grant
    /// lives only in memory, so a stale one behaves as "always allow" while
    /// staying invisible to permissions.json and unrevokable short of a full
    /// reset — the opposite of what the user was asked.
    #[tokio::test]
    async fn a_session_grant_does_not_survive_the_runtime_it_was_given_in() {
        let camera = FakeCamera::with_bytes(vec![1]);
        let h = harness(DeviceCapabilities {
            camera: Some(camera),
            ..DeviceCapabilities::default()
        })
        .await;
        declare(&h, AppCapability::Camera);
        h.broker
            .session_permissions
            .lock()
            .await
            .grant(&h.app_id, AppCapability::Camera);

        // The grant is live: no prompt, straight through.
        let (ok, _, error, code) = timeout(
            Duration::from_secs(2),
            execute(&h, BridgeOperation::CapturePhoto, json!({})),
        )
        .await
        .expect("a session grant answers without a prompt");
        assert!(ok, "{error:?} {code:?}");

        h.broker
            .manage_runtime_value(json!({"app_id": h.app_id, "action": "start"}))
            .await
            .expect("start");
        h.broker
            .manage_runtime_value(json!({"app_id": h.app_id, "action": "stop"}))
            .await
            .expect("stop");

        assert!(
            !h.broker
                .session_permissions
                .lock()
                .await
                .allows(&h.app_id, AppCapability::Camera),
            "the session grant must lapse with the runtime it was given in"
        );
    }

    #[tokio::test]
    async fn get_location_returns_the_fix() {
        let h = harness(DeviceCapabilities {
            location: Some(Arc::new(FakeLocation {
                hang: false,
                error: None,
            })),
            ..DeviceCapabilities::default()
        })
        .await;
        declare_and_grant(&h, AppCapability::Location);

        let (ok, result, error, code) = execute(&h, BridgeOperation::GetLocation, json!({})).await;
        assert!(ok, "{error:?} {code:?}");
        assert_eq!(result["latitude"], 31.2304);
        assert_eq!(result["longitude"], 121.4737);
        assert_eq!(result["accuracyM"], 65.0);
        assert_eq!(result["timestampMs"], 1_753_000_000_000u64);
    }

    #[tokio::test(start_paused = true)]
    async fn get_location_times_out_typed() {
        let h = harness(DeviceCapabilities {
            location: Some(Arc::new(FakeLocation {
                hang: true,
                error: None,
            })),
            ..DeviceCapabilities::default()
        })
        .await;
        declare_and_grant(&h, AppCapability::Location);

        let (ok, _, _, code) = execute(&h, BridgeOperation::GetLocation, json!({})).await;
        assert!(!ok);
        assert_eq!(code.as_deref(), Some("timeout"));
    }

    #[tokio::test]
    async fn post_notification_prefixes_the_identifier_per_app() {
        let notifications = Arc::new(FakeNotifications::default());
        let h = harness(DeviceCapabilities {
            notifications: Some(notifications.clone()),
            ..DeviceCapabilities::default()
        })
        .await;
        declare_and_grant(&h, AppCapability::Notifications);

        let (ok, result, error, code) = execute(
            &h,
            BridgeOperation::PostNotification,
            json!({"title": "提醒", "body": "该喝水了", "tag": "hydrate"}),
        )
        .await;
        assert!(ok, "{error:?} {code:?}");
        let expected_tag = format!("local-app.{}.hydrate", h.app_id);
        {
            let requests = notifications.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].title, "提醒");
            assert_eq!(requests[0].body, "该喝水了");
            assert_eq!(
                requests[0].tag.as_deref(),
                Some(expected_tag.as_str()),
                "the app-scoped prefix must be applied BEFORE the request reaches \
                 the native layer, so no app can replace another app's (or the \
                 assistant's) notification"
            );
        }

        // The page gets ITS OWN tag back, never the composed identifier: a
        // `tag` exists to be passed back so a later post REPLACES this one,
        // and the composed form contains `.`, which this operation's own
        // grammar rejects. Echoing it would hand the page a value that fails
        // validation on the very next call.
        assert_eq!(result["tag"], "hydrate");
        let (ok, second, error, code) = execute(
            &h,
            BridgeOperation::PostNotification,
            json!({
                "title": "提醒",
                "body": "还是该喝水了",
                "tag": result["tag"].as_str().expect("tag"),
            }),
        )
        .await;
        assert!(
            ok,
            "the returned tag must be re-postable: {error:?} {code:?}"
        );
        assert_eq!(second["tag"], "hydrate");
        let requests = notifications.requests.lock().unwrap();
        assert_eq!(
            requests[1].tag.as_deref(),
            Some(expected_tag.as_str()),
            "a replace must resolve to the SAME native identifier as the first post"
        );
    }

    #[tokio::test]
    async fn post_notification_rejects_an_illegal_tag_or_oversized_title() {
        let notifications = Arc::new(FakeNotifications::default());
        let h = harness(DeviceCapabilities {
            notifications: Some(notifications.clone()),
            ..DeviceCapabilities::default()
        })
        .await;
        declare_and_grant(&h, AppCapability::Notifications);

        let (ok, _, _, code) = execute(
            &h,
            BridgeOperation::PostNotification,
            json!({"title": "t", "body": "b", "tag": "../escape"}),
        )
        .await;
        assert!(!ok);
        assert_eq!(code.as_deref(), Some("invalid_request"));

        let (ok, _, _, code) = execute(
            &h,
            BridgeOperation::PostNotification,
            json!({"title": "字".repeat(101), "body": "b"}),
        )
        .await;
        assert!(!ok);
        assert_eq!(code.as_deref(), Some("invalid_request"));
        assert!(notifications.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn clipboard_share_and_tts_use_declared_native_capabilities() {
        let clipboard = Arc::new(FakeClipboard::default());
        let share = Arc::new(FakeShare::default());
        let audio = Arc::new(FakeAudio::default());
        let h = harness(DeviceCapabilities {
            clipboard: Some(clipboard.clone()),
            share: Some(share.clone()),
            audio: Some(audio.clone()),
            ..DeviceCapabilities::default()
        })
        .await;
        declare_and_grant(&h, AppCapability::Clipboard);
        declare_and_grant(&h, AppCapability::Share);
        declare_and_grant(&h, AppCapability::TextToSpeech);
        start_runtime(&h).await;

        let (ok, result, error, code) = execute(
            &h,
            BridgeOperation::ClipboardSetText,
            json!({"text": "copied"}),
        )
        .await;
        assert!(ok, "{error:?} {code:?}");
        assert_eq!(result["written"], true);
        assert_eq!(clipboard.text.lock().unwrap().as_deref(), Some("copied"));

        let (ok, result, error, code) =
            execute(&h, BridgeOperation::ClipboardGetText, json!({})).await;
        assert!(ok, "{error:?} {code:?}");
        assert_eq!(result["text"], "copied");

        let (ok, result, error, code) = execute(
            &h,
            BridgeOperation::Share,
            json!({"text": "share me", "url": "https://example.com"}),
        )
        .await;
        assert!(ok, "{error:?} {code:?}");
        assert_eq!(result["shared"], true);
        let payloads = share.payloads.lock().unwrap();
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].text.as_deref(), Some("share me"));
        assert_eq!(payloads[0].url.as_deref(), Some("https://example.com"));
        drop(payloads);

        let (expected_generation, _) = h
            .broker
            .runtime_identity(&h.app_id)
            .await
            .expect("runtime lookup")
            .expect("runtime is active");
        let (ok, result, error, code) = execute(
            &h,
            BridgeOperation::SynthesizeSpeech,
            json!({
                "text": "speech",
                "voice": "default",
                "language": "en-US",
                "rate": 1.25,
                "requestId": "attacker-request",
                "runtimeGeneration": 999_999,
            }),
        )
        .await;
        assert!(ok, "{error:?} {code:?}");
        assert_eq!(result["mimeType"], "audio/pcm");
        assert_eq!(result["sampleRateHz"], 24_000);
        let encoded = result["base64"].as_str().expect("audio base64");
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .unwrap(),
            vec![0, 0, 1, 0]
        );
        let (audio_context, audio_operation) = audio
            .operations
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("synthesis request reaches AudioService");
        assert!(matches!(
            audio_operation,
            AudioOperation::Synthesize {
                language: Some(language),
                rate: Some(rate),
                voice: Some(voice),
                ..
            } if language == "en-US" && rate == 1.25 && voice == "default"
        ));
        assert!(matches!(
            audio_context.owner,
            AudioOwner::LocalApp { ref app_id, runtime_generation } if app_id == &h.app_id && runtime_generation == expected_generation
        ));
        assert_eq!(
            audio_context
                .initiator
                .and_then(|initiator| initiator.request_id),
            Some("req-1".into()),
            "the trusted bridge request id reaches the device operation"
        );
    }

    #[tokio::test]
    async fn late_synthesis_from_a_stopped_runtime_is_not_published() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let entered = Arc::new(tokio::sync::Notify::new());
        let audio = Arc::new(FakeAudio {
            synthesize_gate: Some(gate.clone()),
            synthesize_entered: Some(entered.clone()),
            ..FakeAudio::default()
        });
        let h = harness(DeviceCapabilities {
            audio: Some(audio.clone()),
            ..DeviceCapabilities::default()
        })
        .await;
        declare_and_grant(&h, AppCapability::TextToSpeech);
        start_runtime(&h).await;
        let (runtime_generation, _) = h
            .broker
            .runtime_identity(&h.app_id)
            .await
            .expect("runtime lookup")
            .expect("runtime is active");

        let request = BridgeRequest {
            request_id: "synth-late".into(),
            app_id: h.app_id.clone(),
            operation: BridgeOperation::SynthesizeSpeech,
            payload_json: Some(r#"{"text":"hello"}"#.into()),
        };
        let pending = tokio::spawn({
            let broker = h.broker.clone();
            async move { broker.execute_bridge(request).await }
        });
        timeout(Duration::from_secs(1), entered.notified())
            .await
            .expect("synthesis reaches the device AudioService");

        h.broker
            .manage_runtime_value(json!({"app_id": h.app_id, "action": "stop"}))
            .await
            .expect("runtime stops while synthesis is pending");
        gate.notify_one();
        pending.await.expect("bridge task completes");

        let response = h
            .sink
            .events()
            .await
            .into_iter()
            .rev()
            .find_map(|event| match event {
                ClientEvent::AppEvent {
                    event: AppEventDto::AppBridgeResponse { response },
                } if response.request_id == "synth-late" => Some(response),
                _ => None,
            })
            .expect("synthesis response");
        assert!(!response.ok);
        assert_eq!(response.error_code.as_deref(), Some("cancelled"));
        let (context, operation) = audio
            .operations
            .lock()
            .unwrap()
            .iter()
            .find(|(_, operation)| matches!(operation, AudioOperation::Synthesize { .. }))
            .cloned()
            .expect("synthesis operation");
        assert!(matches!(operation, AudioOperation::Synthesize { .. }));
        assert!(matches!(
            context.owner,
            AudioOwner::LocalApp { ref app_id, runtime_generation: generation }
                if app_id == &h.app_id && generation == runtime_generation
        ));
        assert_eq!(
            context.initiator.and_then(|initiator| initiator.request_id),
            Some("synth-late".into())
        );
    }
}
