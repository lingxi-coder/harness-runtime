//! Bidirectional byte transport established before desktop assembly.
//!
//! SDK MCP construction and permission callbacks use this same response pump,
//! queue lifecycle, and orphan sink after assembly. Moving the prepared fields
//! into the turn driver transfers ownership; no second reader/resolver is started.
use super::control_plane::{OrphanedPermission, StdioControlPlane};
use super::io::{Input, Output};
use super::queued_commands::QueueLifecycle;
use super::stream_json::StreamJsonStream;
use super::stream_json_input::{
    spawn_stdin_router, InputError, PendingInputQueue, StdinChannels, StdinControlFrame,
    StdinReaderControl, StdinReaderStatus, StreamInput,
};
use std::sync::Arc;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// The pre-build transport resources handed to the running session unchanged.
///
/// On startup failure, the owner calls `shutdown` before returning. On success,
/// the reader and auxiliary handles move into the existing print lifecycle.
pub struct PreparedStdio {
    pub input_rx: mpsc::Receiver<StreamInput>,
    pub input_pending: PendingInputQueue,
    pub control_req_rx: mpsc::UnboundedReceiver<StdinControlFrame>,
    pub reader_status: watch::Receiver<StdinReaderStatus>,
    pub reader: StdinReaderControl,
    pub queue_lifecycle: Arc<QueueLifecycle>,
    pub orphan_rx: mpsc::UnboundedReceiver<OrphanedPermission>,
    pub auxiliary_tasks: Vec<JoinHandle<()>>,
    pub stop: CancellationToken,
    pub plane: Arc<StdioControlPlane>,
}

impl PreparedStdio {
    pub async fn start(
        input: Input,
        stderr: Output,
        replay_user_messages: bool,
        session_id: String,
        stream: Arc<StreamJsonStream>,
        plane: Arc<StdioControlPlane>,
        startup_cancel: CancellationToken,
    ) -> Self {
        let session_handle = stream.session_id_handle();
        *session_handle.lock().await = session_id.clone();
        plane.set_session_id(session_handle);
        stream.ensure_drain_started().await;
        let queue_lifecycle = Arc::new(QueueLifecycle::new(
            stream.outbound_tx(),
            session_id.clone(),
        ));
        let channels = spawn_stdin_router(
            input,
            stderr,
            replay_user_messages,
            session_id,
            stream.outbound_tx(),
            queue_lifecycle.clone(),
        );
        Self::from_channels(channels, queue_lifecycle, plane, startup_cancel).await
    }

    /// Adopt injected router channels in tests or when the host prepared them.
    /// `queue_lifecycle` must be the registry supplied to that exact router.
    pub async fn from_channels(
        channels: StdinChannels,
        queue_lifecycle: Arc<QueueLifecycle>,
        plane: Arc<StdioControlPlane>,
        startup_cancel: CancellationToken,
    ) -> Self {
        let StdinChannels {
            input_rx,
            input_pending,
            control_req_rx,
            mut control_resp_rx,
            status: reader_status,
            reader,
        } = channels;
        let stop = startup_cancel.child_token();
        let (orphan_tx, orphan_rx) = mpsc::unbounded_channel();
        // Establish the recovery sink before the first response is resolved.
        plane.set_orphan_sender(orphan_tx).await;

        let resolver_plane = plane.clone();
        let resolver_stop = stop.clone();
        let resolver = tokio::spawn(async move {
            loop {
                let response = tokio::select! {
                    biased;
                    _ = resolver_stop.cancelled() => break,
                    response = control_resp_rx.recv() => match response {
                        Some(response) => response,
                        None => break,
                    },
                };
                resolver_plane.resolve_response(&response).await;
            }
            resolver_plane
                .close_input("Tool permission stream closed before response received")
                .await;
        });

        let fatal_plane = plane.clone();
        let fatal_stop = stop.clone();
        let mut fatal_status = reader_status.clone();
        let fatal = tokio::spawn(async move {
            loop {
                let status = fatal_status.borrow_and_update().clone();
                match status {
                    StdinReaderStatus::Failed(error) => {
                        fatal_plane.shutdown(&error.to_string()).await;
                        startup_cancel.cancel();
                        return;
                    }
                    StdinReaderStatus::Eof | StdinReaderStatus::Stopped => return,
                    StdinReaderStatus::Reading => {}
                }
                tokio::select! {
                    biased;
                    _ = fatal_stop.cancelled() => return,
                    changed = fatal_status.changed() => if changed.is_err() { return; },
                }
            }
        });
        Self {
            input_rx,
            input_pending,
            control_req_rx,
            reader_status,
            reader,
            queue_lifecycle,
            orphan_rx,
            auxiliary_tasks: vec![resolver, fatal],
            stop,
            plane,
        }
    }

    /// Startup failure cleanup; running sessions transfer these handles to
    /// their existing joined lifecycle instead of starting a new scope.
    pub async fn shutdown(&mut self, reason: &str) -> Result<(), InputError> {
        self.reader.stop();
        self.plane.shutdown(reason).await;
        self.stop.cancel();
        let mut outcome = self.reader.join().await;
        // Await stored handles by reference so a cancelled cleanup future can
        // be resumed without losing ownership of a still-running helper.
        while let Some(task) = self.auxiliary_tasks.last_mut() {
            if task.await.is_err() && outcome.is_ok() {
                outcome = Err(InputError::ReadFailed);
            }
            self.auxiliary_tasks.pop();
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::types::utf16_json::Utf16JsonProjection;
    use serde_json::json;
    use tokio::io::AsyncWriteExt;

    async fn prepare() -> (
        PreparedStdio,
        tokio::io::DuplexStream,
        Arc<StreamJsonStream>,
        CancellationToken,
    ) {
        let (peer, input) = tokio::io::duplex(8192);
        let stream = Arc::new(StreamJsonStream::new_placeholder(Output::new(
            tokio::io::sink(),
        )));
        let plane = StdioControlPlane::new(stream.outbound_tx());
        let cancel = CancellationToken::new();
        let prepared = PreparedStdio::start(
            Box::pin(input),
            Output::new(tokio::io::sink()),
            false,
            "injected-session".into(),
            stream.clone(),
            plane,
            cancel.clone(),
        )
        .await;
        (prepared, peer, stream, cancel)
    }

    #[tokio::test]
    async fn response_pump_resolves_sdk_request_before_desktop_exists() {
        let (mut prepared, mut peer, stream, _cancel) = prepare().await;
        let (request_id, response) = prepared.plane.send_request(Utf16JsonProjection::plain(json!({"subtype":"mcp_message","server_name":"injected-sdk","message":{"jsonrpc":"2.0","id":1,"method":"initialize"}})), None).await;
        let line = json!({"type":"control_response","response":{"subtype":"success","request_id":request_id,"response":{"message":{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05"}}}}}).to_string();
        peer.write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap();
        let resolved = tokio::time::timeout(std::time::Duration::from_secs(1), response)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            resolved.value["message"]["result"]["protocolVersion"],
            "2024-11-05"
        );
        prepared.shutdown("startup test complete").await.unwrap();
        assert!(prepared.auxiliary_tasks.is_empty());
        stream.finish().await.unwrap();
    }

    #[tokio::test]
    async fn early_initialize_and_user_input_remain_ordered_and_projected() {
        let (mut prepared, mut peer, stream, _cancel) = prepare().await;
        peer.write_all(br#"{"type":"control_request","request_id":"init","request":{"subtype":"initialize","sdkMcpServers":{"server":{"\ud800":"\udfff"}}}}
{"type":"user","message":{"role":"user","content":"queued before build"}}
"#).await.unwrap();
        let frame = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            prepared.control_req_rx.recv(),
        )
        .await
        .unwrap()
        .unwrap();
        let StdinControlFrame::Request(frame) = frame else {
            panic!("initialize")
        };
        assert_eq!(
            frame
                .subprojection("/request/sdkMcpServers/server")
                .unwrap()
                .to_json_string()
                .unwrap(),
            r#"{"\ud800":"\udfff"}"#
        );
        let input = prepared.input_rx.recv().await.unwrap();
        assert!(matches!(input, StreamInput::User(turn) if turn.content == "queued before build"));
        prepared.shutdown("startup test complete").await.unwrap();
        stream.finish().await.unwrap();
    }

    #[tokio::test]
    async fn orphan_sink_is_live_before_the_first_response() {
        let (mut prepared, mut peer, stream, _cancel) = prepare().await;
        peer.write_all(br#"{"type":"control_response","response":{"subtype":"success","request_id":"old-permission","response":{"behavior":"allow","toolUseID":"toolu_old","updatedInput":{"\ud800":"\udfff"}}}}
"#).await.unwrap();
        let orphan =
            tokio::time::timeout(std::time::Duration::from_secs(1), prepared.orphan_rx.recv())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(orphan.tool_use_id.as_str(), "toolu_old");
        assert_eq!(
            orphan
                .permission_decision
                .subprojection("/updatedInput")
                .unwrap()
                .to_json_string()
                .unwrap(),
            r#"{"\ud800":"\udfff"}"#
        );
        prepared.shutdown("startup test complete").await.unwrap();
        stream.finish().await.unwrap();
    }

    #[tokio::test]
    async fn fatal_input_cancels_startup_and_rejects_pending_sdk_build_request() {
        let (mut prepared, mut peer, stream, cancel) = prepare().await;
        let (_, pending) = prepared
            .plane
            .send_request(
                Utf16JsonProjection::plain(json!({"subtype":"mcp_message"})),
                None,
            )
            .await;
        peer.write_all(b"{malformed}\n").await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), cancel.cancelled())
            .await
            .unwrap();
        assert_eq!(
            *prepared.reader_status.borrow(),
            StdinReaderStatus::Failed(InputError::MalformedJson)
        );
        assert!(pending.await.unwrap().is_err());
        prepared.shutdown("startup input failed").await.unwrap();
        assert!(prepared.auxiliary_tasks.is_empty());
        stream.finish().await.unwrap();
    }

    #[tokio::test]
    async fn startup_failure_joins_reader_and_pump_with_open_peer() {
        let (mut prepared, _open_peer, stream, _cancel) = prepare().await;
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            prepared.shutdown("desktop build failed"),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(*prepared.reader_status.borrow(), StdinReaderStatus::Stopped);
        assert!(prepared.auxiliary_tasks.is_empty());
        stream.finish().await.unwrap();
    }
}
