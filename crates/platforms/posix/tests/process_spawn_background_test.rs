//! `spawn_background` lands a real handle and writes output to the task file.

#![cfg(unix)]

use mobile_linux_api::SandboxBackend;
use platform_api::{ProcessCommand, ProcessRunner, SandboxedCommand, SandboxedTag};
use platform_posix::process::{task_output_path, PosixProcess};
use std::collections::HashMap;
use std::time::Duration;

fn mk_sandboxed(command: &str, args: Vec<&str>) -> SandboxedCommand {
    SandboxedCommand::__new_sandboxed(
        ProcessCommand {
            command: command.into(),
            args: args.into_iter().map(String::from).collect(),
            cwd: None,
            env: HashMap::new(),
            timeout: None,
            stdin: None,
        },
        SandboxedTag::Wrapped {
            backend: SandboxBackend::None,
        },
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_background_writes_output_file_and_kills_cleanly() {
    let proc = PosixProcess::new();
    // Print "go" and sleep so we have something to read while killing.
    let cmd = mk_sandboxed("/bin/sh", vec!["-c", "echo go; sleep 30"]);
    let handle = proc.spawn_background(&cmd).await.expect("spawn_background");
    assert!(handle.pid > 0, "expected positive pid, got {}", handle.pid);

    // Give the child a moment to print "go".
    tokio::time::sleep(Duration::from_millis(300)).await;

    let out_path = task_output_path(&handle.task_id);
    let contents = tokio::fs::read_to_string(&out_path)
        .await
        .expect("read task output file");
    assert!(
        contents.contains("go"),
        "task output file missing expected line; got: {contents}"
    );

    // Kill cleanly via the runner.
    proc.kill(&handle).await.expect("kill");
}
