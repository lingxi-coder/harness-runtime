//! Pre-flight symlink guard for the desktop `git worktree`-backed
//! [`WorktreeManager`](lingxi_core::host::WorktreeManager) create path (posix + windows).
//!
//! Behavioral port of claude-code 2.1.212's `yWi(repoRoot, target)`, called
//! immediately before the `git worktree add` spawn. It `lstat`s the managed
//! dot-dir chain — `<repo>/<dot>`, `<repo>/<dot>/worktrees`, and the target
//! `<repo>/<dot>/worktrees/<name>` — and refuses to proceed if any is a symlink,
//! because a repository-committed symlink there could redirect the checkout
//! outside the repository (e.g. into `~/.lingxi/skills`). A non-`ENOENT` lstat
//! error is likewise fatal; a missing path (`ENOENT`) is fine and skipped.
//!
//! CC additionally increments the `tengu_feature_bad`
//! (`feature_name:"git_worktree_create"`) counter with
//! `error_code:"git_worktree_create_symlink_rejected"` /
//! `"git_worktree_create_lstat_failed"`. That whole `git_worktree_create_*`
//! metric family is unported in lingxi (see `lsp/src/registry.rs` for the same
//! documented-but-unwired `tengu_feature_bad` convention), so this guard mirrors
//! only the observable refusal + byte-faithful message, not the telemetry.

use lingxi_core::host::WorktreeError;
use std::path::Path;

/// Reject worktree creation when a committed symlink at the managed dot-dir
/// could redirect the checkout outside the repository (CC 2.1.212 `yWi`).
///
/// `dot_dir` is the repo-relative managed directory name (e.g. `.lingxi`); it is
/// threaded through so the byte-faithful message names the actual on-disk paths.
/// Returns [`WorktreeError::SymlinkRejected`] on a symlink hit or a non-`ENOENT`
/// lstat failure; `Ok(())` when every checked path is a non-symlink or absent.
pub async fn reject_worktree_create_symlinks(
    repo_root: &Path,
    dot_dir: &str,
    target: &Path,
) -> Result<(), WorktreeError> {
    // CC `r=[join(e,".claude"), Xqe(e), t]` — the dot-dir, its `worktrees`
    // subdir, and the concrete target, checked in that order.
    let paths = [
        repo_root.join(dot_dir),
        repo_root.join(dot_dir).join("worktrees"),
        target.to_path_buf(),
    ];
    for p in paths {
        // `lstat` (does NOT follow symlinks) so a symlink is observed as such.
        match tokio::fs::symlink_metadata(&p).await {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(WorktreeError::SymlinkRejected(format!(
                        "Cannot create worktree: {} is a symlink. A repository-committed symlink at {dot_dir}, {dot_dir}/worktrees, or {dot_dir}/worktrees/<name> could redirect worktree creation outside the repository. Remove the symlink and retry.",
                        p.display()
                    )));
                }
            }
            // ENOENT: the path does not exist yet — nothing to guard, continue.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            // Any other lstat error is fatal (CC `git_worktree_create_lstat_failed`).
            Err(e) => {
                return Err(WorktreeError::SymlinkRejected(format!(
                    "Cannot create worktree: failed to lstat {}: {e}",
                    p.display()
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn ok_when_dot_dir_absent() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join(".lingxi/worktrees/feat");
        // Nothing exists yet — the guard must pass.
        reject_worktree_create_symlinks(tmp.path(), ".lingxi", &target)
            .await
            .expect("absent dot-dir chain is not a symlink");
    }

    #[tokio::test]
    async fn ok_when_real_dirs() {
        let tmp = TempDir::new().unwrap();
        tokio::fs::create_dir_all(tmp.path().join(".lingxi/worktrees"))
            .await
            .unwrap();
        let target = tmp.path().join(".lingxi/worktrees/feat");
        reject_worktree_create_symlinks(tmp.path(), ".lingxi", &target)
            .await
            .expect("real directories are not symlinks");
    }

    #[tokio::test]
    async fn rejects_symlinked_worktrees_dir() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        tokio::fs::create_dir_all(tmp.path().join(".lingxi"))
            .await
            .unwrap();
        // `.lingxi/worktrees` is a committed symlink pointing outside the repo.
        symlink_dir(outside.path(), &tmp.path().join(".lingxi/worktrees")).await;
        let target = tmp.path().join(".lingxi/worktrees/feat");
        let err = reject_worktree_create_symlinks(tmp.path(), ".lingxi", &target)
            .await
            .unwrap_err();
        match err {
            WorktreeError::SymlinkRejected(msg) => {
                assert!(
                    msg.contains("is a symlink")
                        && msg.contains(
                            "could redirect worktree creation outside the repository. Remove the symlink and retry."
                        ),
                    "byte-faithful message: {msg}"
                );
                assert!(msg.starts_with("Cannot create worktree: "));
            }
            other => panic!("expected SymlinkRejected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_symlinked_dot_dir() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        // The dot-dir itself is a symlink.
        symlink_dir(outside.path(), &tmp.path().join(".lingxi")).await;
        let target = tmp.path().join(".lingxi/worktrees/feat");
        let err = reject_worktree_create_symlinks(tmp.path(), ".lingxi", &target)
            .await
            .unwrap_err();
        assert!(matches!(err, WorktreeError::SymlinkRejected(_)));
    }

    #[tokio::test]
    async fn rejects_symlinked_target() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        tokio::fs::create_dir_all(tmp.path().join(".lingxi/worktrees"))
            .await
            .unwrap();
        // A stale symlink left at the target path.
        let target = tmp.path().join(".lingxi/worktrees/feat");
        symlink_dir(outside.path(), &target).await;
        let err = reject_worktree_create_symlinks(tmp.path(), ".lingxi", &target)
            .await
            .unwrap_err();
        assert!(matches!(err, WorktreeError::SymlinkRejected(_)));
    }

    #[cfg(unix)]
    async fn symlink_dir(src: &Path, dst: &Path) {
        tokio::fs::symlink(src, dst).await.unwrap();
    }

    #[cfg(windows)]
    async fn symlink_dir(src: &Path, dst: &Path) {
        tokio::fs::symlink_dir(src, dst).await.unwrap();
    }
}
