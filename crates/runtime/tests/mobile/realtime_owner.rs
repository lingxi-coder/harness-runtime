//! Native sessions must occupy the real mobile turn and permission owner.
use async_trait::async_trait;
use client::adapter::PermissionRequestSink;
use client::protocol::{
    commands::ClientCommand, events::ClientEvent, permission::PermissionRequest,
};
use futures_util::StreamExt;
use harness_runtime::mobile::test_support::{
    new_engine_with_streaming, test_config, FakeListener, HostFakePlatform,
};
use lingxi_core::host::{PermissionDecision, PermissionGate};
use llm_runtime::services::sdk::realtime::*;
use std::{sync::Arc, time::Duration};
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;
#[derive(Default)]
struct Requests(Mutex<Vec<PermissionRequest>>);
#[async_trait]
impl PermissionRequestSink for Requests {
    async fn emit_request(&self, request: PermissionRequest) {
        self.0.lock().await.push(request);
    }
}
struct Codec;
impl RealtimeCodec for Codec {
    fn capabilities(&self) -> RealtimeCapabilities {
        RealtimeCapabilities {
            history_import: true,
            tools: true,
            input_transcription: true,
            output_transcription: true,
            interruption: true,
            ..Default::default()
        }
    }
    fn encode(&self, _: &RealtimeInput) -> Result<Vec<RealtimeFrame>, RealtimeError> {
        Ok(vec![])
    }
    fn decode(&self, _: RealtimeFrame) -> Result<Vec<RealtimeEvent>, RealtimeError> {
        Ok(vec![])
    }
}
struct Sink;
#[async_trait]
impl RealtimeSink for Sink {
    async fn send(&mut self, _: RealtimeFrame) -> Result<(), RealtimeError> {
        Ok(())
    }
    async fn ping(&mut self, _: bytes::Bytes) -> Result<(), RealtimeError> {
        Err(RealtimeError::InvalidInput {
            message: "fixture transport does not support Ping".into(),
        })
    }
    fn abort(&mut self) {
        // No network state to release; the driver drops both fixture halves.
    }
    async fn close(&mut self, _: RealtimeClose) -> Result<(), RealtimeError> {
        Ok(())
    }
}
struct Transport;
#[async_trait]
impl RealtimeTransport for Transport {
    async fn connect(
        &self,
        _: RealtimeConnectRequest,
    ) -> Result<RealtimeConnection, RealtimeError> {
        Ok(RealtimeConnection {
            outbound: Box::new(Sink),
            inbound: futures_util::stream::pending().boxed(),
        })
    }
}
#[test]
fn native_audio_uses_mobile_listener_owner_and_drains_permissions_before_release() {
    let dir = tempfile::tempdir().unwrap();
    let listener = Arc::new(FakeListener::default());
    let requests = Arc::new(Requests::default());
    let storage = Arc::new(lingxi_core::host::InMemorySecureStorage::new());
    let engine = new_engine_with_streaming(
        test_config(dir.path()),
        Arc::new(HostFakePlatform::new(dir.path().into()).with_secure_storage(storage)),
        listener.clone(),
        requests.clone(),
        None,
    )
    .unwrap();
    engine.runtime().block_on(async {
        let snapshot = engine
            .provider_account_command(
                serde_json::json!({
                    "action":"add", "expectedRevision":0, "providerId":"openai",
                    "displayName":"Realtime fixture", "executionBindingId":"openai",
                    "authKind":"api_key"
                })
                .to_string(),
                Some("realtime-fixture-key".into()),
            )
            .await
            .expect("create a current audio account");
        let snapshot: lingxi_core::host::provider_accounts::AccountSnapshot =
            serde_json::from_str(&snapshot).unwrap();
        let account = snapshot.accounts.first().expect("created account");
        let mut prepared = engine
            .audio_orchestrator()
            .prepare_realtime_agent()
            .await
            .unwrap();
        let expected_session = prepared.session_id.clone();
        let expected_profile = prepared.profile_name.clone().unwrap();
        engine
            .submit(ClientCommand::GetAudioSessionContext { request_id: "mobile-context-1".into() })
            .await
            .unwrap();
        assert!(listener.received.lock().await.iter().any(|event| {
            matches!(event, ClientEvent::AudioSessionContext {
                request_id, session_id: Some(session_id), profile_id: Some(profile_id), account_scope: Some(account_scope), media_owner: None,
            } if request_id == "mobile-context-1"
                && session_id == &expected_session
                && profile_id == &expected_profile
                && account_scope == lingxi_core::host::provider_accounts::UNBOUND_ACCOUNT_SCOPE)
        }));
        // Explicit audio may use another logical provider, while the
        // conversation remains bound to its captured session profile.
        assert_ne!(expected_profile, "openai");
        prepared.audio_profile_name = "openai".into();
        prepared.profile_name = Some(format!("account.{}", account.account_id));
        prepared.account_scope = Some(
            serde_json::to_string(&llm_runtime::accounts::AccountResourceScope {
                store_scope: account.credential_ref.store_scope.clone(),
                account_id: account.account_id.clone(),
                identity_generation: account.identity_generation,
            })
            .unwrap(),
        );
        // A concrete scope for another account must not borrow this route.
        let mut foreign = prepared.clone();
        foreign.account_scope = Some(
            serde_json::to_string(&llm_runtime::accounts::AccountResourceScope {
                store_scope: account.credential_ref.store_scope.clone(),
                account_id: uuid::Uuid::new_v4().to_string(),
                identity_generation: account.identity_generation,
            })
            .unwrap(),
        );
        let (foreign_session, foreign_driver) = RealtimeSession::connect(
            &Transport,
            RealtimeConnectRequest {
                endpoint: "wss://fixture".into(),
                headers: vec![],
                max_frame_bytes: 1024,
            },
            Arc::new(Codec),
            RealtimeLimits::default(),
        )
        .await
        .unwrap();
        let foreign_driver = tokio::spawn(foreign_driver.run());
        let (foreign_control, foreign_events) = foreign_session.into_parts();
        let (_foreign_input, foreign_inputs) = mpsc::channel(1);
        let (foreign_output, _foreign_outputs) = mpsc::channel(1);
        let error = engine
            .audio_orchestrator()
            .run_realtime_agent(
                foreign,
                foreign_control,
                foreign_events,
                foreign_inputs,
                foreign_output,
                Default::default(),
                false,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("provider profile changed"));
        let _ = tokio::time::timeout(Duration::from_secs(2), foreign_driver).await;
        let (session, driver) = RealtimeSession::connect(
            &Transport,
            RealtimeConnectRequest {
                endpoint: "wss://fixture".into(),
                headers: vec![],
                max_frame_bytes: 1024,
            },
            Arc::new(Codec),
            RealtimeLimits::default(),
        )
        .await
        .unwrap();
        let driver = tokio::spawn(driver.run());
        let (control, events) = session.into_parts();
        let (_inputs, input_rx) = mpsc::channel(8);
        let (output, _output_rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let run = tokio::spawn({
            let engine = engine.clone();
            let cancel = cancel.clone();
            async move {
                engine
                    .run_realtime_agent_owned(
                        prepared,
                        control,
                        events,
                        input_rx,
                        output,
                        Default::default(),
                        cancel,
                    )
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if listener
                    .received
                    .lock()
                    .await
                    .iter()
                    .any(|event| matches!(event, ClientEvent::TurnStarted { .. }))
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(engine
            .submit(ClientCommand::NewSession {
                mode: client::protocol::listings::SessionModeDto::Code,
                cwd: None,
                model: None
            })
            .await
            .is_err());
        let permission = tokio::spawn({
            let gate = engine.permission_gate();
            async move {
                let input = lingxi_core::types::utf16_json::Utf16JsonProjection::parse("{}")
                    .expect("fixture input is valid source JSON");
                gate.check("FixtureProtected", &input).await
            }
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if !requests.0.lock().await.is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let request = requests.0.lock().await[0].clone();
        assert_eq!(
            request.owner.unwrap().session_id.as_deref(),
            Some(expected_session.as_str())
        );
        assert!(engine
            .permission_gate()
            .request_owner_id(request.request_id)
            .await
            .is_some());
        cancel.cancel();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), permission)
                .await
                .unwrap()
                .unwrap(),
            PermissionDecision::Deny { .. }
        ));
        assert!(tokio::time::timeout(Duration::from_secs(2), run)
            .await
            .unwrap()
            .unwrap()
            .is_ok());
        assert_eq!(engine.permission_gate().pending_count().await, 0);
        assert!(listener
            .received
            .lock()
            .await
            .iter()
            .any(|event| matches!(event, ClientEvent::TurnEnded { .. })));
        engine
            .submit(ClientCommand::NewSession {
                mode: client::protocol::listings::SessionModeDto::Code,
                cwd: None,
                model: None,
            })
            .await
            .unwrap();
        driver.await.unwrap().unwrap();
    });
}
