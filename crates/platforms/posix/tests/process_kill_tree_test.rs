//! `kill_tree_unix` kills the whole process group of a setsid-detached child.
//!
//! Forks a `/bin/sh` that backgrounds two `sleep 60` grandchildren and
//! `wait`s. Without `setsid()` + `killpg(2)` the grandchildren would
//! survive the parent's exit. We verify the entire tree is gone after
//! `kill_tree_unix` returns.

#![cfg(unix)]

use platform_posix::process::kill_tree::kill_tree_with_grace;
use platform_posix::process::spawn_unsafe::attach_setsid;
use std::time::Duration;
use tokio::process::Command;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_tree_terminates_grandchildren() {
    // A marker we can grep for in `ps` without false positives from other
    // `sleep` calls running on the host. Random-ish but constant so the
    // assertion below is deterministic.
    let marker = "lingxi-kt-marker-7d9c";
    // POSIX sh accepts a positional $0; dash does not support `exec -a`.
    // Keep both marked shells alive while they wait for their sleep children.
    let script = format!(
        r#"/bin/sh -c 'sleep 60 & wait' {marker} &
           /bin/sh -c 'sleep 60 & wait' {marker} &
           wait"#,
    );

    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(&script);
    attach_setsid(&mut cmd);

    let mut child = cmd.spawn().expect("spawn parent shell");
    let pid = child.id().expect("pid");

    // Wait for both child shells, rather than assuming a fixed fork delay.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if run_ps_filter(marker).len() >= 3 {
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
    let outcome = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("child exits within 5s after kill_tree")
        .expect("wait");
    // The shell either dies on SIGTERM (signal exit, no code) or SIGKILL
    // (signal exit). Either way the wait completes.
    let _ = outcome;

    // The grandchildren may take a tick to be reaped — poll briefly.
    let mut leaked: Vec<String> = Vec::new();
    for _ in 0..20 {
        leaked = run_ps_filter(marker);
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
