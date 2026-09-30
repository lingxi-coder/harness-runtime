//! Parity: lock the 86 builtin slash-command names plus the per-command
//! command/target status matrix across the full surface.
//!
//! See plan `docs/superpowers/plans/2026-05-25-m5-09-commands-surface.md`
//! Task 6. Locks (2026-06-20 slash-parity pass #66/#67 re-locked from 99→94 —
//! removed cost/stats as /usage aliases + deleted vim/pr-comments/output-style):
//!
//! - Total name count = 86
//! - Core name count = 19
//! - Target implemented status is explicit per command
//! - Stub literal template = "{name}: not implemented in v0.6.0 (M5)"
//! - Unknown literal template = "Unknown command: /{name}"

use command_api::builtin_support::names::{
    BUILTIN_COMMAND_NAMES, BUILTIN_CORE_NAMES, CORRECT_BY_DESIGN_STUBS, HOST_BOUND_DEFERRED_GAPS,
};
use command_api::builtins::{
    register_all_builtin_commands, register_core_batch_1, register_core_batch_2,
    register_core_batch_4, register_core_batch_5, register_core_batch_8,
};
use command_api::CommandRegistry;
use command_api::RegistrySlashDispatcher;
use lingxi_core::host::{
    AuthError, AuthHandle, LoginInfo, SlashCommandDispatcher, SlashDispatchResult,
};
use orchestrator::test_support::MockOrchestratorHandle;
use serde::Deserialize;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use test_harness::parity::load_fixture;
use tokio::sync::RwLock;

struct MockAuth {
    result: StdMutex<Result<LoginInfo, AuthError>>,
}

#[async_trait::async_trait]
impl AuthHandle for MockAuth {
    async fn login(&self) -> Result<LoginInfo, AuthError> {
        self.result.lock().unwrap().clone()
    }

    async fn logout(&self) -> Result<(), AuthError> {
        Ok(())
    }

    async fn current_user(&self) -> Option<LoginInfo> {
        self.result.lock().unwrap().as_ref().ok().cloned()
    }
}

fn fully_wired_registry() -> CommandRegistry {
    let mut reg = CommandRegistry::new();
    register_all_builtin_commands(&mut reg);
    let handle: Arc<MockOrchestratorHandle> = Arc::new(MockOrchestratorHandle::new());
    register_core_batch_1(&mut reg, handle.clone());
    let auth: Arc<dyn AuthHandle> = Arc::new(MockAuth {
        result: StdMutex::new(Ok(LoginInfo {
            email: "u@example.com".to_string(),
            org_id: "org".to_string(),
        })),
    });
    register_core_batch_2(&mut reg, handle.clone(), auth);
    register_core_batch_4(&mut reg, handle.clone());
    register_core_batch_5(&mut reg, handle.clone());
    register_core_batch_8(
        &mut reg,
        handle,
        Arc::new(RwLock::new(CommandRegistry::new())),
        std::path::PathBuf::from("."),
        std::path::PathBuf::from("."),
        None,
        std::path::PathBuf::from("."),
        Vec::new(),
        false,
        false,
    );
    reg
}

#[derive(Debug, Deserialize)]
struct ParityFile {
    #[serde(rename = "_meta")]
    meta: ParityMeta,
    commands: Vec<ParityCommand>,
}

#[derive(Debug, Deserialize)]
struct ParityMeta {
    total_count_lock: usize,
    core_count_lock: usize,
    unimplemented_count_lock: usize,
    stub_literal_template: String,
    unknown_literal_template: String,
}

#[derive(Debug, Deserialize)]
struct ParityCommand {
    name: String,
    is_core: bool,
    #[serde(default)]
    description: Option<String>,
}

// ============================================================================
// T3 — V2 fixture types: adds `implemented` field (M5-14)
// ============================================================================

#[derive(Debug, Deserialize)]
struct ParityFileV2 {
    #[serde(rename = "_meta")]
    #[allow(dead_code)]
    meta: ParityMeta,
    commands: Vec<ParityCommandV2>,
}

#[derive(Debug, Deserialize)]
struct ParityCommandV2 {
    name: String,
    is_core: bool,
    implemented: bool,
    #[serde(default)]
    claude_type: String,
    #[serde(default)]
    rust_status: String,
    #[serde(default)]
    target_status: String,
    #[serde(default)]
    requires_tui: bool,
    #[serde(default)]
    defer_reason: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    description: Option<String>,
}

const TARGET_IMPLEMENTED: &[&str] = &[
    "add-dir",
    "agents",
    "auto-mode-setup",
    "autocompact",
    "background",
    "branch",
    "brief",
    "btw",
    "cd",
    "clear",
    "color",
    "commit",
    "commit-push-pr",
    "compact",
    "config",
    "context",
    "copy",
    "diff",
    "doctor",
    "effort",
    "exit",
    "export",
    "fast",
    "files",
    "focus",
    "fork",
    "goal",
    "help",
    "hooks",
    "ide",
    "init",
    "init-verifiers",
    "insights",
    "keybindings",
    "login",
    "logout",
    "mcp",
    "memory",
    "model",
    // cc2.1.269 reinstated `/output-style` as a `type:"local"` command.
    "output-style",
    "permissions",
    "plan",
    "plugin",
    "powerup",
    "privacy-settings",
    "recap",
    "release-notes",
    "reload-plugins",
    "reload-skills",
    "rename",
    "resume",
    "rewind",
    "security-review",
    "skill-doctor",
    "skills",
    "status",
    "statusline",
    "stickers",
    "stop",
    "subtask",
    "tasks",
    "terminal-setup",
    "theme",
    "tui",
    "usage",
    "usage-credits",
    "version",
    "workflows",
];

fn fixture() -> ParityFile {
    // Fixture filename retained as `parity_slash_commands_102` for git-history
    // continuity; the counts inside reflect the current 86-name lock.
    load_fixture("parity_slash_commands_102")
}

fn fixture_v2() -> ParityFileV2 {
    load_fixture("parity_slash_commands_102")
}

#[test]
fn fixture_total_matches_constant() {
    let f = fixture();
    assert_eq!(f.meta.total_count_lock, 87);
    assert_eq!(f.commands.len(), 87);
    assert_eq!(BUILTIN_COMMAND_NAMES.len(), 87);
    assert_eq!(f.commands.len(), BUILTIN_COMMAND_NAMES.len());
}

#[test]
fn fixture_core_matches_constant() {
    let f = fixture();
    let fixture_core: Vec<&str> = f
        .commands
        .iter()
        .filter(|c| c.is_core)
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(fixture_core.len(), 19);
    assert_eq!(f.meta.core_count_lock, 19);
    assert_eq!(BUILTIN_CORE_NAMES.len(), 19);

    // Order-sensitive equality (both are ASCII-sorted by construction).
    let const_core: Vec<&str> = BUILTIN_CORE_NAMES.to_vec();
    assert_eq!(fixture_core, const_core);
}

#[test]
fn fixture_unimplemented_count_lock_matches_field() {
    let f = fixture();
    let v2 = fixture_v2();
    let n_unimpl = v2.commands.iter().filter(|c| !c.implemented).count();
    assert_eq!(f.meta.unimplemented_count_lock, n_unimpl);
}

#[test]
fn stub_template_lock() {
    let f = fixture();
    assert_eq!(
        f.meta.stub_literal_template,
        "{name}: not implemented in v0.6.0 (M5)"
    );
}

#[test]
fn unknown_template_lock() {
    let f = fixture();
    assert_eq!(f.meta.unknown_literal_template, "Unknown command: /{name}");
}

#[test]
fn fixture_names_match_constant_order() {
    let f = fixture();
    let fixture_names: Vec<&str> = f.commands.iter().map(|c| c.name.as_str()).collect();
    let const_names: Vec<&str> = BUILTIN_COMMAND_NAMES.to_vec();
    assert_eq!(fixture_names, const_names);
}

#[tokio::test]
async fn every_fixture_command_dispatches_to_handled() {
    // Parity lock: every builtin command name is REGISTERED and dispatches to
    // `Handled`. The original M5-era assertion that each returns the stub
    // literal ("{name}: not implemented in v0.6.0 (M5)") is obsolete — later
    // parity batches (merged from main) implemented many commands with real
    // output (e.g. /commit), so this locks the full name surface dispatches,
    // not the exact body.
    let f = fixture();
    let reg = fully_wired_registry();
    let d = RegistrySlashDispatcher::new(Arc::new(RwLock::new(reg)));

    for entry in &f.commands {
        let raw = format!("/{}", entry.name);
        match d.dispatch(&raw).await {
            SlashDispatchResult::Handled { .. } => {}
            other => panic!("/{} must dispatch to Handled, got {other:?}", entry.name),
        }
    }
}

#[tokio::test]
async fn unknown_command_uses_locked_literal() {
    let reg = fully_wired_registry();
    let d = RegistrySlashDispatcher::new(Arc::new(RwLock::new(reg)));

    let outcome = d.dispatch("/definitely-not-a-real-command").await;
    match outcome {
        SlashDispatchResult::Unknown { name, display } => {
            assert_eq!(name, "definitely-not-a-real-command");
            assert_eq!(display, "Unknown command: /definitely-not-a-real-command");
        }
        other => panic!("expected Unknown, got {other:?}"),
    }
}

#[test]
fn core_command_description_matches_fixture() {
    let f = fixture();
    let mut reg = CommandRegistry::new();
    register_all_builtin_commands(&mut reg);
    for entry in f.commands.iter().filter(|c| c.is_core) {
        let expected = entry
            .description
            .as_deref()
            .unwrap_or_else(|| panic!("core /{} missing description in fixture", entry.name));
        let cmd = reg.resolve(&entry.name).expect("core cmd missing");
        assert_eq!(
            cmd.description, expected,
            "/{}: description drift (fixture vs registry)",
            entry.name
        );
    }
}

// ============================================================================
// T3 — M5-14: `implemented` field coverage (67 implemented / 19 unimplemented)
// ============================================================================

#[test]
fn implemented_count_tracks_target_runtime_surface() {
    let f = fixture_v2();
    let n_impl = f.commands.iter().filter(|c| c.implemented).count();
    assert_eq!(
        n_impl,
        TARGET_IMPLEMENTED.len(),
        "implemented field should track the current target runtime surface"
    );
}

#[test]
fn target_status_matrix_fields_are_populated() {
    let f = fixture_v2();
    for c in &f.commands {
        assert!(!c.claude_type.is_empty(), "/{} missing claude_type", c.name);
        assert!(!c.rust_status.is_empty(), "/{} missing rust_status", c.name);
        assert!(
            !c.target_status.is_empty(),
            "/{} missing target_status",
            c.name
        );
        if c.target_status == "deferred" {
            assert!(c.defer_reason.as_deref().is_some_and(|r| !r.is_empty()));
        }
        if c.rust_status == "interactive_only" {
            assert_eq!(
                c.target_status, "implemented",
                "/{} interactive-only target status drift",
                c.name
            );
            assert!(
                c.requires_tui,
                "/{} interactive-only row should require TUI",
                c.name
            );
            assert!(
                c.defer_reason.as_deref().is_some_and(|r| !r.is_empty()),
                "/{} interactive-only row missing defer_reason",
                c.name
            );
        }
        if c.requires_tui {
            assert!(
                matches!(c.claude_type.as_str(), "local-jsx" | "local"),
                "/{} requires_tui must be a local command",
                c.name
            );
        }
        assert_eq!(
            c.is_core,
            BUILTIN_CORE_NAMES.contains(&c.name.as_str()),
            "/{} is_core drift",
            c.name
        );
    }
}

#[test]
fn implemented_set_matches_target_implemented_names() {
    let f = fixture_v2();
    let mut fixture_impl: Vec<&str> = f
        .commands
        .iter()
        .filter(|c| c.implemented)
        .map(|c| c.name.as_str())
        .collect();
    fixture_impl.sort_unstable();
    let mut expected = TARGET_IMPLEMENTED.to_vec();
    expected.sort_unstable();
    assert_eq!(fixture_impl, expected);
}

#[test]
fn reload_plugins_tracks_the_live_interactive_refresh_path() {
    let row = fixture_v2()
        .commands
        .into_iter()
        .find(|command| command.name == "reload-plugins")
        .expect("reload-plugins row");
    assert!(row.implemented);
    assert_eq!(row.target_status, "implemented");
    assert_eq!(row.rust_status, "interactive_only");
    assert!(row.requires_tui);
    assert!(row
        .defer_reason
        .as_deref()
        .is_some_and(|reason| reason.contains("PluginRuntime")));
}

#[test]
fn fast_tracks_the_live_tui_cli_mode_path() {
    let row = fixture_v2()
        .commands
        .into_iter()
        .find(|command| command.name == "fast")
        .expect("fast row");
    assert!(row.implemented);
    assert_eq!(row.claude_type, "remote-or-host");
    assert_eq!(row.rust_status, "implemented_handle_bound");
    assert_eq!(row.target_status, "implemented");
    assert!(!row.requires_tui);
}

#[test]
fn correct_by_design_and_host_bound_sets_remain_explicit() {
    // Three faithful stubs remain after the 2.1.252 stale-name audit.
    assert_eq!(CORRECT_BY_DESIGN_STUBS.len(), 3);
    assert!(
        HOST_BOUND_DEFERRED_GAPS.is_empty(),
        "/btw now has a handle-bound command and reopenable TUI panel"
    );
}

#[tokio::test]
async fn target_implemented_commands_do_not_return_m5_stub() {
    let reg = fully_wired_registry();

    for name in TARGET_IMPLEMENTED {
        // These commands are installed by the interactive composition root:
        // `/reload-plugins` needs the retained PluginRuntime refresh callback,
        // while `/fast` crosses the TUI→ChatWidget→CLI mode callback. The
        // headless registry intentionally has no corresponding handlers.
        if matches!(*name, "reload-plugins" | "fast") {
            continue;
        }
        let h = reg
            .get_handler(name)
            .unwrap_or_else(|| panic!("/{name} handler missing"));
        let args = command_api::ParsedSlashCommand {
            name: (*name).to_string(),
            raw_args: String::new(),
            positional_args: vec![],
        };
        if let command_api::CommandResult::Done { display: Some(s) } = h.handle(&args).await {
            assert_ne!(s, format!("{name}: not implemented in v0.6.0 (M5)"));
        }
    }
}
