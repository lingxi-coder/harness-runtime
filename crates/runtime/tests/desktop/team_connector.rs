#![cfg(target_os = "macos")]
//! Actual major 3 host entry tests. Backend lifecycle is covered by Desktop/Emulator.
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

fn private(root: &Path, name: &str) -> std::path::PathBuf {
    let path = root.join(name);
    std::fs::create_dir(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path
}
fn invoke(version: u32, inherited: bool) -> (Value, bool) {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let home = private(&root, "home");
    let tmp = private(&root, "tmp");
    let storage = private(&root, "runtime");
    let mut command = Command::new(env!("CARGO_BIN_EXE_harness-team-host"));
    command
        .env_clear()
        .env("HOME", &home)
        .env(branding::CONFIG_DIR_ENV, &home)
        .env("TMPDIR", &tmp)
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .env("CFFIXED_USER_HOME", &home)
        .env(
            "__CF_USER_TEXT_ENCODING",
            format!("0x{:X}:0x8000100:0x0", nix::unistd::geteuid().as_raw()),
        )
        .current_dir(&home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if inherited {
        command.env("OWNER_SECRET", "must-not-inherit");
    }
    let mut child = command.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let bootstrap = json!({"version":version,"type":"inspect_scope","storageRoot":storage,"workerExecutable":"/unused-worker",
      "scope":{"teamId":"team00001","sharedBotId":"sharedbot001","conversationId":"chat00001","sessionEpoch":1}});
    writeln!(stdin, "{bootstrap}").unwrap();
    stdin.flush().unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let success = child.wait().unwrap().success();
    (serde_json::from_str(&line).unwrap(), success)
}
#[test]
fn major_three_inspects_empty_canonical_scope_without_model() {
    let (result, success) = invoke(3, false);
    assert!(success);
    assert_eq!(result["status"], "inspected");
    assert_eq!(result["cursor"]["sessionRevision"], 0);
}
#[test]
fn retired_major_is_rejected_without_execution() {
    let (result, success) = invoke(1, false);
    assert!(!success);
    assert_eq!(result["type"], "error");
    assert_eq!(result["version"], 3);
}
#[test]
fn inherited_owner_environment_is_rejected() {
    let (result, success) = invoke(3, true);
    assert!(!success);
    assert_eq!(result["code"], "ENVIRONMENT");
}
