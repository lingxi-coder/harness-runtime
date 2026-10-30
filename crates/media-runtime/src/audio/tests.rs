use super::*;
use bytes::Bytes;
use futures_util::{stream, StreamExt};
use lingxi_llm_client as sdk;
use media_api::tts::TtsAudio;
use media_api::voice::VoiceRecording;
use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

struct Local {
    kinds: Vec<AudioOperationKind>,
    readiness: AudioReadinessState,
    calls: AtomicUsize,
    cancel: AtomicUsize,
    pending: bool,
    operations: Mutex<Vec<AudioOperationKind>>,
    requests: Mutex<Vec<AudioOperation>>,
}

struct EpochAudio {
    epoch: AtomicU64,
    entered: AtomicUsize,
    pending: AtomicBool,
    contexts: Mutex<Vec<AudioOperationContext>>,
    cancellations: Mutex<Vec<AudioOperationId>>,
}

impl EpochAudio {
    fn new(epoch: u64) -> Arc<Self> {
        Arc::new(Self {
            epoch: AtomicU64::new(epoch),
            entered: AtomicUsize::new(0),
            pending: AtomicBool::new(false),
            contexts: Mutex::new(Vec::new()),
            cancellations: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl AudioService for EpochAudio {
    fn capabilities(&self) -> AudioCapabilitySnapshot {
        let supported_operations = vec![AudioOperationKind::Record, AudioOperationKind::Speak];
        AudioCapabilitySnapshot {
            service_epoch: self.epoch.load(Ordering::SeqCst),
            support_revision: 1,
            readiness: supported_operations
                .iter()
                .map(|operation| AudioOperationReadiness {
                    operation: *operation,
                    state: AudioReadinessState::Ready,
                })
                .collect(),
            supported_operations,
            max_payload_bytes: 1024,
        }
    }

    async fn execute(
        &self,
        context: AudioOperationContext,
        operation: AudioOperation,
    ) -> Result<AudioOperationSuccess, AudioError> {
        self.contexts.lock().unwrap().push(context);
        self.entered.fetch_add(1, Ordering::SeqCst);
        if self.pending.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        match operation {
            AudioOperation::StartRecording { .. } => Ok(AudioOperationSuccess::RecordingStarted {
                handle: media_api::RecordingRef(media_api::RecordingId::from("recording-1")),
            }),
            AudioOperation::Speak { .. } => {
                Ok(AudioOperationSuccess::PlaybackCompleted { duration_ms: 1 })
            }
            _ => Err(AudioError::new(
                AudioErrorKind::Unsupported,
                "the epoch test backend supports recording and speech only",
            )),
        }
    }

    async fn cancel(&self, context: AudioOperationContext) -> Result<(), AudioError> {
        self.cancellations.lock().unwrap().push(context.identity);
        Ok(())
    }
}
impl Local {
    fn new(kinds: Vec<AudioOperationKind>) -> Arc<Self> {
        Arc::new(Self {
            kinds,
            readiness: AudioReadinessState::Ready,
            calls: AtomicUsize::new(0),
            cancel: AtomicUsize::new(0),
            pending: false,
            operations: Mutex::new(vec![]),
            requests: Mutex::new(vec![]),
        })
    }
}
#[async_trait]
impl AudioService for Local {
    fn capabilities(&self) -> AudioCapabilitySnapshot {
        AudioCapabilitySnapshot {
            service_epoch: 7,
            support_revision: 1,
            supported_operations: self.kinds.clone(),
            readiness: self
                .kinds
                .iter()
                .map(|operation| AudioOperationReadiness {
                    operation: *operation,
                    state: self.readiness,
                })
                .collect(),
            max_payload_bytes: 1024,
        }
    }
    async fn execute(
        &self,
        _: AudioOperationContext,
        operation: AudioOperation,
    ) -> Result<AudioOperationSuccess, AudioError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.requests.lock().unwrap().push(operation.clone());
        if let Some(kind) = operation.kind() {
            self.operations.lock().unwrap().push(kind);
        }
        if self.pending {
            std::future::pending::<()>().await;
        }
        Ok(match operation {
            AudioOperation::Capture { .. } => AudioOperationSuccess::Recording {
                recording: VoiceRecording {
                    audio_bytes: vec![1, 2],
                    mime_type: "audio/mp4".into(),
                },
            },
            AudioOperation::Play { audio } => {
                assert_eq!(audio.pcm, vec![1, 2]);
                AudioOperationSuccess::PlaybackCompleted { duration_ms: 3 }
            }
            AudioOperation::EndOwner => AudioOperationSuccess::OwnerEnded,
            _ => AudioOperationSuccess::Synthesized {
                audio: TtsAudio {
                    pcm: vec![1, 2],
                    sample_rate_hz: 24_000,
                },
            },
        })
    }
    async fn execute_host_configured_local(
        &self,
        context: AudioOperationContext,
        operation: AudioOperation,
    ) -> Result<AudioOperationSuccess, AudioError> {
        self.execute(context, operation).await
    }
    async fn cancel(&self, _: AudioOperationContext) -> Result<(), AudioError> {
        self.cancel.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
#[tokio::test]
async fn host_configured_local_uses_one_native_route_and_never_selects_cloud_or_offline_twice() {
    let device = Local::new(vec![AudioOperationKind::Synthesize]);
    let offline = Local::new(vec![AudioOperationKind::Synthesize]);
    let service = UnifiedAudioService::new(
        device.clone(),
        vec![OfflineAudioBackend {
            id: "installed-model".into(),
            service: offline.clone(),
        }],
        None,
        AudioRoutingSettings {
            operations: [(
                AudioOperationKind::Synthesize,
                AudioRouteMode::HostConfigured,
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        },
    );

    service.execute(context("a"), synthesis()).await.unwrap();
    assert_eq!(device.calls.load(Ordering::SeqCst), 1);
    assert_eq!(offline.calls.load(Ordering::SeqCst), 0);
}
fn context(owner: &str) -> AudioOperationContext {
    AudioOperationContext::new(
        AudioOperationId::new(1, 7),
        AudioOwner::Session {
            session_id: owner.into(),
        },
    )
    .with_timeout_budget_ms(1000)
    .with_max_payload_bytes(1024)
}

fn logical_context(owner: &str, service_epoch: u64) -> AudioOperationContext {
    let mut context = context(owner);
    context.identity.service_epoch = service_epoch;
    context
}

fn speak() -> AudioOperation {
    AudioOperation::Speak {
        text: "hello".into(),
        language: None,
        rate: None,
        voice: None,
    }
}

#[tokio::test]
async fn logical_audio_epoch_maps_to_the_current_native_epoch_for_execute_and_cancel() {
    let native = EpochAudio::new(77);
    let service = Arc::new(UnifiedAudioService::new_with_service_epoch(
        native.clone(),
        vec![],
        None,
        AudioRoutingSettings::default(),
        1,
    ));
    let owner = "logical-epoch-owner";
    assert_eq!(service.capabilities().service_epoch, 1);

    let record_context = logical_context(owner, 1);
    let record_identity = record_context.identity.clone();
    assert!(matches!(
        service
            .execute(
                record_context,
                AudioOperation::StartRecording {
                    sample_rate_hz: 24_000,
                    format: "audio/wav".into(),
                },
            )
            .await
            .unwrap(),
        AudioOperationSuccess::RecordingStarted { .. }
    ));
    let native_record_context = native.contexts.lock().unwrap()[0].clone();
    assert_eq!(native_record_context.identity.id, record_identity.id);
    assert_eq!(
        native_record_context.identity.generation,
        record_identity.generation
    );
    assert_eq!(native_record_context.identity.service_epoch, 77);

    let speak_context = logical_context(owner, 1);
    let speak_identity = speak_context.identity.clone();
    assert!(matches!(
        service.execute(speak_context, speak()).await.unwrap(),
        AudioOperationSuccess::PlaybackCompleted { .. }
    ));
    let native_speak_context = native.contexts.lock().unwrap()[1].clone();
    assert_eq!(native_speak_context.identity.id, speak_identity.id);
    assert_eq!(
        native_speak_context.identity.generation,
        speak_identity.generation
    );
    assert_eq!(native_speak_context.identity.service_epoch, 77);

    native.pending.store(true, Ordering::SeqCst);
    let pending_context = logical_context(owner, 1);
    let public_identity = pending_context.identity.clone();
    let cancel_context = pending_context.clone();
    let pending = tokio::spawn({
        let service = service.clone();
        async move { service.execute(pending_context, speak()).await }
    });
    while native.entered.load(Ordering::SeqCst) < 3 {
        tokio::task::yield_now().await;
    }
    service.cancel(cancel_context).await.unwrap();
    assert_eq!(
        pending.await.unwrap().unwrap_err().kind,
        AudioErrorKind::Cancelled
    );
    let native_cancel_identity = AudioOperationId {
        service_epoch: 77,
        ..public_identity.clone()
    };
    assert!(native
        .cancellations
        .lock()
        .unwrap()
        .contains(&native_cancel_identity));
    assert!(!native
        .cancellations
        .lock()
        .unwrap()
        .contains(&public_identity));

    native.pending.store(false, Ordering::SeqCst);
    native.epoch.store(78, Ordering::SeqCst);
    let next_context = logical_context(owner, 1);
    assert!(matches!(
        service.execute(next_context, speak()).await.unwrap(),
        AudioOperationSuccess::PlaybackCompleted { .. }
    ));
    assert_eq!(
        native.contexts.lock().unwrap()[3].identity.service_epoch,
        78
    );
    assert_eq!(
        service
            .capabilities_for(&AudioOwner::Session {
                session_id: owner.into()
            })
            .service_epoch,
        1
    );

    let before_stale = native.entered.load(Ordering::SeqCst);
    let stale_context = logical_context(owner, 77);
    assert_eq!(
        service
            .execute(stale_context, speak())
            .await
            .unwrap_err()
            .kind,
        AudioErrorKind::InvalidRequest
    );
    assert_eq!(native.entered.load(Ordering::SeqCst), before_stale);
}

fn synthesis() -> AudioOperation {
    AudioOperation::Synthesize {
        text: "hello".into(),
        language: None,
        rate: None,
        voice: None,
    }
}
#[tokio::test]
async fn automatic_uses_system_then_installed_offline_and_never_cloud() {
    let device = Local::new(vec![]);
    let offline = Local::new(vec![AudioOperationKind::Synthesize]);
    let service = UnifiedAudioService::new(
        device.clone(),
        vec![OfflineAudioBackend {
            id: "installed".into(),
            service: offline.clone(),
        }],
        None,
        AudioRoutingSettings::default(),
    );
    service.execute(context("a"), synthesis()).await.unwrap();
    assert_eq!(device.calls.load(Ordering::SeqCst), 0);
    assert_eq!(offline.calls.load(Ordering::SeqCst), 1);
    let unsupported = service
        .execute(context("a"), AudioOperation::Listen { language: None })
        .await
        .unwrap_err();
    assert_eq!(unsupported.kind, AudioErrorKind::Unsupported);
    assert_eq!(device.calls.load(Ordering::SeqCst), 0);
    let system = Local::new(vec![AudioOperationKind::Synthesize]);
    let service = UnifiedAudioService::new(
        system.clone(),
        vec![OfflineAudioBackend {
            id: "installed".into(),
            service: offline.clone(),
        }],
        None,
        AudioRoutingSettings::default(),
    );
    service.execute(context("a"), synthesis()).await.unwrap();
    assert_eq!(system.calls.load(Ordering::SeqCst), 1);
    assert_eq!(offline.calls.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn unavailable_system_is_not_silently_rerouted_and_zero_budget_never_dispatches() {
    let device = Arc::new(Local {
        readiness: AudioReadinessState::Unavailable,
        ..Local::new(vec![AudioOperationKind::Synthesize])
            .as_ref()
            .clone_for_test()
    });
    let offline = Local::new(vec![AudioOperationKind::Synthesize]);
    let service = UnifiedAudioService::new(
        device.clone(),
        vec![OfflineAudioBackend {
            id: "installed".into(),
            service: offline.clone(),
        }],
        None,
        AudioRoutingSettings {
            operations: [(AudioOperationKind::Synthesize, AudioRouteMode::System)]
                .into_iter()
                .collect(),
            ..Default::default()
        },
    );
    assert_eq!(
        service
            .execute(context("a"), synthesis())
            .await
            .unwrap_err()
            .kind,
        AudioErrorKind::Unavailable
    );
    assert_eq!(device.calls.load(Ordering::SeqCst), 0);
    assert_eq!(offline.calls.load(Ordering::SeqCst), 0);
    let mut ctx = context("a");
    ctx.timeout_budget_ms = Some(0);
    assert_eq!(
        service.execute(ctx, synthesis()).await.unwrap_err().kind,
        AudioErrorKind::Timeout
    );
}
impl Local {
    fn clone_for_test(&self) -> Self {
        Self {
            kinds: self.kinds.clone(),
            readiness: self.readiness,
            calls: AtomicUsize::new(0),
            cancel: AtomicUsize::new(0),
            pending: self.pending,
            operations: Mutex::new(vec![]),
            requests: Mutex::new(vec![]),
        }
    }
}
#[tokio::test]
async fn cancellation_deadline_and_owner_end_target_only_matching_active_calls() {
    let device = Arc::new(Local {
        pending: true,
        ..Local::new(vec![AudioOperationKind::Synthesize])
            .as_ref()
            .clone_for_test()
    });
    let service = Arc::new(UnifiedAudioService::new(
        device.clone(),
        vec![],
        None,
        AudioRoutingSettings::default(),
    ));
    let a = context("a");
    let b = context("b");
    let task_a = tokio::spawn({
        let service = service.clone();
        async move { service.execute(a, synthesis()).await }
    });
    let task_b = tokio::spawn({
        let service = service.clone();
        async move { service.execute(b, synthesis()).await }
    });
    while device.calls.load(Ordering::SeqCst) < 2 {
        tokio::task::yield_now().await;
    }
    // EndOwner cancels only owner a. Native lifecycle pending here is cancelled
    // explicitly to keep the fixture's behavior deterministic.
    let end_context = context("a");
    let cancel_end_context = end_context.clone();
    let ending = tokio::spawn({
        let service = service.clone();
        async move { service.execute(end_context, AudioOperation::EndOwner).await }
    });
    assert_eq!(
        task_a.await.unwrap().unwrap_err().kind,
        AudioErrorKind::Cancelled
    );
    assert!(!task_b.is_finished());
    service.cancel(cancel_end_context).await.unwrap();
    assert_eq!(
        ending.await.unwrap().unwrap_err().kind,
        AudioErrorKind::Cancelled
    );
    task_b.abort();
    let _ = task_b.await;
    let mut deadline = context("a");
    deadline.timeout_budget_ms = Some(1);
    assert_eq!(
        service
            .execute(deadline, synthesis())
            .await
            .unwrap_err()
            .kind,
        AudioErrorKind::Timeout
    );
    tokio::task::yield_now().await;
    assert!(device.cancel.load(Ordering::SeqCst) >= 3);
}

struct Transport {
    calls: AtomicUsize,
    urls: Mutex<Vec<String>>,
    response: Vec<u8>,
}
impl Transport {
    fn response(&self, url: String) -> sdk::StreamResponse {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.urls.lock().unwrap().push(url);
        let body = self.response.clone();
        sdk::StreamResponse {
            status: 200,
            headers: vec![],
            body: stream::once(async move { Ok(Bytes::from(body)) }).boxed(),
        }
    }
}
#[async_trait]
impl sdk::Transport for Transport {
    async fn send(
        &self,
        r: sdk::HttpRequest,
    ) -> Result<sdk::StreamResponse, sdk::protocol::LlmError> {
        Ok(self.response(r.url))
    }
    async fn send_stream(
        &self,
        r: sdk::HttpStreamRequest,
    ) -> Result<sdk::StreamResponse, sdk::protocol::LlmError> {
        Ok(self.response(r.url))
    }
}
struct Host {
    snapshot: sdk::ClientSnapshot,
    profile: Mutex<String>,
    resolutions: AtomicUsize,
    usage: AtomicUsize,
}
struct TestCredentials {
    profile_name: String,
}
#[async_trait]
impl CloudAudioCredentialSource for TestCredentials {
    async fn request_options(
        &self,
        _: &AudioOperationContext,
    ) -> Result<sdk::RequestOptions, AudioError> {
        Ok(sdk::RequestOptions {
            credential: Some(sdk::protocol::Secret::new(format!(
                "host-key-{}",
                self.profile_name
            ))),
            ..Default::default()
        })
    }
}
#[async_trait]
impl CloudAudioHost for Host {
    fn resolve(
        &self,
        owner: &AudioOwner,
        binding: &CloudBinding,
    ) -> Result<HostedCloudRoute, AudioError> {
        self.resolutions.fetch_add(1, Ordering::SeqCst);
        let profile_name = match binding {
            CloudBinding::FollowSession => {
                if !matches!(owner, AudioOwner::Session { .. }) {
                    return Err(AudioError::new(
                        AudioErrorKind::Unavailable,
                        "no trusted session",
                    ));
                }
                self.profile.lock().unwrap().clone()
            }
            CloudBinding::ExplicitProfile { profile_name } => profile_name.clone(),
        };
        let credentials = Arc::new(TestCredentials {
            profile_name: profile_name.clone(),
        });
        Ok(HostedCloudRoute::new(
            self.snapshot.clone(),
            profile_name.clone(),
            "account-a".into(),
            format!("host:{profile_name}:account-a:v1"),
            credentials,
        ))
    }
    fn catalog_supported_operations(&self) -> Vec<AudioOperationKind> {
        vec![
            AudioOperationKind::Listen,
            AudioOperationKind::Transcribe,
            AudioOperationKind::Synthesize,
            AudioOperationKind::Speak,
        ]
    }
    fn record_usage(&self, _: &AudioOperationContext, _: &sdk::audio::AudioUsage) {
        self.usage.fetch_add(1, Ordering::SeqCst);
    }
}
fn host(response: Vec<u8>) -> (Arc<Host>, Arc<Transport>) {
    let transport = Arc::new(Transport {
        calls: AtomicUsize::new(0),
        urls: Mutex::new(vec![]),
        response,
    });
    let profiles=["session-profile","override-profile"].iter().map(|name|serde_json::from_value::<sdk::protocol::ProviderProfile>(json!({"provider_id":"openai","profile_name":name,"base_url":"https://chat.example/v1","protocol":"open_ai_responses","auth":"none","models":[],"audio":{"mode":"enabled","value":{"transcriptions_endpoint":format!("https://{name}.example/v1/audio/transcriptions"),"translations_endpoint":format!("https://{name}.example/v1/audio/translations"),"speech_endpoint":format!("https://{name}.example/v1/audio/speech"),"auth":{"type":"bearer"}}}})).unwrap()).collect::<Vec<_>>();
    let client = sdk::LlmClientBuilder::with_transport(transport.clone(), &profiles)
        .with_region(sdk::protocol::Region::International)
        .build()
        .unwrap();
    (
        Arc::new(Host {
            snapshot: client.snapshot(),
            profile: Mutex::new("session-profile".into()),
            resolutions: AtomicUsize::new(0),
            usage: AtomicUsize::new(0),
        }),
        transport,
    )
}
fn cloud_settings() -> AudioRoutingSettings {
    AudioRoutingSettings {
        operations: [
            AudioOperationKind::Listen,
            AudioOperationKind::Synthesize,
            AudioOperationKind::Speak,
        ]
        .into_iter()
        .map(|kind| {
            (
                kind,
                AudioRouteMode::Cloud {
                    binding: CloudBinding::FollowSession,
                    model: None,
                },
            )
        })
        .collect(),
        ..Default::default()
    }
}
#[tokio::test]
async fn cloud_exact_profile_audio_model_and_per_kind_override_execute_sdk_once() {
    let (host, transport) = host(vec![1, 2]);
    let device = Local::new(vec![AudioOperationKind::Play]);
    let mut settings = cloud_settings();
    settings.defaults.insert(
        AudioOperationKind::Synthesize,
        AudioOperationDefaults {
            voice: Some("alloy".into()),
            voice_scope: Some(AudioVoiceScope {
                profile_name: Some("session-profile".into()),
                model_id: Some("tts-1".into()),
            }),
            ..Default::default()
        },
    );
    settings.operations.insert(
        AudioOperationKind::Synthesize,
        AudioRouteMode::Cloud {
            binding: CloudBinding::FollowSession,
            model: Some("tts-1".into()),
        },
    );
    settings.operations.insert(
        AudioOperationKind::Speak,
        AudioRouteMode::Cloud {
            binding: CloudBinding::ExplicitProfile {
                profile_name: "override-profile".into(),
            },
            model: Some("tts-1".into()),
        },
    );
    settings.defaults.insert(
        AudioOperationKind::Speak,
        AudioOperationDefaults {
            voice: Some("alloy".into()),
            voice_scope: Some(AudioVoiceScope {
                profile_name: Some("override-profile".into()),
                model_id: Some("tts-1".into()),
            }),
            ..Default::default()
        },
    );
    let service = UnifiedAudioService::new(device.clone(), vec![], Some(host.clone()), settings);
    service
        .execute(context("session"), synthesis())
        .await
        .unwrap();
    service
        .execute(
            context("session"),
            AudioOperation::Speak {
                text: "hello".into(),
                language: None,
                rate: None,
                voice: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        *transport.urls.lock().unwrap(),
        vec![
            "https://session-profile.example/v1/audio/speech",
            "https://override-profile.example/v1/audio/speech"
        ]
    );
    assert_eq!(transport.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        device.operations.lock().unwrap().as_slice(),
        &[AudioOperationKind::Play]
    );
    assert_eq!(host.usage.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn saved_cloud_voice_scope_is_checked_against_the_resolved_session_and_effective_model() {
    let (host, transport) = host(vec![1, 2]);
    let mut settings = cloud_settings();
    settings.defaults.insert(
        AudioOperationKind::Synthesize,
        AudioOperationDefaults {
            voice: Some("alloy".into()),
            voice_scope: Some(AudioVoiceScope {
                profile_name: Some("wrong-profile".into()),
                model_id: Some("tts-1".into()),
            }),
            ..Default::default()
        },
    );
    let service = UnifiedAudioService::new(
        Local::new(vec![AudioOperationKind::Play]),
        vec![],
        Some(host.clone()),
        settings,
    );

    let error = service
        .execute(context("session"), synthesis())
        .await
        .unwrap_err();
    assert_eq!(error.kind, AudioErrorKind::InvalidRequest);
    assert_eq!(host.resolutions.load(Ordering::SeqCst), 1);
    assert_eq!(transport.calls.load(Ordering::SeqCst), 0);

    let mut settings = cloud_settings();
    settings.defaults.insert(
        AudioOperationKind::Synthesize,
        AudioOperationDefaults {
            voice: Some("alloy".into()),
            voice_scope: Some(AudioVoiceScope {
                profile_name: Some("session-profile".into()),
                model_id: Some("tts-1".into()),
            }),
            ..Default::default()
        },
    );
    let default_model_service = UnifiedAudioService::new(
        Local::new(vec![AudioOperationKind::Play]),
        vec![],
        Some(host.clone()),
        settings,
    );
    let error = default_model_service
        .execute(context("session"), synthesis())
        .await
        .unwrap_err();
    assert_eq!(error.kind, AudioErrorKind::InvalidRequest);
    assert_eq!(host.resolutions.load(Ordering::SeqCst), 2);
    assert_eq!(transport.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cloud_unsupported_model_or_capture_never_requests_credentials_or_dispatches() {
    let (host, transport) = host(vec![1, 2]);
    let device = Local::new(vec![]);
    let mut settings = cloud_settings();
    settings.operations.insert(
        AudioOperationKind::Synthesize,
        AudioRouteMode::Cloud {
            binding: CloudBinding::FollowSession,
            model: Some("chat-model-must-not-be-used".into()),
        },
    );
    let service = UnifiedAudioService::new(device, vec![], Some(host), settings);
    assert_eq!(
        service
            .execute(context("a"), synthesis())
            .await
            .unwrap_err()
            .kind,
        AudioErrorKind::Unsupported
    );
    assert_eq!(
        service
            .execute(context("a"), AudioOperation::Listen { language: None })
            .await
            .unwrap_err()
            .kind,
        AudioErrorKind::Unsupported
    );
    assert_eq!(transport.calls.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn cloud_capture_transcribes_bounded_native_media_and_reports_usage() {
    let (host, transport) = host(br#"{"text":"recognized"}"#.to_vec());
    let device = Local::new(vec![AudioOperationKind::Capture]);
    let service =
        UnifiedAudioService::new(device.clone(), vec![], Some(host.clone()), cloud_settings());
    let result = service
        .execute(context("a"), AudioOperation::Listen { language: None })
        .await
        .unwrap();
    assert!(
        matches!(result,AudioOperationSuccess::Transcript{transcript} if transcript.text=="recognized")
    );
    assert_eq!(device.calls.load(Ordering::SeqCst), 1);
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
    assert_eq!(host.usage.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn cloud_streams_are_bounded_and_do_not_fallback() {
    let (host, transport) = host(vec![1; 2048]);
    let service =
        UnifiedAudioService::new(Local::new(vec![]), vec![], Some(host), cloud_settings());
    assert_eq!(
        service
            .execute(context("a"), synthesis())
            .await
            .unwrap_err()
            .kind,
        AudioErrorKind::MediaTooLarge
    );
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn automatic_unavailable_system_selects_ready_offline_before_dispatch() {
    let device = Arc::new(Local {
        readiness: AudioReadinessState::Unavailable,
        ..Local::new(vec![AudioOperationKind::Synthesize])
            .as_ref()
            .clone_for_test()
    });
    let offline = Local::new(vec![AudioOperationKind::Synthesize]);
    let service = UnifiedAudioService::new(
        device.clone(),
        vec![OfflineAudioBackend {
            id: "installed".into(),
            service: offline.clone(),
        }],
        None,
        AudioRoutingSettings::default(),
    );
    service.execute(context("a"), synthesis()).await.unwrap();
    assert_eq!(device.calls.load(Ordering::SeqCst), 0);
    assert_eq!(offline.calls.load(Ordering::SeqCst), 1);
}

struct SwitchingHost(Arc<Host>);
struct SwitchingCredentials {
    host: Arc<Host>,
    pinned: HostedCloudRoute,
}
#[async_trait]
impl CloudAudioCredentialSource for SwitchingCredentials {
    async fn request_options(
        &self,
        context: &AudioOperationContext,
    ) -> Result<sdk::RequestOptions, AudioError> {
        // The registry changes after resolve; the route-local source still uses its captured entry.
        *self.host.profile.lock().unwrap() = "override-profile".into();
        tokio::task::yield_now().await;
        self.pinned.request_options(context).await
    }
}
#[async_trait]
impl CloudAudioHost for SwitchingHost {
    fn resolve(
        &self,
        owner: &AudioOwner,
        binding: &CloudBinding,
    ) -> Result<HostedCloudRoute, AudioError> {
        let route = self.0.resolve(owner, binding)?;
        Ok(route.clone().with_credential_source(
            format!("switching:{}", route.credential_source_id),
            Arc::new(SwitchingCredentials {
                host: self.0.clone(),
                pinned: route,
            }),
        ))
    }
    fn catalog_supported_operations(&self) -> Vec<AudioOperationKind> {
        self.0.catalog_supported_operations()
    }
}
#[tokio::test]
async fn an_operation_pins_route_before_credentials_and_next_call_follows_new_session_profile() {
    let (host, transport) = host(vec![1, 2]);
    let service = UnifiedAudioService::new(
        Local::new(vec![]),
        vec![],
        Some(Arc::new(SwitchingHost(host))),
        cloud_settings(),
    );
    service.execute(context("a"), synthesis()).await.unwrap();
    service.execute(context("a"), synthesis()).await.unwrap();
    assert_eq!(
        *transport.urls.lock().unwrap(),
        vec![
            "https://session-profile.example/v1/audio/speech",
            "https://override-profile.example/v1/audio/speech"
        ]
    );
}

#[tokio::test]
async fn global_audio_capabilities_union_configured_cloud_routes_without_faking_an_owner() {
    let (host, transport) = host(vec![1, 2]);
    let service =
        UnifiedAudioService::new(Local::new(vec![]), vec![], Some(host), cloud_settings());
    let catalog = service.capabilities();
    assert!(catalog
        .supported_operations
        .contains(&AudioOperationKind::Synthesize));
    assert!(catalog.readiness.is_empty());

    let unbound = AudioOwner::System {
        instance_id: "ui".into(),
    };
    let owner_caps = service.capabilities_for(&unbound);
    assert!(owner_caps
        .supported_operations
        .contains(&AudioOperationKind::Synthesize));
    assert_eq!(
        owner_caps
            .readiness
            .iter()
            .find(|entry| entry.operation == AudioOperationKind::Synthesize)
            .unwrap()
            .state,
        AudioReadinessState::Unavailable,
    );
    let error = service
        .execute(
            AudioOperationContext::new(AudioOperationId::new(1, 7), unbound)
                .with_timeout_budget_ms(1_000)
                .with_max_payload_bytes(1024),
            synthesis(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, AudioErrorKind::Unavailable);
    assert_eq!(transport.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn session_bound_projection_rejects_another_resource_owner_before_dispatch() {
    let device = Local::new(vec![AudioOperationKind::Synthesize]);
    let service = Arc::new(UnifiedAudioService::new(
        device.clone(),
        vec![],
        None,
        AudioRoutingSettings::default(),
    ));
    let bound = service.for_owner(AudioOwner::Session {
        session_id: "a".into(),
    });
    assert_eq!(
        bound
            .execute(context("b"), synthesis())
            .await
            .unwrap_err()
            .kind,
        AudioErrorKind::InvalidRequest
    );
    assert_eq!(device.calls.load(Ordering::SeqCst), 0);
    bound.execute(context("a"), synthesis()).await.unwrap();
    assert_eq!(device.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn owner_audio_cancel_created_before_reuse_cannot_retarget_same_owner_identity() {
    let device = EpochAudio::new(7);
    let service = Arc::new(UnifiedAudioService::new(
        device.clone(),
        vec![],
        None,
        AudioRoutingSettings::default(),
    ));
    let owner = AudioOwner::Session {
        session_id: "same-owner".into(),
    };
    let bound = service.for_owner(owner.clone());
    let first_context = context("same-owner");
    let identity = first_context.identity.clone();
    bound.execute(first_context.clone(), speak()).await.unwrap();

    let stale_cancel = bound.cancel(first_context);
    let mut reused_context = context("same-owner");
    reused_context.identity = identity;
    device.pending.store(true, Ordering::SeqCst);
    let second_service = bound.clone();
    let second_context = reused_context.clone();
    let second = tokio::spawn(async move { second_service.execute(second_context, speak()).await });
    while device.entered.load(Ordering::SeqCst) < 2 {
        tokio::task::yield_now().await;
    }

    stale_cancel.await.unwrap();
    assert!(!second.is_finished());
    assert!(device.cancellations.lock().unwrap().is_empty());

    bound.cancel(reused_context).await.unwrap();
    assert_eq!(
        second.await.unwrap().unwrap_err().kind,
        AudioErrorKind::Cancelled
    );
    assert_eq!(device.cancellations.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn trusted_settings_preserve_speech_defaults_without_overwriting_explicit_arguments() {
    let device = Local::new(vec![AudioOperationKind::Synthesize]);
    let settings = AudioRoutingSettings {
        defaults: [(
            AudioOperationKind::Synthesize,
            AudioOperationDefaults {
                language: Some("zh-CN".into()),
                rate: Some(1.0),
                voice: Some("configured-voice".into()),
                voice_scope: None,
            },
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let service = UnifiedAudioService::new(device.clone(), vec![], None, settings.clone());
    service.execute(context("a"), synthesis()).await.unwrap();
    service
        .execute(
            context("a"),
            AudioOperation::Synthesize {
                text: "hello".into(),
                language: Some("en-US".into()),
                rate: Some(1.25),
                voice: Some("requested-voice".into()),
            },
        )
        .await
        .unwrap();
    let mut updated = settings;
    updated
        .defaults
        .get_mut(&AudioOperationKind::Synthesize)
        .unwrap()
        .voice = Some("next-voice".into());
    service.update_settings(updated);
    service.execute(context("a"), synthesis()).await.unwrap();
    let requests = device.requests.lock().unwrap();
    assert!(
        matches!(&requests[0], AudioOperation::Synthesize { language: Some(language), rate: Some(1.0), voice: Some(voice), .. }
        if language == "zh-CN" && voice == "configured-voice")
    );
    assert!(
        matches!(&requests[1], AudioOperation::Synthesize { language: Some(language), rate: Some(1.25), voice: Some(voice), .. }
        if language == "en-US" && voice == "requested-voice")
    );
    assert!(
        matches!(&requests[2], AudioOperation::Synthesize { voice: Some(voice), .. } if voice == "next-voice")
    );
}

#[tokio::test]
async fn invalid_selected_configuration_never_falls_through_to_another_route() {
    let device = Local::new(vec![AudioOperationKind::Synthesize]);
    let service = UnifiedAudioService::new(
        device.clone(),
        vec![],
        None,
        AudioRoutingSettings {
            operations: [(
                AudioOperationKind::Synthesize,
                AudioRouteMode::Invalid {
                    reason: "invalid audio binding".into(),
                },
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        },
    );
    assert!(service
        .capabilities()
        .supported_operations
        .contains(&AudioOperationKind::Synthesize));
    let error = service
        .execute(context("a"), synthesis())
        .await
        .unwrap_err();
    assert_eq!(error.kind, AudioErrorKind::InvalidRequest);
    assert_eq!(device.calls.load(Ordering::SeqCst), 0);
}
