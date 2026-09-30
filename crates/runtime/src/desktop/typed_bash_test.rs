use super::*;
use lingxi_core::host::{ProcessCommand, ProcessRunner, SandboxedCommand, SandboxedTag};
use tool_api::bash_runner::BashRunner;

fn command(script: &str, cwd: &std::path::Path) -> SandboxedCommand {
    SandboxedCommand::__new_sandboxed(
        ProcessCommand {
            command: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            cwd: Some(cwd.to_path_buf()),
            env: Default::default(),
            timeout: Some(std::time::Duration::from_secs(10)),
            stdin: None,
        },
        SandboxedTag::BypassAuditedWithReason {
            reason: "isolated typed Bash regression".into(),
        },
    )
}

async fn child_pid(path: &std::path::Path) -> u32 {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if let Ok(text) = std::fs::read_to_string(path) {
                if let Ok(pid) = text.trim().parse() {
                    break pid;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("native child published its PID")
}
fn exists(pid: u32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok()
}

#[tokio::test]
async fn typed_bash_cancel_joins_native_child_and_preserves_foreign_owner() {
    let temp = tempfile::tempdir().unwrap();
    let inner: Arc<dyn ProcessRunner> = Arc::new(platform_posix::PosixProcess::new());
    let own_cancel = tokio_util::sync::CancellationToken::new();
    let other_cancel = tokio_util::sync::CancellationToken::new();
    let owner = format!("typed-test-{}", lingxi_core::types::ToolUseId::new());
    let foreign_owner = format!("foreign-test-{}", lingxi_core::types::ToolUseId::new());
    let owned = Arc::new(TypedBashProcess {
        inner: inner.clone(),
        owner: owner.clone(),
        cancel: own_cancel.clone(),
    });
    let foreign = Arc::new(TypedBashProcess {
        inner,
        owner: foreign_owner.clone(),
        cancel: other_cancel.clone(),
    });
    let a = command("printf '%s' \"$$\" > owned.pid; exec sleep 30", temp.path());
    let b = command(
        "printf '%s' \"$$\" > foreign.pid; exec sleep 30",
        temp.path(),
    );
    let own_task =
        tokio::spawn(async move { owned.run_foreground_with_output_limit(&a, Some(100)).await });
    let foreign_task = tokio::spawn(async move {
        foreign
            .run_foreground_with_output_limit(&b, Some(100))
            .await
    });
    let pid = child_pid(&temp.path().join("owned.pid")).await;
    let foreign_pid = child_pid(&temp.path().join("foreign.pid")).await;
    assert_eq!(
        lingxi_core::host::agent_processes::snapshot(&owner),
        vec![pid]
    );
    own_cancel.cancel();
    let result = tokio::time::timeout(std::time::Duration::from_secs(3), own_task)
        .await
        .unwrap()
        .unwrap();
    assert!(result.is_err(), "cancel is not a successful shell response");
    assert!(
        !exists(pid),
        "return is after native wait/reap, not after a signal"
    );
    assert!(lingxi_core::host::agent_processes::snapshot(&owner).is_empty());
    assert!(exists(foreign_pid), "another invocation remains alive");
    assert!(!foreign_task.is_finished());
    other_cancel.cancel();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(3), foreign_task)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert!(!exists(foreign_pid));
    assert!(lingxi_core::host::agent_processes::snapshot(&foreign_owner).is_empty());
}

#[tokio::test]
async fn typed_bash_cancel_before_delayed_poll_never_launches() {
    let temp = tempfile::tempdir().unwrap();
    let token = tokio_util::sync::CancellationToken::new();
    let mut ctx = tool_api::test_support::shell_test_ctx_in(
        mobile_linux_api::ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        },
        temp.path().to_path_buf(),
    );
    ctx.process = Arc::new(platform_posix::PosixProcess::new());
    let runner = DesktopBashRunner { ctx };
    let pending = runner.run_with_cancel("printf ran > should-not-exist", token.clone());
    token.cancel();
    let output = pending.await;
    assert_ne!(output.exit_code, 0);
    assert!(!temp.path().join("should-not-exist").exists());
}

#[tokio::test]
async fn desktop_typed_bash_api_interrupt_stops_real_shell() {
    let temp = tempfile::tempdir().unwrap();
    let mut ctx = tool_api::test_support::shell_test_ctx_in(
        mobile_linux_api::ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        },
        temp.path().to_path_buf(),
    );
    ctx.process = Arc::new(platform_posix::PosixProcess::new());
    let runner = DesktopBashRunner { ctx };
    let token = tokio_util::sync::CancellationToken::new();
    let cancel = token.clone();
    let task = tokio::spawn(async move {
        runner
            .run_with_cancel("printf '%s' \"$$\" > api.pid; exec sleep 30", token)
            .await
    });
    let pid = child_pid(&temp.path().join("api.pid")).await;
    cancel.cancel();
    let output = tokio::time::timeout(std::time::Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(output.exit_code, 0);
    assert!(!exists(pid));
}

#[tokio::test]
async fn typed_bash_supervisor_helper() {
    let Ok(role) = std::env::var("LINGXI_TYPED_SUPERVISOR_ROLE") else {
        return;
    };
    let directory =
        std::path::PathBuf::from(std::env::var_os("LINGXI_TYPED_SUPERVISOR_DIR").unwrap());
    if role == "supervisor" {
        platform_posix::process::supervisor::serve_supervisor(
            directory,
            super::supervisor_exit_sink,
        )
        .await
        .unwrap();
        return;
    }
    if role == "preaccept-disconnect" {
        preaccept_disconnect(&directory).await;
        return;
    }
    platform_posix::process::supervisor::enable_supervisor(std::path::PathBuf::from(
        std::env::var_os("LINGXI_TYPED_SUPERVISOR_WRAPPER").unwrap(),
    ));
    let token = tokio_util::sync::CancellationToken::new();
    let owner = format!("typed-supervised-{}", lingxi_core::types::ToolUseId::new());
    let process = Arc::new(TypedBashProcess {
        inner: Arc::new(platform_posix::PosixProcess::new()),
        owner: owner.clone(),
        cancel: token.clone(),
    });
    let output = directory.join("supervised.output");
    let cmd = command(
        "printf '%s' \"$$\" > supervised.pid; exec sleep 30",
        &directory,
    )
    .with_background_task(lingxi_core::host::BackgroundTaskBinding {
        task_id: format!("typed-supervised-{}", lingxi_core::types::ToolUseId::new()),
        output_path: output.clone(),
        on_exit: Some(super::supervisor_exit_sink(&output)),
        on_demand: None,
    });
    let task = tokio::spawn(async move {
        process
            .run_foreground_with_output_limit(&cmd, Some(100))
            .await
    });
    let pid = child_pid(&directory.join("supervised.pid")).await;
    assert_eq!(
        lingxi_core::host::agent_processes::snapshot(&owner),
        vec![pid]
    );
    token.cancel();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert!(
        !exists(pid),
        "enabled supervisor returned only after its native child was reaped"
    );
    assert!(lingxi_core::host::agent_processes::snapshot(&owner).is_empty());
    std::fs::write(directory.join("settled"), "verified").unwrap();
}

#[tokio::test]
async fn typed_bash_enabled_supervisor_cancellation_is_settled() {
    run_supervisor_fixture("source", "settled").await;
}

#[tokio::test]
async fn typed_bash_preaccept_disconnect_closes_gate_and_reaps_child() {
    run_supervisor_fixture("preaccept-disconnect", "preaccept-settled").await;
}

async fn run_supervisor_fixture(role: &str, marker: &str) {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let executable = std::env::current_exe().unwrap();
    let wrapper = directory.path().join("supervisor-bootstrap");
    let quoted = |value: &str| format!("'{}'", value.replace('\'', "'\"'\"'"));
    let helper = "desktop::typed_bash_cancellation_tests::typed_bash_supervisor_helper";
    std::fs::write(&wrapper, format!(
        "#!/bin/sh\nexport LINGXI_TYPED_SUPERVISOR_ROLE=supervisor\nexport LINGXI_TYPED_SUPERVISOR_DIR=\"$2\"\nexec {} --exact {} --nocapture\n",
        quoted(executable.to_str().unwrap()), helper,
    )).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let home = directory.path().join("fixture-home");
    std::fs::create_dir(&home).unwrap();
    let status = tokio::process::Command::new(executable)
        .args(["--exact", helper, "--nocapture"])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("LINGXI_CONFIG_DIR", &home)
        .env("LINGXI_TYPED_SUPERVISOR_ROLE", role)
        .env("LINGXI_TYPED_SUPERVISOR_DIR", directory.path())
        .env("LINGXI_TYPED_SUPERVISOR_WRAPPER", &wrapper)
        .env("RUST_MIN_STACK", "33554432")
        .status()
        .await
        .unwrap();
    assert!(status.success());
    assert_eq!(
        std::fs::read_to_string(directory.path().join(marker)).unwrap(),
        "verified"
    );
}

// Exercise the actual Start socket lifecycle without acknowledging Spawned.
// The child is still held in the platform pre-exec capability gate when the
// source disconnects; a cancelled gate must never execute this benign payload.
async fn preaccept_disconnect(directory: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;
    let supervisor_dir = directory.join("preaccept-supervisor");
    std::fs::create_dir(&supervisor_dir).unwrap();
    std::fs::set_permissions(&supervisor_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let directory_identity = lingxi_core::host::rooted_fs::root_identity(&supervisor_dir).unwrap();
    let output_root_identity = lingxi_core::host::rooted_fs::root_identity(directory).unwrap();
    let output = directory.join("preaccept.output");
    let file = lingxi_core::host::rooted_fs::open_append_file_pinned(
        directory,
        std::path::Path::new("preaccept.output"),
        Some(&output_root_identity),
    )
    .unwrap();
    let output_file_identity = lingxi_core::host::rooted_fs::opened_file_identity(&file).unwrap();
    drop(file);
    let mut supervisor =
        tokio::process::Command::new(std::env::var_os("LINGXI_TYPED_SUPERVISOR_WRAPPER").unwrap())
            .args([
                std::ffi::OsStr::new("--lingxi-shell-supervisor"),
                supervisor_dir.as_os_str(),
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
    let socket = supervisor_dir.join("socket");
    let mut start = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Ok(stream) = UnixStream::connect(&socket).await {
                break stream;
            }
            assert!(
                supervisor.try_wait().unwrap().is_none(),
                "supervisor exited before listen"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let nonce = lingxi_core::types::ToolUseId::new().to_string();
    let cmd = command("printf ran > preaccept-payload.started", directory);
    let request = serde_json::json!({
        "request": "Start", "command": cmd.inner(), "tag": cmd.tag(),
        "owner": "preaccept-disconnect", "auto_background": false,
        "explicit": false, "limit": 100, "task_id": "preaccept-disconnect",
        "output": output, "nonce": nonce,
        "supervisor_directory_identity": directory_identity,
        "output_root_identity": output_root_identity,
        "output_file_identity": output_file_identity,
    });
    let mut bytes = serde_json::to_vec(&request).unwrap();
    bytes.push(b'\n');
    start.write_all(&bytes).await.unwrap();
    let mut spawn = UnixStream::connect(&socket).await.unwrap();
    let mut bytes =
        serde_json::to_vec(&serde_json::json!({"request": "AwaitSpawn", "nonce": nonce})).unwrap();
    bytes.push(b'\n');
    spawn.write_all(&bytes).await.unwrap();
    let mut response = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        BufReader::new(spawn).read_line(&mut response),
    )
    .await
    .unwrap()
    .unwrap();
    let response: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(response["response"], "Spawned", "{response}");
    let pid = response["handoff"]["pid"].as_u64().unwrap() as u32;
    assert!(
        exists(pid),
        "real gated child exists before source disconnect"
    );
    assert!(!directory.join("preaccept-payload.started").exists());
    drop(start); // No SpawnAccepted request was sent.
    let status = tokio::time::timeout(std::time::Duration::from_secs(5), supervisor.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(
        status.success(),
        "supervisor finishes only after its held runner joins"
    );
    assert!(
        !exists(pid),
        "pre-exec child was reaped before supervisor exit"
    );
    assert!(
        !directory.join("preaccept-payload.started").exists(),
        "cancelled capability never executes the payload"
    );
    std::fs::write(directory.join("preaccept-settled"), "verified").unwrap();
}
