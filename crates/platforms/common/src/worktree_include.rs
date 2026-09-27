//! Shared `.worktreeinclude` copy step for the desktop `git worktree`-backed
//! [`WorktreeManager`](platform_api::WorktreeManager) impls (posix + windows).
//!
//! Behavioral port of claude-code 2.1.207 `copyWorktreeIncludeFiles` (binary
//! fn `TZc`, invoked as the last step of the post-create setup `H6i` for BOTH
//! the agent-isolation worktree and the `--worktree` session flow). It reads
//! `<repo_root>/.worktreeinclude`, resolves which git-*ignored* files those
//! patterns select, and copies them into the fresh worktree — the mechanism by
//! which untracked-but-wanted files (e.g. `.env`, `secrets/`) reach a worktree.
//!
//! The Rust `ignore` crate stands in for npm `ignore` (same gitignore
//! semantics; the exact set of "uncompilable" patterns necessarily differs
//! between engines — a log-only concern). Byte-exact strings that ARE
//! observable — the `warn`/`info` log lines and the
//! `tengu_uncompilable_ignore_pattern` `{site:"worktreeinclude"}` telemetry
//! payload — match CC.

use std::path::{Path, PathBuf};
use tokio::process::Command;

/// Run `git <args>` in `cwd`, returning `(exit_code, stdout)`. `None` when the
/// process could not be spawned. `exit_code` is `-1` for a signal-killed child.
async fn run_git(cwd: &Path, args: &[&str]) -> Option<(i32, String)> {
    let out = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .await
        .ok()?;
    Some((
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    ))
}

/// Build the gitignore matcher from the RAW `.worktreeinclude` content (CC
/// `ignore().add(zet(BGn(r),"worktreeinclude"))`): every non-empty line
/// (comments/blank lines are no-ops inside the matcher) is added; a line the
/// engine cannot compile is dropped with a `warn` log + the
/// `tengu_uncompilable_ignore_pattern` telemetry event, exactly as CC's `zet`.
fn build_matcher(repo_root: &Path, raw_content: &str) -> ignore::gitignore::Gitignore {
    let mut builder = ignore::gitignore::GitignoreBuilder::new(repo_root);
    for line in raw_content.split('\n') {
        // CC `BGn`: split on /\r?\n/ then drop empty strings (comments and
        // whitespace lines are kept — the matcher treats them as no-ops).
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        if let Err(e) = builder.add_line(None, line) {
            tracing::warn!(
                pattern = %line,
                error = %e,
                "[worktreeinclude] gitignore-style pattern failed to compile; treating it as matching nothing"
            );
            telemetry::emit_uncompilable_ignore_pattern(
                telemetry::tengu::ignore_pattern::SITE_WORKTREEINCLUDE,
            );
        }
    }
    builder
        .build()
        .unwrap_or_else(|_| ignore::gitignore::Gitignore::empty())
}

/// `s.ignores(rel)` — does the worktreeinclude matcher select `rel`?
/// `matched_path_or_any_parents` is the faithful analogue of npm `ignore`'s
/// `.ignores()` (a file matches when an ANCESTOR dir matches), mirroring the
/// idiom already used by `orchestrator::prompt::conditional_rules`.
fn matcher_ignores(
    matcher: &ignore::gitignore::Gitignore,
    repo_root: &Path,
    rel: &str,
    is_dir: bool,
) -> bool {
    matcher
        .matched_path_or_any_parents(repo_root.join(rel), is_dir)
        .is_ignore()
}

fn literal_prefix(pattern: &str) -> &str {
    let prefix_len = pattern.find(['*', '?', '[']).unwrap_or(pattern.len());
    &pattern[..prefix_len]
}

fn pattern_selects_collapsed_dir(pattern: &str, dir: &str) -> bool {
    let mut normalized = pattern.strip_prefix('/').unwrap_or(pattern);
    let mut had_leading_globstar = false;
    while let Some(rest) = normalized.strip_prefix("**/") {
        normalized = rest;
        had_leading_globstar = true;
    }

    if normalized.starts_with(dir) {
        return true;
    }

    if !had_leading_globstar {
        let literal = literal_prefix(normalized);
        return !literal.is_empty() && dir.starts_with(literal);
    }

    let first_segment = normalized.split('/').next().unwrap_or(normalized);
    let glob_index = first_segment.find(['*', '?', '[']);
    let literal = glob_index.map_or(first_segment, |idx| &first_segment[..idx]);
    let literal = literal.replace('\\', "").to_ascii_lowercase();
    if literal.is_empty() {
        return false;
    }
    dir.trim_end_matches('/')
        .split('/')
        .map(str::to_ascii_lowercase)
        .any(|component| {
            glob_index.map_or_else(|| component == literal, |_| component.starts_with(&literal))
        })
}

/// CC `gVn(dest, worktreeReal)` — would writing to `dest` escape the worktree
/// via a committed symlink? Walk up `dest`'s ancestors, `realpath`-ing each: if
/// any resolves outside `worktree_real` (or a non-ENOENT error / a broken
/// symlink / walking off the filesystem root is hit) the destination is unsafe.
/// Finally, a `dest` that is itself a symlink is unsafe too.
async fn dest_escapes_worktree(dest: &Path, worktree_real: &Path) -> bool {
    let mut cursor = match dest.parent() {
        Some(p) => p.to_path_buf(),
        None => return true,
    };
    loop {
        match tokio::fs::canonicalize(&cursor).await {
            Ok(resolved) => {
                // `n !== t && !n.startsWith(t+sep)` — component-based
                // `starts_with` already treats `n == worktree_real` as inside.
                if !resolved.starts_with(worktree_real) {
                    return true;
                }
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // ENOENT: the cursor does not resolve. If it nonetheless exists
                // (a broken symlink), that is unsafe; otherwise walk up.
                match tokio::fs::symlink_metadata(&cursor).await {
                    Ok(_) => return true,
                    Err(e2) if e2.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => return true,
                }
                match cursor.parent() {
                    Some(parent) if parent != cursor => cursor = parent.to_path_buf(),
                    _ => return true,
                }
            }
            Err(_) => return true,
        }
    }
    match tokio::fs::symlink_metadata(dest).await {
        Ok(m) if m.file_type().is_symlink() => true,
        Ok(_) => false,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => true,
    }
}

/// Read `<repo_root>/.worktreeinclude` and copy the git-ignored files it selects
/// into `worktree_path`. Returns the relative paths that were copied (may be
/// empty). Best-effort and infallible: a missing `.worktreeinclude`, an empty
/// pattern set, or a failed git query is a silent no-op; per-file failures are
/// `warn`-logged 1:1 with CC and skipped.
pub async fn copy_worktree_include_files(repo_root: &Path, worktree_path: &Path) -> Vec<PathBuf> {
    // (1) read <repo_root>/.worktreeinclude; ENOENT (or any read error) → [].
    let raw = match tokio::fs::read_to_string(repo_root.join(".worktreeinclude")).await {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    // (2) trimmed, non-blank, non-comment patterns (CC `n`). Empty set → [].
    let patterns: Vec<String> = raw
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l).trim().to_string())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    if patterns.is_empty() {
        return Vec::new();
    }

    // (3) the untracked+ignored inventory (dirs collapsed via `--directory`).
    let Some((code, stdout)) = run_git(
        repo_root,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
        ],
    )
    .await
    else {
        return Vec::new();
    };
    if code != 0 || stdout.trim().is_empty() {
        return Vec::new();
    }
    let entries: Vec<&str> = stdout
        .trim()
        .split('\n')
        .filter(|l| !l.is_empty())
        .collect();

    // (4) matcher over the RAW content (comments/blanks are no-ops).
    let matcher = build_matcher(repo_root, &raw);

    // (5) plain ignored files the matcher selects (CC `l`).
    let mut to_copy: Vec<String> = entries
        .iter()
        .filter(|p| !p.ends_with('/') && matcher_ignores(&matcher, repo_root, p, false))
        .map(|p| (*p).to_string())
        .collect();

    // (6) relevant collapsed directories (CC `c`): a `--directory` entry whose
    // path is prefixed by a pattern (leading `/` stripped, or the literal prefix
    // before the first glob metachar), or that the matcher itself ignores.
    let relevant_dirs: Vec<&str> = entries
        .iter()
        .filter(|p| p.ends_with('/'))
        .filter(|p| {
            let dir = **p;
            let by_pattern = patterns
                .iter()
                .any(|pattern| pattern_selects_collapsed_dir(pattern, dir));
            by_pattern || matcher_ignores(&matcher, repo_root, dir.trim_end_matches('/'), true)
        })
        .copied()
        .collect();

    // (7) expand relevant dirs to their ignored files (a second, un-collapsed
    // ls-files pass scoped to those dirs) and add the matcher-selected ones.
    if !relevant_dirs.is_empty() {
        let mut args: Vec<&str> = vec![
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--",
        ];
        args.extend(relevant_dirs.iter().copied());
        if let Some((code, stdout)) = run_git(repo_root, &args).await {
            if code == 0 && !stdout.trim().is_empty() {
                for f in stdout.trim().split('\n').filter(|l| !l.is_empty()) {
                    if matcher_ignores(&matcher, repo_root, f, false) {
                        to_copy.push(f.to_string());
                    }
                }
            }
        }
    }

    // (8) realpath the worktree once; a failure aborts the whole copy.
    let worktree_real = match tokio::fs::canonicalize(worktree_path).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                "Skipping .worktreeinclude copy: realpath({}) failed: {e}",
                worktree_path.display()
            );
            return Vec::new();
        }
    };

    // (9) copy each selected file, with CC's symlink + escape guards.
    let mut copied: Vec<PathBuf> = Vec::new();
    for rel in &to_copy {
        let src = repo_root.join(rel);
        let dst = worktree_path.join(rel);
        match tokio::fs::symlink_metadata(&src).await {
            Ok(m) if m.file_type().is_symlink() => {
                tracing::warn!("Skipping symlink in .worktreeinclude: {rel}");
                continue;
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("Failed to copy {rel} to worktree: {e}");
                continue;
            }
        }
        if dest_escapes_worktree(&dst, &worktree_real).await {
            tracing::warn!(
                "Skipping .worktreeinclude entry: destination escapes worktree via committed symlink: {rel}"
            );
            continue;
        }
        if let Some(parent) = dst.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                tracing::warn!("Failed to copy {rel} to worktree: {e}");
                continue;
            }
        }
        if let Err(e) = tokio::fs::copy(&src, &dst).await {
            tracing::warn!("Failed to copy {rel} to worktree: {e}");
            continue;
        }
        copied.push(PathBuf::from(rel));
    }

    if !copied.is_empty() {
        let list = copied
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(", ");
        tracing::info!(
            "Copied {} files from .worktreeinclude: {list}",
            copied.len()
        );
    }
    copied
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn git(dir: &Path, args: &[&str]) {
        let mut c = Command::new("git");
        c.current_dir(dir);
        for a in args {
            c.arg(a);
        }
        assert!(c.output().await.unwrap().status.success(), "git {args:?}");
    }

    /// A git repo whose `.gitignore` ignores `.env` and `secrets/`, with those
    /// untracked-but-ignored paths present on disk.
    async fn init_repo(dir: &Path) {
        git(dir, &["init", "-q", "-b", "main"]).await;
        git(dir, &["config", "user.email", "ci@test"]).await;
        git(dir, &["config", "user.name", "ci"]).await;
        tokio::fs::write(dir.join(".gitignore"), ".env\nsecrets/\nbuild/\n")
            .await
            .unwrap();
        tokio::fs::write(dir.join("seed.txt"), "seed")
            .await
            .unwrap();
        git(dir, &["add", ".gitignore", "seed.txt"]).await;
        git(dir, &["commit", "-qm", "seed"]).await;
    }

    /// A real sibling worktree created with `git worktree add`, so `realpath`
    /// and escape checks operate on a genuine checkout.
    async fn add_worktree(repo: &Path, name: &str) -> PathBuf {
        let wt = repo.join(".lingxi/worktrees").join(name);
        tokio::fs::create_dir_all(wt.parent().unwrap())
            .await
            .unwrap();
        git(
            repo,
            &[
                "worktree",
                "add",
                "-b",
                &format!("worktree-{name}"),
                wt.to_str().unwrap(),
            ],
        )
        .await;
        wt
    }

    #[tokio::test]
    async fn copies_matching_ignored_file() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        init_repo(repo).await;
        tokio::fs::write(repo.join(".env"), "API_KEY=secret")
            .await
            .unwrap();
        tokio::fs::write(repo.join(".worktreeinclude"), ".env\n")
            .await
            .unwrap();
        let wt = add_worktree(repo, "a").await;

        let copied = copy_worktree_include_files(repo, &wt).await;
        assert_eq!(copied, vec![PathBuf::from(".env")]);
        assert_eq!(
            tokio::fs::read_to_string(wt.join(".env")).await.unwrap(),
            "API_KEY=secret"
        );
    }

    #[tokio::test]
    async fn blanks_and_comments_are_ignored() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        init_repo(repo).await;
        tokio::fs::write(repo.join(".env"), "K=V").await.unwrap();
        tokio::fs::write(
            repo.join(".worktreeinclude"),
            "\n# a comment\n   \n.env\n# trailing\n",
        )
        .await
        .unwrap();
        let wt = add_worktree(repo, "b").await;

        let copied = copy_worktree_include_files(repo, &wt).await;
        assert_eq!(copied, vec![PathBuf::from(".env")]);
    }

    #[tokio::test]
    async fn missing_worktreeinclude_is_noop() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        init_repo(repo).await;
        tokio::fs::write(repo.join(".env"), "K=V").await.unwrap();
        let wt = add_worktree(repo, "c").await;

        // No `.worktreeinclude` file at all.
        let copied = copy_worktree_include_files(repo, &wt).await;
        assert!(copied.is_empty());
        assert!(!wt.join(".env").exists());
    }

    #[tokio::test]
    async fn empty_pattern_set_is_noop() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        init_repo(repo).await;
        tokio::fs::write(repo.join(".env"), "K=V").await.unwrap();
        // Only blanks + comments → no effective patterns.
        tokio::fs::write(repo.join(".worktreeinclude"), "\n#only a comment\n  \n")
            .await
            .unwrap();
        let wt = add_worktree(repo, "d").await;

        let copied = copy_worktree_include_files(repo, &wt).await;
        assert!(copied.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinked_source_is_skipped() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        init_repo(repo).await;
        // `.env` is a symlink (to seed.txt); must be skipped, not copied.
        tokio::fs::symlink("seed.txt", repo.join(".env"))
            .await
            .unwrap();
        tokio::fs::write(repo.join(".worktreeinclude"), ".env\n")
            .await
            .unwrap();
        let wt = add_worktree(repo, "e").await;

        let copied = copy_worktree_include_files(repo, &wt).await;
        assert!(copied.is_empty(), "symlinked source must be skipped");
        assert!(!wt.join(".env").exists());
    }

    #[tokio::test]
    async fn trailing_slash_directory_entry_is_expanded() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        init_repo(repo).await;
        // An ignored directory with two files. `--directory` collapses it to
        // `secrets/` in the first ls-files; the second pass must expand it.
        tokio::fs::create_dir(repo.join("secrets")).await.unwrap();
        tokio::fs::write(repo.join("secrets/a.key"), "AAA")
            .await
            .unwrap();
        tokio::fs::write(repo.join("secrets/b.key"), "BBB")
            .await
            .unwrap();
        tokio::fs::write(repo.join(".worktreeinclude"), "secrets/\n")
            .await
            .unwrap();
        let wt = add_worktree(repo, "f").await;

        let mut copied = copy_worktree_include_files(repo, &wt).await;
        copied.sort();
        assert_eq!(
            copied,
            vec![
                PathBuf::from("secrets/a.key"),
                PathBuf::from("secrets/b.key")
            ]
        );
        assert_eq!(
            tokio::fs::read_to_string(wt.join("secrets/a.key"))
                .await
                .unwrap(),
            "AAA"
        );
    }

    #[tokio::test]
    async fn leading_globstar_directory_pattern_expands_collapsed_dir() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        init_repo(repo).await;
        tokio::fs::create_dir_all(repo.join("nested/secrets"))
            .await
            .unwrap();
        tokio::fs::write(repo.join("nested/secrets/a.key"), "AAA")
            .await
            .unwrap();
        tokio::fs::write(repo.join("nested/secrets/b.key"), "BBB")
            .await
            .unwrap();
        tokio::fs::write(repo.join(".worktreeinclude"), "**/secrets/\n")
            .await
            .unwrap();
        let wt = add_worktree(repo, "globstar").await;

        let mut copied = copy_worktree_include_files(repo, &wt).await;
        copied.sort();
        assert_eq!(
            copied,
            vec![
                PathBuf::from("nested/secrets/a.key"),
                PathBuf::from("nested/secrets/b.key")
            ]
        );
        assert_eq!(
            tokio::fs::read_to_string(wt.join("nested/secrets/a.key"))
                .await
                .unwrap(),
            "AAA"
        );
    }

    #[test]
    fn leading_globstar_only_expands_matching_path_components() {
        assert!(pattern_selects_collapsed_dir(
            "**/secrets/*.key",
            "nested/secrets/"
        ));
        assert!(pattern_selects_collapsed_dir(
            "**/sec*/a.key",
            "nested/secrets/"
        ));
        assert!(!pattern_selects_collapsed_dir(
            "**/secrets/*.key",
            "nested/cache/"
        ));
        assert!(!pattern_selects_collapsed_dir(
            "**/target.txt",
            "nested/cache/"
        ));
    }

    #[tokio::test]
    async fn uncompilable_pattern_dropped_others_still_copy() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        init_repo(repo).await;
        tokio::fs::write(repo.join(".env"), "K=V").await.unwrap();
        // A malformed character-class pattern the matcher cannot compile,
        // alongside a valid one that must still copy.
        tokio::fs::write(repo.join(".worktreeinclude"), "a[b\n.env\n")
            .await
            .unwrap();
        let wt = add_worktree(repo, "g").await;

        let copied = copy_worktree_include_files(repo, &wt).await;
        assert_eq!(copied, vec![PathBuf::from(".env")]);
    }

    #[tokio::test]
    async fn non_matching_ignored_file_not_copied() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        init_repo(repo).await;
        tokio::fs::write(repo.join(".env"), "K=V").await.unwrap();
        tokio::fs::create_dir(repo.join("build")).await.unwrap();
        tokio::fs::write(repo.join("build/out.o"), "obj")
            .await
            .unwrap();
        // Only `.env` requested; ignored `build/` must NOT be dragged along.
        tokio::fs::write(repo.join(".worktreeinclude"), ".env\n")
            .await
            .unwrap();
        let wt = add_worktree(repo, "h").await;

        let copied = copy_worktree_include_files(repo, &wt).await;
        assert_eq!(copied, vec![PathBuf::from(".env")]);
        assert!(!wt.join("build/out.o").exists());
    }
}
