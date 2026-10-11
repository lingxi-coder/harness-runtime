use super::*;
use bytes::Bytes;
use futures_util::{stream, StreamExt};
use lingxi_core::host::{TtsAudio, VoiceRecording};
use lingxi_llm_client as sdk;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Local {
    kinds: Vec<AudioOperationKind>,
    readiness: AudioReadinessState,
    calls: AtomicUsize,
    cancel: AtomicUsize,
    pending: bool,
    operations: Mutex<Vec<AudioOperationKind>>,
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
    async fn cancel(&self, _: AudioOperationId) -> Result<(), AudioError> {
        self.cancel.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
fn context(owner: &str) -> AudioOperationContext {
    AudioOperationContext {
        identity: AudioOperationId::new(1, 7),
        owner: AudioOwner::Session {
            session_id: owner.into(),
        },
        initiator: None,
        timeout_budget_ms: Some(1000),
        max_payload_bytes: 1024,
    }
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
    let end_id = end_context.identity.clone();
    let ending = tokio::spawn({
        let service = service.clone();
        async move { service.execute(end_context, AudioOperation::EndOwner).await }
    });
    assert_eq!(
        task_a.await.unwrap().unwrap_err().kind,
        AudioErrorKind::Cancelled
    );
    assert!(!task_b.is_finished());
    service.cancel(end_id).await.unwrap();
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
        Ok(HostedCloudRoute {
            snapshot: self.snapshot.clone(),
            profile_name,
            account_scope: "account-a".into(),
        })
    }
    async fn request_options(
        &self,
        route: &HostedCloudRoute,
        _: &AudioOperationContext,
    ) -> Result<sdk::RequestOptions, AudioError> {
        assert_eq!(route.account_scope, "account-a");
        Ok(sdk::RequestOptions {
            credential: Some(sdk::protocol::Secret::new("host-key".into())),
            ..Default::default()
        })
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
    }
}
#[tokio::test]
async fn cloud_exact_profile_audio_model_and_per_kind_override_execute_sdk_once() {
    let (host, transport) = host(vec![1, 2]);
    let device = Local::new(vec![AudioOperationKind::Play]);
    let mut settings = cloud_settings();
    settings.operations.insert(
        AudioOperationKind::Speak,
        AudioRouteMode::Cloud {
            binding: CloudBinding::ExplicitProfile {
                profile_name: "override-profile".into(),
            },
            model: Some("tts-1".into()),
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
#[async_trait]
impl CloudAudioHost for SwitchingHost {
    fn resolve(
        &self,
        owner: &AudioOwner,
        binding: &CloudBinding,
    ) -> Result<HostedCloudRoute, AudioError> {
        self.0.resolve(owner, binding)
    }
    async fn request_options(
        &self,
        route: &HostedCloudRoute,
        context: &AudioOperationContext,
    ) -> Result<sdk::RequestOptions, AudioError> {
        // Session selection changes while asynchronous credentials are acquired.
        *self.0.profile.lock().unwrap() = "override-profile".into();
        tokio::task::yield_now().await;
        self.0.request_options(route, context).await
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
