use super::*;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::OrchestratorConfig;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tool_api::registry::ToolRegistry;

const MEM: &str = branding::MEMORY_FILE;
const DOT: &str = branding::DOT_DIR;

fn touch(p: &Path, body: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

/// cwd=`<root>/repo`, trigger `<root>/repo/pkg/api/handler.rs`, and a HOME
/// under the same temp root so the User tier can never reach the real one.
struct Fixture {
    _tmp: tempfile::TempDir,
    cwd: PathBuf,
    home: PathBuf,
    trigger: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let cwd = root.join("repo");
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let trigger = cwd.join("pkg").join("api").join("handler.rs");
    touch(&trigger, "fn main(){}");
    Fixture {
        _tmp: tmp,
        cwd,
        home,
        trigger,
    }
}

fn orch(f: &Fixture) -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::with_files(vec![])),
        f.cwd.clone(),
    )
    // Hermetic roots: without this the User/Managed pass would probe the
    // developer's real `~/.lingxi/rules`.
    .with_nested_memory_roots(f.home.clone(), None)
}

fn push_touched(orch: &ConversationOrchestrator, path: &Path) {
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

#[tokio::test]
async fn surfaces_ancestor_memory_once_then_never_again() {
    let f = fixture();
    touch(&f.cwd.join("pkg").join(MEM), "pkg guidance");
    let orch = orch(&f);
    push_touched(&orch, &f.trigger);

    let text = orch
        .nested_memory_reminder_messages()
        .await
        .into_iter()
        .next()
        .expect("the memory governing the touched file must surface")
        .text_content();
    assert!(text.starts_with("<system-reminder>"), "got: {text}");
    assert!(text.contains("pkg guidance"), "got: {text}");

    assert!(
        orch.nested_memory_reminder_messages().await.is_empty(),
        "`loadedNestedMemoryPaths` must stop a second emission"
    );
}

#[tokio::test]
async fn no_touched_file_yields_none() {
    let f = fixture();
    touch(&f.cwd.join("pkg").join(MEM), "pkg guidance");
    assert!(orch(&f).nested_memory_reminder_messages().await.is_empty());
}

/// A retained raw history body suppresses repetition after ReadState eviction.
#[tokio::test]
async fn eviction_from_read_state_does_not_resurrect_a_sent_file() {
    let f = fixture();
    let mem = f.cwd.join("pkg").join(MEM);
    touch(&mem, "pkg guidance");
    let orch = orch(&f);
    push_touched(&orch, &f.trigger);
    assert!(orch
        .nested_memory_reminder_messages()
        .await
        .into_iter()
        .next()
        .is_some());

    // Simulate the LRU dropping the seeded entry.
    let canon = std::fs::canonicalize(&mem).unwrap();
    assert!(
        orch.prompt_runtime
            .read_state_map
            .lock()
            .unwrap()
            .remove(&canon)
            .is_some(),
        "the seed must have been there to evict"
    );

    assert!(
        orch.nested_memory_reminder_messages().await.is_empty(),
        "already-sent memory must stay sent after its read-state entry is evicted"
    );
}

#[tokio::test]
async fn a_file_the_model_already_read_is_not_surfaced() {
    // `k$o`: `if(!t.readFileState.has(i.path))` — a memory file the model
    // already Read is in context verbatim; re-sending it is pure waste.
    let f = fixture();
    let mem = f.cwd.join("pkg").join(MEM);
    touch(&mem, "pkg guidance");
    let orch = orch(&f);
    push_touched(&orch, &f.trigger);
    push_touched(&orch, &mem);

    assert!(orch.nested_memory_reminder_messages().await.is_empty());
}

#[tokio::test]
async fn surfacing_seeds_read_state_so_a_later_read_dedups() {
    let f = fixture();
    let mem = f.cwd.join("pkg").join(MEM);
    touch(&mem, "pkg guidance");
    let orch = orch(&f);
    push_touched(&orch, &f.trigger);
    assert!(orch
        .nested_memory_reminder_messages()
        .await
        .into_iter()
        .next()
        .is_some());

    let entry = tool_api::read_file_state::get(
        &orch.prompt_runtime.read_state_map,
        &std::fs::canonicalize(&mem).unwrap(),
    )
    .expect("the surfaced file must be seeded under its CANONICAL path");
    assert!(
        entry.seeded_from_context,
        "`seededFromContext:!0` is unconditional at this site — it is what \
         makes the next Read return the dedup stub"
    );
    assert!(!entry.from_read, "a seed is not a Read");
}

#[tokio::test]
async fn a_rule_already_sent_by_conditional_rules_is_not_resent() {
    let f = fixture();
    let rule = f.cwd.join(DOT).join("rules").join("api.md");
    touch(&rule, "---\npaths:\n  - \"pkg/api/**\"\n---\napi rule\n");
    let rules = crate::prompt::nested_memory::discover_conditional_rules(
        &f.trigger,
        &f.cwd,
        &f.home,
        None,
        None,
        memory::lingxi_md::agents::InstructionFilesMode::default(),
    );
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::with_files(rules)),
        f.cwd.clone(),
    )
    .with_nested_memory_roots(f.home.clone(), None);
    push_touched(&orch, &f.trigger);
    let conditional = orch.nested_memory_reminder_messages().await;
    assert_eq!(conditional.len(), 1);
    assert!(conditional[0].text_content().contains("api rule"));
    assert_eq!(orch.nested_memory_history().await.nested[&rule], "api rule");
    assert!(
        orch.nested_memory_reminder_messages().await.is_empty(),
        "conditional-rules already sent this rule"
    );
}
