//! Native acceptance of the exact private Team wrapper, without model traffic.
//! Run outside an enclosing Seatbelt sandbox; refusal is a failure, never a skip.
#![cfg(target_os = "macos")]

#[path = "../../src/desktop/team_sandbox.rs"]
mod team_sandbox;

use sandbox::runtime_config::Platform;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use tool_api::isolated_shell::IsolatedShellPolicy;
use tool_api::SandboxRunner;

fn quote(value: &Path) -> String {
    format!("'{}'", value.to_str().unwrap().replace('\'', "'\\''"))
}

fn success(output: Output, context: &str) -> String {
    assert!(
        output.status.success(),
        "{context}: status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).unwrap()
}

fn fixture(root: &Path) -> PathBuf {
    let executable = root.join("workspace/process-boundary-probe");
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/desktop/fixtures/team_process_boundary.c");
    success(
        Command::new("/usr/bin/clang")
            .arg("-Wno-deprecated-declarations")
            .arg(source)
            .arg("-o")
            .arg(&executable)
            .output()
            .expect("compile native acceptance fixture"),
        "compile native acceptance fixture",
    );
    executable
}

async fn run_private(policy: Arc<IsolatedShellPolicy>, command: &str) -> Output {
    let runner = team_sandbox::TeamSandboxRunner(policy.clone());
    let wrapped = runner
        .wrap(
            command,
            policy.sandbox(),
            Platform::Mac,
            None,
            Some(policy.workspace()),
        )
        .await
        .expect("private wrapper");
    Command::new(IsolatedShellPolicy::EXECUTABLE)
        .args(IsolatedShellPolicy::arguments(wrapped))
        .env_clear()
        .envs(policy.environment())
        .current_dir(policy.workspace())
        .process_group(0)
        .output()
        .expect("launch private wrapper")
}

#[tokio::test]
async fn private_team_shell_retains_fork_exec_but_cannot_change_its_process_group() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    for child in ["workspace", "home", "shell-home", "tmp", "inputs"] {
        std::fs::create_dir(root.join(child)).unwrap();
    }
    let policy = Arc::new(IsolatedShellPolicy::new(&root));
    let executable = fixture(&root);
    // Each control forks a nonleader, so setsid/setpgid would succeed without
    // the policy. It never leaves a long-running process behind.
    for mode in [
        "setsid",
        "setpgid",
        "raw-setsid",
        "raw-setpgid",
        "spawn-session",
        "spawn-group",
        "spawn",
    ] {
        let positive = success(
            Command::new(&executable)
                .args([mode, "allow"])
                .output()
                .expect("unconfined positive control"),
            &format!("positive control {mode}"),
        );
        assert!(positive.contains("errno=0"), "{positive}");
        let denied = success(
            run_private(
                policy.clone(),
                &format!("{} {mode} deny", quote(&executable)),
            )
            .await,
            &format!("private profile {mode}"),
        );
        assert!(denied.contains("result=-1"), "{denied}");
        assert!(denied.contains("group_unchanged=1"), "{denied}");
    }
    let output = success(
        run_private(
            policy,
            "printf 'fork-exec:'; /usr/bin/printf 'works\\n' | /usr/bin/tr a-z A-Z; printf 'substitution:%s\\n' \"$(/bin/echo child)\"",
        )
        .await,
        "ordinary Bash fork/exec",
    );
    assert_eq!(output, "fork-exec:WORKS\nsubstitution:child\n");
}
