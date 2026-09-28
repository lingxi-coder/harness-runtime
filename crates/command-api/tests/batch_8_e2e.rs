#![cfg(feature = "builtin-handlers")]

//! End-to-end: build a registry with `register_all_builtin_commands` +
//! `register_core_batch_8`, dispatch each batch-8 command through the
//! `RegistrySlashDispatcher`, and verify behaviour against the default mock
//! orchestrator.
//!
//! Batch-8 commands: `/autocompact`, `/btw`, `/fork`, `/goal`, `/recap`,
//! `/reload-skills`, `/skill-doctor`, `/stop`.

use command_api::builtins::{register_all_builtin_commands, register_core_batch_8};
use command_api::CommandRegistry;
use command_api::RegistrySlashDispatcher;
use orchestrator::test_support::MockOrchestratorHandle;
use platform_api::{SlashCommandDispatcher, SlashDispatchResult};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

/// A fresh, uniquely-named temp directory (no `tempfile` dependency here — same
/// idiom as the handler unit tests). Callers are responsible for removing it.
fn tmp_root(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "lingxi-batch8-e2e-{tag}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create temp root");
    dir
}

/// Build a dispatcher with the full builtin surface plus batch-8, rooted in an
/// empty temp skill tree so `/reload-skills` and `/skill-doctor` render their
/// deterministic "nothing here" states. Returns the dispatcher, the mock
/// handle, and the temp root (kept alive so the caller can clean it up).
fn fresh(
    tag: &str,
) -> (
    RegistrySlashDispatcher,
    Arc<MockOrchestratorHandle>,
    PathBuf,
) {
    let root = tmp_root(tag);
    let cwd = root.join("repo");
    let home = root.join("home");
    let lingxi_home = home.join(".lingxi");
    std::fs::create_dir_all(cwd.join(".git")).expect("git marker");
    std::fs::create_dir_all(&lingxi_home).expect("lingxi home");

    let mut reg = CommandRegistry::new();
    register_all_builtin_commands(&mut reg);
    let handle: Arc<MockOrchestratorHandle> = Arc::new(MockOrchestratorHandle::new());
    let shared: Arc<RwLock<CommandRegistry>> = Arc::new(RwLock::new(CommandRegistry::new()));
    register_core_batch_8(
        &mut reg,
        handle.clone(),
        shared,
        cwd,
        lingxi_home,
        None,
        home,
        Vec::new(),
        false,
        false,
    );
    let d = RegistrySlashDispatcher::new(Arc::new(RwLock::new(reg)));
    (d, handle, root)
}

async fn handled(d: &RegistrySlashDispatcher, raw: &str) -> String {
    match d.dispatch(raw).await {
        SlashDispatchResult::Handled { display } => display,
        other => panic!("{raw} expected Handled, got {other:?}"),
    }
}

#[test]
fn all_8_batch_8_names_resolve() {
    let (d, _h, root) = fresh("resolve");
    // Access the underlying registry through a fresh build to assert resolution.
    let mut reg = CommandRegistry::new();
    register_all_builtin_commands(&mut reg);
    let handle: Arc<MockOrchestratorHandle> = Arc::new(MockOrchestratorHandle::new());
    let shared: Arc<RwLock<CommandRegistry>> = Arc::new(RwLock::new(CommandRegistry::new()));
    register_core_batch_8(
        &mut reg,
        handle,
        shared,
        PathBuf::from("."),
        PathBuf::from("."),
        None,
        PathBuf::from("."),
        Vec::new(),
        false,
        false,
    );
    for name in [
        "autocompact",
        "btw",
        "fork",
        "goal",
        "recap",
        "reload-skills",
        "skill-doctor",
        "stop",
    ] {
        assert!(reg.resolve(name).is_some(), "/{name} missing");
        assert!(reg.get_handler(name).is_some(), "/{name} handler missing");
    }
    drop(d);
    std::fs::remove_dir_all(root).ok();
}

#[tokio::test]
async fn btw_is_a_real_handle_bound_command_not_a_stub() {
    let (dispatcher, _handle, root) = fresh("btw");
    assert_eq!(
        handled(&dispatcher, "/btw").await,
        "Usage: /btw <your question>"
    );
    std::fs::remove_dir_all(root).ok();
}

#[tokio::test]
async fn autocompact_with_no_env_reports_auto_window() {
    // With no `LINGXI_AUTO_COMPACT_WINDOW` override the headless reporter shows
    // the model-default ("auto") status block.
    std::env::remove_var("LINGXI_AUTO_COMPACT_WINDOW");
    let (d, _h, root) = fresh("autocompact");
    let out = handled(&d, "/autocompact").await;
    assert!(
        out.starts_with("Auto-compact window: auto\n"),
        "unexpected /autocompact output: {out}"
    );
    assert!(out.contains("The actual threshold is the minimum of this setting"));
    std::fs::remove_dir_all(root).ok();
}

/// cc2.1.212: the DEFAULT (agent-view enabled) `/fork` (`vAd`) copies the
/// conversation into a new background session — the `[prompt]` argument is
/// optional, so a bare `/fork` reaches the `fork_to_background_session` seam
/// (no "Usage:" gate) and renders its system line. The mock returns a
/// deterministic background-session-copy line.
#[tokio::test]
async fn fork_without_prompt_copies_to_background_session() {
    // Ensure the default (agent-view enabled) surface regardless of ambient env.
    std::env::remove_var("CLAUDE_CODE_DISABLE_AGENT_VIEW");
    let (d, _h, root) = fresh("fork");
    assert_eq!(
        handled(&d, "/fork").await,
        "Copied conversation into a new background session (mock-bg-abcd)."
    );
    std::fs::remove_dir_all(root).ok();
}

#[tokio::test]
async fn goal_with_no_active_goal_reports_no_goal_set() {
    let (d, _h, root) = fresh("goal");
    assert_eq!(
        handled(&d, "/goal").await,
        "No goal set. Usage: `/goal <condition>`"
    );
    std::fs::remove_dir_all(root).ok();
}

#[tokio::test]
async fn recap_with_empty_transcript_reports_nothing_to_recap() {
    let (d, _h, root) = fresh("recap");
    assert_eq!(
        handled(&d, "/recap").await,
        "Nothing to recap yet — send a message first."
    );
    std::fs::remove_dir_all(root).ok();
}

#[tokio::test]
async fn reload_skills_on_empty_tree_reports_no_changes() {
    let (d, _h, root) = fresh("reload");
    assert_eq!(
        handled(&d, "/reload-skills").await,
        "Reloaded skills: 0 skills available (no changes)"
    );
    std::fs::remove_dir_all(root).ok();
}

#[tokio::test]
async fn skill_doctor_on_empty_tree_reports_no_skills() {
    let (d, _h, root) = fresh("doctor");
    assert_eq!(
        handled(&d, "/skill-doctor").await,
        "Skills loaded this session\n\n  (no skills loaded)"
    );
    std::fs::remove_dir_all(root).ok();
}

#[tokio::test]
async fn stop_requests_exit_and_returns_locked_literal() {
    // Ensure no stray job-dir rewrite path is exercised.
    std::env::remove_var("LINGXI_JOB_DIR");
    let (d, mock, root) = fresh("stop");
    assert_eq!(handled(&d, "/stop").await, "Session stopped.");
    assert!(mock.was_exit_requested());
    std::fs::remove_dir_all(root).ok();
}
