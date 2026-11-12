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
    invoke_with(version, inherited, "inspect_scope", json!({}))
}
fn invoke_with(version: u32, inherited: bool, kind: &str, extra: Value) -> (Value, bool) {
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
    let mut bootstrap = json!({"version":version,"type":kind,"storageRoot":storage,"workerExecutable":"/unused-worker",
      "scope":{"teamId":"team00001","sharedBotId":"sharedbot001","conversationId":"chat00001","sessionEpoch":1}});
    for (key, value) in extra.as_object().unwrap() {
        bootstrap[key] = value.clone();
    }
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

fn chat_model() -> Value {
    json!({"model_id":"fixture-chat-model","model_provider":"fixture-openai-chat","model_protocol":"open_ai_chat",
      "model_profile":{"provider_id":"fixture","profile_name":"fixture-openai-chat","base_url":"https://fixture.invalid/v1",
        "protocol":"open_ai_chat","auth":"api_key","credential":{"source":"host_managed","key":"team-selected-model"},
        "models":[{"display_model":"fixture-chat-model","request_model":"fixture-chat-model","billing_model":"fixture-chat-model",
        "capability_support":{"streaming":"supported","tools":"supported"}}],"extra":{}},
      "model_endpoint":"https://fixture.invalid/v1"})
}
#[test]
fn inspect_task_reports_an_unadmitted_task_without_a_model_or_worker() {
    let (result, success) = invoke_with(3, false, "inspect_task", json!({"taskId":"task_00000001","model":chat_model()}));
    assert!(success, "{result}");
    assert_eq!(result["status"], "task_checkpoint");
    assert_eq!(result["version"], 3);
    assert_eq!(result["phase"], "absent");
    assert_eq!(result["continuable"], false);
    assert_eq!(result["reason"], "not_admitted");
    assert_eq!(result["attempts"], 0);
    assert_eq!(result["dispatchesObserved"], 0);
    assert_eq!(result["pendingToolCalls"], json!([]));
    assert_eq!(result["cursor"]["sessionRevision"], 0);
    assert_eq!(result["checkpointDigest"].as_str().unwrap().len(), 64);
}
#[test]
fn inspect_task_and_continue_reject_malformed_bootstraps_before_any_io() {
    for (kind, extra) in [
        ("inspect_task", json!({"model":chat_model()})),
        ("inspect_task", json!({"taskId":"task_00000001"})),
        ("inspect_scope", json!({"continuation":{"epoch":2,"expected_checkpoint_digest":"0".repeat(64)}})),
        ("continue", json!({"taskId":"task_00000001"})),
        ("inspect_scope", json!({"askRequester":true})),
        ("inspect_scope", json!({"sharedList":{"title":"Groceries","description":"What to buy"}})),
        ("inspect_task", json!({"taskId":"task_00000001","model":chat_model(),"sharedList":{"title":"Groceries","description":"What to buy"}})),
        ("inspect_task", json!({"taskId":"task_00000001","model":chat_model(),"askRequester":true})),
        ("run", json!({"taskId":"task_00000001","continuation":{"epoch":2,"expected_checkpoint_digest":"0".repeat(64)}})),
    ] {
        let (result, success) = invoke_with(3, false, kind, extra);
        assert!(!success, "{kind}");
        assert_eq!(result["type"], "error");
        assert_eq!(result["code"], "PROTOCOL");
    }
}
