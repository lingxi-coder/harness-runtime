use super::tests::fixture_runtime;
use super::*;
use crate::headless::io::Output;
use crate::headless::stream_json_input::spawn_stdin_router_from_reader;
use tokio::io::AsyncWriteExt;

#[tokio::test]
async fn end_session_returns_while_its_input_pipe_remains_open() {
    let root = tempfile::tempdir().unwrap();
    let stream = Arc::new(StreamJsonStream::new_placeholder(Output::new(
        tokio::io::sink(),
    )));
    let runtime = fixture_runtime(stream.clone(), root.path()).await;
    let plane = StdioControlPlane::new(stream.outbound_tx());
    let (mut peer, input) = tokio::io::duplex(1024);
    peer.write_all(b"{\"type\":\"control_request\",\"request_id\":\"end\",\"request\":{\"subtype\":\"end_session\"}}\n").await.unwrap();
    let lifecycle = Arc::new(crate::headless::queued_commands::QueueLifecycle::new(
        stream.outbound_tx(),
        runtime
            .orchestrator
            .session()
            .lock()
            .await
            .session_id
            .to_string(),
    ));
    let channels = spawn_stdin_router_from_reader(
        input,
        Output::new(tokio::io::sink()),
        false,
        "fixture".into(),
        stream.outbound_tx(),
        lifecycle.clone(),
    );
    let status = channels.status.clone();
    let prepared = super::super::stdio::PreparedStdio::from_channels(
        channels,
        lifecycle,
        plane.clone(),
        runtime.shutdown.child_token(),
    )
    .await;
    let tasks = Arc::new(PrintAuxTaskGroup::default());
    let code = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        run_stream_json_input_loop_inner(
            &Argv::default(),
            &runtime,
            stream.clone(),
            permission::PermissionMode::Default,
            plane.clone(),
            tasks.clone(),
            prepared,
        ),
    )
    .await
    .expect("end_session must not await peer EOF");
    assert_eq!(code, exit_codes::SUCCESS);
    assert_eq!(
        *status.borrow(),
        StdinReaderStatus::Stopped,
        "reader cancellation is joined before returning"
    );
    assert!(!plane.is_busy().await);
    let (_, denied) = plane
        .send_request(
            lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
                &json!({"subtype":"can_use_tool"}),
            )
            .expect("permission request projection"),
            None,
        )
        .await;
    assert!(
        denied.await.unwrap().is_err(),
        "closed input cannot accept another permission waiter"
    );
    tasks.abort_and_join().await;
    assert!(
        runtime
            .session_lifecycle
            .shutdown_and_drain()
            .await
            .complete
    );
    stream.finish().await.unwrap();
    drop(peer);
}

#[tokio::test]
async fn fatal_input_is_distinct_from_eof_and_returns_nonzero_before_any_turn() {
    for input in [
        "secret-invalid-json",
        r#"{"type":"user","message":{"role":"secret-role","content":"secret"}}"#,
        r#"{"type":"control_request","request_id":"bad"}"#,
    ] {
        let root = tempfile::tempdir().unwrap();
        let stream = Arc::new(StreamJsonStream::new_placeholder(Output::new(
            tokio::io::sink(),
        )));
        let runtime = fixture_runtime(stream.clone(), root.path()).await;
        let plane = StdioControlPlane::new(stream.outbound_tx());
        let lifecycle = Arc::new(crate::headless::queued_commands::QueueLifecycle::new(
            stream.outbound_tx(),
            runtime
                .orchestrator
                .session()
                .lock()
                .await
                .session_id
                .to_string(),
        ));
        let channels = spawn_stdin_router_from_reader(
            std::io::Cursor::new(format!("{input}\n").into_bytes()),
            Output::new(tokio::io::sink()),
            false,
            "fixture".into(),
            stream.outbound_tx(),
            lifecycle.clone(),
        );
        let prepared = super::super::stdio::PreparedStdio::from_channels(
            channels,
            lifecycle,
            plane.clone(),
            runtime.shutdown.child_token(),
        )
        .await;
        let tasks = Arc::new(PrintAuxTaskGroup::default());
        let code = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            run_stream_json_input_loop_inner(
                &Argv::default(),
                &runtime,
                stream.clone(),
                permission::PermissionMode::Default,
                plane,
                tasks.clone(),
                prepared,
            ),
        )
        .await
        .unwrap();
        assert_eq!(code, exit_codes::RUNTIME_ERROR);
        tasks.abort_and_join().await;
        assert!(
            runtime
                .session_lifecycle
                .shutdown_and_drain()
                .await
                .complete
        );
        stream.finish().await.unwrap();
    }
}

#[tokio::test]
async fn closed_lifecycle_writer_after_turn_start_fails_the_run_and_releases_busy_state() {
    let root = tempfile::tempdir().unwrap();
    let stream = Arc::new(StreamJsonStream::new_placeholder(Output::new(
        tokio::io::sink(),
    )));
    let runtime = fixture_runtime(stream.clone(), root.path()).await;
    let plane = StdioControlPlane::new(stream.outbound_tx());
    let session_id = runtime
        .orchestrator
        .session()
        .lock()
        .await
        .session_id
        .to_string();
    let (lifecycle_tx, mut lifecycle_rx) = tokio::sync::mpsc::unbounded_channel();
    let lifecycle = Arc::new(crate::headless::queued_commands::QueueLifecycle::new(
        Arc::new(lifecycle_tx),
        session_id.clone(),
    ));
    let uuid = uuid::Uuid::new_v4().to_string();
    let input = format!(
        "{{\"type\":\"user\",\"uuid\":{uuid:?},\"message\":{{\"role\":\"user\",\"content\":\"/compact\"}}}}\n"
    );
    let channels = spawn_stdin_router_from_reader(
        std::io::Cursor::new(input.into_bytes()),
        Output::new(tokio::io::sink()),
        false,
        session_id,
        stream.outbound_tx(),
        lifecycle.clone(),
    );
    let prepared = super::super::stdio::PreparedStdio::from_channels(
        channels,
        lifecycle,
        plane.clone(),
        runtime.shutdown.child_token(),
    )
    .await;
    // Hold the run-loop operation lease: it emits `started` immediately before
    // taking this lease, giving the test a deterministic close point.
    let operation_lease = plane.lock_operation().await;
    let tasks = Arc::new(PrintAuxTaskGroup::default());
    let argv = Argv::default();
    let running = run_stream_json_input_loop_inner(
        &argv,
        &runtime,
        stream.clone(),
        permission::PermissionMode::Default,
        plane.clone(),
        tasks.clone(),
        prepared,
    );
    tokio::pin!(running);
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            tokio::select! {
                code = &mut running => panic!("run ended before lifecycle failure: {code}"),
                message = lifecycle_rx.recv() => {
                    let crate::headless::stream_json::OutboundMsg::Line(line) =
                        message.expect("queued/start lifecycle frame")
                    else {
                        panic!("expected lifecycle line");
                    };
                    let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
                    if frame["state"] == "started" {
                        break;
                    }
                }
            }
        }
    })
    .await
    .expect("turn must reach its started boundary");

    // The next production lifecycle send (compact's completed terminal) now
    // sees a closed single-writer owner.
    drop(lifecycle_rx);
    drop(operation_lease);
    let code = tokio::time::timeout(std::time::Duration::from_secs(3), &mut running)
        .await
        .expect("closed lifecycle output must unwind the turn loop");
    assert_eq!(code, exit_codes::RUNTIME_ERROR);
    assert!(
        !plane.is_busy().await,
        "terminal failure must release turn state"
    );
    tasks.abort_and_join().await;
    assert!(
        runtime
            .session_lifecycle
            .shutdown_and_drain()
            .await
            .complete
    );
    stream.finish().await.unwrap();
}

#[tokio::test]
async fn reader_shutdown_is_joinable_while_peer_remains_open() {
    let stream = Arc::new(StreamJsonStream::new_placeholder(Output::new(
        tokio::io::sink(),
    )));
    let (peer, input) = tokio::io::duplex(1024);
    let lifecycle = Arc::new(crate::headless::queued_commands::QueueLifecycle::new(
        stream.outbound_tx(),
        "fixture".into(),
    ));
    let channels = spawn_stdin_router_from_reader(
        input,
        Output::new(tokio::io::sink()),
        false,
        "fixture".into(),
        stream.outbound_tx(),
        lifecycle.clone(),
    );
    channels.reader.stop();
    tokio::time::timeout(std::time::Duration::from_secs(1), channels.reader.join())
        .await
        .expect("reader join must finish")
        .expect("reader shutdown must be clean");
    assert_eq!(*channels.status.borrow(), StdinReaderStatus::Stopped);
    drop(peer);
}
