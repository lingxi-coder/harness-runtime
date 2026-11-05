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

/// Whether the running session has attempted to bind and flush its queue journal.
pub(super) enum FailedStartupJournal {
    /// Assembly finished, but the journal has not yet been bound.
    Unbound(Option<Arc<session::jsonl::JsonlWriter>>),
    /// Binding or flushing already failed; do not repeat an uncertain write.
    AlreadyFailed,
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

    async fn stop_and_join(&mut self, reason: &str) -> Result<(), InputError> {
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

    fn discard_queued(&self) -> Result<(), InputError> {
        let mut outcome = Ok(());
        for uuid in self.queue_lifecycle.queued.drain_for_discard() {
            let delivery = self
                .queue_lifecycle
                .emit(&uuid, super::queued_commands::LIFECYCLE_DISCARDED);
            outcome = outcome.and(delivery);
        }
        outcome
    }

    /// Startup failure cleanup before a session writer exists.
    ///
    /// Stop intake before retiring every accepted UUID. There is no session
    /// writer to bind here; the caller finishes the outbound writer afterwards.
    pub async fn shutdown(&mut self, reason: &str) -> Result<(), InputError> {
        let outcome = self.stop_and_join(reason).await;
        let discarded = self.discard_queued();
        outcome.and(discarded)
    }

    /// Clean up a failed stream startup after the session has been assembled.
    ///
    /// Preserve accepted enqueue records before reporting discarded UUIDs. If
    /// the initial journal write already failed, retain its original error and
    /// buffered operations without silently retrying an uncertain write.
    pub(super) async fn shutdown_after_assembly_failure(
        &mut self,
        reason: &str,
        journal: FailedStartupJournal,
    ) -> Vec<String> {
        let mut errors = Vec::new();
        if let Err(error) = self.stop_and_join(reason).await {
            errors.push(error.to_string());
        }
        if let FailedStartupJournal::Unbound(writer) = journal {
            match self.queue_lifecycle.bind_journal(writer) {
                Ok(()) => {
                    if let Err(error) = self.queue_lifecycle.flush_journal().await {
                        errors.push(format!("Failed to flush queued input journal: {error}"));
                    }
                }
                Err(error) => errors.push(format!("Failed to bind queued input journal: {error}")),
            }
        }
        if let Err(error) = self.discard_queued() {
            errors.push(error.to_string());
        }
        errors
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::headless::stream_json::OutboundMsg;
    use crate::headless::stream_json_input::spawn_stdin_router_from_reader;
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
        let request = Utf16JsonProjection::from_serde_value(&json!({"subtype":"mcp_message","server_name":"injected-sdk","message":{"jsonrpc":"2.0","id":1,"method":"initialize"}})).unwrap();
        let (request_id, response) = prepared.plane.send_request(request, None).await;
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
            resolved.string_at("/message/result/protocolVersion"),
            Some("2024-11-05")
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
                Utf16JsonProjection::from_serde_value(&json!({"subtype":"mcp_message"})).unwrap(),
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

    const FIRST_UUID: &str = "00000000-0000-4000-8000-000000000101";
    const SECOND_UUID: &str = "00000000-0000-4000-8000-000000000102";

    async fn next_lifecycle(rx: &mut mpsc::UnboundedReceiver<OutboundMsg>) -> (String, String) {
        let message = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("lifecycle frame is delivered")
            .expect("lifecycle channel remains open");
        let OutboundMsg::Line(line) = message else {
            panic!("expected a lifecycle line");
        };
        let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(frame["type"], "command_lifecycle");
        (
            frame["command_uuid"].as_str().unwrap().to_owned(),
            frame["state"].as_str().unwrap().to_owned(),
        )
    }

    async fn prepare_queued(
        session_id: &str,
    ) -> (
        PreparedStdio,
        tokio::io::DuplexStream,
        Arc<StreamJsonStream>,
        mpsc::UnboundedReceiver<OutboundMsg>,
    ) {
        let (mut peer, input) = tokio::io::duplex(8192);
        let stream = Arc::new(StreamJsonStream::new_placeholder(Output::new(
            tokio::io::sink(),
        )));
        let plane = StdioControlPlane::new(stream.outbound_tx());
        let (lifecycle_tx, mut lifecycle_rx) = mpsc::unbounded_channel();
        let lifecycle = Arc::new(QueueLifecycle::new(
            Arc::new(lifecycle_tx),
            session_id.into(),
        ));
        let channels = spawn_stdin_router_from_reader(
            input,
            Output::new(tokio::io::sink()),
            false,
            session_id.into(),
            stream.outbound_tx(),
            lifecycle.clone(),
        );
        let prepared =
            PreparedStdio::from_channels(channels, lifecycle, plane, CancellationToken::new())
                .await;
        let input = format!(
            concat!(
                "{{\"type\":\"user\",\"uuid\":\"{}\",\"message\":{{\"role\":\"user\",\"content\":\"\\ud800\"}}}}\n",
                "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"without uuid\"}}}}\n",
                "{{\"type\":\"user\",\"uuid\":\"{}\",\"message\":{{\"role\":\"user\",\"content\":\"last\"}}}}\n"
            ),
            FIRST_UUID, SECOND_UUID
        );
        peer.write_all(input.as_bytes()).await.unwrap();
        assert_eq!(
            next_lifecycle(&mut lifecycle_rx).await,
            (FIRST_UUID.into(), "queued".into())
        );
        assert_eq!(
            next_lifecycle(&mut lifecycle_rx).await,
            (SECOND_UUID.into(), "queued".into())
        );
        (prepared, peer, stream, lifecycle_rx)
    }

    async fn assert_discarded(rx: &mut mpsc::UnboundedReceiver<OutboundMsg>) {
        assert_eq!(
            next_lifecycle(rx).await,
            (FIRST_UUID.into(), "discarded".into())
        );
        assert_eq!(
            next_lifecycle(rx).await,
            (SECOND_UUID.into(), "discarded".into())
        );
        assert!(rx.try_recv().is_err(), "each UUID has only one terminal");
    }

    #[tokio::test]
    async fn assembled_startup_failure_joins_and_journals_before_discarding() {
        let root = tempfile::tempdir().unwrap();
        let session_id = uuid::Uuid::new_v4().to_string();
        let path = root.path().join(format!("{session_id}.jsonl"));
        let writer = Arc::new(session::jsonl::JsonlWriter::new(
            path.clone(),
            Arc::new(platform_posix::fs::PosixFileSystem::new(
                root.path().to_path_buf(),
            )),
        ));
        writer
            .initialize_fixed_session_mode(
                &session_id,
                lingxi_core::host::session_context::ConversationMode::Code,
                0,
            )
            .await
            .unwrap();
        let (mut prepared, _open_peer, stream, mut lifecycle_rx) =
            prepare_queued(&session_id).await;
        let status = prepared.reader_status.clone();
        let helpers = prepared
            .auxiliary_tasks
            .iter()
            .map(tokio::task::JoinHandle::abort_handle)
            .collect::<Vec<_>>();
        let (_, pending) = prepared
            .plane
            .send_request(
                Utf16JsonProjection::from_serde_value(&json!({"subtype":"can_use_tool"})).unwrap(),
                None,
            )
            .await;

        let task_writer = writer.clone();
        let cleanup = tokio::spawn(async move {
            prepared
                .shutdown_after_assembly_failure(
                    "init failed",
                    FailedStartupJournal::Unbound(Some(task_writer)),
                )
                .await
        });
        assert_eq!(
            next_lifecycle(&mut lifecycle_rx).await,
            (FIRST_UUID.into(), "discarded".into())
        );
        // The first terminal is observable only after all accepted enqueues
        // have reached their session journal.
        let raw = std::fs::read_to_string(path).unwrap();
        let rows = raw
            .lines()
            .map(|line| Utf16JsonProjection::parse(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].string_at("/type"), Some("fixed-session-mode"));
        for row in &rows[1..] {
            assert_eq!(row.string_at("/type"), Some("queue-operation"));
            assert_eq!(row.string_at("/operation"), Some("enqueue"));
            assert_eq!(row.string_at("/sessionId"), Some(session_id.as_str()));
        }
        assert_eq!(rows[1].string_units("/content"), Some(vec![0xd800]));
        assert_eq!(rows[2].string_at("/content"), Some("without uuid"));
        assert_eq!(rows[3].string_at("/content"), Some("last"));
        assert_eq!(
            next_lifecycle(&mut lifecycle_rx).await,
            (SECOND_UUID.into(), "discarded".into())
        );
        assert!(lifecycle_rx.try_recv().is_err());
        let errors = cleanup.await.unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(*status.borrow(), StdinReaderStatus::Stopped);
        assert!(helpers.iter().all(tokio::task::AbortHandle::is_finished));
        assert!(pending.await.unwrap().is_err());
        stream.finish().await.unwrap();
    }

    #[tokio::test]
    async fn preassembly_startup_failure_discards_accepted_uuids() {
        let session_id = uuid::Uuid::new_v4().to_string();
        let (mut prepared, _open_peer, stream, mut lifecycle_rx) =
            prepare_queued(&session_id).await;
        prepared.shutdown("build failed").await.unwrap();
        assert_eq!(*prepared.reader_status.borrow(), StdinReaderStatus::Stopped);
        assert!(prepared.auxiliary_tasks.is_empty());
        assert_discarded(&mut lifecycle_rx).await;
        stream.finish().await.unwrap();
    }

    #[tokio::test]
    async fn failed_journal_is_reported_and_keeps_accepted_operations() {
        for already_failed in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let session_id = uuid::Uuid::new_v4().to_string();
            let blocker = root.path().join("blocked");
            std::fs::write(&blocker, b"not a directory").unwrap();
            let path = blocker.join(format!("{session_id}.jsonl"));
            let writer = Arc::new(session::jsonl::JsonlWriter::new(
                path.clone(),
                Arc::new(platform_posix::fs::PosixFileSystem::new(
                    root.path().to_path_buf(),
                )),
            ));
            let (mut prepared, _open_peer, stream, mut lifecycle_rx) =
                prepare_queued(&session_id).await;
            let repair_target = || {
                std::fs::remove_file(&blocker).unwrap();
                std::fs::create_dir(&blocker).unwrap();
            };
            let journal = if already_failed {
                prepared
                    .queue_lifecycle
                    .bind_journal(Some(writer.clone()))
                    .unwrap();
                assert!(prepared.queue_lifecycle.flush_journal().await.is_err());
                // Make a second write possible: cleanup still must not retry it.
                repair_target();
                writer
                    .initialize_fixed_session_mode(
                        &session_id,
                        lingxi_core::host::session_context::ConversationMode::Code,
                        0,
                    )
                    .await
                    .unwrap();
                FailedStartupJournal::AlreadyFailed
            } else {
                FailedStartupJournal::Unbound(Some(writer.clone()))
            };
            let errors = prepared
                .shutdown_after_assembly_failure("init failed", journal)
                .await;
            if already_failed {
                assert!(errors.is_empty(), "{errors:?}");
                assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 1);
            } else {
                assert_eq!(errors.len(), 1, "{errors:?}");
                assert!(errors[0].contains("Failed to flush queued input journal"));
                repair_target();
                writer
                    .initialize_fixed_session_mode(
                        &session_id,
                        lingxi_core::host::session_context::ConversationMode::Code,
                        0,
                    )
                    .await
                    .unwrap();
            }
            assert_eq!(*prepared.reader_status.borrow(), StdinReaderStatus::Stopped);
            assert!(prepared.auxiliary_tasks.is_empty());
            assert_discarded(&mut lifecycle_rx).await;

            // After a known pre-write failure, an explicit owner can flush the
            // retained operations without losing their order.
            prepared.queue_lifecycle.flush_journal().await.unwrap();
            let raw = std::fs::read_to_string(path).unwrap();
            let rows = raw
                .lines()
                .map(|line| Utf16JsonProjection::parse(line).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(rows.len(), 4);
            assert_eq!(rows[1].string_units("/content"), Some(vec![0xd800]));
            assert_eq!(rows[2].string_at("/content"), Some("without uuid"));
            assert_eq!(rows[3].string_at("/content"), Some("last"));
            stream.finish().await.unwrap();
        }
    }
}
