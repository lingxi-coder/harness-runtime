use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::ConversationOrchestrator;
use crate::OrchestratorConfig;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;
use tool_api::registry::ToolRegistry;

fn orch() -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::with_files(vec![])),
        PathBuf::from("/work/repo"),
    )
}

fn seed(
    orch: &ConversationOrchestrator,
    path: &std::path::Path,
    content: &str,
    mtime_ms: i64,
    entry: tool_api::read_file_state::ReadFileEntry,
) {
    tool_api::read_file_state::set_with_model_context(
        &orch.prompt_runtime.read_state_map,
        path.to_path_buf(),
        tool_api::read_file_state::ReadFileEntry {
            content: content.to_string(),
            mtime_ms,
            ..entry
        },
        true,
    );
}

fn full_read(content: &str) -> tool_api::read_file_state::ReadFileEntry {
    tool_api::read_file_state::ReadFileEntry {
        content: content.to_string(),
        mtime_ms: 0,
        offset: None,
        limit: None,
        from_read: true,
        seeded_from_context: false,
        is_partial_view: false,
    }
}

#[tokio::test]
async fn a_file_changed_on_disk_emits_one_wrapped_reminder_and_does_not_repeat() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "a\nb\nc\n").unwrap();
    let orch = orch();
    // The model read the OLD bytes; the file has since been rewritten.
    seed(&orch, &path, "a\nOLD\nc\n", 0, full_read("a\nOLD\nc\n"));

    let msgs = orch.changed_files_reminder_messages().await;
    assert_eq!(msgs.len(), 1, "exactly one changed file");
    let text = msgs[0].text_content();
    assert!(text.starts_with("<system-reminder>\n"), "got: {text}");
    assert!(text.ends_with("\n</system-reminder>"), "got: {text}");
    assert!(
        text.contains("changed on disk since you last read it."),
        "2.1.238 copy expected; got: {text}"
    );
    assert!(
        !text.contains("either by the user or by a linter"),
        "the 2.1.220 wording must be gone; got: {text}"
    );
    assert!(
        text.contains("Here are the relevant changes (shown with line numbers):\n1\ta\n2\tb\n3\tc"),
        "numbered diff expected; got: {text}"
    );

    // The re-read refreshed the entry ⇒ silent next turn.
    assert!(orch.changed_files_reminder_messages().await.is_empty());
}

#[tokio::test]
async fn an_mtime_bump_with_identical_bytes_emits_nothing() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "same\n").unwrap();
    let orch = orch();
    seed(&orch, &path, "same\n", 0, full_read("same\n"));
    assert!(
        orch.changed_files_reminder_messages().await.is_empty(),
        "`vNe` content compare suppresses the reminder"
    );
}

#[tokio::test]
async fn partial_and_seeded_entries_never_fire() {
    let dir = TempDir::new().unwrap();
    let orch = orch();

    let partial = dir.path().join("partial.rs");
    std::fs::write(&partial, "x\ny\n").unwrap();
    seed(
        &orch,
        &partial,
        "OLD\n",
        0,
        tool_api::read_file_state::ReadFileEntry {
            offset: Some(1),
            limit: Some(10),
            ..full_read("OLD\n")
        },
    );

    let seeded = dir.path().join("LINGXI.md");
    std::fs::write(&seeded, "# real\n").unwrap();
    seed(
        &orch,
        &seeded,
        "# stripped\n",
        0,
        tool_api::read_file_state::ReadFileEntry {
            seeded_from_context: true,
            is_partial_view: true,
            ..full_read("# stripped\n")
        },
    );

    assert!(orch.changed_files_reminder_messages().await.is_empty());
}

#[tokio::test]
async fn a_vanished_file_drops_its_read_state_entry() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("gone.rs");
    let orch = orch();
    seed(&orch, &path, "old\n", 0, full_read("old\n"));
    assert!(orch.changed_files_reminder_messages().await.is_empty());
    assert!(
        !orch
            .prompt_runtime
            .read_state_map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&path),
        "`if(ur(c))e.readFileState.delete(s)` — the entry must be dropped"
    );
}

/// The 16384-char cross-file budget (`m3T`): once the accumulator has
/// crossed it, every LATER file renders the "diff is omitted here" arm.
///
/// Iteration is MRU→LRU, so the file seeded FIRST is visited LAST — the two
/// big files (each snippet capped at 8192 by `truncate_snippet`, together
/// over the budget) are visited before it.
#[tokio::test]
async fn the_snippet_budget_blanks_later_files() {
    let dir = TempDir::new().unwrap();
    let orch = orch();

    let small = dir.path().join("small.rs");
    std::fs::write(&small, "new\n").unwrap();
    seed(&orch, &small, "old\n", 0, full_read("old\n"));

    let bulk: String = (0..4000).map(|i| format!("l{i}\n")).collect();
    for name in ["big1.rs", "big2.rs"] {
        let p = dir.path().join(name);
        std::fs::write(&p, &bulk).unwrap();
        seed(&orch, &p, "l0\n", 0, full_read("l0\n"));
    }

    let msgs = orch.changed_files_reminder_messages().await;
    assert_eq!(msgs.len(), 3, "three changed files");
    let last = msgs[2].text_content();
    assert!(
        last.contains(&small.to_string_lossy().to_string()),
        "the LRU-oldest entry is rendered last; got: {last}"
    );
    assert!(
        last.contains(
            "The diff is omitted here because other changed files this turn already filled \
the snippet budget; use Read if you need the current content."
        ),
        "the post-budget file must be blanked; got: {last}"
    );
    assert!(
        msgs[0]
            .text_content()
            .contains("Here are the relevant changes (shown with line numbers):"),
        "the first file keeps its snippet"
    );
}
