use super::*;
use crate::prompt::MemoryFile;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::OrchestratorConfig;
use memory::lingxi_md::LingxiMdTier;
use std::path::PathBuf;
use std::sync::Arc;
use tool_api::registry::ToolRegistry;

/// A Project-tier conditional rule living at `<cwd>/.lingxi/rules/{name}.md`
/// (so its derived base dir is `<cwd>`) carrying the given `paths:` globs.
fn project_rule(cwd: &std::path::Path, name: &str, globs: &[&str]) -> MemoryFile {
    MemoryFile {
        path: cwd.join(".lingxi").join("rules").join(format!("{name}.md")),
        body: format!("BODY OF {name}"),
        is_local_override: false,
        tier: LingxiMdTier::Project,
        globs: Some(globs.iter().map(|s| (*s).to_string()).collect()),
        raw_content: format!("BODY OF {name}"),
        content_differs_from_disk: false,
    }
}

/// Build an orchestrator whose memory provider returns `rules` and whose cwd
/// is `cwd`. Conditional rules need no Skill tool / skill provider.
fn orch_with_rules(cwd: PathBuf, rules: Vec<MemoryFile>) -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::with_files(rules)),
        cwd,
    )
}

fn push_touched(orch: &ConversationOrchestrator, path: &std::path::Path) {
    // Seed the ONE shared read-state registry the way a file tool's
    // `readFileState.set` does (content is irrelevant to rule matching,
    // which keys off the path).
    tool_api::read_file_state::set(
        &orch.prompt_runtime.read_state_map,
        path.to_path_buf(),
        tool_api::read_file_state::ReadFileEntry {
            content: String::new(),
            mtime_ms: 0,
            offset: None,
            limit: None,
            from_read: true,
            seeded_from_context: false,
            is_partial_view: false,
        },
    );
}

fn push_host_seed(orch: &ConversationOrchestrator, path: &std::path::Path) {
    tool_api::read_file_state::set_with_model_context(
        &orch.prompt_runtime.read_state_map,
        path.to_path_buf(),
        tool_api::read_file_state::ReadFileEntry {
            content: String::new(),
            mtime_ms: 0,
            offset: None,
            limit: None,
            from_read: false,
            seeded_from_context: false,
            is_partial_view: false,
        },
        false,
    );
}

#[tokio::test]
async fn matching_touched_file_injects_rule() {
    let cwd = PathBuf::from("/work/repo");
    let orch = orch_with_rules(cwd.clone(), vec![project_rule(&cwd, "scoped", &["src"])]);
    // A touched file under `src/` matches `paths: src/**`.
    push_touched(&orch, &cwd.join("src/x.rs"));

    let msg = orch
        .conditional_rules_reminder_message()
        .await
        .expect("matching rule must be injected");
    let text = msg.text_content();
    assert!(text.starts_with("<system-reminder>"), "got: {text}");
    assert!(
        text.contains("Contents of /work/repo/.lingxi/rules/scoped.md:"),
        "got: {text}"
    );
    assert!(text.contains("BODY OF scoped"), "got: {text}");
    // It is a BARE nested-memory render — no eager-block preamble.
    assert!(!text.contains("Codebase and user instructions"));
}

#[tokio::test]
async fn non_matching_touched_file_does_not_inject() {
    let cwd = PathBuf::from("/work/repo");
    let orch = orch_with_rules(cwd.clone(), vec![project_rule(&cwd, "scoped", &["src"])]);
    // `docs/y.md` does NOT match `paths: src/**`.
    push_touched(&orch, &cwd.join("docs/y.md"));
    assert!(
        orch.conditional_rules_reminder_message().await.is_none(),
        "a non-matching touched file must not activate the rule"
    );
}

#[tokio::test]
async fn host_seeded_file_does_not_activate_conditional_rule() {
    let cwd = PathBuf::from("/work/repo");
    let orch = orch_with_rules(cwd.clone(), vec![project_rule(&cwd, "scoped", &["src"])]);
    push_host_seed(&orch, &cwd.join("src/x.rs"));
    assert!(
        orch.conditional_rules_reminder_message().await.is_none(),
        "host-seeded paths are not model context and must not trigger rules"
    );
}

#[tokio::test]
async fn rule_injected_once_then_not_reinjected() {
    let cwd = PathBuf::from("/work/repo");
    let orch = orch_with_rules(cwd.clone(), vec![project_rule(&cwd, "scoped", &["src"])]);
    push_touched(&orch, &cwd.join("src/x.rs"));

    // Turn 0: injected.
    assert!(
        orch.conditional_rules_reminder_message().await.is_some(),
        "first activation must inject"
    );
    // Turn 1: the same file is still touched, but the rule was already sent →
    // not re-injected (sent-tracking dedup).
    assert!(
        orch.conditional_rules_reminder_message().await.is_none(),
        "an already-sent rule must not be re-injected"
    );
}

#[tokio::test]
async fn no_touched_file_yields_none() {
    let cwd = PathBuf::from("/work/repo");
    let orch = orch_with_rules(cwd.clone(), vec![project_rule(&cwd, "scoped", &["src"])]);
    // read_file_state empty → no rule can match.
    assert!(orch.conditional_rules_reminder_message().await.is_none());
}

#[tokio::test]
async fn no_conditional_rules_yields_none() {
    // Provider returns an unconditional file only (globs == None): the
    // conditional cache is empty, so the reminder is a strict no-op even with
    // a touched file present.
    let cwd = PathBuf::from("/work/repo");
    let unconditional = MemoryFile {
        path: cwd.join("LINGXI.md"),
        body: "always".into(),
        is_local_override: false,
        tier: LingxiMdTier::Project,
        globs: None,
        raw_content: "always".into(),
        content_differs_from_disk: false,
    };
    let orch = orch_with_rules(cwd.clone(), vec![unconditional]);
    push_touched(&orch, &cwd.join("src/x.rs"));
    assert!(orch.conditional_rules_reminder_message().await.is_none());
}

#[tokio::test]
async fn newly_matching_rule_injected_on_later_turn() {
    // Two rules; only one matches initially. After a second file is touched,
    // the second rule activates and is injected (delta across turns).
    let cwd = PathBuf::from("/work/repo");
    let orch = orch_with_rules(
        cwd.clone(),
        vec![
            project_rule(&cwd, "src-rule", &["src"]),
            project_rule(&cwd, "docs-rule", &["docs"]),
        ],
    );
    push_touched(&orch, &cwd.join("src/a.rs"));
    let t0 = orch
        .conditional_rules_reminder_message()
        .await
        .expect("src-rule active")
        .text_content();
    assert!(t0.contains("src-rule.md"));
    assert!(!t0.contains("docs-rule.md"));

    // Now touch a docs file → docs-rule newly activates; src-rule already sent.
    push_touched(&orch, &cwd.join("docs/readme.md"));
    let t1 = orch
        .conditional_rules_reminder_message()
        .await
        .expect("docs-rule newly active")
        .text_content();
    assert!(t1.contains("docs-rule.md"), "got: {t1}");
    assert!(
        !t1.contains("src-rule.md"),
        "already-sent src-rule must not re-inject: {t1}"
    );
}
