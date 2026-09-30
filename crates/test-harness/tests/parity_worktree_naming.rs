//! Parity fixture: worktree branch and path naming.
//!
//! Locks the on-disk layout produced by `PosixWorktreeManager` against
//! claude-code's `src/utils/worktree.ts`:
//!
//! - Branch prefix is the literal `worktree-` (NOT `lingxi/`, NOT `claude/`).
//! - Worktrees live at `<repo_root>/.lingxi/worktrees/<flatten_slug(slug)>`.
//! - `flatten_slug` replaces every `/` with `+` so the layout stays flat;
//!   `+` is outside the allowed slug character set so the mapping is
//!   injective.
//! - Slug validation: each `/`-separated segment matches
//!   `[a-zA-Z0-9._-]+`, total length 1..=64.

use lingxi_core::host::worktree::{WorktreeError, WorktreeManager};
use serde::Deserialize;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;
use test_harness::parity::load_fixture;

#[derive(Deserialize)]
struct Case {
    slug: String,
    expected_branch: String,
    expected_path_suffix: String,
}

#[derive(Deserialize)]
struct Fixture {
    cases: Vec<Case>,
    invalid_slugs: Vec<String>,
}

fn init_git_repo(root: &Path) {
    let run = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("git invoke")
    };
    let _ = run(&["init", "-q", "-b", "main"]);
    let _ = run(&["config", "user.email", "parity@example.com"]);
    let _ = run(&["config", "user.name", "Parity"]);
    std::fs::write(root.join("README"), "x").unwrap();
    let _ = run(&["add", "."]);
    let _ = run(&["commit", "-q", "-m", "init"]);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn worktree_branch_naming_matches_claude_code() {
    let fx: Fixture = load_fixture("worktree_branch_naming");

    for case in &fx.cases {
        let tmp = TempDir::new().expect("tempdir");
        init_git_repo(tmp.path());
        // Canonicalize so symlink-prefixed temp dirs (e.g. /var → /private/var
        // on macOS) match the canonicalised path git emits below.
        let root_canon = std::fs::canonicalize(tmp.path()).expect("canonicalize repo root");
        let manager = platform_posix::PosixWorktreeManager::new(root_canon.clone());

        let handle = manager
            .create_worktree(&case.slug, None, &[])
            .await
            .unwrap_or_else(|e| panic!("create_worktree({:?}) must succeed: {e}", case.slug));

        assert_eq!(
            handle.branch_name, case.expected_branch,
            "slug {:?} → branch must equal {:?}, got {:?}",
            case.slug, case.expected_branch, handle.branch_name,
        );

        let suffix = handle
            .path
            .strip_prefix(&root_canon)
            .unwrap_or_else(|e| panic!("path strip_prefix({root_canon:?}) failed: {e}"));
        assert_eq!(
            suffix.to_str().expect("suffix utf8"),
            case.expected_path_suffix,
            "slug {:?} → path suffix must equal {:?}, got {:?}",
            case.slug,
            case.expected_path_suffix,
            suffix,
        );
    }

    for bad in &fx.invalid_slugs {
        let tmp = TempDir::new().expect("tempdir");
        init_git_repo(tmp.path());
        let root_canon = std::fs::canonicalize(tmp.path()).expect("canonicalize repo root");
        let manager = platform_posix::PosixWorktreeManager::new(root_canon);
        let r = manager.create_worktree(bad, None, &[]).await;
        match r {
            Err(WorktreeError::InvalidSlug(_)) => {}
            other => panic!("slug {bad:?} must be rejected as InvalidSlug, got {other:?}",),
        }
    }
}

#[cfg(target_os = "windows")]
#[test]
fn worktree_naming_fixture_loads_on_windows() {
    // The Windows worktree manager exists but `git worktree` semantics
    // differ subtly on Windows; full driver coverage runs only on POSIX
    // hosts. The fixture is still parsed so a syntax error in the JSON
    // would fail this test on every platform.
    let fx: Fixture = load_fixture("worktree_branch_naming");
    assert!(!fx.cases.is_empty(), "fixture must declare cases");
    assert!(
        !fx.invalid_slugs.is_empty(),
        "fixture must declare invalid slugs"
    );
}
