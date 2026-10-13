//! Parity: lock the 82 builtin slash-command names plus the per-command
//! command/target status matrix across the full surface.
//!
//! See plan `docs/superpowers/plans/2026-05-25-m5-09-commands-surface.md`
//! Task 6. Locks (2026-06-20 slash-parity pass #66/#67 re-locked from 99→94 —
//! removed cost/stats as /usage aliases + deleted vim/pr-comments/output-style):
//!
//! - Total builtin name count = 82
//! - Core builtin name count = 18
//! - Target implemented status is explicit per command
//! - Stub literal template = "{name}: not implemented in v0.6.0 (M5)"
//! - Unknown literal template = "Unknown command: /{name}"

use command_api::RegistrySlashDispatcher;
use command_api::builtin_support::names::{
    BUILTIN_COMMAND_NAMES, BUILTIN_CORE_NAMES, CORRECT_BY_DESIGN_STUBS, HOST_BOUND_DEFERRED_GAPS,
};
use command_api::builtins::{
    INIT_PROMPT, InsightsCommandAvailability, InsightsReport, InsightsReportProvider,
    register_all_builtin_commands, register_bundled_skills, register_core_batch_1,
    register_core_batch_2, register_core_batch_4, register_core_batch_5, register_core_batch_8,
    register_insights_command,
};
use command_api::{
    BuiltinCommandHandler, CommandRegistry, CommandResult, GitWorktreeContext, ParsedSlashCommand,
    ShellExpansionCtx, ShellExpansionProvider, ShellOut, ShellPermissionDecision,
    ShellPermissionGate, ShellRunError, ShellRunner,
};
use lingxi_core::host::{
    AuthError, AuthHandle, LoginInfo, SlashCommandDispatcher, SlashDispatchResult, StatusSnapshot,
};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use orchestrator::test_support::MockOrchestratorHandle;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use tempfile::TempDir;
use test_harness::parity::load_fixture;
use tokio::sync::RwLock;

struct MockAuth {
    result: StdMutex<Result<LoginInfo, AuthError>>,
}

const PARITY_INSIGHTS_JSON: &str = r#"{"total_sessions":2}"#;
const PARITY_INSIGHTS_URL: &str = "file:///tmp/report-2026-10-12.html";
const PARITY_INSIGHTS_HTML_PATH: &str = "/tmp/report-2026-10-12.html";
const PARITY_INSIGHTS_FACETS: &str = "/tmp/usage-data/facets";
const PARITY_INSIGHTS_RECOMMENDATION: &str = "Try Auto Mode for this project.";

struct ParityInsightsProvider;

#[async_trait::async_trait]
impl InsightsReportProvider for ParityInsightsProvider {
    fn command_availability(&self) -> InsightsCommandAvailability {
        InsightsCommandAvailability {
            workspace_available: Some(true),
            allow_insights: Some(true),
        }
    }

    async fn generate_report(&self) -> Result<InsightsReport, Utf16JsonProjection> {
        Ok(InsightsReport {
            insights_json: PARITY_INSIGHTS_JSON.into(),
            report_url: PARITY_INSIGHTS_URL.into(),
            html_path: PARITY_INSIGHTS_HTML_PATH.into(),
            facets_directory: PARITY_INSIGHTS_FACETS.into(),
            summary_header: "HEADER\n".into(),
            summary_text: "SUMMARY".into(),
            auto_mode_recommendation: Some(PARITY_INSIGHTS_RECOMMENDATION.into()),
        })
    }
}

fn expected_insights_body() -> String {
    format!(
        "The user just ran /insights to generate a usage report analyzing their LingXi sessions.\n\n\
         Here is the full insights data:\n{PARITY_INSIGHTS_JSON}\n\n\
         Report URL: {PARITY_INSIGHTS_URL}\n\
         HTML file: {PARITY_INSIGHTS_HTML_PATH}\n\
         Facets directory: {PARITY_INSIGHTS_FACETS}\n\n\
         At-a-glance summary (for your context only — the user has not seen any output yet):\n\
         HEADER\nSUMMARY\n\n\
         Respond with exactly the following, and nothing else. Do not add, omit, or reword any line:\n\n\
         Your shareable insights report is ready:\n{PARITY_INSIGHTS_URL}\n\n\
         {PARITY_INSIGHTS_RECOMMENDATION}\n\n\
         Want to dig into any section or try one of the suggestions?"
    )
}

const SECURITY_REVIEW_SHELL_OUTPUTS: [(&str, &str); 4] = [
    (
        "git status",
        "On branch parity-fixture\nnothing to commit, working tree clean",
    ),
    (
        "git diff --name-only origin/HEAD...",
        "crates/command-api/src/shell_expansion.rs\ncrates/test-harness/tests/parity_slash_commands.rs",
    ),
    (
        "git log --no-decorate origin/HEAD...",
        "abc1234 parity fixture commit",
    ),
    (
        "git diff origin/HEAD...",
        "diff --git a/parity-fixture b/parity-fixture\n+fixed source output",
    ),
];

fn expected_security_review_body() -> String {
    let mut body =
        include_str!("../../command-api/tests/fixtures/native_295/security_review_body.md")
            .to_owned();
    for (command, output) in SECURITY_REVIEW_SHELL_OUTPUTS {
        body = body.replace(&format!("!`{command}`"), output);
    }
    body
}

struct ParityShellExpansionProvider {
    cwd: Utf16JsonProjection,
    commands: Arc<StdMutex<Vec<String>>>,
}

struct ParityShellRunner {
    commands: Arc<StdMutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl ShellRunner for ParityShellRunner {
    async fn run(
        &self,
        command: &Utf16JsonProjection,
        _shell: Option<command_api::FrontmatterShell>,
    ) -> Result<ShellOut, ShellRunError> {
        let command = command.string_at("").unwrap_or_default().to_owned();
        self.commands.lock().unwrap().push(command.clone());
        let Some((_, stdout)) = SECURITY_REVIEW_SHELL_OUTPUTS
            .iter()
            .find(|(expected, _)| *expected == command)
        else {
            return Err(ShellRunError {
                generic_message: Some(format!("unexpected parity shell command: {command}")),
                ..ShellRunError::default()
            });
        };
        Ok(ShellOut {
            stdout: (*stdout).to_owned(),
            stderr: String::new(),
            interrupted: false,
        })
    }
}

struct ParityAllowShell;

impl ShellPermissionGate for ParityAllowShell {
    fn check(
        &self,
        _command: &Utf16JsonProjection,
        _shell: Option<command_api::FrontmatterShell>,
    ) -> ShellPermissionDecision {
        ShellPermissionDecision::Allow
    }
}

#[async_trait::async_trait]
impl ShellExpansionProvider for ParityShellExpansionProvider {
    async fn git_worktree_context(&self) -> GitWorktreeContext {
        GitWorktreeContext {
            cwd: self.cwd.clone(),
            inside: true,
        }
    }

    fn build(
        &self,
        _allowed_tools: &[String],
        _shell: Option<command_api::FrontmatterShell>,
    ) -> ShellExpansionCtx {
        ShellExpansionCtx {
            runner: Arc::new(ParityShellRunner {
                commands: self.commands.clone(),
            }),
            permission_gate: Arc::new(ParityAllowShell),
        }
    }
}

#[async_trait::async_trait]
impl AuthHandle for MockAuth {
    fn register_account_change_observer(
        &self,
        _observer: std::sync::Weak<dyn lingxi_core::host::auth::AccountChangeObserver>,
    ) {
    }

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

fn fully_wired_registry(cwd: &std::path::Path) -> CommandRegistry {
    let mut reg = CommandRegistry::new();
    register_all_builtin_commands(&mut reg);
    let handle: Arc<MockOrchestratorHandle> = Arc::new(MockOrchestratorHandle::new());
    handle.set_status_snapshot(StatusSnapshot {
        cwd: cwd.to_path_buf(),
        ..StatusSnapshot::default()
    });
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
    register_bundled_skills(&mut reg, false);
    assert!(register_insights_command(
        &mut reg,
        Arc::new(ParityInsightsProvider)
    ));
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
    "compact",
    "config",
    "context",
    "copy",
    "diff",
    "effort",
    "exit",
    "export",
    "fast",
    "focus",
    "fork",
    "goal",
    "help",
    "hooks",
    "ide",
    "init",
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
    assert_eq!(f.meta.total_count_lock, 82);
    assert_eq!(f.commands.len(), 82);
    assert_eq!(BUILTIN_COMMAND_NAMES.len(), 82);
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
    assert_eq!(fixture_core.len(), 18);
    assert_eq!(f.meta.core_count_lock, 18);
    assert_eq!(BUILTIN_CORE_NAMES.len(), 18);

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
    // The Native prompt commands enter the model as a separate command header,
    // body companion, and ordered hook rows. Other builtin command types keep
    // their host-visible Handled result.
    let f = fixture();
    let export_dir = TempDir::new().expect("temporary export directory");
    let reg = fully_wired_registry(export_dir.path());

    let mut expected_prompt_bodies = BTreeMap::new();
    for entry in &f.commands {
        let command = reg
            .resolve(&Utf16JsonProjection::from(entry.name.as_str()))
            .unwrap_or_else(|| panic!("/{} command missing", entry.name));
        if !command.is_prompt() {
            continue;
        }
        let expected_body = match entry.name.as_str() {
            "init" => INIT_PROMPT.as_str().to_owned(),
            "insights" => expected_insights_body(),
            "statusline" => "Create an Agent with subagent_type \"statusline-setup\" and the prompt \"Configure my statusLine from my shell PS1 configuration\"".to_owned(),
            "security-review" => expected_security_review_body(),
            other => panic!("add an exact body expectation for Native prompt /{other}"),
        };
        expected_prompt_bodies.insert(entry.name.clone(), expected_body);
    }

    let shell_commands = Arc::new(StdMutex::new(Vec::new()));
    let shell_provider = Arc::new(ParityShellExpansionProvider {
        cwd: export_dir.path().to_string_lossy().into_owned().into(),
        commands: shell_commands.clone(),
    });
    let d = RegistrySlashDispatcher::new(Arc::new(RwLock::new(reg)))
        .with_shell_expansion(shell_provider);

    for entry in &f.commands {
        let raw = format!("/{}", entry.name);
        match d.dispatch(Utf16JsonProjection::from(raw)).await {
            SlashDispatchResult::RunSkillAsTurn {
                command,
                body,
                hook_messages,
            } => {
                let expected_body = expected_prompt_bodies
                    .get(&entry.name)
                    .unwrap_or_else(|| panic!("non-prompt /{} ran as a prompt", entry.name));
                let expected_header = format!(
                    "<command-message>{0}</command-message>\n<command-name>/{0}</command-name>",
                    entry.name
                );
                assert_eq!(command.string_at(""), Some(expected_header.as_str()));
                assert_eq!(body.string_at(""), Some(expected_body.as_str()));
                assert!(
                    hook_messages.is_empty(),
                    "/{} has no configured hooks",
                    entry.name
                );
            }
            SlashDispatchResult::Handled { .. } => assert!(
                !expected_prompt_bodies.contains_key(&entry.name),
                "Native prompt /{} was flattened into a display result",
                entry.name
            ),
            other => panic!("/{} unexpected dispatch result: {other:?}", entry.name),
        }
    }

    assert!(
        std::fs::read_dir(export_dir.path())
            .expect("list temporary export directory")
            .filter_map(Result::ok)
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with("conversation-")),
        "/export should write its default output inside the isolated command cwd"
    );
    let mut actual_shell_commands = shell_commands.lock().unwrap().clone();
    actual_shell_commands.sort();
    let mut expected_shell_commands = SECURITY_REVIEW_SHELL_OUTPUTS
        .iter()
        .map(|(command, _)| (*command).to_owned())
        .collect::<Vec<_>>();
    expected_shell_commands.sort();
    assert_eq!(actual_shell_commands, expected_shell_commands);
}

#[tokio::test]
async fn unknown_command_uses_locked_literal() {
    let scratch = TempDir::new().expect("temporary command directory");
    let reg = fully_wired_registry(scratch.path());
    let d = RegistrySlashDispatcher::new(Arc::new(RwLock::new(reg)));

    let outcome = d
        .dispatch(Utf16JsonProjection::from("/definitely-not-a-real-command"))
        .await;
    match outcome {
        SlashDispatchResult::Unknown { name, display } => {
            assert_eq!(name.string_at(""), Some("definitely-not-a-real-command"));
            assert_eq!(
                display.string_at(""),
                Some("Unknown command: /definitely-not-a-real-command")
            );
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
        let source_description = entry
            .description
            .as_deref()
            .unwrap_or_else(|| panic!("core /{} missing description in fixture", entry.name));
        // Native HSo in 2.1.295 src_194447854.js uses Claude; /agents uses the current product name.
        let expected = if entry.name == "agents" {
            assert_eq!(
                source_description,
                format!(
                    "(removed) Ask Claude to create/manage subagents, or edit {}/agents/",
                    branding::DOT_DIR
                )
            );
            source_description.replace("Ask Claude", &format!("Ask {}", branding::PRODUCT_NAME))
        } else {
            source_description.to_owned()
        };
        let cmd = reg
            .resolve(&Utf16JsonProjection::from(entry.name.as_str()))
            .expect("core cmd missing");
        assert_eq!(
            cmd.description.string_at(""),
            Some(expected.as_str()),
            "/{}: description drift (fixture vs registry)",
            entry.name
        );
    }
}

// ============================================================================
// T3 — M5-14: `implemented` field coverage (63 implemented / 19 unimplemented)
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
    assert!(
        row.defer_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("PluginRuntime"))
    );
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
    let scratch = TempDir::new().expect("temporary command directory");
    let reg = fully_wired_registry(scratch.path());

    for name in TARGET_IMPLEMENTED {
        // These commands are installed by the interactive composition root:
        // `/reload-plugins` needs the retained PluginRuntime refresh callback,
        // while `/fast` crosses the TUI→ChatWidget→CLI mode callback. The
        // headless registry intentionally has no corresponding handlers.
        if matches!(*name, "reload-plugins" | "fast") {
            continue;
        }
        let h = reg
            .get_handler(&Utf16JsonProjection::from(*name))
            .unwrap_or_else(|| panic!("/{name} handler missing"));
        let args = command_api::ParsedSlashCommand {
            name: Utf16JsonProjection::from(*name),
            raw_args: Utf16JsonProjection::from(""),
            positional_args: vec![],
        };
        if *name == "insights" {
            match h.handle(&args).await {
                CommandResult::InjectMessage { content } => {
                    assert_eq!(
                        content.string_at(""),
                        Some(expected_insights_body().as_str())
                    );
                }
                other => {
                    panic!("/insights must render the explicitly provided Native report: {other:?}")
                }
            }
            continue;
        }
        if let command_api::CommandResult::Done { display: Some(s) } = h.handle(&args).await {
            assert_ne!(
                s.string_at(""),
                Some(format!("{name}: not implemented in v0.6.0 (M5)").as_str())
            );
        }
    }
}
