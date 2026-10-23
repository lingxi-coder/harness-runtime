//! A controllable SDK boundary verifies ownership/cancellation independently of
//! device execution. These tests do not claim a physical Linux runtime passed.
use super::*;
use mobile_linux_api::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct ControlledRuntime {
    base: UnavailableMobileLinuxRuntime,
    blocked: bool,
    delay_spawn: AtomicBool,
    spawning: tokio::sync::Notify,
    release_spawn: tokio::sync::Notify,
    fail_first_kill: AtomicBool,
    reached: tokio::sync::Notify,
    kills: Mutex<Vec<String>>,
    maintenance_calls: AtomicUsize,
    delay_maintenance: AtomicBool,
    maintaining: tokio::sync::Notify,
    finish_maintenance: tokio::sync::Notify,
    tasks: Mutex<Vec<MobileLinuxTaskSnapshot>>,
}
impl ControlledRuntime {
    fn new(blocked: bool) -> Self {
        Self {
            base: UnavailableMobileLinuxRuntime::unavailable(
                SandboxBackend::IosIsh,
                MobileLinuxRuntimeMode::MobileLinux,
                "ios",
                "x86",
                "test",
            ),
            blocked,
            delay_spawn: AtomicBool::new(false),
            spawning: tokio::sync::Notify::new(),
            release_spawn: tokio::sync::Notify::new(),
            fail_first_kill: AtomicBool::new(true),
            reached: tokio::sync::Notify::new(),
            kills: Mutex::new(Vec::new()),
            maintenance_calls: AtomicUsize::new(0),
            delay_maintenance: AtomicBool::new(false),
            maintaining: tokio::sync::Notify::new(),
            finish_maintenance: tokio::sync::Notify::new(),
            tasks: Mutex::new(Vec::new()),
        }
    }
}
#[async_trait::async_trait]
impl MobileLinuxRuntime for ControlledRuntime {
    fn backend(&self) -> SandboxBackend {
        self.base.backend()
    }
    fn mode(&self) -> MobileLinuxRuntimeMode {
        self.base.mode()
    }
    async fn probe_capability(&self) -> MobileLinuxCapability {
        let mut value = self.base.probe_capability().await;
        value.available = true;
        value.background_processes = true;
        value.reason = None;
        value
    }
    async fn boot(&self) -> Result<RootfsStatus, MobileLinuxError> {
        self.base.rootfs_status().await
    }
    async fn shutdown(&self) -> Result<(), MobileLinuxError> {
        panic!("an app must never shut down the shared runtime")
    }
    async fn run(
        &self,
        _request: LinuxCommandRequest,
    ) -> Result<LinuxCommandResult, MobileLinuxError> {
        Err(MobileLinuxError::Unsupported)
    }
    async fn spawn_background(
        &self,
        request: LinuxCommandRequest,
    ) -> Result<LinuxProcessHandle, MobileLinuxError> {
        assert!(request.env.contains_key("LOCAL_APP_RUN_ID"));
        assert!(
            request.env["LOCAL_APP_DATA_DIR"].ends_with("/preparation-data"),
            "install/build must not receive the live data directory"
        );
        assert!(request
            .cwd
            .as_ref()
            .unwrap()
            .starts_with("/home/lingxi/.lingxi/local-apps/"));
        if self.delay_spawn.load(Ordering::SeqCst) {
            self.spawning.notify_one();
            self.release_spawn.notified().await;
        }
        Ok(LinuxProcessHandle {
            id: "only-owned-task".into(),
            enforcement: LinuxEnforcementReceipt::default(),
        })
    }
    async fn kill(&self, handle: &LinuxProcessHandle) -> Result<(), MobileLinuxError> {
        assert_eq!(handle.id, "only-owned-task");
        self.kills.lock().unwrap().push(handle.id.clone());
        if self.fail_first_kill.swap(false, Ordering::SeqCst) {
            Err(MobileLinuxError::Io("injected first reap failure".into()))
        } else {
            Ok(())
        }
    }
    async fn task_status(
        &self,
        _id: &str,
    ) -> Result<Option<MobileLinuxTaskSnapshot>, MobileLinuxError> {
        self.reached.notify_one();
        if self.blocked {
            std::future::pending().await
        } else {
            Ok(Some(MobileLinuxTaskSnapshot {
                task_id: "only-owned-task".into(),
                status: MobileLinuxTaskStatus::Failed,
                command: "test".into(),
                started_at_ms: None,
                finished_at_ms: None,
                exit_code: Some(17),
                detail: Some("injected preparation failure".into()),
            }))
        }
    }
    async fn open_pty(&self, r: PtyOpenRequest) -> Result<PtySessionHandle, MobileLinuxError> {
        self.base.open_pty(r).await
    }
    async fn write_pty(&self, h: &PtySessionHandle, b: Vec<u8>) -> Result<(), MobileLinuxError> {
        self.base.write_pty(h, b).await
    }
    async fn resize_pty(&self, h: &PtySessionHandle, s: PtySize) -> Result<(), MobileLinuxError> {
        self.base.resize_pty(h, s).await
    }
    async fn close_pty(&self, h: &PtySessionHandle) -> Result<(), MobileLinuxError> {
        self.base.close_pty(h).await
    }
    async fn rootfs_status(&self) -> Result<RootfsStatus, MobileLinuxError> {
        self.base.rootfs_status().await
    }
    async fn verify_rootfs(&self) -> Result<RootfsStatus, MobileLinuxError> {
        self.base.verify_rootfs().await
    }
    async fn repair_rootfs(&self) -> Result<RootfsStatus, MobileLinuxError> {
        self.maintenance_calls.fetch_add(1, Ordering::SeqCst);
        if self.delay_maintenance.load(Ordering::SeqCst) {
            self.maintaining.notify_one();
            self.finish_maintenance.notified().await;
        }
        self.base.rootfs_status().await
    }
    async fn reset_rootfs(&self) -> Result<RootfsStatus, MobileLinuxError> {
        panic!("an app must not reset the shared rootfs")
    }
    async fn configure_mounts(&self, _mounts: Vec<MountSpec>) -> Result<(), MobileLinuxError> {
        panic!("an app must not replace shared mounts")
    }
    async fn list_tasks(&self) -> Result<Vec<MobileLinuxTaskSnapshot>, MobileLinuxError> {
        Ok(self.tasks.lock().unwrap().clone())
    }
}
async fn setup(
    blocked: bool,
) -> (
    tempfile::TempDir,
    Arc<MobileLocalAppServiceHandle>,
    Arc<ControlledRuntime>,
    String,
) {
    let root = tempfile::tempdir().unwrap();
    let runtime = Arc::new(ControlledRuntime::new(blocked));
    let service = MobileLocalAppServiceHandle::new(
        root.path().join("user/.lingxi"),
        Some(super::super::linux_maintenance::coordinate_runtime(
            &root.path().join("linux"),
            runtime.clone(),
        )),
        "ios".into(),
    )
    .unwrap();
    service.request_value(json!({"op":"configureTarget","target":{"platform":"ios","environment":"mobile-linux","capabilities":["process"]}})).await.unwrap();
    let source = root.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("index.html"), "fixture").unwrap();
    std::fs::write(source.join("lingxi-app.json"),json!({"schema":2,"name":"build fixture","entry":{"type":"static","path":"index.html"},"install":[{"command":"controlled install"}],"targets":[{"platform":"ios","environment":"mobile-linux"}],"capabilities":["process"]}).to_string()).unwrap();
    let published = service
        .request_value(json!({"op":"publish","expectedRevision":0,"sourcePath":source}))
        .await
        .unwrap();
    let id = published["record"]["appId"].as_str().unwrap().to_owned();
    service
        .request_value(json!({"op":"grant","appId":id,"digest":published["release"]["digest"]}))
        .await
        .unwrap();
    (root, service, runtime, id)
}
#[tokio::test]
async fn failed_start_retains_process_owner_when_reap_fails_and_stop_can_retry() {
    let (_root, service, runtime, id) = setup(false).await;
    let error = service
        .request_value(json!({"op":"start","appId":id}))
        .await
        .unwrap_err();
    assert_eq!(error.code, "stopFailed");
    let status = service
        .request_value(json!({"op":"status","appId":id}))
        .await
        .unwrap();
    assert_eq!(status["run"]["phase"], "stopping");
    assert_eq!(status["run"]["services"], json!([]));
    service
        .request_value(json!({"op":"stop","appId":id}))
        .await
        .unwrap();
    assert_eq!(runtime.kills.lock().unwrap().len(), 2);
    assert!(service
        .request_value(json!({"op":"status","appId":id}))
        .await
        .unwrap()["run"]
        .is_null());
}

#[tokio::test]
async fn repair_rebuilds_same_release_and_failed_preparation_keeps_serving_release() {
    let (root, service, runtime, id) = setup(false).await;
    runtime.fail_first_kill.store(false, Ordering::SeqCst);
    let source = root.path().join("source");
    let manifest_path = source.join("lingxi-app.json");
    let original: Value = serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    let mut ready_manifest = original.clone();
    ready_manifest.as_object_mut().unwrap().remove("install");
    std::fs::write(&manifest_path, ready_manifest.to_string()).unwrap();
    let ready = service
        .request_value(json!({"op":"publish","appId":id,"expectedRevision":1,"sourcePath":source}))
        .await
        .unwrap();
    service
        .request_value(json!({"op":"grant","appId":id,"digest":ready["release"]["digest"]}))
        .await
        .unwrap();
    let old = service
        .request_value(json!({"op":"start","appId":id,"revision":2}))
        .await
        .unwrap();
    std::fs::write(&manifest_path, original.to_string()).unwrap();
    let broken = service
        .request_value(json!({"op":"publish","appId":id,"expectedRevision":2,"sourcePath":source}))
        .await
        .unwrap();
    service
        .request_value(json!({"op":"grant","appId":id,"digest":broken["release"]["digest"]}))
        .await
        .unwrap();
    let failure = service
        .request_value(json!({"op":"repair","appId":id,"revision":3}))
        .await
        .unwrap_err();
    assert!(failure.message.contains("17"), "{failure}");
    let status = service
        .request_value(json!({"op":"status","appId":id}))
        .await
        .unwrap();
    assert_eq!(status["run"]["runId"], old["run"]["runId"]);
    let port = service.library.get(&id).unwrap().origin_port;
    let origin = format!("http://127.0.0.1:{port}");
    let path = old["url"].as_str().unwrap().strip_prefix(&origin).unwrap();
    assert!(super::tests::get(port, path).await.ends_with("fixture"));
    let rebuilt = service
        .request_value(json!({"op":"repair","appId":id,"revision":2}))
        .await
        .unwrap();
    assert_ne!(rebuilt["run"]["runId"], old["run"]["runId"]);
    service
        .request_value(json!({"op":"stop","appId":id}))
        .await
        .unwrap();
}
#[tokio::test]
async fn cancelling_tool_wait_reaps_its_run_and_retains_failed_reap_for_retry() {
    let (_root, service, runtime, id) = setup(true).await;
    let waiting = service.clone();
    let run_id = id.clone();
    let task = tokio::spawn(async move {
        waiting
            .request_for_tool(json!({"op":"start","appId":run_id}))
            .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        runtime.reached.notified(),
    )
    .await
    .unwrap();
    task.abort();
    let _ = task.await;
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if !runtime.kills.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(service.active.lock().await.contains_key(&id));
    service
        .request_value(json!({"op":"stop","appId":id}))
        .await
        .unwrap();
    assert_eq!(runtime.kills.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn cancellation_during_native_spawn_waits_until_handle_is_registered_and_reaped() {
    let (_root, service, runtime, id) = setup(true).await;
    runtime.delay_spawn.store(true, Ordering::SeqCst);
    runtime.fail_first_kill.store(false, Ordering::SeqCst);
    let waiting = service.clone();
    let app_id = id.clone();
    let task = tokio::spawn(async move {
        waiting
            .request_for_tool(json!({"op":"start","appId":app_id}))
            .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        runtime.spawning.notified(),
    )
    .await
    .unwrap();
    task.abort();
    let _ = task.await;
    assert!(runtime.kills.lock().unwrap().is_empty());
    runtime.release_spawn.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if !service.active.lock().await.contains_key(&id) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        runtime.kills.lock().unwrap().as_slice(),
        &["only-owned-task"]
    );
}

#[tokio::test]
async fn shared_maintenance_waits_for_successful_app_reap_and_reports_owner() {
    let (_root, service, runtime, id) = setup(false).await;
    assert_eq!(
        service
            .request_value(json!({"op":"start","appId":id}))
            .await
            .unwrap_err()
            .code,
        "stopFailed"
    );
    let shared = service.runtime().unwrap();
    let busy = shared.repair_rootfs().await.unwrap_err().to_string();
    assert!(
        busy.contains("runtimeBusy") && busy.contains(&id) && busy.contains("runId"),
        "{busy}"
    );
    assert_eq!(runtime.maintenance_calls.load(Ordering::SeqCst), 0);
    service
        .request_value(json!({"op":"stop","appId":id}))
        .await
        .unwrap();
    shared.repair_rootfs().await.unwrap();
    assert_eq!(runtime.maintenance_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn shared_maintenance_blocks_chat_and_other_workspace_app_start() {
    let (root, service, runtime, id) = setup(false).await;
    runtime.delay_maintenance.store(true, Ordering::SeqCst);
    let shared = service.runtime().unwrap();
    let maintaining = shared.clone();
    let repair = tokio::spawn(async move { maintaining.repair_rootfs().await });
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        runtime.maintaining.notified(),
    )
    .await
    .unwrap();
    assert!(service
        .request_value(json!({"op":"start","appId":id}))
        .await
        .unwrap_err()
        .message
        .contains("runtimeBusy"));
    let other = super::super::linux_maintenance::coordinate_runtime(
        &root.path().join("linux"),
        runtime.clone(),
    );
    let request: LinuxCommandRequest = serde_json::from_value(json!({"command":"chat-command","args":[],"cwd":null,"env":{},"stdin":null,"timeout_ms":1000,"network":"Allowed","mounts":[]})).unwrap();
    assert!(other
        .run(request)
        .await
        .unwrap_err()
        .to_string()
        .contains("runtimeBusy"));
    runtime.finish_maintenance.notify_one();
    repair.await.unwrap().unwrap();
}

#[tokio::test]
async fn maintenance_rejects_active_sdk_terminal_even_without_an_app_run() {
    let (_root, service, runtime, _id) = setup(false).await;
    runtime.tasks.lock().unwrap().push(MobileLinuxTaskSnapshot {
        task_id: "terminal-actual-sdk-id".into(),
        status: MobileLinuxTaskStatus::Running,
        command: "interactive shell".into(),
        started_at_ms: None,
        finished_at_ms: None,
        exit_code: None,
        detail: None,
    });
    let shared = service.runtime().unwrap();
    let error = shared.repair_rootfs().await.unwrap_err().to_string();
    assert!(error.contains("terminal-actual-sdk-id"));
    assert_eq!(runtime.maintenance_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn revoking_during_prepare_cancels_that_run_and_denies_a_new_start() {
    let (_root, service, runtime, id) = setup(true).await;
    runtime.fail_first_kill.store(false, Ordering::SeqCst);
    let starting = service.clone();
    let app_id = id.clone();
    let task = tokio::spawn(async move {
        starting
            .request_for_tool(json!({"op":"start","appId":app_id}))
            .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        runtime.reached.notified(),
    )
    .await
    .unwrap();
    service
        .request_value(json!({"op":"revoke","appId":id}))
        .await
        .unwrap();
    assert_eq!(task.await.unwrap().unwrap_err().code, "cancelled");
    assert_eq!(
        service
            .request_value(json!({"op":"start","appId":id}))
            .await
            .unwrap_err()
            .code,
        "consentRequired"
    );
    assert_eq!(
        runtime.kills.lock().unwrap().as_slice(),
        &["only-owned-task"]
    );
    assert!(service.active.lock().await.is_empty());
}

#[tokio::test]
async fn abandoned_native_spawn_retains_maintenance_gate_until_reap_retry_succeeds() {
    let (_root, service, runtime, _id) = setup(true).await;
    runtime.delay_spawn.store(true, Ordering::SeqCst);
    let shared = service.runtime().unwrap();
    let spawning = shared.clone();
    let request: LinuxCommandRequest = serde_json::from_value(json!({
        "command":"controlled install","args":[],"cwd":"/home/lingxi/.lingxi/local-apps/owned/work",
        "env":{"LOCAL_APP_RUN_ID":"owned-run","LOCAL_APP_DATA_DIR":"/home/lingxi/preparation-data"},
        "stdin":null,"timeout_ms":1000,"network":"Allowed","mounts":[],
    }))
    .unwrap();
    let task = tokio::spawn(async move { spawning.spawn_background(request).await });
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        runtime.spawning.notified(),
    )
    .await
    .unwrap();
    task.abort();
    let _ = task.await;
    assert!(shared
        .repair_rootfs()
        .await
        .unwrap_err()
        .to_string()
        .contains("runtimeBusy"));
    runtime.release_spawn.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while runtime.kills.lock().unwrap().len() < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    shared.repair_rootfs().await.unwrap();
    assert_eq!(
        runtime.kills.lock().unwrap().as_slice(),
        &["only-owned-task", "only-owned-task"]
    );
}
