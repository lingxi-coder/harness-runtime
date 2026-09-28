//! THE WIRING TEST for the seeded Read-dedup producer.
//!
//! Port of claude-code's startup memory-seeding loop `xCt` (2.1.220
//! @245883373):
//!
//! ```text
//! for(let Fr of yt){
//!   if(tHt(Fr.path))continue;
//!   let jn=MLu(Fr),Ao;
//!   try{Ao=jn?FQ(Fr.path):Date.now()}catch{Ao=Date.now()}
//!   vM.current.set(Fr.path,{
//!     content: Fr.contentDiffersFromDisk?Fr.rawContent??Fr.content:X9(Fr.content),
//!     timestamp: Ao, offset:void 0, limit:void 0,
//!     isPartialView: Fr.contentDiffersFromDisk,
//!     seededFromContext: jn,
//!     ...!jn&&{contentNotInModelContext:!0},
//!     keepContent:!0}), …}
//! ```
//!
//! LingXi reuses the ALREADY-WIRED session-start seam
//! `fire_instructions_loaded` (called once by every composition root — see
//! `runtime/src/desktop/mod.rs`), so this test drives the exact production
//! entry point rather than the private helper.

use async_trait::async_trait;
use hooks::registry::HookRegistry;
use hooks::HookExecutorImpl;
use orchestrator::test_support::{MockApiClient, MockOutputStream, NoOpPermissionGate};
use orchestrator::{ConversationOrchestrator, MemoryFile, OrchestratorConfig};
use platform_api::{HttpError, HttpTransport, RuntimeError, RuntimeSpawner};
use protocol::{HttpRequest, HttpResponse};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::RwLock;
use tool_api::read_file_state::{ReadFileEntry, ReadFileStateMap};
use tool_api::registry::ToolRegistry;

struct UnusedHttp;
#[async_trait]
impl HttpTransport for UnusedHttp {
    async fn request(&self, _req: HttpRequest) -> Result<HttpResponse, HttpError> {
        Err(HttpError::InvalidRequest("unused".into()))
    }
    async fn stream_sse(
        &self,
        _req: HttpRequest,
    ) -> Result<platform_api::http::SseStream, HttpError> {
        Err(HttpError::InvalidRequest("unused".into()))
    }
}
struct UnusedRuntime;
#[async_trait]
impl RuntimeSpawner for UnusedRuntime {
    async fn spawn(
        &self,
        _name: &str,
        _task: Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
    ) -> Result<platform_api::BackgroundTaskHandle, RuntimeError> {
        Err(RuntimeError::Internal("unused".into()))
    }
    async fn sleep(&self, _d: Duration) {}
    async fn cancel(&self, _h: &platform_api::BackgroundTaskHandle) -> Result<(), RuntimeError> {
        Ok(())
    }
}

/// Load the real memory hierarchy for `cwd` with the Managed tier pointed at an
/// empty directory, so the fixture sees exactly the files this test wrote.
///
/// This exercises `RealMemoryHierarchyProvider` -> `MemoryEntry` ->
/// `MemoryFile` carry-through of `raw_content` / `content_differs_from_disk`,
/// then hands the result to a static provider so the orchestrator's cwd walk
/// cannot pull in the developer's own `~/.lingxi/LINGXI.md`.
async fn load_fixture_files(cwd: &Path, empty_managed: &Path) -> Vec<MemoryFile> {
    use orchestrator::prompt::{MemoryHierarchyProvider, RealMemoryHierarchyProvider};
    std::env::set_var(memory::lingxi_md::hierarchy::MANAGED_DIR_ENV, empty_managed);
    let files = RealMemoryHierarchyProvider.load(cwd).await;
    std::env::remove_var(memory::lingxi_md::hierarchy::MANAGED_DIR_ENV);
    // Keep only files under the fixture cwd (drops any real ~/.lingxi/LINGXI.md
    // the walk finds on the developer's machine).
    files
        .into_iter()
        .filter(|f| f.path.starts_with(cwd))
        .collect()
}

fn orch(files: Vec<MemoryFile>, cwd: PathBuf, map: ReadFileStateMap) -> ConversationOrchestrator {
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    let hooks = Arc::new(HookExecutorImpl::new(
        registry,
        Arc::new(UnusedHttp),
        Arc::new(UnusedRuntime),
    ));
    ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        hooks,
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(orchestrator::test_support::StaticMemoryProvider::with_files(files)),
        cwd,
    )
    .with_read_state_map(map)
}

fn disk_mtime_ms(p: &Path) -> i64 {
    tool_api::read_file_state::mtime_ms_floor(std::fs::metadata(p).unwrap().modified().unwrap())
}

fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

fn entry_for(map: &ReadFileStateMap, p: &Path) -> ReadFileEntry {
    tool_api::read_file_state::get(map, p)
        .unwrap_or_else(|| panic!("no seeded entry for {}", p.display()))
}

#[tokio::test]
async fn fire_instructions_loaded_seeds_memory_files_into_read_state() {
    let tmp = TempDir::new().unwrap();
    let cwd = std::fs::canonicalize(tmp.path()).unwrap();
    let managed = tmp.path().join("empty-managed");
    std::fs::create_dir_all(&managed).unwrap();
    std::fs::create_dir_all(cwd.join(".lingxi/rules")).unwrap();

    // (a) A plain LINGXI.md: no frontmatter, no HTML comment, trailing newline
    //     — `bn_`'s `p = d !== e` is FALSE, so this seeds the dedup-eligible
    //     shape.
    let plain = cwd.join("LINGXI.md");
    std::fs::write(&plain, "# repo rules\nbe careful\n").unwrap();
    // (b) A conditional (`paths:`-gated) rule: NOT rendered into context, so
    //     `MLu` is false -> `seededFromContext: false` and the Date.now()
    //     timestamp branch.
    let cond = cwd.join(".lingxi/rules/cond.md");
    std::fs::write(&cond, "---\npaths: src/**\n---\nscoped body\n").unwrap();
    // (c) An UNCONDITIONAL rule that still has frontmatter (no `paths:` key):
    //     rendered into context, but `contentDiffersFromDisk` -> partial view,
    //     and the stored content is the RAW text INCLUDING the frontmatter.
    let fm = cwd.join(".lingxi/rules/withfm.md");
    std::fs::write(&fm, "---\ndescription: hi\n---\nplain body\n").unwrap();
    // (d) A CRLF file: seeding must NOT collapse CRLF (LingXi's Read stores
    //     `decode_utf8_strict`, which strips the BOM only), or the staleness
    //     guard's content-equality fallback would fail forever.
    let crlf = cwd.join(".lingxi/rules/crlf.md");
    std::fs::write(&crlf, "alpha\r\nbeta\r\n").unwrap();

    let files = load_fixture_files(&cwd, &managed).await;
    let paths: Vec<_> = files.iter().map(|f| f.path.clone()).collect();
    assert!(
        paths.contains(&plain),
        "fixture must load LINGXI.md: {paths:?}"
    );
    assert!(
        paths.contains(&cond),
        "fixture must load the conditional rule"
    );
    assert!(
        paths.contains(&fm),
        "fixture must load the frontmatter rule"
    );
    assert!(paths.contains(&crlf), "fixture must load the CRLF rule");

    let map = tool_api::read_file_state::new_read_file_state_map();
    let o = orch(files, cwd.clone(), Arc::clone(&map));
    let before = now_ms();
    o.fire_instructions_loaded().await;

    // (a) plain LINGXI.md -------------------------------------------------
    let e = entry_for(&map, &plain);
    assert!(
        e.seeded_from_context,
        "rendered file seeds with MLu == true"
    );
    assert!(!e.is_partial_view, "plain file does not differ from disk");
    assert!(!e.from_read, "seeding is not a Read");
    assert_eq!(e.offset, None);
    assert_eq!(e.limit, None);
    assert_eq!(
        e.mtime_ms,
        disk_mtime_ms(&plain),
        "rendered file uses the DISK mtime (`FQ(path)`), not Date.now()"
    );
    assert_eq!(
        e.content, "# repo rules\nbe careful\n",
        "content must be the byte-exact disk text (trailing newline kept)"
    );

    // (b) conditional rule -------------------------------------------------
    let e = entry_for(&map, &cond);
    assert!(
        !e.seeded_from_context,
        "a paths:-gated rule is NOT in model context -> seededFromContext false"
    );
    assert!(
        e.mtime_ms >= before,
        "non-rendered file uses the Date.now() branch"
    );
    assert!(
        !map.lock().unwrap().model_context_keys().contains(&cond),
        "the `...!jn && {{contentNotInModelContext:!0}}` spread -> not model-visible"
    );

    // (c) unconditional-but-differing rule ---------------------------------
    let e = entry_for(&map, &fm);
    assert!(
        e.seeded_from_context,
        "no `paths:` key -> still rendered eagerly"
    );
    assert!(
        e.is_partial_view,
        "stripped frontmatter -> isPartialView: contentDiffersFromDisk"
    );
    assert_eq!(
        e.content, "---\ndescription: hi\n---\nplain body\n",
        "the differs branch stores `rawContent ?? content` UNnormalized"
    );

    // (d) CRLF -------------------------------------------------------------
    let e = entry_for(&map, &crlf);
    assert_eq!(
        e.content, "alpha\r\nbeta\r\n",
        "CRLF must be preserved (LingXi's Read stores BOM-stripped bytes only)"
    );

    // NO seeded memory file joins the model-context set — not even a rendered
    // one. That set is LingXi's post-compact RESTORE set
    // (`drain_model_context` -> `restore_post_compact_attachments`) and the
    // input `conditional_rules_reminder_message` treats as "files touched this
    // turn". Memory files are re-injected by the SYSTEM PROMPT every turn, so
    // restoring them as attachments would duplicate them after every compaction,
    // and listing them as touched would fire conditional rules nobody opened.
    //
    // This is a DIFFERENT axis from the oracle's `seededFromContext`/`MLu`,
    // which only gates the dedup stub. An earlier draft passed the MLu result to
    // both and so silently enrolled LINGXI.md in post-compact restore.
    let visible = map.lock().unwrap().model_context_keys();
    assert!(
        !visible.contains(&plain),
        "a rendered memory file is still not a restore candidate"
    );
    assert!(!visible.contains(&fm));
    assert!(!visible.contains(&cond));
}

#[tokio::test]
async fn seeding_never_clobbers_an_existing_read_state_entry() {
    // The oracle's site-2 guard `!readFileState.has(path)` (@237715046): an
    // entry the model actually read must survive a (re-)seed — including the
    // `reason = Compact` re-entry into `fire_instructions_loaded_with_reason`.
    let tmp = TempDir::new().unwrap();
    let cwd = std::fs::canonicalize(tmp.path()).unwrap();
    let managed = tmp.path().join("empty-managed");
    std::fs::create_dir_all(&managed).unwrap();
    let plain = cwd.join("LINGXI.md");
    std::fs::write(&plain, "# repo rules\n").unwrap();

    let files = load_fixture_files(&cwd, &managed).await;
    let map = tool_api::read_file_state::new_read_file_state_map();
    tool_api::read_file_state::set(
        &map,
        plain.clone(),
        ReadFileEntry {
            content: "WHAT THE MODEL ACTUALLY READ".into(),
            mtime_ms: 7,
            offset: Some(3),
            limit: Some(9),
            from_read: true,
            seeded_from_context: false,
            is_partial_view: false,
        },
    );

    let o = orch(files, cwd.clone(), Arc::clone(&map));
    o.fire_instructions_loaded().await;

    let e = entry_for(&map, &plain);
    assert_eq!(e.content, "WHAT THE MODEL ACTUALLY READ");
    assert_eq!(e.mtime_ms, 7);
    assert_eq!(e.offset, Some(3));
    assert!(e.from_read);
    assert!(!e.seeded_from_context);
}
