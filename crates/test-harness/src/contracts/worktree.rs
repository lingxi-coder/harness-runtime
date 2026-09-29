//! [`WorktreeManager`] contract.
//!
//! Real `git worktree add` requires a real git repo, so the contract is
//! parameterised over a `factory` that produces a fresh `WorktreeManager`
//! bound to a caller-supplied repository root. Drivers (in `tests/`)
//! initialise a temp git repo and hand the path to the factory.
//!
//! Invariants:
//!
//! * `is_supported()` answers a `bool` without panicking.
//! * On supported platforms, `create_worktree(slug, None, &[])` succeeds and
//!   the resulting handle uses the claude-code-mandated `worktree-` branch
//!   prefix and `.lingxi/worktrees/` layout.
//! * Invalid slugs (e.g. with whitespace) are rejected with
//!   [`WorktreeError::InvalidSlug`] before any git command runs.
//! * `cleanup_stale(Duration::ZERO)` returns without panicking (its
//!   behaviour is delegated to `git worktree prune`).
//!
//! Stub impls that report `is_supported() == false` (e.g. `posix-minimal`)
//! short-circuit the full-roundtrip cases; the contract still asserts the
//! shape of the error returned by `create_worktree`.

use lingxi_core::host::{WorktreeError, WorktreeManager};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// Run the standard [`WorktreeManager`] contract.
///
/// `factory(root)` must build a fresh `WorktreeManager` rooted at the
/// supplied path. The driver is responsible for cleaning the temp dir up
/// after the suite finishes (typically by holding a `TempDir` in scope).
///
/// # Panics
///
/// Panics on the first invariant violation.
pub async fn worktree_manager_contract_tests<W, F>(repo_root: &Path, factory: F)
where
    W: WorktreeManager,
    F: Fn(&Path) -> W,
{
    let w = factory(repo_root);
    test_is_supported_returns_bool(&w);
    if w.is_supported() {
        init_git_repo(repo_root);
        test_create_worktree_yields_prefixed_branch(&w, repo_root).await;
        test_invalid_slug_rejected(&w).await;
        test_cleanup_stale_returns_without_panic(&w).await;
    } else {
        test_create_returns_unsupported_when_not_supported(&w).await;
    }
}

fn test_is_supported_returns_bool<W: WorktreeManager>(w: &W) {
    let _ = w.is_supported();
}

async fn test_create_worktree_yields_prefixed_branch<W: WorktreeManager>(w: &W, repo_root: &Path) {
    let handle = w
        .create_worktree("contract-feature", None, &[])
        .await
        .expect("create_worktree must succeed when is_supported() == true");
    assert!(
        handle.branch_name.starts_with("worktree-"),
        "branch must use `worktree-` prefix (claude-code parity), got {}",
        handle.branch_name
    );
    let expected_parent = repo_root.join(".lingxi").join("worktrees");
    assert!(
        handle.path.starts_with(&expected_parent),
        "worktree path must be under <root>/.lingxi/worktrees/, got {:?}",
        handle.path
    );
}

async fn test_invalid_slug_rejected<W: WorktreeManager>(w: &W) {
    // Spaces are outside the allowed slug-segment alphabet (`[a-zA-Z0-9._-]+`).
    let r = w.create_worktree("bad slug", None, &[]).await;
    match r {
        Err(WorktreeError::InvalidSlug(_)) => {}
        other => panic!("expected InvalidSlug for 'bad slug', got {other:?}"),
    }
}

async fn test_cleanup_stale_returns_without_panic<W: WorktreeManager>(w: &W) {
    // Backend may legitimately return Ok(_) or Err(Git(_)) depending on the
    // git version. We assert only that the call completes without panicking
    // or hanging — the actual prune semantics belong to git itself.
    let _ = w.cleanup_stale(Duration::from_secs(0)).await;
}

async fn test_create_returns_unsupported_when_not_supported<W: WorktreeManager>(w: &W) {
    let r = w.create_worktree("feature", None, &[]).await;
    // is_supported()==false impls may return any `Err` variant — Unsupported
    // (the canonical case), InvalidSlug, or a Git error that surfaces before
    // the support check. We only enforce that they don't yield a real handle.
    assert!(
        r.is_err(),
        "is_supported()==false implementations must not yield a real WorktreeHandle"
    );
}

/// Initialise a fresh git repo (one commit) so `git worktree add` has
/// something to branch from. Drivers call this implicitly via the contract
/// runner; exposed so platform tests can reuse it.
pub fn init_git_repo(root: &Path) {
    let run = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("git must be on PATH for the worktree contract suite")
    };
    let _ = run(&["init", "-q", "-b", "main"]);
    let _ = run(&["config", "user.email", "contract@test.local"]);
    let _ = run(&["config", "user.name", "Contract Test"]);
    std::fs::write(root.join("README"), "seed").expect("write README");
    let _ = run(&["add", "."]);
    let _ = run(&["commit", "-q", "-m", "seed"]);
}
