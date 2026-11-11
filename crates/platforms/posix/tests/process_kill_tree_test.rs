//! `kill_tree_unix` kills the whole process group of a setsid-detached child.
//!
//! Forks a `/bin/sh` that backgrounds two `sleep 60` grandchildren and
//! `wait`s. Without `setsid()` + `killpg(2)` the grandchildren would
//! survive the parent's exit. We verify the entire tree is gone after
//! `kill_tree_unix` returns.

#![cfg(unix)]

use platform_posix::process::kill_tree::{kill_tree_force, kill_tree_with_grace};
use platform_posix::process::spawn_unsafe::attach_setsid;
use std::time::Duration;
use tokio::process::Command;

struct OwnedTestGroup {
    child: tokio::process::Child,
}

impl Drop for OwnedTestGroup {
    fn drop(&mut self) {
        // Retain the child identity through failure cleanup. A completed wait
        // clears it, so success cannot signal a subsequently reused PGID.
        if let Some(pid) = self.child.id() {
            let _ = kill_tree_force(pid);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_tree_terminates_grandchildren() {
    // Concurrent test runs must never satisfy one another's readiness check.
    let marker_directory = tempfile::Builder::new()
        .prefix("lingxi-kt-marker-")
        .tempdir()
        .unwrap();
    let marker = marker_directory
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    // POSIX sh accepts a positional $0; dash does not support `exec -a`.
    // Keep both marked shells alive while they wait for their sleep children.
    let script = format!(
        r#"/bin/sh -c 'sleep 60 & wait' {marker} &
           /bin/sh -c 'sleep 60 & wait' {marker} &
           wait"#,
    );

    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(&script).kill_on_drop(true);
    attach_setsid(&mut cmd);

    let mut group = OwnedTestGroup {
        child: cmd.spawn().expect("spawn parent shell"),
    };
    let pid = group.child.id().expect("pid");

    // Wait for both child shells, rather than assuming a fixed fork delay.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if run_ps_filter(&marker).len() >= 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the parent and both marked child shells must be running");

    // Kill the whole tree with a short grace so the test stays under 5 s.
    kill_tree_with_grace(pid, Duration::from_millis(200))
        .await
        .expect("kill_tree");

    // The parent shell must exit promptly.
    let outcome = tokio::time::timeout(Duration::from_secs(5), group.child.wait())
        .await
        .expect("child exits within 5s after kill_tree")
        .expect("wait");
    // The shell either dies on SIGTERM (signal exit, no code) or SIGKILL
    // (signal exit). Either way the wait completes.
    let _ = outcome;

    // The grandchildren may take a tick to be reaped — poll briefly.
    let mut leaked: Vec<String> = Vec::new();
    for _ in 0..20 {
        leaked = run_ps_filter(&marker);
        if leaked.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("leaked {marker} processes after kill_tree: {leaked:?}");
}

fn run_ps_filter(marker: &str) -> Vec<String> {
    // `ps -A` enumerates all processes; `-o command=` prints the full
    // argv with no header so substring matching works on macOS + Linux.
    let ps = std::process::Command::new("ps")
        .args(["-A", "-o", "pid,command"])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&ps.stdout)
        .lines()
        .filter(|line| line.contains(marker))
        .map(str::to_owned)
        .collect()
}
