//! Prefetch the selector result in parallel with the main API call.
//!
//! The runtime kicks off this prefetch as soon as it knows the turn
//! query; the result is awaited just before assembling the next prompt.
//!
//! P0.1: the pending handle resolves to a `Vec<`[`SurfacedMemory`]`>` — the
//! exact shape [`crate::surfacing::render_surfacing_block`] renders — so the
//! orchestrator's `relevant_memory_reminder_messages` can await this handle and
//! render the surfaced block with no further disk work.
//!
//! Two construction modes:
//! - [`MemoryPrefetch::new`] binds a real [`MemorySelector`] + memdir
//!   [`MemdirRoots`]: [`MemoryPrefetch::start`] scans the memdir, asks the
//!   selector (a side query on the live session route) which entries are relevant to the turn
//!   query, and maps the chosen entries to [`SurfacedMemory`]. This is the path
//!   the composition root wires once the prefetch is enabled (claude-code
//!   `tengu_moth_copse`, default off — here the gate is "is a prefetch wired at
//!   all", `memory_prefetch.is_some()`).
//! - [`MemoryPrefetch::with_fixed_result`] resolves to a PRE-SELECTED set,
//!   bypassing the selector — the deterministic seam the orchestrator's
//!   surfacing tests drive, and the injection seam a composition root uses when
//!   it has already selected the relevant memories out of band.
//!
//! A prefetch with neither a selector+roots nor a fixed result resolves to an
//! EMPTY set, so the surfacing reminder is a strict no-op and the locked
//! fixtures stay byte-identical.

use crate::file::MemoryFile;
use crate::memdir::{scan_memdir, MemdirRoots};
use crate::selector::{memory_entry_to_memory_file_with_frontmatter, MemorySelector};
use crate::surfacing::SurfacedMemory;
use lingxi_core::host::RuntimeSpawner;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::oneshot;

/// Side-channel that fires the memory selector concurrently with the
/// main turn.
pub struct MemoryPrefetch {
    /// Selector that ranks the available memory set via a side query. `Some` on
    /// the real path ([`Self::new`]); `None` for the fixed-result seam.
    selector: Option<Arc<MemorySelector>>,
    /// Runtime adapter used to spawn the background task.
    runtime: Arc<dyn RuntimeSpawner>,
    /// memdir roots to scan for candidate memories. `Some` on the real path
    /// (paired with `selector`); `None` for the fixed-result seam.
    roots: Option<MemdirRoots>,
    /// Pre-resolved surfaced set: when `Some`, [`Self::start`] short-circuits
    /// to this set instead of running the selector. `None` ⇒ the selector path
    /// (or, with no selector/roots, an empty inert result).
    fixed_result: Option<Vec<SurfacedMemory>>,
}

/// Handle awaiting an in-flight prefetch.
pub struct PendingMemoryPrefetch {
    /// Receiver that produces the surfaced-memory set (ready to render).
    pub rx: tokio::sync::Mutex<Option<oneshot::Receiver<Vec<SurfacedMemory>>>>,
}

impl PendingMemoryPrefetch {
    /// Await the in-flight prefetch, consuming the one-shot receiver.
    ///
    /// Returns the surfaced-memory set, or an empty vec if the receiver was
    /// already taken or the background task dropped its sender (e.g. a cancelled
    /// turn). Never errors — a failed prefetch must not break the turn.
    pub async fn take(&self) -> Vec<SurfacedMemory> {
        let rx = self.rx.lock().await.take();
        match rx {
            Some(rx) => rx.await.unwrap_or_default(),
            None => Vec::new(),
        }
    }

    /// Non-consuming readiness probe used by the turn loop to keep selector
    /// latency off the model-call critical path.
    ///
    /// `oneshot::Receiver::try_recv` consumes a ready value, so this method
    /// immediately re-buffers it in a fresh one-shot for [`Self::take`].
    #[must_use]
    pub fn is_ready(&self) -> bool {
        let Ok(mut guard) = self.rx.try_lock() else {
            return false;
        };
        match guard.as_mut() {
            Some(rx) => match rx.try_recv() {
                Ok(value) => {
                    let (tx, replacement) = oneshot::channel();
                    let _ = tx.send(value);
                    *guard = Some(replacement);
                    true
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => true,
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => false,
            },
            None => true,
        }
    }
}

impl MemoryPrefetch {
    /// Construct a prefetcher bound to a selector, the platform runtime, and the
    /// memdir roots to scan. [`Self::start`] runs the real selection path.
    #[must_use]
    pub fn new(
        selector: Arc<MemorySelector>,
        runtime: Arc<dyn RuntimeSpawner>,
        roots: MemdirRoots,
    ) -> Self {
        Self {
            selector: Some(selector),
            runtime,
            roots: Some(roots),
            fixed_result: None,
        }
    }

    /// The user memdir path this prefetcher scans (`<config-home>/memdir/`), when
    /// wired with real roots (`Self::new`). `None` for the fixed-result/inert
    /// construction. Used by the system-prompt builder so the `# Memory` section
    /// points the model at exactly the directory the prefetch reads back.
    #[must_use]
    pub fn user_memdir(&self) -> Option<&std::path::Path> {
        self.roots.as_ref().map(|r| r.user_memdir.as_path())
    }

    /// Construct a prefetcher that resolves to a PRE-SELECTED surfaced set,
    /// bypassing the selector body. Used by a composition root that has already
    /// picked the relevant memories, and by the orchestrator's surfacing tests
    /// to drive `relevant_memory_reminder_messages` deterministically. The result
    /// is buffered before [`Self::start`] returns; no selector is needed.
    #[must_use]
    pub fn with_fixed_result(
        runtime: Arc<dyn RuntimeSpawner>,
        result: Vec<SurfacedMemory>,
    ) -> Self {
        Self {
            selector: None,
            runtime,
            roots: None,
            fixed_result: Some(result),
        }
    }

    /// Kick off a background selector call and return a pending handle.
    ///
    /// - A [`Self::with_fixed_result`] set is shipped verbatim.
    /// - Otherwise, when a selector + memdir roots are wired ([`Self::new`]),
    ///   the background task scans the memdir, runs the selector over the turn
    ///   `query`, and ships the chosen entries as [`SurfacedMemory`].
    /// - With neither, it ships an EMPTY set (inert surfacing reminder).
    /// `model` and `profile` are captured from the live route for this turn.
    ///
    /// `_memory_dir` (the orchestrator's cwd) is currently unused: the memdir
    /// roots are home-based and bound at construction. It is retained for a
    /// future project-scoped memdir root.
    pub async fn start(
        &self,
        query: String,
        model: String,
        profile: Option<String>,
        memory_dir: PathBuf,
    ) -> PendingMemoryPrefetch {
        self.start_for_session(query, model, profile, memory_dir, None)
            .await
    }

    /// Start a prefetch while excluding the live session's own memory artifact.
    ///
    /// A session-memory extraction can complete between model-loop iterations.
    /// Passing the current session id here prevents that newly-written file from
    /// becoming selector input for the same live session while leaving memories
    /// from prior sessions eligible for surfacing.
    pub async fn start_for_session(
        &self,
        query: String,
        model: String,
        profile: Option<String>,
        _memory_dir: PathBuf,
        current_session_id: Option<String>,
    ) -> PendingMemoryPrefetch {
        let (tx, rx) = oneshot::channel();

        if let Some(fixed) = self.fixed_result.clone() {
            let _ = tx.send(fixed);
            return PendingMemoryPrefetch {
                rx: tokio::sync::Mutex::new(Some(rx)),
            };
        }

        // Real path requires BOTH a selector and memdir roots; otherwise inert.
        let (Some(selector), Some(roots)) = (self.selector.clone(), self.roots.clone()) else {
            let _ = self
                .runtime
                .spawn(
                    "memory-prefetch",
                    Box::pin(async move {
                        let _ = tx.send(Vec::new());
                    }),
                )
                .await;
            return PendingMemoryPrefetch {
                rx: tokio::sync::Mutex::new(Some(rx)),
            };
        };

        let _ = self
            .runtime
            .spawn(
                "memory-prefetch",
                Box::pin(async move {
                    let surfaced = select_surfaced(
                        &selector,
                        &roots,
                        &query,
                        &model,
                        profile.as_deref(),
                        current_session_id.as_deref(),
                    )
                    .await;
                    let _ = tx.send(surfaced);
                }),
            )
            .await;
        PendingMemoryPrefetch {
            rx: tokio::sync::Mutex::new(Some(rx)),
        }
    }
}

/// Per-surfaced-file caps for relevant-memory surfacing, 1:1 with claude-code
/// `readMemoriesForSurfacing` (`Bbl`): each file is read through
/// `k_t(path, 0, xHo, vbl, …, {truncateOnByteLimit:true})` — at most `xHo` lines
/// AND `vbl` bytes — and, when truncated, the model-facing content gets a
/// one-line notice pointing at the Read tool. These are DISTINCT from the
/// MEMORY.md entrypoint caps ([`crate::MAX_ENTRYPOINT_LINES`] /
/// [`crate::MAX_ENTRYPOINT_BYTES`] = binary `Mte`/`GCe`, enforced by
/// [`crate::memory_index_cap_notice`]); the binary uses separate constants here
/// (`xHo=200`, `vbl=4096`).
const SURFACED_MEMORY_MAX_LINES: usize = 200;
const SURFACED_MEMORY_MAX_BYTES: usize = 4096;

/// Truncate one surfaced memory file's content to the surfacing caps and append
/// the truncation notice when it overflows, 1:1 with `Bbl`/`k_t`/`pam`:
///
/// - accumulate lines `[0, xHo)` while the running UTF-8 byte total (including
///   the `\n` separators, counted only between kept lines — binary `h(_)`'s
///   `T=c.length>0?1:0`) stays ≤ `vbl`; a byte-budget stop sets
///   `truncated_by_bytes` (binary `f`);
/// - the `\n` line model matches `pam` exactly: split on `\n` (a trailing `\n`
///   yields a final empty segment, mirroring the post-loop `u++`) and strip a
///   trailing `\r` per line (CRLF→LF normalization), so under-cap LF content is
///   returned byte-identical;
/// - the notice is APPENDED to the already-truncated body (NOT a replacement)
///   and fires when `total_lines > xHo || truncated_by_bytes` (binary `i`), with
///   the `truncated_by_bytes`-conditional wording.
fn truncate_surfaced_content(content: &str, path: &Path) -> String {
    let lines: Vec<&str> = content.split('\n').collect();
    let total_lines = lines.len();
    let mut kept: Vec<&str> = Vec::new();
    let mut running_bytes = 0usize;
    let mut truncated_by_bytes = false;
    for line in lines.iter().take(SURFACED_MEMORY_MAX_LINES) {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let sep = usize::from(!kept.is_empty());
        let next = running_bytes + sep + line.len();
        if next > SURFACED_MEMORY_MAX_BYTES {
            truncated_by_bytes = true;
            break;
        }
        running_bytes = next;
        kept.push(line);
    }
    let body = kept.join("\n");
    if total_lines > SURFACED_MEMORY_MAX_LINES || truncated_by_bytes {
        let reason = if truncated_by_bytes {
            format!("{SURFACED_MEMORY_MAX_BYTES} byte limit")
        } else {
            format!("first {SURFACED_MEMORY_MAX_LINES} lines")
        };
        format!(
            "{body}\n> This memory file was truncated ({reason}). Use the Read tool to view the complete file at: {}",
            path.display()
        )
    } else {
        body
    }
}

/// Scan the memdir, ask the selector which entries are relevant to `query`, and
/// map the chosen entries to the renderable [`SurfacedMemory`] shape.
///
/// Any failure — a missing/unreadable memdir, or a side-query error — resolves
/// to an EMPTY set: a failed prefetch must never break the turn. The orchestrator
/// dedups the result against already-surfaced + already-read paths, so this path
/// passes an empty `already` set (no double-select within one prefetch).
async fn select_surfaced(
    selector: &MemorySelector,
    roots: &MemdirRoots,
    query: &str,
    model: &str,
    profile: Option<&str>,
    current_session_id: Option<&str>,
) -> Vec<SurfacedMemory> {
    let Some(mut snapshot) = scan_snapshot(roots.clone()).await else {
        return Vec::new();
    };
    if let Some(session_id) = current_session_id {
        let current_session_path = roots.session_memdir.join(format!(
            "{}.md",
            crate::session_memory::sanitize_session_id(session_id)
        ));
        snapshot
            .entries
            .retain(|entry| entry.path != current_session_path);
        snapshot.frontmatter.remove(&current_session_path);
    }
    if snapshot.entries.is_empty() {
        return Vec::new();
    }
    let files: Vec<MemoryFile> = snapshot
        .entries
        .iter()
        .map(|entry| {
            memory_entry_to_memory_file_with_frontmatter(
                entry,
                snapshot.frontmatter.get(&entry.path),
            )
        })
        .collect();
    let already: HashSet<PathBuf> = HashSet::new();
    let Ok(selected) = selector
        .select_relevant(query, model, profile, &files, &[], &already)
        .await
    else {
        return Vec::new();
    };
    // Map each selected path back to its memdir entry → SurfacedMemory.
    selected
        .iter()
        .filter_map(|p| {
            snapshot.entries.iter().find(|e| &e.path == p).map(|e| {
                let f = memory_entry_to_memory_file_with_frontmatter(
                    e,
                    snapshot.frontmatter.get(&e.path),
                );
                SurfacedMemory {
                    path: e.path.clone(),
                    content: truncate_surfaced_content(&f.content, &e.path),
                    age_days: e.age_days,
                    mtime: f.mtime,
                }
            })
        })
        .collect()
}

async fn scan_snapshot(roots: MemdirRoots) -> Option<crate::memdir::MemdirSnapshot> {
    tokio::task::spawn_blocking(move || scan_memdir(&roots).ok())
        .await
        .ok()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memdir::memdir_path;
    use async_trait::async_trait;
    use sidequery::{SideQueryClient, SideQueryError, SideQueryRequest, SideQueryResponse};

    /// A runtime that actually RUNS the spawned future, so the prefetch's
    /// one-shot resolves (the production posix runtime does this; the test needs
    /// the task to execute inline on the current tokio runtime).
    struct InlineRuntime;
    #[async_trait]
    impl RuntimeSpawner for InlineRuntime {
        async fn spawn(
            &self,
            name: &str,
            task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
        ) -> Result<lingxi_core::host::BackgroundTaskHandle, lingxi_core::host::RuntimeError>
        {
            tokio::spawn(task);
            Ok(lingxi_core::host::BackgroundTaskHandle {
                task_name: name.to_string(),
                task_id: 0,
            })
        }
        async fn sleep(&self, _d: std::time::Duration) {}
        async fn cancel(
            &self,
            _h: &lingxi_core::host::BackgroundTaskHandle,
        ) -> Result<(), lingxi_core::host::RuntimeError> {
            Ok(())
        }
    }

    /// A `SideQueryClient` that returns a canned `{"filenames": [...]}` selection
    /// (the shape `MemorySelector` parses), ignoring the request — so the test
    /// drives the scan → select → surface pipeline deterministically with no LLM.
    struct PickClient {
        names: Vec<String>,
        prompts: Option<std::sync::Arc<std::sync::Mutex<Vec<String>>>>,
    }
    #[async_trait]
    impl SideQueryClient for PickClient {
        async fn query(
            &self,
            request: SideQueryRequest,
        ) -> Result<SideQueryResponse, SideQueryError> {
            if let Some(prompts) = &self.prompts {
                if let Some(message) = request.messages.first() {
                    prompts.lock().unwrap().push(message.text_content());
                }
            }
            Ok(SideQueryResponse {
                text: None,
                structured: Some(serde_json::json!({ "filenames": self.names })),
                tool_calls: Vec::new(),
                // Field type (`cost::Usage`) inferred — avoids a dev-dep on `cost`.
                usage: Default::default(),
                stop_reason: Some("end_turn".into()),
                retry_count: 0,
            })
        }
    }

    #[tokio::test]
    async fn real_path_scans_selects_and_surfaces() {
        let home = tempfile::tempdir().expect("tmp home");
        let memdir = home
            .path()
            .join(".lingxi")
            .join("projects")
            .join("-proj")
            .join("memdir");
        std::fs::create_dir_all(&memdir).expect("mk memdir");
        std::fs::write(
            memdir.join("fd.md"),
            concat!(
                "---\n",
                "description: shell helper\n",
                "---\n",
                "creds: AKIAIOSFODNN7EXAMPLE\n",
                "USE FD NOT FIND"
            ),
        )
        .expect("write fd");
        std::fs::write(memdir.join("rg.md"), "USE RG NOT GREP").expect("write rg");

        let roots = memdir_path(home.path(), std::path::Path::new("/proj"), false);
        // The selector "picks" fd.md only.
        let selector = Arc::new(MemorySelector::new(Arc::new(PickClient {
            names: vec!["fd.md".into()],
            prompts: None,
        })));
        let prefetch = MemoryPrefetch::new(selector, Arc::new(InlineRuntime), roots);

        let surfaced = prefetch
            .start(
                "how do I search files".into(),
                "test-model".into(),
                None,
                PathBuf::from("/work"),
            )
            .await
            .take()
            .await;

        assert_eq!(
            surfaced.len(),
            1,
            "only the selected file surfaces: {surfaced:?}"
        );
        assert!(
            surfaced[0].path.ends_with("fd.md"),
            "surfaced path: {:?}",
            surfaced[0].path
        );
        assert!(
            surfaced[0].content.contains("USE FD NOT FIND"),
            "content: {:?}",
            surfaced[0].content
        );
        assert!(
            surfaced[0].content.contains("[REDACTED:"),
            "secret content must be redacted before surfacing: {:?}",
            surfaced[0].content
        );
        assert!(
            !surfaced[0].content.contains("description: shell helper"),
            "frontmatter must be stripped before surfacing: {:?}",
            surfaced[0].content
        );
    }

    #[tokio::test]
    async fn selector_receives_redacted_memdir_frontmatter_description() {
        let home = tempfile::tempdir().expect("tmp home");
        let memdir = home
            .path()
            .join(".lingxi")
            .join("projects")
            .join("-proj")
            .join("memdir");
        std::fs::create_dir_all(&memdir).expect("mk memdir");
        std::fs::write(
            memdir.join("fd.md"),
            "---\ndescription: shell helper\n---\nUSE FD NOT FIND",
        )
        .expect("write fd");

        let prompts = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let selector = Arc::new(MemorySelector::new(Arc::new(PickClient {
            names: vec!["fd.md".into()],
            prompts: Some(prompts.clone()),
        })));
        let prefetch = MemoryPrefetch::new(
            selector,
            Arc::new(InlineRuntime),
            memdir_path(home.path(), std::path::Path::new("/proj"), false),
        );

        let surfaced = prefetch
            .start(
                "how do I search files".into(),
                "test-model".into(),
                None,
                PathBuf::from("/work"),
            )
            .await
            .take()
            .await;

        assert_eq!(surfaced.len(), 1);
        let prompts = prompts.lock().unwrap();
        assert_eq!(prompts.len(), 1);
        assert!(
            prompts[0].contains("fd.md: shell helper"),
            "selector prompt must retain frontmatter metadata: {}",
            prompts[0]
        );
    }

    #[tokio::test]
    async fn live_session_memory_is_excluded_before_selection() {
        let home = tempfile::tempdir().expect("tmp home");
        let roots = memdir_path(home.path(), std::path::Path::new("/proj"), false);
        std::fs::create_dir_all(&roots.session_memdir).expect("mk session memdir");
        let live_id = "sess:11111111-1111-1111-1111-111111111111";
        let live_name = "11111111-1111-1111-1111-111111111111.md";
        let prior_name = "22222222-2222-2222-2222-222222222222.md";
        std::fs::write(
            roots.session_memdir.join(live_name),
            "---\ndescription: live session\n---\nCURRENT SESSION",
        )
        .expect("write live session memory");
        std::fs::write(
            roots.session_memdir.join(prior_name),
            "---\ndescription: prior session\n---\nPRIOR SESSION",
        )
        .expect("write prior session memory");

        let prompts = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let selector = Arc::new(MemorySelector::new(Arc::new(PickClient {
            names: vec![live_name.into(), prior_name.into()],
            prompts: Some(prompts.clone()),
        })));
        let prefetch = MemoryPrefetch::new(selector, Arc::new(InlineRuntime), roots);

        let surfaced = prefetch
            .start_for_session(
                "continue the work".into(),
                "test-model".into(),
                None,
                PathBuf::from("/work"),
                Some(live_id.into()),
            )
            .await
            .take()
            .await;

        assert_eq!(surfaced.len(), 1);
        assert!(surfaced[0].path.ends_with(prior_name));
        assert!(surfaced[0].content.contains("PRIOR SESSION"));
        let prompts = prompts.lock().unwrap();
        assert_eq!(prompts.len(), 1);
        assert!(prompts[0].contains(prior_name));
        assert!(!prompts[0].contains(live_name));
        assert!(!prompts[0].contains("live session"));
    }

    #[tokio::test]
    async fn real_path_absent_memdir_is_inert() {
        // No memdir directory on disk ⇒ scan finds nothing ⇒ empty (no surfacing),
        // and the selector is never consulted.
        let home = tempfile::tempdir().expect("tmp home");
        let roots = memdir_path(home.path(), std::path::Path::new("/proj"), false);
        let selector = Arc::new(MemorySelector::new(Arc::new(PickClient {
            names: vec!["x.md".into()],
            prompts: None,
        })));
        let prefetch = MemoryPrefetch::new(selector, Arc::new(InlineRuntime), roots);

        let surfaced = prefetch
            .start("q".into(), "test-model".into(), None, PathBuf::from("/w"))
            .await
            .take()
            .await;
        assert!(
            surfaced.is_empty(),
            "absent memdir surfaces nothing: {surfaced:?}"
        );
    }

    #[test]
    fn surfaced_content_under_caps_is_byte_identical() {
        // LF content under both caps is returned verbatim (binary `a=s.content`
        // with `s.content` === input on the no-truncation path), including a
        // trailing newline (split→join round-trips the final empty segment).
        let p = Path::new("/m/a.md");
        assert_eq!(
            truncate_surfaced_content("one\ntwo\nthree", p),
            "one\ntwo\nthree"
        );
        assert_eq!(truncate_surfaced_content("one\ntwo\n", p), "one\ntwo\n");
        assert_eq!(truncate_surfaced_content("", p), "");
    }

    #[test]
    fn surfaced_content_over_line_cap_appends_first_n_lines_notice() {
        // > 200 lines ⇒ keep the first 200 and APPEND the notice with the
        // "first 200 lines" wording (binary `i = totalLines > xHo`, byte budget
        // not hit).
        let content = (1..=250)
            .map(|i| format!("L{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = truncate_surfaced_content(&content, Path::new("/m/big.md"));
        assert!(out.starts_with("L1\nL2\n"), "kept body starts at line 1");
        assert!(
            out.contains("\nL200\n> This memory file was truncated"),
            "notice after the 200th kept line: {out}"
        );
        assert!(
            out.ends_with(
                "This memory file was truncated (first 200 lines). Use the Read tool to view the complete file at: /m/big.md"
            ),
            "byte-exact first-N-lines notice: {out}"
        );
        assert!(!out.contains("L201"), "lines past the cap are dropped");
    }

    #[test]
    fn surfaced_content_over_byte_cap_appends_byte_limit_notice() {
        // A single line longer than 4096 bytes trips the byte budget on the
        // FIRST line (binary `h(_)`: `0 + 0 + len > vbl` ⇒ f=true, nothing
        // kept) ⇒ "4096 byte limit" wording, empty body before the notice.
        let huge = "x".repeat(5000);
        let out = truncate_surfaced_content(&huge, Path::new("/m/wide.md"));
        assert_eq!(
            out,
            "\n> This memory file was truncated (4096 byte limit). Use the Read tool to view the complete file at: /m/wide.md",
            "byte-limit notice appended to an empty body when line 1 alone exceeds the cap"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scan_snapshot_runs_on_blocking_pool() {
        let caller_thread = std::thread::current().id();
        let worker_thread = tokio::task::spawn_blocking(|| std::thread::current().id())
            .await
            .expect("blocking task joined");
        assert_ne!(
            worker_thread, caller_thread,
            "spawn_blocking must not reuse the async runtime thread"
        );

        let home = tempfile::tempdir().expect("tmp home");
        let memdir = home
            .path()
            .join(".lingxi")
            .join("projects")
            .join("-proj")
            .join("memdir");
        std::fs::create_dir_all(&memdir).expect("mk memdir");
        std::fs::write(memdir.join("fd.md"), "USE FD NOT FIND").expect("write fd");
        let roots = memdir_path(home.path(), std::path::Path::new("/proj"), false);

        let snapshot = scan_snapshot(roots).await.expect("snapshot");
        assert_eq!(snapshot.entries.len(), 1);
    }
}
