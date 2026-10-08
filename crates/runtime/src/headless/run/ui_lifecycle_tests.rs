//! Real Mod callbacks must not monopolize the inbound control dispatcher.
use super::tests::fixture_runtime;
use super::*;
use crate::headless::remote_ui::HeadlessRemoteUiHost;
use crate::headless::stream_json::OutboundMsg;
use lingxi_core::types::utf16_json::Utf16JsonProjection;

async fn dispatch(
    runtime: &Runtime,
    plane: &Arc<StdioControlPlane>,
    owner: &Arc<PrintAuxTaskGroup>,
    lifecycle: &crate::headless::queued_commands::QueueLifecycle,
    cancel: &tokio::sync::watch::Sender<bool>,
    end: &Arc<tokio::sync::Notify>,
    id: &str,
    request: Value,
) {
    let subtype = request["subtype"].as_str().unwrap().to_owned();
    let frame = Utf16JsonProjection::plain(
        json!({"type":"control_request","request_id":id,"request":request}),
    );
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        dispatch_control_request(
            &subtype,
            id,
            &frame,
            &plane.outbound_writer(),
            cancel,
            lifecycle,
            &runtime.orchestrator,
            &runtime.task_registry,
            &runtime.session_cwd,
            plane,
            end,
            &[],
            &[],
            &[],
            &json!({}),
            "off",
            None,
            &StreamFileSuggestionIndex::default(),
            runtime.services.as_ref(),
            owner,
            &runtime.execution_errors,
        ),
    )
    .await
    .expect("inbound dispatch must not await a Mod callback")
    .unwrap();
}

async fn frame(rx: &mut tokio::sync::mpsc::UnboundedReceiver<OutboundMsg>) -> Value {
    let message = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
        .await
        .unwrap()
        .unwrap();
    let OutboundMsg::Line(line) = message else {
        panic!("expected control frame")
    };
    serde_json::from_str(&line).unwrap()
}

fn render() -> Value {
    json!({"subtype":"ui_render","surface":"mobile","client_id":"phone","component":"Pane","instance_id":"pane","props":{}})
}

#[tokio::test]
async fn actual_mod_render_cancel_keeps_owner_and_stop_controls_remain_responsive() {
    let root = tempfile::tempdir().unwrap();
    let stream = Arc::new(StreamJsonStream::new_placeholder(
        crate::headless::io::Output::new(tokio::io::sink()),
    ));
    let runtime = fixture_runtime(stream.clone(), root.path()).await;
    let module = root.path().join("render-callback.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
      on('ui.render', { surface: 'mobile', component: 'Pane' }, async ($, event, next) => {
        await $.prompt.read();
        return next(event);
      });
    }"#,
    )
    .unwrap();
    let mods = hooks::mods::ModHost::start(None).await.unwrap();
    mods.load("render-callback", root.path(), &module, json!({}))
        .await
        .unwrap();
    runtime.hook_registry.write().await.set_mod_host(mods);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let tx = Arc::new(tx);
    let plane = StdioControlPlane::new(tx.clone());
    let owner = Arc::new(PrintAuxTaskGroup::default());
    plane.set_auxiliary_tasks(Arc::downgrade(&owner));
    runtime
        .orchestrator
        .set_remote_ui_host(HeadlessRemoteUiHost::new(plane.clone()));
    runtime.orchestrator.prepare_sdk_ui_surface_attach(
        "phone",
        orchestrator::config::ModRenderSurface::Mobile,
        None,
        Some(vec![lingxi_core::host::ModRemoteUiAnswer::PromptRead]),
    );
    let lifecycle = crate::headless::queued_commands::QueueLifecycle::new(tx, "fixture".into());
    let (cancel, _receiver) = tokio::sync::watch::channel(false);
    let end = Arc::new(tokio::sync::Notify::new());

    dispatch(
        &runtime,
        &plane,
        &owner,
        &lifecycle,
        &cancel,
        &end,
        "render",
        render(),
    )
    .await;
    let callback = frame(&mut rx).await;
    assert_eq!(callback["request"]["subtype"], "ui_prompt_read");
    plane.cancel_inbound_request(&Utf16JsonProjection::plain(json!("render")));
    dispatch(
        &runtime,
        &plane,
        &owner,
        &lifecycle,
        &cancel,
        &end,
        "stop",
        json!({"subtype":"interrupt"}),
    )
    .await;
    let receipt = frame(&mut rx).await;
    assert_eq!(receipt["response"]["request_id"], "stop");
    let joining = {
        let owner = owner.clone();
        tokio::spawn(async move { owner.join().await })
    };
    tokio::task::yield_now().await;
    assert!(
        !joining.is_finished(),
        "render remains owned while its callback is unresolved"
    );
    assert!(matches!(
        rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    plane.resolve_response(&Utf16JsonProjection::plain(json!({"type":"control_response","response":{
        "subtype":"success","request_id":callback["request_id"],"response":{"text":"settled","cursor":7}
    }}))).await;
    tokio::time::timeout(std::time::Duration::from_secs(3), joining)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ),
        "cancelled render response is suppressed even after actual settlement"
    );

    dispatch(
        &runtime,
        &plane,
        &owner,
        &lifecycle,
        &cancel,
        &end,
        "render-end",
        render(),
    )
    .await;
    let callback = frame(&mut rx).await;
    assert_eq!(callback["request"]["subtype"], "ui_prompt_read");
    dispatch(
        &runtime,
        &plane,
        &owner,
        &lifecycle,
        &cancel,
        &end,
        "end",
        json!({"subtype":"end_session"}),
    )
    .await;
    // Closing the connection settles existing callback receivers. Depending
    // on scheduling, their cancellation may precede or follow the stop reply.
    loop {
        let reply = frame(&mut rx).await;
        if reply["type"] == "control_cancel_request" {
            assert_eq!(reply["request_id"], callback["request_id"]);
            continue;
        }
        assert_eq!(reply["response"]["request_id"], "end");
        break;
    }
    tokio::time::timeout(std::time::Duration::from_secs(3), owner.join())
        .await
        .unwrap();
    assert!(runtime.execution_errors.lock().unwrap().is_empty());
    assert!(
        runtime
            .session_lifecycle
            .shutdown_and_drain()
            .await
            .complete
    );
    stream.finish().await.unwrap();
}
