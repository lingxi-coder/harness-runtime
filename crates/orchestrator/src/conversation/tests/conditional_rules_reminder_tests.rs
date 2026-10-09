use super::*;
use crate::prompt::{MemoryFile, MemoryHierarchyProvider};
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::OrchestratorConfig;
use memory::lingxi_md::{agents::InstructionFilesMode, LingxiMdTier};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tool_api::registry::ToolRegistry;

/// A Project-tier conditional rule living at `<cwd>/.lingxi/rules/{name}.md`
/// (so its derived base dir is `<cwd>`) carrying the given `paths:` globs.
fn project_rule(cwd: &std::path::Path, name: &str, globs: &[&str]) -> MemoryFile {
    MemoryFile {
        parent: None,
        source_content: None,
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
    orch.prompt_runtime
        .nested_memory_triggers
        .enqueue(path.to_path_buf());
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
        .nested_memory_reminder_messages()
        .await
        .into_iter()
        .next()
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
        orch.nested_memory_reminder_messages().await.is_empty(),
        "a non-matching touched file must not activate the rule"
    );
}

#[tokio::test]
async fn host_seeded_file_does_not_activate_conditional_rule() {
    let cwd = PathBuf::from("/work/repo");
    let orch = orch_with_rules(cwd.clone(), vec![project_rule(&cwd, "scoped", &["src"])]);
    push_host_seed(&orch, &cwd.join("src/x.rs"));
    assert!(
        orch.nested_memory_reminder_messages().await.is_empty(),
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
        orch.nested_memory_reminder_messages()
            .await
            .into_iter()
            .next()
            .is_some(),
        "first activation must inject"
    );
    // Turn 1: the same file is still touched, but the rule was already sent →
    // not re-injected (sent-tracking dedup).
    assert!(
        orch.nested_memory_reminder_messages().await.is_empty(),
        "an already-sent rule must not be re-injected"
    );
}

#[tokio::test]
async fn no_touched_file_yields_none() {
    let cwd = PathBuf::from("/work/repo");
    let orch = orch_with_rules(cwd.clone(), vec![project_rule(&cwd, "scoped", &["src"])]);
    // read_file_state empty → no rule can match.
    assert!(orch.nested_memory_reminder_messages().await.is_empty());
}

#[tokio::test]
async fn no_conditional_rules_yields_none() {
    // Provider returns an unconditional file only (globs == None): the
    // conditional cache is empty, so the reminder is a strict no-op even with
    // a touched file present.
    let cwd = PathBuf::from("/work/repo");
    let unconditional = MemoryFile {
        parent: None,
        source_content: None,
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
    assert!(orch.nested_memory_reminder_messages().await.is_empty());
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
        .nested_memory_reminder_messages()
        .await
        .into_iter()
        .next()
        .expect("src-rule active")
        .text_content();
    assert!(t0.contains("src-rule.md"));
    assert!(!t0.contains("docs-rule.md"));

    // Now touch a docs file → docs-rule newly activates; src-rule already sent.
    push_touched(&orch, &cwd.join("docs/readme.md"));
    let t1 = orch
        .nested_memory_reminder_messages()
        .await
        .into_iter()
        .next()
        .expect("docs-rule newly active")
        .text_content();
    assert!(t1.contains("docs-rule.md"), "got: {t1}");
    assert!(
        !t1.contains("src-rule.md"),
        "already-sent src-rule must not re-inject: {t1}"
    );
}

struct FilesystemRuleProvider {
    home: PathBuf,
    managed: PathBuf,
    eager: PathBuf,
    eager_loads: AtomicUsize,
}

#[async_trait::async_trait]
impl MemoryHierarchyProvider for FilesystemRuleProvider {
    async fn load(&self, cwd: &Path) -> Vec<MemoryFile> {
        self.eager_loads.fetch_add(1, Ordering::SeqCst);
        // Read only the fixture's explicit eager path. Discovery roots below
        // are likewise temporary, so neither producer uses the real home.
        let mut processed = std::collections::HashSet::new();
        memory::lingxi_md::loader::expand_memory_file_with_excluder(
            &self.eager,
            &mut processed,
            false,
            cwd,
            Some(&self.home),
            0,
            LingxiMdTier::Project,
            None,
        )
        .into_iter()
        .map(|file| MemoryFile {
            source_content: None,
            parent: file.parent,
            path: file.path,
            body: file.body,
            is_local_override: false,
            tier: LingxiMdTier::Project,
            globs: file.globs,
            raw_content: file.raw_content,
            content_differs_from_disk: file.content_differs_from_disk,
        })
        .collect()
    }

    async fn load_conditional_rules(
        &self,
        cwd: &Path,
        trigger: &Path,
        mode: InstructionFilesMode,
    ) -> Vec<MemoryFile> {
        crate::prompt::nested_memory::discover_conditional_rules(
            trigger,
            cwd,
            &self.home,
            Some(&self.managed),
            None,
            mode,
        )
    }

    fn filesystem_discovery(&self) -> bool {
        true
    }

    fn hierarchy_roots(&self) -> Option<(PathBuf, Option<PathBuf>)> {
        Some((self.home.clone(), Some(self.managed.clone())))
    }
}

struct FilesystemRuleFixture {
    _temp: tempfile::TempDir,
    cwd: PathBuf,
    home: PathBuf,
    managed: PathBuf,
    rule: PathBuf,
    eager: PathBuf,
}

impl FilesystemRuleFixture {
    fn new(tier: LingxiMdTier, glob: &str, body: &str) -> Self {
        let temp = tempfile::tempdir().expect("temporary instruction roots");
        let root = std::fs::canonicalize(temp.path()).expect("canonical fixture root");
        let cwd = root.join("repo");
        let home = root.join("home");
        let managed = root.join("managed");
        for directory in [&cwd, &home, &managed] {
            std::fs::create_dir_all(directory).expect("temporary instruction root");
        }
        let base = match tier {
            LingxiMdTier::Managed => &managed,
            LingxiMdTier::User => &home,
            LingxiMdTier::Project => &cwd,
            LingxiMdTier::Local => panic!("conditional fixture needs a rules tier"),
        };
        let rule = base.join(branding::DOT_DIR).join("rules").join("fresh.md");
        let eager = cwd.join(branding::MEMORY_FILE);
        std::fs::write(&eager, "frozen eager sentinel\n").expect("ordinary eager file");
        let fixture = Self {
            _temp: temp,
            cwd,
            home,
            managed,
            rule,
            eager,
        };
        fixture.write_rule(glob, body);
        fixture
    }

    fn write_rule(&self, glob: &str, body: &str) {
        std::fs::create_dir_all(self.rule.parent().unwrap()).expect("fixture rules directory");
        std::fs::write(&self.rule, format!("---\npaths: {glob}\n---\n{body}\n"))
            .expect("fixture conditional rule");
    }

    fn trigger(&self, relative: &str) -> PathBuf {
        let path = self.cwd.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).expect("fixture trigger directory");
        std::fs::write(&path, "trigger contents\n").expect("fixture trigger file");
        path
    }

    fn orchestrator(&self) -> (ConversationOrchestrator, Arc<FilesystemRuleProvider>) {
        let memory = Arc::new(FilesystemRuleProvider {
            home: self.home.clone(),
            managed: self.managed.clone(),
            eager: self.eager.clone(),
            eager_loads: AtomicUsize::new(0),
        });
        let orchestrator = ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            memory.clone(),
            self.cwd.clone(),
        )
        .with_nested_memory_roots(self.home.clone(), Some(self.managed.clone()));
        (orchestrator, memory)
    }
}

async fn freeze_unsent_rule(
    fixture: &FilesystemRuleFixture,
    orchestrator: &ConversationOrchestrator,
    body: &str,
) -> Arc<Vec<MemoryFile>> {
    let (_, load) = orchestrator.main_instruction_load().await;
    let frozen = load.memory_files().await;
    assert_eq!(
        frozen.len(),
        1,
        "native eager snapshot excludes conditional rules"
    );
    assert_eq!(frozen[0].body, "frozen eager sentinel\n");
    assert!(frozen[0].globs.is_none());
    assert!(std::fs::read_to_string(&fixture.rule)
        .unwrap()
        .ends_with(&format!("{body}\n")));
    push_touched(orchestrator, &fixture.trigger("unrelated/probe.txt"));
    // Exercise the real producer order. A cached conditional producer used
    // to run first and claim a stale path before fresh nested discovery could
    // replace its body, deletion status, or globs on a later model step.
    let first = orchestrator.collect_turn_reminders(true, false, None).await;
    assert!(rule_reminder_texts(&first.model_reminders, &fixture.rule).is_empty());
    assert!(
        !orchestrator
            .nested_memory_history()
            .await
            .nested
            .contains_key(&fixture.rule),
        "a non-matching rule remains eligible for a future fresh discovery"
    );
    frozen
}

fn rule_reminder_texts(messages: &[(usize, ConversationMessage)], rule: &Path) -> Vec<String> {
    let heading = format!("Contents of {}:\n\n", rule.display());
    messages
        .iter()
        .map(|(_, message)| message.text_content())
        .filter(|text| text.contains(&heading))
        .collect()
}

#[tokio::test]
async fn unsent_rule_body_changes_are_fresh_across_all_rule_tiers() {
    for tier in [
        LingxiMdTier::Managed,
        LingxiMdTier::User,
        LingxiMdTier::Project,
    ] {
        let fixture = FilesystemRuleFixture::new(tier, "src/**", "old rule body");
        let (orchestrator, provider) = fixture.orchestrator();
        let frozen = freeze_unsent_rule(&fixture, &orchestrator, "old rule body").await;

        fixture.write_rule("src/**", "new rule body");
        push_touched(&orchestrator, &fixture.trigger("src/next.rs"));
        let next = orchestrator.collect_turn_reminders(true, false, None).await;
        assert_eq!(
            rule_reminder_texts(&next.model_reminders, &fixture.rule),
            vec![format!(
                "<system-reminder>\nContents of {}:\n\nnew rule body\n\n</system-reminder>",
                fixture.rule.display()
            )],
            "the real reminder order must send fresh bytes for {tier:?}"
        );
        assert_eq!(frozen[0].body, "frozen eager sentinel\n");
        assert_eq!(
            provider.eager_loads.load(Ordering::SeqCst),
            1,
            "fresh rules must not reload the root's frozen eager file snapshot"
        );

        fixture.write_rule("src/**", "edited after already sent");
        push_touched(&orchestrator, &fixture.trigger("src/another.rs"));
        let repeated = orchestrator.collect_turn_reminders(true, false, None).await;
        assert!(
            rule_reminder_texts(&repeated.model_reminders, &fixture.rule).is_empty(),
            "editing an already-sent path must preserve session dedup for {tier:?}"
        );
    }
}

#[tokio::test]
async fn deleted_unsent_rule_is_not_claimed_from_the_eager_snapshot() {
    let fixture = FilesystemRuleFixture::new(LingxiMdTier::Project, "src/**", "deleted rule body");
    let (orchestrator, provider) = fixture.orchestrator();
    let frozen = freeze_unsent_rule(&fixture, &orchestrator, "deleted rule body").await;

    std::fs::remove_file(&fixture.rule).expect("delete unsent rule");
    push_touched(&orchestrator, &fixture.trigger("src/next.rs"));
    let next = orchestrator.collect_turn_reminders(true, false, None).await;
    assert!(rule_reminder_texts(&next.model_reminders, &fixture.rule).is_empty());
    assert!(
        !orchestrator
            .nested_memory_history()
            .await
            .nested
            .contains_key(&fixture.rule),
        "deleted rules must not consume their session delivery claim"
    );
    assert_eq!(frozen[0].body, "frozen eager sentinel\n");
    assert_eq!(provider.eager_loads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn changed_unsent_rule_globs_stop_a_previous_match() {
    let fixture = FilesystemRuleFixture::new(LingxiMdTier::Project, "src/**", "stale match body");
    let (orchestrator, provider) = fixture.orchestrator();
    let frozen = freeze_unsent_rule(&fixture, &orchestrator, "stale match body").await;

    fixture.write_rule("docs/**", "now docs only");
    push_touched(&orchestrator, &fixture.trigger("src/next.rs"));
    let next = orchestrator.collect_turn_reminders(true, false, None).await;
    assert!(rule_reminder_texts(&next.model_reminders, &fixture.rule).is_empty());
    assert!(
        !orchestrator
            .nested_memory_history()
            .await
            .nested
            .contains_key(&fixture.rule),
        "the old cached glob must not claim a rule that no longer matches"
    );
    assert!(frozen[0].globs.is_none());
    assert_eq!(provider.eager_loads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn changed_unsent_rule_globs_activate_a_new_match_with_fresh_body() {
    let fixture = FilesystemRuleFixture::new(LingxiMdTier::Project, "docs/**", "old docs body");
    let (orchestrator, provider) = fixture.orchestrator();
    let frozen = freeze_unsent_rule(&fixture, &orchestrator, "old docs body").await;

    fixture.write_rule("src/**", "new src body");
    push_touched(&orchestrator, &fixture.trigger("src/next.rs"));
    let next = orchestrator.collect_turn_reminders(true, false, None).await;
    let oracle: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/nested_memory_2_1_287.json"
    ))
    .unwrap();
    let native = oracle["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == "native-acquired-path-gated-body-keeps-trailing-newline")
        .unwrap();
    assert_eq!(
        tokio::fs::read_to_string(&fixture.rule).await.unwrap(),
        native["input"]["files"][0]["rawContent"].as_str().unwrap()
    );
    assert_eq!(
        rule_reminder_texts(&next.model_reminders, &fixture.rule),
        vec![native["expected"]["rendered"][0]
            .as_str()
            .unwrap()
            .replace("/repo/LINGXI.md", &fixture.rule.display().to_string())]
    );
    assert_eq!(frozen[0].body, "frozen eager sentinel\n");
    assert_eq!(provider.eager_loads.load(Ordering::SeqCst), 1);
}
