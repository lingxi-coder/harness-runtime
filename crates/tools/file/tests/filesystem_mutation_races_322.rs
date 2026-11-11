//! Public mutation regressions at the async backup-hook/owned-commit boundary.
//!
//! Every file belongs to a TempDir. The fake history sink is a deterministic
//! interleaving point after preparation, rather than a timer-based race.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use lingxi_core::host::{rooted_fs::RootedWriteParent, FileHistorySink};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::Barrier;
use tokio_util::sync::CancellationToken;
use tool_api::read_file_state::ReadFileEntry;
use tool_api::test_support::{ctx_for_file_tools, fresh_ctx, fresh_tx, make_dummy_fs};
use tool_api::tool_trait::{Tool, ToolCallResult, ToolError};
use tool_api::BuiltinToolContext;
use tool_file::{FileEditTool, FileReadTool, FileWriteTool, FILE_CONTENT_CHANGED_LINTER_MESSAGE};

const ORIGINAL: &[u8] = b"ORIGINAL\n";
const USER_EDIT: &[u8] = b"USEREDIT\n";

fn context(root: &Path) -> BuiltinToolContext {
    ctx_for_file_tools(
        make_dummy_fs(),
        Arc::new(telemetry::AnalyticsBus::new()),
        vec![root.to_path_buf()],
    )
}

fn input(value: &Value) -> Utf16JsonProjection {
    Utf16JsonProjection::from_serde_value(value).unwrap()
}

#[derive(Clone, Copy, Debug)]
enum Mutation {
    Edit,
    Write,
}

impl Mutation {
    fn tool(self, ctx: BuiltinToolContext) -> Box<dyn Tool> {
        match self {
            Self::Edit => Box::new(FileEditTool::new(ctx)),
            Self::Write => Box::new(FileWriteTool::new(ctx)),
        }
    }

    fn request(self, target: &Path, exists: bool, replacement: &str) -> Utf16JsonProjection {
        input(&match self {
            Self::Edit => json!({
                "file_path": target,
                "old_string": if exists { "ORIGINAL\n" } else { "" },
                "new_string": replacement,
            }),
            Self::Write => json!({"file_path": target, "content": replacement}),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DiskObservation {
    bytes: Vec<u8>,
    modified: SystemTime,
}

fn disk_observation(path: &Path) -> DiskObservation {
    DiskObservation {
        bytes: std::fs::read(path).unwrap(),
        modified: std::fs::metadata(path).unwrap().modified().unwrap(),
    }
}

#[derive(Debug, PartialEq, Eq)]
struct CachedEntry {
    path: PathBuf,
    entry: ReadFileEntry,
    units: Option<Vec<u16>>,
    requested_aliases: Vec<Vec<PathBuf>>,
}

#[derive(Debug, PartialEq, Eq)]
struct CacheSnapshot {
    entries: Vec<CachedEntry>,
    model_context_keys: Vec<PathBuf>,
    bytes: u64,
}

fn cache_snapshot(ctx: &BuiltinToolContext) -> CacheSnapshot {
    let mut state = ctx.read_file_state.lock().unwrap();
    let mut keys = state.keys();
    keys.sort();
    let entries = keys
        .into_iter()
        .map(|path| CachedEntry {
            entry: state.peek(&path).unwrap(),
            units: state.get_content_units(&path),
            requested_aliases: state.requested_path_groups(&path),
            path,
        })
        .collect();
    let mut model_context_keys = state.model_context_keys();
    model_context_keys.sort();
    CacheSnapshot {
        entries,
        model_context_keys,
        bytes: state.total_bytes(),
    }
}

async fn observe_with_read(ctx: &BuiltinToolContext, target: &Path) {
    FileReadTool::new(ctx.clone())
        .call(
            input(&json!({"file_path": target})),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap_or_else(|error| panic!("Read failed before race admission: {error:?}"));
}

enum BeforeCommit {
    Replace {
        bytes: Vec<u8>,
        preserve_mtime: bool,
    },
    Cancel(CancellationToken),
    Barrier(Arc<Barrier>),
}

struct HistoryHook {
    target: PathBuf,
    before_commit: BeforeCommit,
    preparations: AtomicUsize,
    completions: Mutex<Vec<PathBuf>>,
    user_write: Mutex<Option<DiskObservation>>,
}

impl HistoryHook {
    fn new(target: PathBuf, before_commit: BeforeCommit) -> Arc<Self> {
        Arc::new(Self {
            target,
            before_commit,
            preparations: AtomicUsize::new(0),
            completions: Mutex::new(Vec::new()),
            user_write: Mutex::new(None),
        })
    }

    fn completion_count(&self) -> usize {
        self.completions.lock().unwrap().len()
    }
}

#[async_trait]
impl FileHistorySink for HistoryHook {
    async fn track_edit(&self, file_path: &str) {
        assert_eq!(Path::new(file_path), self.target);
        assert_eq!(self.preparations.fetch_add(1, Ordering::SeqCst), 0);
        match &self.before_commit {
            BeforeCommit::Replace {
                bytes,
                preserve_mtime,
            } => {
                let prior_mtime = preserve_mtime.then(|| {
                    filetime::FileTime::from_last_modification_time(
                        &std::fs::metadata(&self.target).unwrap(),
                    )
                });
                std::fs::write(&self.target, bytes).unwrap();
                if let Some(mtime) = prior_mtime {
                    filetime::set_file_mtime(&self.target, mtime).unwrap();
                }
                *self.user_write.lock().unwrap() = Some(disk_observation(&self.target));
            }
            BeforeCommit::Cancel(token) => token.cancel(),
            BeforeCommit::Barrier(barrier) => {
                barrier.wait().await;
            }
        }
    }

    fn track_completed_write(&self, file_path: &str, _parent: RootedWriteParent) {
        // Deliberately synchronous: this receipt is part of the owned commit
        // and must not wait for any executor work that cancellation can drop.
        assert_eq!(Path::new(file_path), self.target);
        self.completions
            .lock()
            .unwrap()
            .push(PathBuf::from(file_path));
    }
}

fn assert_content_changed(result: Result<ToolCallResult, ToolError>) {
    match result {
        Err(ToolError::InvalidInput(message)) => {
            assert_eq!(
                message.string_at(""),
                Some(FILE_CONTENT_CHANGED_LINTER_MESSAGE)
            );
        }
        Err(other) => panic!("expected the commit-time stale-content error, got {other:?}"),
        Ok(_) => panic!("a changed target must not be overwritten"),
    }
}

#[tokio::test]
async fn edit_and_write_preserve_same_metadata_user_changes_before_commit() {
    assert_eq!(ORIGINAL.len(), USER_EDIT.len());
    for mutation in [Mutation::Edit, Mutation::Write] {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let target = root.join("owned.txt");
        std::fs::write(&target, ORIGINAL).unwrap();
        let before = disk_observation(&target);
        let ctx = context(&root);
        observe_with_read(&ctx, &target).await;
        let cache_before = cache_snapshot(&ctx);
        let hook = HistoryHook::new(
            target.clone(),
            BeforeCommit::Replace {
                bytes: USER_EDIT.to_vec(),
                preserve_mtime: true,
            },
        );
        let mut invocation = fresh_ctx();
        invocation.file_history = Some(hook.clone());
        let result = mutation
            .tool(ctx.clone())
            .call(
                mutation.request(&target, true, "MODEL-UPDATE\n"),
                invocation,
                fresh_tx(),
            )
            .await;
        assert_content_changed(result);
        let user_write = hook.user_write.lock().unwrap().clone().unwrap();
        assert_eq!(user_write.bytes, USER_EDIT);
        assert_eq!(
            user_write.modified, before.modified,
            "the hook restores mtime to defeat a metadata-only guard"
        );
        assert_eq!(
            disk_observation(&target),
            user_write,
            "{mutation:?} changed the user's intervening write"
        );
        assert_eq!(
            cache_snapshot(&ctx),
            cache_before,
            "a refused commit must not publish model-owned content"
        );
        assert_eq!(hook.preparations.load(Ordering::SeqCst), 1);
        assert_eq!(hook.completion_count(), 0);
    }
}

#[tokio::test]
async fn missing_edit_and_write_preserve_a_file_created_after_preparation() {
    for mutation in [Mutation::Edit, Mutation::Write] {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let target = root.join("new.txt");
        let ctx = context(&root);
        let cache_before = cache_snapshot(&ctx);
        let hook = HistoryHook::new(
            target.clone(),
            BeforeCommit::Replace {
                bytes: b"CREATED-BY-USER\n".to_vec(),
                preserve_mtime: false,
            },
        );
        let mut invocation = fresh_ctx();
        invocation.file_history = Some(hook.clone());
        let result = mutation
            .tool(ctx.clone())
            .call(
                mutation.request(&target, false, "MODEL-CREATION\n"),
                invocation,
                fresh_tx(),
            )
            .await;
        assert_content_changed(result);
        assert_eq!(
            disk_observation(&target),
            hook.user_write.lock().unwrap().clone().unwrap()
        );
        assert_eq!(cache_snapshot(&ctx), cache_before);
        assert_eq!(hook.preparations.load(Ordering::SeqCst), 1);
        assert_eq!(hook.completion_count(), 0);
    }
}

#[tokio::test]
async fn cancellation_from_history_hook_does_not_commit_or_create_parents() {
    for (mutation, exists) in [
        (Mutation::Edit, true),
        (Mutation::Write, true),
        (Mutation::Edit, false),
        (Mutation::Write, false),
    ] {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        // Edit requires an existing immediate parent before its history hook.
        // Write supports missing parents, which must stay absent on cancel.
        let target = if !exists && matches!(mutation, Mutation::Write) {
            root.join("new/parents/owned.txt")
        } else {
            root.join("owned.txt")
        };
        let ctx = context(&root);
        let before = if exists {
            std::fs::write(&target, ORIGINAL).unwrap();
            observe_with_read(&ctx, &target).await;
            Some(disk_observation(&target))
        } else {
            None
        };
        let cache_before = cache_snapshot(&ctx);
        let cancel = CancellationToken::new();
        let hook = HistoryHook::new(target.clone(), BeforeCommit::Cancel(cancel.clone()));
        let mut invocation = fresh_ctx();
        invocation.cancel = Some(cancel);
        invocation.file_history = Some(hook.clone());
        let result = mutation
            .tool(ctx.clone())
            .call(
                mutation.request(&target, exists, "CANCELLED\n"),
                invocation,
                fresh_tx(),
            )
            .await;
        assert!(
            matches!(result, Err(ToolError::Aborted)),
            "{mutation:?} did not cancel before commit"
        );
        if let Some(before) = before {
            assert_eq!(disk_observation(&target), before);
        } else {
            assert!(!target.exists());
        }
        assert!(
            !root.join("new").exists(),
            "cancelled Write created parent directories"
        );
        assert_eq!(cache_snapshot(&ctx), cache_before);
        assert_eq!(hook.preparations.load(Ordering::SeqCst), 1);
        assert_eq!(hook.completion_count(), 0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_prepared_mutations_cannot_silently_overwrite_each_other() {
    for (left_kind, right_kind) in [
        (Mutation::Edit, Mutation::Edit),
        (Mutation::Write, Mutation::Write),
        (Mutation::Edit, Mutation::Write),
    ] {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let target = root.join("owned.txt");
        std::fs::write(&target, ORIGINAL).unwrap();
        let ctx = context(&root);
        observe_with_read(&ctx, &target).await;
        let barrier = Arc::new(Barrier::new(2));
        let left_hook = HistoryHook::new(target.clone(), BeforeCommit::Barrier(barrier.clone()));
        let right_hook = HistoryHook::new(target.clone(), BeforeCommit::Barrier(barrier));
        let mut left_ctx = fresh_ctx();
        left_ctx.file_history = Some(left_hook.clone());
        let mut right_ctx = fresh_ctx();
        right_ctx.file_history = Some(right_hook.clone());
        let left = left_kind.tool(ctx.clone());
        let right = right_kind.tool(ctx.clone());
        let (left_result, right_result) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(
                left.call(
                    left_kind.request(&target, true, "LEFT-RESULT\n"),
                    left_ctx,
                    fresh_tx()
                ),
                right.call(
                    right_kind.request(&target, true, "RIGHT-RESULT\n"),
                    right_ctx,
                    fresh_tx()
                ),
            )
        })
        .await
        .expect("both preparations and commits must settle");
        let left_succeeded = left_result.is_ok();
        let right_succeeded = right_result.is_ok();
        assert_ne!(
            left_succeeded, right_succeeded,
            "{left_kind:?}/{right_kind:?} must settle one winner and one stale snapshot"
        );
        let expected = if left_succeeded {
            assert_content_changed(right_result);
            assert_eq!(left_hook.completion_count(), 1);
            assert_eq!(right_hook.completion_count(), 0);
            "LEFT-RESULT\n"
        } else {
            assert_content_changed(left_result);
            assert_eq!(left_hook.completion_count(), 0);
            assert_eq!(right_hook.completion_count(), 1);
            "RIGHT-RESULT\n"
        };
        assert_eq!(left_hook.preparations.load(Ordering::SeqCst), 1);
        assert_eq!(right_hook.preparations.load(Ordering::SeqCst), 1);
        assert_eq!(std::fs::read(&target).unwrap(), expected.as_bytes());
        let route = tool_file::normalize_model_file_path(target.to_str().unwrap(), &ctx.cwd());
        let cached = tool_api::read_file_state::get(&ctx.read_file_state, &route).unwrap();
        assert_eq!(
            cached.content, expected,
            "only the winning write may publish its content"
        );
        assert!(!cached.from_read);
        assert_eq!(
            tool_api::read_file_state::get_content_units(&ctx.read_file_state, &route).unwrap(),
            expected.encode_utf16().collect::<Vec<_>>()
        );
    }
}
