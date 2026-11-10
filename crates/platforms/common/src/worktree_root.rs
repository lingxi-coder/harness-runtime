//! Isolation root validation shared by hook-managed POSIX and Windows worktrees.

use lingxi_core::host::worktree::WorktreeError;
use std::path::Path;
use tokio::process::Command;

/// A hook may supply a non-git checkout, but a successful git discovery must
/// resolve to that folder itself. Otherwise every command would edit a parent
/// checkout or a core.worktree redirect instead of the promised isolation.
pub async fn validate_hook_worktree(path: &Path) -> Result<(), WorktreeError> {
    let canonical = tokio::fs::canonicalize(path)
        .await
        .map_err(|error| WorktreeError::Io(error.to_string()))?;
    if !tokio::fs::metadata(&canonical)
        .await
        .map_err(|error| WorktreeError::Io(error.to_string()))?
        .is_dir()
    {
        return Err(WorktreeError::Hook(format!(
            "Cannot use the WorktreeCreate hook's worktree: {} is not a directory",
            path.display()
        )));
    }
    let probe = match Command::new("git")
        .arg("-C")
        .arg(path)
        .arg("rev-parse")
        .arg("--show-toplevel")
        .output()
        .await
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(WorktreeError::Io(error.to_string())),
    };
    if !probe.status.success() {
        // Other VCS systems are precisely why WorktreeCreate exists.
        return Ok(());
    }
    let root = String::from_utf8_lossy(&probe.stdout).trim().to_owned();
    let root_path = tokio::fs::canonicalize(&root)
        .await
        .map_err(|error| WorktreeError::Io(error.to_string()))?;
    #[cfg(windows)]
    let same_root =
        canonical.to_string_lossy().to_lowercase() == root_path.to_string_lossy().to_lowercase();
    #[cfg(not(windows))]
    let same_root = canonical == root_path;
    if !same_root {
        return Err(WorktreeError::Hook(format!(
            "Refusing to use {} as an isolation worktree: git resolves its working tree to {} (a core.worktree redirect, or a checkout discovered above it), so commands run there would write outside the worktree. Remove the redirect, restore the worktree's own .git, or recreate the worktree, then retry.", path.display(), root_path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(cwd: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[tokio::test]
    async fn native_322_hook_root_rejects_parent_checkout_discovery() {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-q"]);
        let child = root.path().join("selected-by-hook");
        std::fs::create_dir(&child).unwrap();
        let error = validate_hook_worktree(&child)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.starts_with(&format!(
            "Refusing to use {} as an isolation worktree: git resolves its working tree to",
            child.display()
        )));
        assert!(error.contains("commands run there would write outside the worktree"));
        validate_hook_worktree(root.path()).await.unwrap();
    }

    #[tokio::test]
    async fn native_322_hook_root_accepts_other_vcs_and_rejects_worktree_redirect() {
        let root = tempfile::tempdir().unwrap();
        let other_vcs = root.path().join("other-vcs");
        let git_repo = root.path().join("git-repo");
        std::fs::create_dir(&other_vcs).unwrap();
        std::fs::create_dir(&git_repo).unwrap();
        validate_hook_worktree(&other_vcs).await.unwrap();
        git(&git_repo, &["init", "-q"]);
        git(
            &git_repo,
            &["config", "core.worktree", other_vcs.to_str().unwrap()],
        );
        assert!(validate_hook_worktree(&git_repo)
            .await
            .unwrap_err()
            .to_string()
            .contains("core.worktree redirect"));
    }
}
