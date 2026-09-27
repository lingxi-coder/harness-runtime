//! Real-process witness that the production background drain polls health.
use async_trait::async_trait;
use platform_api::{
    BackgroundExitSink, BackgroundTaskBinding, ProcessCommand, ProcessRunner, SandboxedCommand,
    SandboxedTag,
};
use platform_posix::PosixProcess;
use std::sync::Arc;
use std::time::Duration;

#[derive(Default)]
struct Notices {
    tail: tokio::sync::Mutex<Option<String>>,
    ready: tokio::sync::Notify,
}
#[async_trait]
impl BackgroundExitSink for Notices {
    async fn on_exit(&self, _: &str, _: Option<i32>) {}
    async fn on_stall(&self, _: &str, tail: &str) {
        *self.tail.lock().await = Some(tail.to_string());
        self.ready.notify_one();
    }
}

#[tokio::test]
async fn production_background_drain_emits_stall_before_process_exit() {
    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("bwatchdog.output");
    std::fs::write(&output_path, "").unwrap();
    let notices = Arc::new(Notices::default());
    let command = SandboxedCommand::__new_sandboxed(
        ProcessCommand {
            command: "/bin/sh".into(),
            args: vec!["-c".into(), "printf 'Continue?'; sleep 120".into()],
            cwd: None,
            env: Default::default(),
            timeout: None,
            stdin: None,
        },
        SandboxedTag::BypassAuditedWithReason {
            reason: "watchdog_test".into(),
        },
    )
    .with_background_task(BackgroundTaskBinding {
        task_id: "bwatchdog".into(),
        output_path,
        on_exit: Some(notices.clone()),
        on_demand: None,
    });
    let process = PosixProcess::new();
    let handle = process.spawn_background(&command).await.unwrap();
    let observed = tokio::time::timeout(Duration::from_secs(65), notices.ready.notified()).await;
    process.kill(&handle).await.unwrap();
    observed.expect("50-second poll must deliver while child still runs");
    assert_eq!(notices.tail.lock().await.as_deref(), Some("Continue?"));
}
