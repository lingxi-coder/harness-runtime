//! Git worktree management abstraction.
//!
//! Engine code receives an `Arc<dyn WorktreeManager>` so subagents that need
//! isolated working copies (e.g. parallel coding tasks) can create one without
//! the engine depending on git directly. Platform crates provide concrete
//! implementations; on platforms without git the manager reports
//! [`is_supported`](WorktreeManager::is_supported) as `false` and subagents
//! degrade gracefully — see [`crate::worktree::WorktreeError::Unsupported`].
//!
//! See spec §10.7 (Worktree degradation policy).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;
use thiserror::Error;

/// Manages disposable git worktrees for subagents.
///
/// Implementations are platform-specific: a POSIX implementation may shell out
/// to `git worktree`, a Windows implementation may use libgit2, and platforms
/// without git support can return [`WorktreeError::Unsupported`] from every
/// method.
#[async_trait]
pub trait WorktreeManager: Send + Sync {
    /// Create a new worktree for the given `slug`, optionally branched from
    /// `base_branch`. `copy_includes` lists untracked paths (relative to the
    /// repository root) that should be copied into the worktree.
    async fn create_worktree(
        &self,
        slug: &str,
        base_branch: Option<&str>,
        copy_includes: &[PathBuf],
    ) -> Result<WorktreeHandle, WorktreeError>;

    /// Remove the worktree identified by `handle`. Implementations should be
    /// idempotent so callers can safely retry on transient errors.
    async fn remove_worktree(&self, handle: &WorktreeHandle) -> Result<(), WorktreeError>;

    /// Enumerate all worktrees currently managed.
    async fn list_worktrees(&self) -> Result<Vec<WorktreeInfo>, WorktreeError>;

    /// Garbage-collect worktrees older than `max_age`, returning the paths
    /// that were removed.
    async fn cleanup_stale(&self, max_age: Duration) -> Result<Vec<PathBuf>, WorktreeError>;

    /// `true` if the host platform supports worktrees. Engine code checks this
    /// before calling [`Self::create_worktree`] so that subagents marked as
    /// [`WorktreeRequirement::Optional`](crate::worktree::WorktreeError) can
    /// degrade to in-place execution.
    fn is_supported(&self) -> bool;

    /// Summarize the dirty state of the worktree at `handle` — how many
    /// uncommitted files it has and how many commits sit ahead of its base.
    ///
    /// Mirrors claude-code's `countWorktreeChanges`
    /// (`src/tools/ExitWorktreeTool/ExitWorktreeTool.ts:79-113`): a
    /// `git status --porcelain` non-blank line count, plus a
    /// `git rev-list --count base..HEAD` ahead-commit count.
    ///
    /// Returns `Ok(None)` to mean "state could not be reliably determined" —
    /// callers using this as a safety gate before a destructive removal MUST
    /// treat `None` as *unknown, assume unsafe* (fail-closed). A silent
    /// `0/0` would let a removal destroy real work. `None` is returned when
    /// git cannot be queried (lock file, corrupt index, not a git dir) or
    /// when no baseline commit is available to count ahead-commits.
    ///
    /// The default implementation returns `Ok(None)` so platforms without git
    /// (and the pre-existing impls predating this method) compile unchanged
    /// and fail-closed by default — only platforms that actually shell out to
    /// git override it.
    async fn worktree_change_summary(
        &self,
        handle: &WorktreeHandle,
    ) -> Result<Option<WorktreeChangeSummary>, WorktreeError> {
        let _ = handle;
        Ok(None)
    }

    /// Enter an already-existing git worktree at `path` (206 `EnterWorktree`
    /// `e.path` branch). Resolves the worktree's current branch. Returns a
    /// handle pointing at the existing worktree; does NOT create anything
    /// (contrast [`Self::create_worktree`], which always makes a new one).
    ///
    /// The default implementation returns [`WorktreeError::Unsupported`] so
    /// platforms/mocks predating this method compile unchanged and degrade
    /// gracefully — mirrors [`Self::worktree_change_summary`]'s degradation
    /// policy.
    async fn enter_existing(
        &self,
        path: &std::path::Path,
    ) -> Result<WorktreeHandle, WorktreeError> {
        let _ = path;
        Err(WorktreeError::Unsupported)
    }
}

/// Dirty-state summary of a worktree returned by
/// [`WorktreeManager::worktree_change_summary`].
///
/// Byte-faithful to claude-code's `ChangeSummary`
/// (`src/tools/ExitWorktreeTool/ExitWorktreeTool.ts:62-65`): the uncommitted
/// working-tree file count plus the number of commits ahead of the base.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeChangeSummary {
    /// Number of uncommitted files (non-blank `git status --porcelain` lines).
    pub changed_files: usize,
    /// Number of commits on the worktree branch ahead of its base.
    pub commits: usize,
}

impl WorktreeChangeSummary {
    /// `true` when the worktree carries work a removal would discard.
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.changed_files > 0 || self.commits > 0
    }

    /// The uncommitted-files fragment, e.g. `"3 uncommitted files"` or
    /// `"1 uncommitted file"`. Byte-faithful to claude-code
    /// (`ExitWorktreeTool.ts:205-208`). Returns `None` when there are none.
    #[must_use]
    pub fn changed_files_phrase(&self) -> Option<String> {
        if self.changed_files == 0 {
            return None;
        }
        let noun = if self.changed_files == 1 {
            "file"
        } else {
            "files"
        };
        Some(format!("{} uncommitted {noun}", self.changed_files))
    }

    /// The ahead-commits fragment, e.g. `"2 commits on <branch>"` or
    /// `"1 commit on <branch>"`. Byte-faithful to claude-code
    /// (`ExitWorktreeTool.ts:210-213`). `branch` falls back to
    /// `"the worktree branch"` upstream when unknown. Returns `None` when
    /// there are no ahead-commits.
    #[must_use]
    pub fn commits_phrase(&self, branch: &str) -> Option<String> {
        if self.commits == 0 {
            return None;
        }
        let noun = if self.commits == 1 {
            "commit"
        } else {
            "commits"
        };
        Some(format!("{} {noun} on {branch}", self.commits))
    }

    /// The trailing "Discarded …" note for a removal, byte-faithful to
    /// claude-code (`ExitWorktreeTool.ts:299-309`): the commits fragment
    /// comes first, then the uncommitted-files fragment, joined by `" and "`,
    /// wrapped as `" Discarded <parts>."`. Empty string when nothing to
    /// discard. The commits fragment here omits the branch (matches TS, which
    /// uses the bare `${commits} commit(s)` form in the discard note).
    #[must_use]
    pub fn discard_note(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.commits > 0 {
            let noun = if self.commits == 1 {
                "commit"
            } else {
                "commits"
            };
            parts.push(format!("{} {noun}", self.commits));
        }
        if let Some(files) = self.changed_files_phrase() {
            parts.push(files);
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!(" Discarded {}.", parts.join(" and "))
        }
    }
}

/// Post-run keep/cleanup judgment for an agent's isolation worktree — the
/// single source of truth for claude-code's `getWorktreeResult` closure: once
/// the agent reached a terminal state, KEEP the worktree (returning its
/// `(path, branch)`) if it left changes, else REMOVE it (auto-clean).
///
/// [`WorktreeManager::worktree_change_summary`]'s
/// [`WorktreeChangeSummary::is_dirty`] is claude's full keep test — `dirty ||
/// commitsAhead > 0` — where `dirty` is `git status --porcelain` non-empty and
/// `commitsAhead` is `git rev-list --count <originalHeadCommit>..HEAD` (the
/// handle's `base_commit`, captured at creation). So a clean working tree
/// carrying commits ahead of base is correctly KEPT, not discarded. Runs for
/// ANY terminal outcome so a worktree never leaks on a failed/killed agent;
/// removal is best-effort (idempotent per the
/// [`WorktreeManager::remove_worktree`] contract).
///
/// A git error while inspecting the worktree — `git status`/`rev-list` exiting
/// non-zero (lock contention, corrupt index), or a manager `Err` — is treated
/// as KEEP, not remove. Claude's `ZVt` reports such a failure as
/// `{ dirty: true, gitError: true }` (2.1.208 binary) so its consumer keeps the
/// worktree rather than delete work it could not verify; the port's
/// [`WorktreeManager::worktree_change_summary`] collapses every such
/// "state could not be reliably determined" case to `Ok(None)`, and its own
/// contract mandates callers treat `None` as *unknown, assume unsafe*
/// (fail-closed). Only a positively-clean `Ok(Some(clean))` summary is removed.
///
/// Shared by the SYNC AgentTool finalizer and the ASYNC (`run_in_background`)
/// lifecycle owner — claude 2.1.207 hands the same `getWorktreeResult` closure
/// to the detached background task, so both paths must judge identically.
pub async fn agent_worktree_result(
    manager: &dyn WorktreeManager,
    handle: &WorktreeHandle,
) -> Option<(String, String)> {
    let keep = match manager.worktree_change_summary(handle).await {
        Ok(Some(summary)) => summary.is_dirty(),
        // Unknown state (git-status/rev-list non-zero exit, no baseline) or a
        // manager error ⇒ fail-closed KEEP, mirroring claude `ZVt`'s
        // `gitError ⇒ dirty:true` branch.
        Ok(None) | Err(_) => true,
    };
    if keep {
        Some((
            handle.path.to_string_lossy().into_owned(),
            handle.branch_name.clone(),
        ))
    } else {
        let _ = manager.remove_worktree(handle).await;
        None
    }
}

/// Stable handle to a worktree created by [`WorktreeManager::create_worktree`].
///
/// The handle is serializable so it can be embedded in snapshots and survive
/// process restarts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeHandle {
    /// Absolute filesystem path to the worktree root.
    pub path: PathBuf,
    /// Git branch name checked out inside the worktree.
    pub branch_name: String,
    /// The commit `git worktree add` checked out at creation — claude-code's
    /// `originalHeadCommit` (`ExitWorktreeTool.ts`). Used by
    /// [`WorktreeManager::worktree_change_summary`] to count ahead-commits as
    /// `git rev-list --count <base>..HEAD`; without it a clean-but-committed
    /// worktree would report `commits: 0` and be auto-removed (data loss).
    /// `None` when the baseline could not be captured — the count then falls to
    /// `0`, exactly claude's `if (!headCommit) commitsAhead = 0`. Additive
    /// (`#[serde(default)]`) + `skip_serializing_if` so a `None` serializes
    /// byte-identically to the pre-`base_commit` shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
}

/// Metadata for a worktree returned by [`WorktreeManager::list_worktrees`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeInfo {
    /// Absolute filesystem path to the worktree root.
    pub path: PathBuf,
    /// Git branch name checked out inside the worktree.
    pub branch: String,
    /// Timestamp when the worktree was created.
    pub created_at: std::time::SystemTime,
}

/// Failure modes for [`WorktreeManager`] calls.
#[derive(Debug, Clone, Error)]
pub enum WorktreeError {
    /// Worktrees are not available on this platform (e.g. no git binary).
    #[error("worktree not supported on this platform")]
    Unsupported,
    /// Caller-supplied slug failed validation (bad char, too long, empty
    /// segment, ...). Validation rules are platform-agnostic — see
    /// `validate_worktree_slug` in the platform crates.
    #[error("invalid slug: {0}")]
    InvalidSlug(String),
    /// Underlying git invocation failed with the embedded message.
    #[error("git error: {0}")]
    Git(String),
    /// Filesystem error (permissions, disk full, ...).
    #[error("io error: {0}")]
    Io(String),
    /// Pre-flight worktree-creation guard refused the operation because a
    /// repository-committed symlink at the managed dot-dir (`<dot>`,
    /// `<dot>/worktrees`, or the target `<dot>/worktrees/<name>`) could redirect
    /// creation outside the repository — or that dot-dir entry could not be
    /// `lstat`ed. Byte-faithful port of claude-code 2.1.212's `yWi` guard, whose
    /// message is carried verbatim (Display is the bare `{0}`, matching CC's
    /// direct `ev(...)` surfacing with no extra prefix).
    #[error("{0}")]
    SymlinkRejected(String),
}

#[cfg(test)]
mod m2_01_tests {
    use super::*;

    #[test]
    fn invalid_slug_carries_message() {
        let e = WorktreeError::InvalidSlug("contains '*'".into());
        let s = format!("{e}");
        assert!(s.contains("invalid slug"));
        assert!(s.contains("contains '*'"));
    }

    #[test]
    fn change_summary_is_dirty_reflects_either_count() {
        assert!(!WorktreeChangeSummary {
            changed_files: 0,
            commits: 0
        }
        .is_dirty());
        assert!(WorktreeChangeSummary {
            changed_files: 1,
            commits: 0
        }
        .is_dirty());
        assert!(WorktreeChangeSummary {
            changed_files: 0,
            commits: 1
        }
        .is_dirty());
        assert!(WorktreeChangeSummary {
            changed_files: 3,
            commits: 2
        }
        .is_dirty());
    }

    #[test]
    fn changed_files_phrase_singular_and_plural() {
        assert_eq!(
            WorktreeChangeSummary {
                changed_files: 0,
                commits: 0
            }
            .changed_files_phrase(),
            None
        );
        assert_eq!(
            WorktreeChangeSummary {
                changed_files: 1,
                commits: 0
            }
            .changed_files_phrase(),
            Some("1 uncommitted file".to_string())
        );
        assert_eq!(
            WorktreeChangeSummary {
                changed_files: 5,
                commits: 0
            }
            .changed_files_phrase(),
            Some("5 uncommitted files".to_string())
        );
    }

    #[test]
    fn commits_phrase_singular_and_plural_with_branch() {
        assert_eq!(
            WorktreeChangeSummary {
                changed_files: 0,
                commits: 0
            }
            .commits_phrase("worktree-feat"),
            None
        );
        assert_eq!(
            WorktreeChangeSummary {
                changed_files: 0,
                commits: 1
            }
            .commits_phrase("worktree-feat"),
            Some("1 commit on worktree-feat".to_string())
        );
        assert_eq!(
            WorktreeChangeSummary {
                changed_files: 0,
                commits: 4
            }
            .commits_phrase("worktree-feat"),
            Some("4 commits on worktree-feat".to_string())
        );
    }

    /// Minimal in-crate manager for `agent_worktree_result`: scripted change
    /// summary + a removal recorder (the tool-api `MockWorktreeManager` lives
    /// downstream and cannot be used here without a dep cycle). `summary` is
    /// the full `worktree_change_summary` result so tests can script the
    /// unknown (`Ok(None)`) and git-error (`Err`) branches, not just a value.
    struct JudgmentMock {
        summary: Result<Option<WorktreeChangeSummary>, WorktreeError>,
        removed: std::sync::Mutex<Vec<WorktreeHandle>>,
    }
    #[async_trait]
    impl WorktreeManager for JudgmentMock {
        async fn create_worktree(
            &self,
            _slug: &str,
            _base_branch: Option<&str>,
            _copy_includes: &[PathBuf],
        ) -> Result<WorktreeHandle, WorktreeError> {
            Err(WorktreeError::Unsupported)
        }
        async fn remove_worktree(&self, handle: &WorktreeHandle) -> Result<(), WorktreeError> {
            self.removed.lock().unwrap().push(handle.clone());
            Ok(())
        }
        async fn list_worktrees(&self) -> Result<Vec<WorktreeInfo>, WorktreeError> {
            Ok(Vec::new())
        }
        async fn cleanup_stale(&self, _max_age: Duration) -> Result<Vec<PathBuf>, WorktreeError> {
            Ok(Vec::new())
        }
        fn is_supported(&self) -> bool {
            true
        }
        async fn worktree_change_summary(
            &self,
            _handle: &WorktreeHandle,
        ) -> Result<Option<WorktreeChangeSummary>, WorktreeError> {
            self.summary.clone()
        }
    }

    fn judgment_handle() -> WorktreeHandle {
        WorktreeHandle {
            path: PathBuf::from("/repo/.lingxi/worktrees/agent-1"),
            branch_name: "worktree-agent-1".into(),
            base_commit: Some("abc123".into()),
        }
    }

    /// Dirty (uncommitted files OR commits ahead) ⇒ KEEP: `(path, branch)`
    /// returned, no removal (claude `dirty || commitsAhead > 0`).
    #[tokio::test]
    async fn agent_worktree_result_keeps_dirty_worktree() {
        for summary in [
            WorktreeChangeSummary {
                changed_files: 1,
                commits: 0,
            },
            WorktreeChangeSummary {
                changed_files: 0,
                commits: 2,
            },
        ] {
            let mock = JudgmentMock {
                summary: Ok(Some(summary)),
                removed: std::sync::Mutex::new(Vec::new()),
            };
            let kept = agent_worktree_result(&mock, &judgment_handle()).await;
            assert_eq!(
                kept,
                Some((
                    "/repo/.lingxi/worktrees/agent-1".to_string(),
                    "worktree-agent-1".to_string()
                )),
                "a worktree carrying work is KEPT"
            );
            assert!(
                mock.removed.lock().unwrap().is_empty(),
                "kept worktree must not be removed"
            );
        }
    }

    /// A positively-clean summary ⇒ REMOVE + `None` — the auto-clean branch
    /// (claude `ZVt` returns `{dirty:false, commitsAhead:0}` and the consumer
    /// removes).
    #[tokio::test]
    async fn agent_worktree_result_removes_clean_worktree() {
        let mock = JudgmentMock {
            summary: Ok(Some(WorktreeChangeSummary {
                changed_files: 0,
                commits: 0,
            })),
            removed: std::sync::Mutex::new(Vec::new()),
        };
        let kept = agent_worktree_result(&mock, &judgment_handle()).await;
        assert_eq!(kept, None, "clean worktree is auto-cleaned");
        assert_eq!(
            mock.removed.lock().unwrap().len(),
            1,
            "clean worktree removed exactly once"
        );
    }

    /// Regression (review RV13): an UNKNOWN state — a git error while
    /// inspecting the worktree (`git status`/`rev-list` non-zero exit, lock
    /// contention) surfaces as `Ok(None)`, and a manager `Err` — must fail
    /// closed to KEEP, matching claude `ZVt`'s `{dirty:true, gitError:true}`
    /// branch (2.1.208 binary). Previously both collapsed to `dirty=false`
    /// and the checkout was removed, discarding a worktree claude keeps.
    #[tokio::test]
    async fn agent_worktree_result_keeps_when_state_unknown() {
        for summary in [Ok(None), Err(WorktreeError::Git("index.lock".into()))] {
            let mock = JudgmentMock {
                summary,
                removed: std::sync::Mutex::new(Vec::new()),
            };
            let kept = agent_worktree_result(&mock, &judgment_handle()).await;
            assert_eq!(
                kept,
                Some((
                    "/repo/.lingxi/worktrees/agent-1".to_string(),
                    "worktree-agent-1".to_string()
                )),
                "unknown/errored state fails closed to KEEP"
            );
            assert!(
                mock.removed.lock().unwrap().is_empty(),
                "a worktree whose state could not be verified is never removed"
            );
        }
    }

    #[test]
    fn discard_note_byte_faithful_to_ts() {
        // Nothing to discard → empty string (no leading space).
        assert_eq!(
            WorktreeChangeSummary {
                changed_files: 0,
                commits: 0
            }
            .discard_note(),
            ""
        );
        // Only uncommitted files.
        assert_eq!(
            WorktreeChangeSummary {
                changed_files: 2,
                commits: 0
            }
            .discard_note(),
            " Discarded 2 uncommitted files."
        );
        // Only commits (no branch in the discard note, matching TS).
        assert_eq!(
            WorktreeChangeSummary {
                changed_files: 0,
                commits: 1
            }
            .discard_note(),
            " Discarded 1 commit."
        );
        // Both — commits first, then files, joined by " and " (TS order).
        assert_eq!(
            WorktreeChangeSummary {
                changed_files: 3,
                commits: 2
            }
            .discard_note(),
            " Discarded 2 commits and 3 uncommitted files."
        );
        // Singular both.
        assert_eq!(
            WorktreeChangeSummary {
                changed_files: 1,
                commits: 1
            }
            .discard_note(),
            " Discarded 1 commit and 1 uncommitted file."
        );
    }
}
