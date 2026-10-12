use super::tests::fixture_runtime;
use super::*;
use crate::headless::control_plane::StdioControlPlane;
use crate::headless::stream_json::{OutboundMsg, OutboundTx, StreamJsonStream};
use crate::headless::stream_json_input::ControlPlaneWriter;
use lingxi_core::types::utf16_json::Utf16JsonProjection;

fn project(value: &Value) -> Utf16JsonProjection {
    Utf16JsonProjection::from_serde_value(value).expect("valid test JSON")
}

async fn dispatch(
    runtime: &Runtime,
    writer: &ControlPlaneWriter,
    outbound: &Arc<OutboundTx>,
    receiver: &mut tokio::sync::mpsc::UnboundedReceiver<OutboundMsg>,
    id: &str,
    request: Value,
) -> Value {
    let subtype = request["subtype"].as_str().unwrap().to_owned();
    let frame = project(&json!({
        "type": "control_request",
        "request_id": id,
        "request": request,
    }));
    let (cancel, _cancel_rx) = tokio::sync::watch::channel(false);
    let lifecycle = crate::headless::queued_commands::QueueLifecycle::new(
        Arc::clone(outbound),
        "proactivity-control-test".to_owned(),
    );
    let plane = StdioControlPlane::new(Arc::clone(outbound));
    dispatch_control_request(
        &subtype,
        id,
        &frame,
        writer,
        &cancel,
        &lifecycle,
        &runtime.orchestrator,
        &runtime.task_registry,
        &runtime.session_cwd,
        &plane,
        &Arc::new(tokio::sync::Notify::new()),
        &[],
        &[],
        &[],
        &json!({}),
        "off",
        None,
        &StreamFileSuggestionIndex::default(),
        &runtime.prompt_suggestions,
        runtime.services.as_ref(),
        &Arc::new(super::PrintAuxTaskGroup::default()),
        &Arc::new(std::sync::Mutex::new(Vec::new())),
    )
    .await
    .unwrap();

    let Some(OutboundMsg::Line(line)) = receiver.recv().await else {
        panic!("expected control response")
    };
    serde_json::from_str(&line).unwrap()
}

#[tokio::test]
async fn initialize_opt_in_and_level_setter_follow_native_gate_order() {
    let root = tempfile::tempdir().unwrap();
    let stream = Arc::new(StreamJsonStream::new_placeholder(
        crate::headless::io::Output::new(tokio::io::sink()),
    ));
    let runtime = fixture_runtime(stream, root.path()).await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let outbound = Arc::new(tx);
    let writer = ControlPlaneWriter::new(Arc::clone(&outbound));

    // Initial initialize is the only public operation that opts in without
    // Native's account-only cloudOptIn service.
    let init = dispatch(
        &runtime,
        &writer,
        &outbound,
        &mut rx,
        "init-on",
        json!({"subtype":"initialize","proactivity":true}),
    )
    .await;
    assert_eq!(init["response"]["subtype"], "success");

    let level = dispatch(
        &runtime,
        &writer,
        &outbound,
        &mut rx,
        "level-proactive",
        json!({"subtype":"set_proactivity_level","level":"proactive"}),
    )
    .await;
    assert_eq!(level["response"]["subtype"], "success");
    assert_eq!(level["response"]["response"]["level"], "proactive");
    // Native K1r/Y1r -> qce chooses Auto for `proactive` when bypass is
    // unavailable and Auto is available; this fixture has that live gate state.
    assert_eq!(level["response"]["response"]["permissionMode"], "auto");

    // Native hC ignores false on reinitialize before changing the selector.
    let reinitialize = dispatch(
        &runtime,
        &writer,
        &outbound,
        &mut rx,
        "reinit-off",
        json!({
            "subtype":"initialize",
            "proactivity":false,
            "reinitialize":true
        }),
    )
    .await;
    assert_eq!(reinitialize["response"]["subtype"], "success");
    let after_false_reinitialize = runtime.orchestrator.current_native_proactivity_snapshot();
    assert!(after_false_reinitialize.opted_in);
    assert_eq!(
        after_false_reinitialize.level,
        orchestrator::conversation::NativeProactivityLevel::Proactive
    );

    let level = dispatch(
        &runtime,
        &writer,
        &outbound,
        &mut rx,
        "level-ask",
        json!({"subtype":"set_proactivity_level","level":"ask"}),
    )
    .await;
    assert_eq!(level["response"]["subtype"], "success");
    assert_eq!(level["response"]["response"]["level"], "ask");
    let permission_mode_before_opt_out = runtime.orchestrator.permission_mode();

    // An ordinary false initialize withdraws the gate; later control fails
    // before level decoding, as in Native iRt's gate-off error order.
    let init = dispatch(
        &runtime,
        &writer,
        &outbound,
        &mut rx,
        "init-off",
        json!({"subtype":"initialize","proactivity":false}),
    )
    .await;
    assert_eq!(init["response"]["subtype"], "success");
    let after_opt_out = runtime.orchestrator.current_native_proactivity_snapshot();
    assert!(!after_opt_out.opted_in);
    assert_eq!(
        after_opt_out.level,
        orchestrator::conversation::NativeProactivityLevel::Ask
    );
    assert_eq!(
        runtime.orchestrator.permission_mode(),
        permission_mode_before_opt_out,
        "ordinary false initialize leaves the live permission mode unchanged"
    );

    let level = dispatch(
        &runtime,
        &writer,
        &outbound,
        &mut rx,
        "level-after-off",
        json!({"subtype":"set_proactivity_level","level":"invalid"}),
    )
    .await;
    assert_eq!(level["response"]["subtype"], "error");
    assert_eq!(
        level["response"]["error"],
        "Cannot set proactivity: levels are not enabled for this session (the host must send proactivity: true on initialize, with no kill switch set)"
    );

    let reenabled = dispatch(
        &runtime,
        &writer,
        &outbound,
        &mut rx,
        "init-on-again",
        json!({"subtype":"initialize","proactivity":true}),
    )
    .await;
    assert_eq!(reenabled["response"]["subtype"], "success");
    let after_reenable = runtime.orchestrator.current_native_proactivity_snapshot();
    assert!(after_reenable.opted_in);
    assert_eq!(
        after_reenable.level,
        orchestrator::conversation::NativeProactivityLevel::Ask
    );
    assert_eq!(
        runtime.orchestrator.permission_mode(),
        permission_mode_before_opt_out
    );
}
