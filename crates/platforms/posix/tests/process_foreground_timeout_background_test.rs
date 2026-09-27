//! PARITY 2.1.210: `run_foreground` moves a timed-out command to the background
//! (keeping it alive with output streaming to the task file) instead of killing
//! it, and returns its output normally when it finishes within the timeout.

#![cfg(unix)]

use platform_api::{
    ForegroundOutcome, ProcessCommand, ProcessRunner, SandboxBackend, SandboxedCommand,
    SandboxedTag,
};
use platform_posix::process::{task_output_path, PosixProcess};
use std::collections::HashMap;
use std::time::Duration;

fn mk_sandboxed(command: &str, args: Vec<&str>, timeout: Option<Duration>) -> SandboxedCommand {
    SandboxedCommand::__new_sandboxed(
        ProcessCommand {
            command: command.into(),
            args: args.into_iter().map(String::from).collect(),
            cwd: None,
            env: HashMap::new(),
            timeout,
            stdin: None,
        },
        SandboxedTag::Wrapped {
            backend: SandboxBackend::None,
        },
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_foreground_completes_fast_command() {
    let proc = PosixProcess::new();
    let cmd = mk_sandboxed(
        "/bin/sh",
        vec!["-c", "echo out; echo err 1>&2"],
        Some(Duration::from_secs(10)),
    );
    match proc.run_foreground(&cmd).await.expect("run_foreground") {
        ForegroundOutcome::Completed(out) => {
            assert_eq!(out.stdout.trim(), "out");
            assert_eq!(out.stderr.trim(), "err");
            assert_eq!(out.exit_code, 0);
            assert!(!out.timed_out);
        }
        ForegroundOutcome::MovedToBackground(_) => {
            panic!("fast command should complete, not move to background")
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_foreground_moves_timed_out_command_to_background() {
    let proc = PosixProcess::new();
    // Emit a line up front, then outlive the timeout: the child must be moved to
    // the background (not killed), and its output file must carry the early line.
    let cmd = mk_sandboxed(
        "/bin/sh",
        vec!["-c", "echo streamed; sleep 30"],
        Some(Duration::from_millis(400)),
    );
    let handle = match proc.run_foreground(&cmd).await.expect("run_foreground") {
        ForegroundOutcome::MovedToBackground(h) => h,
        ForegroundOutcome::Completed(_) => {
            panic!("a command that outlives its timeout must move to background")
        }
    };
    assert!(handle.pid > 0, "expected a live pid, got {}", handle.pid);

    // The detached reaper flushes the pre-timeout output to the task file.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let out_path = task_output_path(&handle.task_id);
    let contents = tokio::fs::read_to_string(&out_path)
        .await
        .expect("read task output file");
    assert!(
        contents.contains("streamed"),
        "backgrounded task output missing early line; got: {contents:?}"
    );

    // The process is still alive (not killed on timeout) — kill it cleanly.
    proc.kill(&handle).await.expect("kill");
}
