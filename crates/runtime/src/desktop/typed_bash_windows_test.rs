use super::*;
use lingxi_core::host::{
    BackgroundExitSink, BackgroundTaskBinding, ProcessCommand, ProcessRunner, SandboxedCommand,
    SandboxedTag,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

struct SourceSink;
#[async_trait::async_trait]
impl BackgroundExitSink for SourceSink {
    fn manages_output(&self) -> bool {
        true
    }
    async fn on_exit(&self, _: &str, _: Option<i32>) {}
}

fn driver() -> PathBuf {
    PathBuf::from(
        std::env::var_os("HARNESS_WINDOWS_TYPED_FIXTURE").expect("fixture executable path"),
    )
}

fn command(directory: &Path, timeout: Duration) -> SandboxedCommand {
    SandboxedCommand::__new_sandboxed(
        ProcessCommand {
            command: driver().to_string_lossy().into_owned(),
            args: vec!["--leader".into(), directory.to_string_lossy().into_owned()],
            cwd: Some(directory.to_path_buf()),
            env: Default::default(),
            timeout: Some(timeout),
            stdin: None,
        },
        SandboxedTag::BypassAuditedWithReason {
            reason: "isolated typed Bash Windows regression".into(),
        },
    )
    .with_background_task(BackgroundTaskBinding {
        task_id: format!("typed-windows-{}", lingxi_core::types::ToolUseId::new()),
        output_path: directory.join("private-output").join("output"),
        on_exit: Some(Arc::new(SourceSink)),
        on_demand: None,
    })
}

async fn wait_for_file(path: &Path) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fixture did not publish its marker");
}

async fn held_child(directory: &Path) -> platform_windows::process::test_support::HeldProcess {
    wait_for_file(&directory.join("child.ready")).await;
    let pid = std::fs::read_to_string(directory.join("child.pid"))
        .unwrap()
        .parse()
        .unwrap();
    platform_windows::process::test_support::HeldProcess::open(pid).unwrap()
}

async fn wait_held_exit(held: &platform_windows::process::test_support::HeldProcess) -> bool {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match held.is_alive() {
                Some(false) => break true,
                None => break false,
                Some(true) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .unwrap_or(false)
}

struct Fixture(tempfile::TempDir);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::write(self.0.path().join("release"), "release");
    }
}

#[tokio::test]
async fn real_typed_bash_stop_preserves_foreground_contract_and_signals_job() {
    let fixture = Fixture(tempfile::tempdir().unwrap());
    platform_windows::process::supervisor::enable_supervisor(driver());
    let token = tokio_util::sync::CancellationToken::new();
    let process = TypedBashProcess {
        inner: Arc::new(platform_windows::process::WindowsProcess::new()),
        owner: format!("typed-stop-{}", lingxi_core::types::ToolUseId::new()),
        cancel: token.clone(),
    };
    let cmd = command(fixture.0.path(), Duration::from_secs(30));
    let running = tokio::spawn(async move {
        process
            .run_foreground_with_output_limit(&cmd, Some(1024))
            .await
    });
    let held = held_child(fixture.0.path()).await;
    assert_eq!(held.is_alive(), Some(true));
    token.cancel();
    let result = tokio::time::timeout(Duration::from_secs(20), running).await;
    let signaled = wait_held_exit(&held).await;
    std::fs::write(fixture.0.path().join("release"), "release").unwrap();
    eprintln!("typed-stop-probe: result={result:?}; held_signaled={signaled}");
    assert!(
        matches!(result, Ok(Ok(Err(_)))),
        "Stop cannot return a successful or background result"
    );
    assert!(
        signaled,
        "Stop must reach the actual retained descendant handle"
    );
}

#[tokio::test]
async fn real_typed_bash_timeout_cannot_report_success_with_live_writer() {
    let fixture = Fixture(tempfile::tempdir().unwrap());
    platform_windows::process::supervisor::enable_supervisor(driver());
    let process = TypedBashProcess {
        inner: Arc::new(platform_windows::process::WindowsProcess::new()),
        owner: format!("typed-timeout-{}", lingxi_core::types::ToolUseId::new()),
        cancel: tokio_util::sync::CancellationToken::new(),
    };
    let cmd = command(fixture.0.path(), Duration::from_secs(5));
    let running = tokio::spawn(async move {
        process
            .run_foreground_with_output_limit(&cmd, Some(1024))
            .await
    });
    let held = held_child(fixture.0.path()).await;
    assert_eq!(held.is_alive(), Some(true));
    let result = tokio::time::timeout(Duration::from_secs(20), running).await;
    let was_alive = held.is_alive();
    std::fs::write(fixture.0.path().join("release"), "release").unwrap();
    let eventually_signaled = wait_held_exit(&held).await;
    eprintln!("typed-timeout-probe: result={result:?}; held_before_release={was_alive:?}; held_after_release={eventually_signaled}");
    assert!(
        matches!(result, Ok(Ok(Err(_)))),
        "an open inherited writer is not a successful foreground completion"
    );
    assert!(
        eventually_signaled,
        "the original child handle must eventually signal"
    );
}
