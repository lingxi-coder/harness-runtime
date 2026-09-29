//! Anchor a command's sandbox at the directory the command belongs to.
//!
//! Filesystem entries in a [`SandboxRuntimeConfig`] may be relative — above
//! all the session seed `.`. Left relative, a backend resolves them against
//! whatever directory it happens to see: the host process's `current_dir()`
//! (the macOS profile builders) or the spawn directory (the legacy Linux
//! `bwrap` wrap). Neither is the command's workspace in general: one host
//! process can serve sessions in many workspaces, a shell `cd` moves the spawn
//! directory, and an agent isolated in a git worktree must not be able to
//! write the main workspace. [`rooted_at`] resolves every relative entry
//! against an explicit root instead, so the backends only ever see absolute
//! paths.

use crate::runtime_config::SandboxRuntimeConfig;
use std::path::{Path, PathBuf};

/// Whose directory a command's sandbox is rooted at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxRootScope {
    /// The session's own workspace (the main loop, prompt shells). When that
    /// workspace is a linked git worktree the main repository stays writable
    /// so git keeps working there (upstream `worktreeMainRepoPath`), with its
    /// hooks and config denied.
    Session,
    /// An agent confined to its own directory (`isolation: "worktree"` or an
    /// explicit `cwd`). Nothing outside the root is added: the main
    /// repository, including its `.git`, stays read-only, so the agent cannot
    /// commit from inside the sandbox; its changes are collected by the host.
    Agent,
}

/// `cfg` with every relative filesystem entry resolved against `root`, plus
/// the root's own settings and skills denied for writes (a sandbox-escape
/// vector wherever the command runs, sandbox-adapter.ts:238-255).
///
/// `root` must be absolute; a relative root would re-introduce exactly the
/// ambiguity this removes, so it is returned unchanged.
#[must_use]
pub fn rooted_at(
    cfg: &SandboxRuntimeConfig,
    root: &Path,
    scope: SandboxRootScope,
) -> SandboxRuntimeConfig {
    let mut cfg = cfg.clone();
    if !root.is_absolute() {
        return cfg;
    }
    let root_str = root.to_string_lossy();
    let fs = &mut cfg.filesystem;
    for list in [
        &mut fs.allow_write,
        &mut fs.deny_write,
        &mut fs.deny_read,
        &mut fs.allow_read,
    ] {
        for entry in list.iter_mut() {
            if let Some(resolved) = resolve_relative(entry, &root_str) {
                *entry = resolved;
            }
        }
    }
    let dot_dir = root.join(branding::DOT_DIR);
    for denied in [
        dot_dir.join("settings.json"),
        dot_dir.join("settings.local.json"),
        dot_dir.join("skills"),
    ] {
        push_unique(&mut fs.deny_write, &denied);
    }
    if scope == SandboxRootScope::Session {
        if let Some(main_repo) = linked_worktree_main_repo(root) {
            push_unique(&mut fs.allow_write, &main_repo);
            push_unique(&mut fs.deny_write, &main_repo.join(".git").join("hooks"));
            if !fs.allow_git_config {
                push_unique(&mut fs.deny_write, &main_repo.join(".git").join("config"));
            }
        }
    }
    cfg
}

/// `entry` resolved against `root` with the backends' own rules (`.`, `./x`,
/// `../x` and bare relative paths or globs), or `None` when it is already
/// absolute or home-relative.
fn resolve_relative(entry: &str, root: &str) -> Option<String> {
    if entry.is_empty() || entry.starts_with('/') || entry == "~" || entry.starts_with("~/") {
        return None;
    }
    // Lexical only: symlinks are resolved later by the backend, exactly as it
    // would have for a path it anchored itself.
    Some(sandbox_runtime::path_utils::normalize_path_for_sandbox_with(entry, "", root, |_| None))
}

fn push_unique(list: &mut Vec<String>, path: &Path) {
    let path = path.to_string_lossy().into_owned();
    if !list.contains(&path) {
        list.push(path);
    }
}

/// The main repository of the linked worktree at `root`: `root/.git` is a file
/// naming the worktree's private git dir, whose `commondir` points at the main
/// repository's `.git`. `None` for a normal checkout or anything unreadable.
fn linked_worktree_main_repo(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    if !dot_git.is_file() {
        return None;
    }
    let pointer = std::fs::read_to_string(&dot_git).ok()?;
    let git_dir = pointer.trim().strip_prefix("gitdir:")?.trim();
    let git_dir = root.join(git_dir);
    let common = std::fs::read_to_string(git_dir.join("commondir")).ok()?;
    let common = std::fs::canonicalize(git_dir.join(common.trim())).ok()?;
    if common.file_name()? != ".git" {
        return None;
    }
    common.parent().map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(allow_write: &[&str], deny_write: &[&str]) -> SandboxRuntimeConfig {
        let mut cfg = SandboxRuntimeConfig::default();
        cfg.filesystem.allow_write = allow_write.iter().map(|s| (*s).to_string()).collect();
        cfg.filesystem.deny_write = deny_write.iter().map(|s| (*s).to_string()).collect();
        cfg
    }

    #[test]
    fn relative_entries_resolve_against_the_root_not_the_process_cwd() {
        let base = cfg(
            &[
                ".",
                "./build",
                "../sibling",
                "/tmp/lingxi",
                "~/cache",
                "target/**",
            ],
            &["**/.env"],
        );
        let rooted = rooted_at(&base, Path::new("/repo/wt"), SandboxRootScope::Agent);
        assert_eq!(
            &rooted.filesystem.allow_write,
            &[
                "/repo/wt",
                "/repo/wt/build",
                "/repo/sibling",
                "/tmp/lingxi",
                "~/cache",
                "/repo/wt/target/**",
            ]
        );
        assert_eq!(rooted.filesystem.deny_write[0], "/repo/wt/**/.env");
    }

    #[test]
    fn the_roots_own_settings_and_skills_are_denied_once() {
        let rooted = rooted_at(
            &cfg(&["."], &[]),
            Path::new("/repo/wt"),
            SandboxRootScope::Agent,
        );
        let dot = format!("/repo/wt/{}", branding::DOT_DIR);
        for denied in ["settings.json", "settings.local.json", "skills"] {
            let path = format!("{dot}/{denied}");
            assert_eq!(
                rooted
                    .filesystem
                    .deny_write
                    .iter()
                    .filter(|p| **p == path)
                    .count(),
                1,
                "{path}"
            );
        }
        let again = rooted_at(&rooted, Path::new("/repo/wt"), SandboxRootScope::Agent);
        assert_eq!(again.filesystem.deny_write, rooted.filesystem.deny_write);
    }

    #[test]
    fn a_relative_root_changes_nothing() {
        let base = cfg(&["."], &[]);
        let rooted = rooted_at(&base, Path::new("wt"), SandboxRootScope::Agent);
        assert_eq!(rooted.filesystem.allow_write, base.filesystem.allow_write);
        assert_eq!(rooted.filesystem.deny_write, base.filesystem.deny_write);
    }

    /// `main/` is a repository and `main/wt` a linked worktree of it, laid out
    /// the way `git worktree add` leaves them.
    fn linked_worktree() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let main = std::fs::canonicalize(dir.path()).unwrap().join("main");
        let private = main.join(".git/worktrees/wt");
        std::fs::create_dir_all(&private).unwrap();
        std::fs::write(private.join("commondir"), "../..\n").unwrap();
        let wt = main.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", private.display())).unwrap();
        (dir, main, wt)
    }

    #[test]
    fn a_session_in_a_linked_worktree_keeps_the_main_repo_writable_but_not_its_hooks() {
        let (_dir, main, wt) = linked_worktree();
        let rooted = rooted_at(&cfg(&["."], &[]), &wt, SandboxRootScope::Session);
        let fs = &rooted.filesystem;
        assert!(fs
            .allow_write
            .contains(&main.to_string_lossy().into_owned()));
        assert!(fs
            .deny_write
            .contains(&main.join(".git/hooks").to_string_lossy().into_owned()));
        assert!(fs
            .deny_write
            .contains(&main.join(".git/config").to_string_lossy().into_owned()));
    }

    #[test]
    fn an_agent_in_a_linked_worktree_gets_nothing_outside_it() {
        let (_dir, main, wt) = linked_worktree();
        let rooted = rooted_at(&cfg(&["."], &[]), &wt, SandboxRootScope::Agent);
        assert_eq!(
            rooted.filesystem.allow_write,
            vec![wt.to_string_lossy().into_owned()]
        );
        let main = main.to_string_lossy().into_owned();
        assert!(!rooted.filesystem.allow_write.contains(&main));
    }

    #[test]
    fn a_normal_checkout_is_not_a_linked_worktree() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        assert_eq!(linked_worktree_main_repo(dir.path()), None);
        assert_eq!(linked_worktree_main_repo(&dir.path().join("missing")), None);
    }
}
