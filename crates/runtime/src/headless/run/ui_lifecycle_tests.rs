//! Real Mod callbacks must not monopolize the inbound control dispatcher.
use super::tests::fixture_runtime;
use super::*;
use crate::headless::remote_ui::HeadlessRemoteUiHost;
use crate::headless::stream_json::OutboundMsg;
use lingxi_core::host::{ModRemoteUiAnswer, ModRemoteUiClient, ModRemoteUiHost};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use std::sync::atomic::{AtomicUsize, Ordering};

fn project(value: &Value) -> Utf16JsonProjection {
    Utf16JsonProjection::from_serde_value(value).expect("valid test JSON")
}

/// Records whether the actual callback route reaches its attached UI host.
struct RemoteUiProbe {
    inner: Arc<HeadlessRemoteUiHost>,
    prompt_read_client_queries: AtomicUsize,
    prompt_read_client_matches: AtomicUsize,
    prompt_read_requests: AtomicUsize,
}

impl RemoteUiProbe {
    fn new(inner: Arc<HeadlessRemoteUiHost>) -> Self {
        Self {
            inner,
            prompt_read_client_queries: AtomicUsize::new(0),
            prompt_read_client_matches: AtomicUsize::new(0),
            prompt_read_requests: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl ModRemoteUiHost for RemoteUiProbe {
    fn attach(&self, client: &str, surface: &str, answers: Option<Vec<ModRemoteUiAnswer>>) {
        self.inner.attach(client, surface, answers);
    }

    fn detach(&self, client: &str) {
        self.inner.detach(client);
    }

    fn clients(&self, answer: ModRemoteUiAnswer) -> Vec<ModRemoteUiClient> {
        let clients = self.inner.clients(answer);
        if answer == ModRemoteUiAnswer::PromptRead {
            self.prompt_read_client_queries
                .fetch_add(1, Ordering::SeqCst);
            if !clients.is_empty() {
                self.prompt_read_client_matches
                    .fetch_add(1, Ordering::SeqCst);
            }
        }
        clients
    }

    async fn request(&self, request: Utf16JsonProjection) -> Option<Utf16JsonProjection> {
        if request.string_at("/subtype") == Some("ui_prompt_read") {
            self.prompt_read_requests.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.request(request).await
    }
}

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
    let frame = project(&json!({"type":"control_request","request_id":id,"request":request}));
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
            &runtime.prompt_suggestions,
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
    mods.load("render-callback", root.path(), &module, project(&json!({})))
        .await
        .unwrap();
    let mod_session: Arc<dyn hooks::mods::ModSessionContext> = runtime.orchestrator.clone();
    let mut hook_registry = runtime.hook_registry.write().await;
    hook_registry.attach_mod_background_context(Arc::downgrade(&mod_session));
    hook_registry.set_mod_host(mods);
    drop(hook_registry);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let tx = Arc::new(tx);
    let plane = StdioControlPlane::new(tx.clone());
    let owner = Arc::new(PrintAuxTaskGroup::default());
    plane.set_auxiliary_tasks(Arc::downgrade(&owner));
    let remote_ui = HeadlessRemoteUiHost::new(plane.clone());
    let remote_ui_probe = Arc::new(RemoteUiProbe::new(remote_ui.clone()));
    runtime
        .orchestrator
        .set_remote_ui_host(remote_ui_probe.clone());
    let lifecycle = crate::headless::queued_commands::QueueLifecycle::new(tx, "fixture".into());
    let (cancel, _receiver) = tokio::sync::watch::channel(false);
    let end = Arc::new(tokio::sync::Notify::new());

    // Exercise the public control ingress that production clients use to
    // declare their callback capabilities, then verify the same host queried
    // by prompt.read holds that capability before rendering begins.
    dispatch(
        &runtime,
        &plane,
        &owner,
        &lifecycle,
        &cancel,
        &end,
        "attach",
        json!({
            "subtype":"ui_attach",
            "client_id":"phone",
            "surface":"mobile",
            "answers":["ui_prompt_read"]
        }),
    )
    .await;
    let attached = frame(&mut rx).await;
    assert_eq!(attached["type"], "control_response", "{attached}");
    assert_eq!(attached["response"]["request_id"], "attach", "{attached}");
    let clients = remote_ui.clients(ModRemoteUiAnswer::PromptRead);
    assert_eq!(
        clients.len(),
        1,
        "prompt.read capability roster: {clients:?}"
    );
    assert_eq!(clients[0].client_id, "phone");
    assert_eq!(clients[0].surface, "mobile");
    assert_eq!(
        clients[0].answers.as_slice(),
        &[ModRemoteUiAnswer::PromptRead]
    );

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
    assert_eq!(
        remote_ui_probe
            .prompt_read_client_queries
            .load(Ordering::SeqCst),
        1,
        "the render callback must query the installed remote UI host for prompt.read; response={callback}"
    );
    assert_eq!(
        remote_ui_probe
            .prompt_read_client_matches
            .load(Ordering::SeqCst),
        1,
        "the callback's host query must see the attached phone capability"
    );
    assert_eq!(
        remote_ui_probe.prompt_read_requests.load(Ordering::SeqCst),
        1,
        "prompt.read must be delivered through the installed remote UI host"
    );
    assert_eq!(
        callback["type"], "control_request",
        "unexpected first outbound frame: {callback}"
    );
    assert_eq!(
        callback["request"]["subtype"], "ui_prompt_read",
        "unexpected callback request: {callback}"
    );
    plane.cancel_inbound_request(&project(&json!("render")));
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
    plane.resolve_response(&project(&json!({"type":"control_response","response":{
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
