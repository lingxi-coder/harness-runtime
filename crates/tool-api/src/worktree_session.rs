//! `WorktreeSession` — the substrate `EnterWorktree` writes and `ExitWorktree`
//! reads/clears (worktree 206 parity plan, Task 8).
//!
//! `EnterWorktreeTool` swaps the shared [`crate::session_cwd::SessionCwd`] cell
//! (Task 2/7) so every FS/Bash tool observes the worktree, but that cell alone
//! carries no memory of WHERE the session came from, or which worktree/branch
//! it just entered — `ExitWorktree`'s 206 `{action, discard_changes}` contract
//! needs both to restore the original cwd and to know what to keep/remove.
//! This module is that missing record: a single, shared, mutable
//! `Option<WorktreeSession>` cell, reachable by both tools through
//! [`crate::builtin_context::BuiltinToolContext::worktree_session`] (mirroring
//! how `session_cwd: Arc<SessionCwd>` is shared).
//!
//! INERT INVARIANT: a fresh cell starts `None`. Until `EnterWorktree` writes a
//! `Some(..)`, the cell stays `None` and `ExitWorktree` takes its no-op path —
//! byte-identical to a session that never touches worktrees at all.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// A record of the single active worktree the session entered via
/// `EnterWorktree`, captured at the moment of a successful create/enter —
/// BEFORE the `session_cwd.swap(..)` that actually moves the session into it.
///
/// `ExitWorktree` reads this to restore [`Self::original_cwd`] and to
/// reconstruct the [`platform_api::worktree::WorktreeHandle`] it needs for
/// `worktree_change_summary`/`remove_worktree`, then clears the cell back to
/// `None` on completion.
#[derive(Debug, Clone)]
pub struct WorktreeSession {
    /// The session's cwd immediately before the `EnterWorktree` call that
    /// created this record — i.e. what `ExitWorktree` restores via
    /// `session_cwd.swap(original_cwd, vec![original_cwd])`. When
    /// `EnterWorktree` is called again while ALREADY inside a worktree (the
    /// `path` mid-session-switch case), this is the PREVIOUS worktree's path,
    /// not the true launch-time cwd — mirrors the 206 prompt's documented
    /// behavior ("the previous worktree is left on disk, untouched, and only
    /// the new one is tracked for exit-time cleanup").
    pub original_cwd: PathBuf,
    /// Absolute path to the worktree the session is currently in.
    pub worktree_path: PathBuf,
    /// Git branch checked out inside the worktree (may be the literal
    /// `"HEAD"` for a detached-HEAD worktree — treated as "no branch" by both
    /// tools' display logic, matching `EnterWorktreeTool`'s `branch_suffix`).
    pub branch_name: String,
    /// The baseline commit `WorktreeHandle::base_commit` captured at
    /// creation/entry, threaded through so `ExitWorktree`'s dirty-state check
    /// can count ahead-commits. `None` when unavailable — the ahead-commit
    /// count then falls to "unknown", matching
    /// `WorktreeManager::worktree_change_summary`'s fail-closed contract.
    pub base_commit: Option<String>,
    /// `true` when `EnterWorktree` ENTERED a pre-existing worktree (`path`
    /// branch), `false` when it CREATED one. 206's `ExitWorktree` refuses a
    /// `remove` on an entered (not owned) worktree (`t.enteredExisting`,
    /// errorCode 4) — this session is not its owner.
    pub entered_existing: bool,
    /// Name of a tmux session attached to this worktree, if any. Populated by
    /// the boot-time `--worktree --tmux` launch path (`engine-desktop`'s
    /// `apply_worktree_launch`), which creates a detached `tmux new-session`
    /// and stores its name here; the interactive `EnterWorktreeTool` always
    /// writes `None` (206's tool never attaches tmux either). When `Some`,
    /// `ExitWorktreeTool` kills the session on `remove` and surfaces its name
    /// for reattach on `keep`.
    pub tmux_session_name: Option<String>,
}

impl WorktreeSession {
    /// Serialize this record to the `worktreeSession` JSON payload persisted in a
    /// `worktree-state` transcript entry (parity 2.1.212's `saveWorktreeState`).
    /// Field names mirror claude-code's `gne` worktree-session object for the
    /// fields LingXi tracks (`originalCwd`, `worktreePath`, `worktreeBranch`,
    /// `originalHeadCommit`, `enteredExisting`, `tmuxSessionName`). LingXi's
    /// simpler `WorktreeSession` has no `worktreeName`/`preEnterOriginalCwd`/
    /// `originalBranch`/`hookBased`, so those keys are omitted rather than
    /// fabricated; the round-trip through [`Self::from_persisted_json`] restores
    /// exactly what is written here.
    #[must_use]
    pub fn to_persisted_json(&self) -> serde_json::Value {
        serde_json::json!({
            "originalCwd": self.original_cwd.to_string_lossy(),
            "worktreePath": self.worktree_path.to_string_lossy(),
            "worktreeBranch": self.branch_name,
            "originalHeadCommit": self.base_commit,
            "enteredExisting": self.entered_existing,
            "tmuxSessionName": self.tmux_session_name,
        })
    }

    /// Reconstruct a [`WorktreeSession`] from a persisted `worktreeSession`
    /// payload (the inner value of a `worktree-state` transcript entry) on
    /// `--continue`/`--resume`. Returns [`None`] when the value is not an object
    /// or is missing the load-bearing `worktreePath`/`originalCwd` — a
    /// half-written record is treated as "no active worktree" so the session
    /// falls back to the no-op path rather than restoring a broken cell.
    /// `worktreeBranch` defaults to `"HEAD"` (the detached-HEAD sentinel both
    /// tools treat as "no branch") when absent.
    #[must_use]
    pub fn from_persisted_json(value: &serde_json::Value) -> Option<Self> {
        let obj = value.as_object()?;
        let worktree_path = obj.get("worktreePath").and_then(|v| v.as_str())?;
        let original_cwd = obj.get("originalCwd").and_then(|v| v.as_str())?;
        Some(Self {
            original_cwd: PathBuf::from(original_cwd),
            worktree_path: PathBuf::from(worktree_path),
            branch_name: obj
                .get("worktreeBranch")
                .and_then(|v| v.as_str())
                .unwrap_or("HEAD")
                .to_string(),
            base_commit: obj
                .get("originalHeadCommit")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            entered_existing: obj
                .get("enteredExisting")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            tmux_session_name: obj
                .get("tmuxSessionName")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        })
    }
}

/// Sink for persisting the session's active-worktree record to the transcript
/// so `--continue`/`--resume` can rehydrate it (parity 2.1.212's
/// `saveWorktreeState`). `EnterWorktree` calls it with `Some(session)` on a
/// successful create/enter; `ExitWorktree` calls it with `None` to write the
/// clear record. The engine supplies a JSONL-backed implementation; when no
/// persister is injected (mobile, offline factory, tests) the tools skip the
/// write — byte-identical to the pre-persist behavior, just without resume
/// restoration.
#[async_trait::async_trait]
pub trait WorktreeStatePersister: Send + Sync {
    /// Append a `worktree-state` transcript entry carrying `session`'s
    /// [`WorktreeSession::to_persisted_json`] payload (or JSON `null` when
    /// `None`). Failures are the implementation's to log/swallow — persistence
    /// is best-effort and never fails the tool call.
    async fn persist_worktree_state(&self, session: Option<&WorktreeSession>);
}

/// Shared, mutable cell holding at most one active [`WorktreeSession`].
/// `None` when no worktree has been entered (or after `ExitWorktree` clears
/// it) — mirrors [`crate::session_cwd::SessionCwd`]'s no-swap-until-called
/// inertness. Held as a plain `Mutex` (not `ArcSwap`): writes are rare
/// (one per `EnterWorktree`/`ExitWorktree` call) and readers need the whole
/// struct, so the extra lock-free-read machinery `SessionCwd` uses for
/// hot-path per-tool-call cwd reads isn't needed here.
pub type WorktreeSessionCell = Arc<Mutex<Option<WorktreeSession>>>;

/// Construct a fresh, empty (`None`) [`WorktreeSessionCell`] — the default
/// every `BuiltinToolContext` construction site wires until an
/// `EnterWorktree` call populates it.
#[must_use]
pub fn new_worktree_session_cell() -> WorktreeSessionCell {
    Arc::new(Mutex::new(None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_cell_is_none() {
        let cell = new_worktree_session_cell();
        assert!(cell.lock().unwrap().is_none());
    }

    #[test]
    fn cell_can_be_populated_and_cleared() {
        let cell = new_worktree_session_cell();
        *cell.lock().unwrap() = Some(WorktreeSession {
            original_cwd: PathBuf::from("/repo"),
            worktree_path: PathBuf::from("/repo/.lingxi/worktrees/feat"),
            branch_name: "worktree-feat".to_string(),
            base_commit: Some("deadbeef".to_string()),
            entered_existing: false,
            tmux_session_name: None,
        });
        assert!(cell.lock().unwrap().is_some());
        *cell.lock().unwrap() = None;
        assert!(cell.lock().unwrap().is_none());
    }

    #[test]
    fn shared_arc_observes_writes_from_either_clone() {
        // Mirrors the SessionCwd sharing pattern: two tool instances holding
        // clones of the SAME cell must observe each other's writes.
        let cell = new_worktree_session_cell();
        let enter_side = Arc::clone(&cell);
        let exit_side = Arc::clone(&cell);

        *enter_side.lock().unwrap() = Some(WorktreeSession {
            original_cwd: PathBuf::from("/repo"),
            worktree_path: PathBuf::from("/repo/.lingxi/worktrees/feat"),
            branch_name: "worktree-feat".to_string(),
            base_commit: None,
            entered_existing: false,
            tmux_session_name: None,
        });

        let seen = exit_side.lock().unwrap().clone();
        assert_eq!(
            seen.unwrap().worktree_path,
            PathBuf::from("/repo/.lingxi/worktrees/feat")
        );
    }

    #[test]
    fn persisted_json_round_trips() {
        let orig = WorktreeSession {
            original_cwd: PathBuf::from("/repo"),
            worktree_path: PathBuf::from("/repo/.lingxi/worktrees/feat"),
            branch_name: "worktree-feat".to_string(),
            base_commit: Some("deadbeef".to_string()),
            entered_existing: true,
            tmux_session_name: Some("lingxi_feat".to_string()),
        };
        let json = orig.to_persisted_json();
        assert_eq!(json["worktreePath"], "/repo/.lingxi/worktrees/feat");
        assert_eq!(json["worktreeBranch"], "worktree-feat");
        assert_eq!(json["enteredExisting"], true);
        let back = WorktreeSession::from_persisted_json(&json).expect("round-trips");
        assert_eq!(back.original_cwd, orig.original_cwd);
        assert_eq!(back.worktree_path, orig.worktree_path);
        assert_eq!(back.branch_name, orig.branch_name);
        assert_eq!(back.base_commit, orig.base_commit);
        assert_eq!(back.entered_existing, orig.entered_existing);
        assert_eq!(back.tmux_session_name, orig.tmux_session_name);
    }

    #[test]
    fn from_persisted_json_rejects_incomplete() {
        // Missing worktreePath / originalCwd → None (no active worktree).
        assert!(WorktreeSession::from_persisted_json(&serde_json::Value::Null).is_none());
        assert!(
            WorktreeSession::from_persisted_json(&serde_json::json!({"worktreePath": "/x"}))
                .is_none()
        );
        // Minimal valid → branch defaults to the detached-HEAD sentinel.
        let m = WorktreeSession::from_persisted_json(&serde_json::json!({
            "worktreePath": "/x", "originalCwd": "/y"
        }))
        .expect("minimal");
        assert_eq!(m.branch_name, "HEAD");
        assert!(!m.entered_existing);
    }
}
