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
    let engine = new_engine_with_streaming(
        test_config(dir.path()),
        Arc::new(HostFakePlatform::new(dir.path().into())),
        listener.clone(),
        requests.clone(),
        None,
    )
    .unwrap();
    engine.runtime().block_on(async {
        let prepared = engine
            .audio_orchestrator()
            .prepare_realtime_agent()
            .await
            .unwrap();
        let expected_session = prepared.session_id.clone();
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
                cwd: None,
                model: None
            })
            .await
            .is_err());
        let permission = tokio::spawn({
            let gate = engine.permission_gate();
            async move { gate.check("FixtureProtected", &serde_json::json!({})).await }
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
                cwd: None,
                model: None,
            })
            .await
            .unwrap();
        driver.await.unwrap().unwrap();
    });
}
