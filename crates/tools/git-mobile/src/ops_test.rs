//! Tests for `ops.rs`, extracted from inline `#[cfg(test)]` blocks.

use super::*;

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn ssh_url_allowed_only_with_ssh_config() {
        let ssh = crate::auth::SshConfig {
            private_key_path: "/x/id".into(),
            ..Default::default()
        };
        assert!(ssh_allowed("git@github.com:o/r.git", Some(&ssh)).is_ok());
        let err = ssh_allowed("git@github.com:o/r.git", None).unwrap_err();
        assert!(matches!(err, GitOpError::InvalidInput(ref m) if m.to_lowercase().contains("ssh")));
        assert!(
            ssh_allowed("https://github.com/o/r.git", None).is_ok(),
            "https unaffected"
        );
        assert!(
            ssh_allowed("ssh://git@host/o/r.git", Some(&ssh)).is_ok(),
            "ssh:// allowed with config"
        );
    }

    #[test]
    fn transport_allowlist_is_https_only() {
        let ssh = crate::auth::SshConfig {
            private_key_path: "/x/id".into(),
            ..Default::default()
        };
        // https is the only allowed remote scheme in production.
        assert!(transport_allowed("https://github.com/o/r.git", None).is_ok());
        assert!(
            transport_allowed("HTTPS://GitHub.com/o/r.git", None).is_ok(),
            "case-insensitive"
        );
        // http:// leaks the PAT in cleartext; git:// is unauthenticated — both rejected.
        for bad in [
            "http://attacker/x.git",
            "git://attacker/x.git",
            "/data/data/pkg/db",
            "ftp://h/x",
        ] {
            let err = transport_allowed(bad, None).unwrap_err();
            assert!(
                matches!(err, GitOpError::InvalidInput(ref m) if m.to_lowercase().contains("https")),
                "{bad} must be rejected naming HTTPS, got {err:?}"
            );
        }
        // SSH remotes still flow through the SSH credential gate.
        assert!(transport_allowed("git@github.com:o/r.git", Some(&ssh)).is_ok());
        assert!(
            transport_allowed("git@github.com:o/r.git", None).is_err(),
            "ssh needs config"
        );
        // file:// is permitted only under cfg(test) (this suite relies on it).
        assert!(
            transport_allowed("file:///tmp/x", None).is_ok(),
            "file:// allowed in tests"
        );
    }

    /// Build a repo at `dir` with a known two-commit history:
    /// - commit "first": adds file `a.txt` = "alpha\n".
    /// - commit "second": modifies `a.txt` -> "alpha2\n" and adds `b.txt`.
    ///
    /// Returns the repo plus the OIDs of (first, second), newest last.
    fn init_history(dir: &Path) -> (git2::Repository, git2::Oid, git2::Oid) {
        let repo = git2::Repository::init(dir).unwrap();
        let sig = git2::Signature::now("Tester", "tester@example.com").unwrap();

        // commit "first"
        std::fs::write(dir.join("a.txt"), "alpha\n").unwrap();
        let first = {
            let mut index = repo.index().unwrap();
            index.add_path(Path::new("a.txt")).unwrap();
            index.write().unwrap();
            let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "first", &tree, &[])
                .unwrap()
        };

        // commit "second"
        std::fs::write(dir.join("a.txt"), "alpha2\n").unwrap();
        std::fs::write(dir.join("b.txt"), "beta\n").unwrap();
        let second = {
            let mut index = repo.index().unwrap();
            index.add_path(Path::new("a.txt")).unwrap();
            index.add_path(Path::new("b.txt")).unwrap();
            index.write().unwrap();
            let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
            let parent = repo.find_commit(first).unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "second", &tree, &[&parent])
                .unwrap()
        };

        (repo, first, second)
    }

    #[test]
    fn status_reports_dirty_file() {
        let dir = tempdir().unwrap();
        let (repo, _first, _second) = init_history(dir.path());
        // Modify a tracked file (don't stage) + add a brand-new untracked file.
        std::fs::write(dir.path().join("a.txt"), "alpha-dirty\n").unwrap();
        std::fs::write(dir.path().join("untracked.txt"), "x\n").unwrap();

        let entries = status(&repo).unwrap();
        let a = entries
            .iter()
            .find(|e| e.path == "a.txt")
            .expect("a.txt should appear in status");
        assert!(
            a.status.iter().any(|s| s == "wt_modified"),
            "a.txt should be wt_modified, got {:?}",
            a.status
        );
        let u = entries
            .iter()
            .find(|e| e.path == "untracked.txt")
            .expect("untracked.txt should appear in status");
        assert!(
            u.status.iter().any(|s| s == "wt_new"),
            "untracked.txt should be wt_new, got {:?}",
            u.status
        );
    }

    #[test]
    fn log_returns_commits_newest_first() {
        let dir = tempdir().unwrap();
        let (repo, first, second) = init_history(dir.path());

        let commits = log(&repo, None).unwrap();
        assert_eq!(commits.len(), 2, "two commits expected");
        assert_eq!(commits[0].summary, "second");
        assert_eq!(commits[0].oid, second.to_string());
        assert_eq!(commits[1].summary, "first");
        assert_eq!(commits[1].oid, first.to_string());
        assert_eq!(commits[0].author, "Tester <tester@example.com>");

        // cap respected
        let capped = log(&repo, Some(1)).unwrap();
        assert_eq!(capped.len(), 1);
        assert_eq!(capped[0].summary, "second");
    }

    #[test]
    fn diff_shows_workdir_change() {
        let dir = tempdir().unwrap();
        let (repo, _first, _second) = init_history(dir.path());
        // Uncommitted change to a tracked file.
        std::fs::write(dir.path().join("a.txt"), "alpha-WORKDIR\n").unwrap();

        let d = diff(&repo).unwrap();
        assert!(
            d.patch.contains("alpha-WORKDIR"),
            "diff should contain the workdir change, got:\n{}",
            d.patch
        );
        assert!(d.patch.contains("a.txt"), "diff should name the file");
        assert!(!d.truncated, "small diff should not truncate");
    }

    #[test]
    fn diff_detects_untracked_files() {
        // Documents the (intentional) divergence from desktop `git diff`: the
        // diff options include untracked files, so a brand-new file shows up in
        // the diff's delta list as `Untracked` (desktop `git diff` hides it).
        // Note: `show_untracked_content` is NOT set, so the rendered patch BODY
        // for an untracked-only change is empty — the file is detected but its
        // content is not printed as a `+` addition.
        let dir = tempdir().unwrap();
        let (repo, _first, _second) = init_history(dir.path());
        std::fs::write(dir.path().join("fresh.txt"), "brand-new\n").unwrap();

        let head_tree = repo.head().and_then(|h| h.peel_to_tree()).unwrap();
        let mut opts = git2::DiffOptions::new();
        opts.include_untracked(true).recurse_untracked_dirs(true);
        let raw = repo
            .diff_tree_to_workdir_with_index(Some(&head_tree), Some(&mut opts))
            .unwrap();
        let listed = raw.deltas().any(|d| {
            d.status() == git2::Delta::Untracked
                && d.new_file().path() == Some(Path::new("fresh.txt"))
        });
        assert!(
            listed,
            "untracked fresh.txt should appear in the diff's delta list"
        );

        // The public `diff` (patch text) does NOT print untracked content.
        let d = diff(&repo).unwrap();
        assert!(
            !d.patch.contains("brand-new"),
            "untracked content is not rendered without show_untracked_content, got:\n{}",
            d.patch
        );
    }

    #[test]
    fn show_returns_commit_diff() {
        let dir = tempdir().unwrap();
        let (repo, _first, second) = init_history(dir.path());

        let (info, d) = show(&repo, &second.to_string()).unwrap();
        assert_eq!(info.summary, "second");
        assert_eq!(info.oid, second.to_string());
        assert!(
            d.patch.contains("b.txt"),
            "show(second) should include new file b.txt, got:\n{}",
            d.patch
        );
        assert!(
            d.patch.contains("alpha2"),
            "show(second) should include the a.txt change, got:\n{}",
            d.patch
        );
    }

    #[test]
    fn branch_list_includes_default() {
        let dir = tempdir().unwrap();
        let (repo, _first, _second) = init_history(dir.path());

        let branches = branch_list(&repo).unwrap();
        // Default branch name varies (master vs main); assert on is_head.
        let head = branches
            .iter()
            .find(|b| b.is_head)
            .expect("a HEAD branch should exist");
        assert!(!head.is_remote, "default branch is local");
        assert!(!head.name.is_empty());
    }

    #[test]
    fn open_repo_under_workspace_ok() {
        let ws = tempdir().unwrap();
        let repo_dir = ws.path().join("r");
        std::fs::create_dir(&repo_dir).unwrap();
        git2::Repository::init(&repo_dir).unwrap();
        let repo = match open_repo(ws.path(), "r") {
            Ok(r) => r,
            Err(e) => panic!("open under workspace failed: {e}"),
        };
        // workdir is the repo dir (canonicalized)
        assert!(repo
            .workdir()
            .unwrap()
            .starts_with(ws.path().canonicalize().unwrap()));
    }

    /// Helper: call `open_repo` and extract the error, panicking if it
    /// unexpectedly succeeded (works around `git2::Repository: !Debug`).
    fn expect_err(workspace_root: &Path, repo_rel: &str, msg: &str) -> GitOpError {
        match open_repo(workspace_root, repo_rel) {
            Ok(_) => panic!("{msg}: expected Err but got Ok"),
            Err(e) => e,
        }
    }

    #[test]
    fn open_repo_escaping_workspace_rejected() {
        let ws = tempdir().unwrap();
        let outside = tempdir().unwrap();
        git2::Repository::init(outside.path()).unwrap();
        // a relative escape: "../" may resolve outside ws → Escape (or
        // NotFound if canonicalize fails because there is no directory there,
        // or Libgit2 if it resolves to something non-git).
        let err = expect_err(ws.path(), "../", "relative escape");
        assert!(
            matches!(
                err,
                GitOpError::Escape(_) | GitOpError::NotFound(_) | GitOpError::Libgit2(_)
            ),
            "unexpected err for ../: {err:?}"
        );

        // A symlink inside ws pointing outside → strict Escape.
        let link = ws.path().join("sneaky");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        let err = expect_err(ws.path(), "sneaky", "symlink escape");
        assert!(
            matches!(err, GitOpError::Escape(_)),
            "symlink escape must produce GitOpError::Escape, got: {err:?}"
        );

        // An ABSOLUTE repo param escapes too: `Path::join` replaces the base
        // with an absolute arg, so `<ws>.join("/abs/repo")` == `/abs/repo`,
        // which canonicalizes outside ws → strict Escape. Locks the guarantee
        // against a future join refactor.
        let abs = outside.path().to_str().unwrap().to_string();
        let err = expect_err(ws.path(), &abs, "absolute-path escape");
        assert!(
            matches!(err, GitOpError::Escape(_)),
            "absolute-path escape must produce GitOpError::Escape, got: {err:?}"
        );
    }

    #[test]
    fn open_nonexistent_repo_named_error() {
        let ws = tempdir().unwrap();
        let err = expect_err(ws.path(), "missing", "nonexistent repo");
        assert!(
            matches!(err, GitOpError::NotFound(_)),
            "missing path must produce GitOpError::NotFound, got: {err:?}"
        );
        // not a panic
    }

    /// Resolve the HEAD commit of `repo` (panics if unborn).
    fn head_commit(repo: &git2::Repository) -> GitCommitInfo {
        let commit = repo.head().unwrap().peel_to_commit().unwrap();
        commit_info(&commit)
    }

    #[test]
    fn add_then_commit_creates_head_commit_with_config_identity() {
        let dir = tempdir().unwrap();
        let (repo, _first, _second) = init_history(dir.path());
        // Set repo-local committer identity.
        {
            let mut cfg = repo.config().unwrap();
            cfg.set_str("user.name", "Cfg User").unwrap();
            cfg.set_str("user.email", "cfg@example.com").unwrap();
        }

        std::fs::write(dir.path().join("c.txt"), "gamma\n").unwrap();
        let added = add(&repo, &["c.txt".to_owned()]).unwrap();
        assert!(added.index_entries >= 1, "at least one path staged");

        let res = commit(&repo, "third").unwrap();
        let head = head_commit(&repo);
        assert_eq!(head.oid, res.oid, "returned oid is the new HEAD");
        assert_eq!(head.summary, "third");
        assert_eq!(
            head.author, "Cfg User <cfg@example.com>",
            "committer identity reflects repo config when set"
        );
    }

    #[test]
    fn commit_falls_back_to_default_identity_when_config_absent() {
        let dir = tempdir().unwrap();
        let (repo, _first, _second) = init_history(dir.path());
        // Ensure NO user identity is set in the repo config.
        {
            let mut cfg = repo.config().unwrap();
            let _ = cfg.remove("user.name");
            let _ = cfg.remove("user.email");
        }

        std::fs::write(dir.path().join("c.txt"), "gamma\n").unwrap();
        add(&repo, &["c.txt".to_owned()]).unwrap();
        let res = commit(&repo, "default-id").unwrap();

        let head = head_commit(&repo);
        assert_eq!(head.oid, res.oid);
        assert_eq!(
            head.author, "LingXi <noreply@lingxi>",
            "committer identity falls back to the fixed default when config is absent"
        );
    }

    #[test]
    fn add_empty_paths_stages_all() {
        let dir = tempdir().unwrap();
        let (repo, _first, _second) = init_history(dir.path());
        std::fs::write(dir.path().join("new1.txt"), "n1\n").unwrap();
        std::fs::write(dir.path().join("new2.txt"), "n2\n").unwrap();

        // Empty paths -> "*" -> stage everything.
        add(&repo, &[]).unwrap();
        let res = commit(&repo, "stage-all").unwrap();
        let (_info, d) = show(&repo, &res.oid).unwrap();
        assert!(d.patch.contains("new1.txt"), "new1 staged via *");
        assert!(d.patch.contains("new2.txt"), "new2 staged via *");
    }

    #[test]
    fn branch_create_makes_branch() {
        let dir = tempdir().unwrap();
        let (repo, _first, _second) = init_history(dir.path());

        branch_create(&repo, "feature").unwrap();
        let branches = branch_list(&repo).unwrap();
        assert!(
            branches.iter().any(|b| b.name == "feature" && !b.is_remote),
            "branch_create should add a local branch 'feature', got {branches:?}"
        );
    }

    #[test]
    fn checkout_switches_clean_worktree() {
        let dir = tempdir().unwrap();
        let (repo, _first, second) = init_history(dir.path());

        // Create a divergent branch "other" with its own commit.
        branch_create(&repo, "other").unwrap();
        checkout(&repo, "other").unwrap();
        std::fs::write(dir.path().join("other.txt"), "only-on-other\n").unwrap();
        add(&repo, &["other.txt".to_owned()]).unwrap();
        commit(&repo, "other-commit").unwrap();
        assert!(
            dir.path().join("other.txt").exists(),
            "other.txt present on 'other'"
        );

        // Back to the original branch (whatever the default name is): HEAD moves
        // to `second` and other.txt disappears from the worktree.
        // Find the default branch (the one that is `second`).
        let default = repo
            .branches(Some(git2::BranchType::Local))
            .unwrap()
            .filter_map(Result::ok)
            .find_map(|(b, _)| {
                let target = b.get().target();
                let name = b.name().ok().flatten().map(str::to_owned);
                match (target, name) {
                    (Some(t), Some(n)) if t == second && n != "other" => Some(n),
                    _ => None,
                }
            })
            .expect("a default branch pointing at `second`");

        checkout(&repo, &default).unwrap();
        let head = head_commit(&repo);
        assert_eq!(head.oid, second.to_string(), "HEAD switched to `second`");
        assert!(
            !dir.path().join("other.txt").exists(),
            "other.txt removed from worktree after switching away from 'other'"
        );
    }

    #[test]
    fn checkout_rejects_dirty_worktree() {
        let dir = tempdir().unwrap();
        let (repo, _first, _second) = init_history(dir.path());
        branch_create(&repo, "feature").unwrap();

        // Dirty the worktree: modify a tracked file without committing.
        std::fs::write(dir.path().join("a.txt"), "DIRTY-EDIT\n").unwrap();

        let err = match checkout(&repo, "feature") {
            Ok(_) => panic!("dirty checkout must be rejected"),
            Err(e) => e,
        };
        assert!(
            matches!(err, GitOpError::Dirty(_)),
            "dirty worktree must produce GitOpError::Dirty, got: {err:?}"
        );
        // No data loss: the uncommitted edit survives.
        let content = std::fs::read_to_string(dir.path().join("a.txt")).unwrap();
        assert_eq!(content, "DIRTY-EDIT\n", "uncommitted change preserved");
    }

    #[test]
    fn merge_fast_forward_advances() {
        let dir = tempdir().unwrap();
        let (repo, first, second) = init_history(dir.path());

        // Create a branch "behind" pointing at `first` (one commit behind HEAD).
        let first_commit = repo.find_commit(first).unwrap();
        repo.branch("behind", &first_commit, false).unwrap();

        // Switch to "behind" (clean worktree), then ff-merge the default branch's
        // tip `second`.
        checkout(&repo, "behind").unwrap();
        assert_eq!(head_commit(&repo).oid, first.to_string(), "on `first`");

        let res = merge(&repo, &second.to_string()).unwrap();
        assert!(res.fast_forward, "should be a fast-forward");
        assert_eq!(res.oid, second.to_string());
        assert_eq!(
            head_commit(&repo).oid,
            second.to_string(),
            "HEAD advanced to `second`"
        );
        // Worktree synced: b.txt (introduced in `second`) now exists.
        assert!(
            dir.path().join("b.txt").exists(),
            "worktree synced to second on ff"
        );
    }

    #[test]
    fn merge_non_ff_named_error() {
        let dir = tempdir().unwrap();
        let (repo, first, _second) = init_history(dir.path());

        // Branch "diverge" off `first`, give it a unique commit -> the two
        // histories diverge (neither is an ancestor of the other).
        let first_commit = repo.find_commit(first).unwrap();
        repo.branch("diverge", &first_commit, false).unwrap();
        checkout(&repo, "diverge").unwrap();
        std::fs::write(dir.path().join("d.txt"), "diverge\n").unwrap();
        add(&repo, &["d.txt".to_owned()]).unwrap();
        let diverge_tip = commit(&repo, "diverge-commit").unwrap();

        // Try to merge the default branch tip (`second`) -> non-ff.
        // Resolve `second` via the branch that still points at it.
        let other_tip = repo
            .branches(Some(git2::BranchType::Local))
            .unwrap()
            .filter_map(Result::ok)
            .find_map(|(b, _)| {
                let name = b.name().ok().flatten().map(str::to_owned)?;
                let target = b.get().target()?;
                (name != "diverge" && name != "behind").then_some(target)
            })
            .expect("default branch tip");

        let err = match merge(&repo, &other_tip.to_string()) {
            Ok(_) => panic!("diverged merge must be non-ff error"),
            Err(e) => e,
        };
        assert!(
            matches!(err, GitOpError::NonFastForward(_)),
            "diverged histories must produce GitOpError::NonFastForward, got: {err:?}"
        );
        // HEAD unchanged (still on the diverge tip), no conflict markers written.
        assert_eq!(
            head_commit(&repo).oid,
            diverge_tip.oid,
            "HEAD unchanged after refused non-ff merge"
        );
        let d_content = std::fs::read_to_string(dir.path().join("d.txt")).unwrap();
        assert!(
            !d_content.contains("<<<<<<<") && !d_content.contains(">>>>>>>"),
            "no conflict markers should be written on a refused non-ff merge"
        );
    }

    // ===== Network ops (Task 8) — file:// bare remotes, no real network =====

    /// Build a non-bare repo at `dir` with one commit (file `r.txt` = "v1\n")
    /// that can serve as a `file://` clone source, and return its `file://` URL
    /// plus the HEAD oid. A normal (non-bare) repo is a valid clone source.
    fn init_remote(dir: &Path) -> (String, git2::Oid) {
        let repo = git2::Repository::init(dir).unwrap();
        let sig = git2::Signature::now("Remote", "remote@example.com").unwrap();
        std::fs::write(dir.join("r.txt"), "v1\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("r.txt")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let oid = repo
            .commit(Some("HEAD"), &sig, &sig, "remote-first", &tree, &[])
            .unwrap();
        let url = format!("file://{}", dir.canonicalize().unwrap().display());
        (url, oid)
    }

    /// Add a second commit (`r2.txt` = "v2\n") to the repo at `dir` and return
    /// the new HEAD oid. Used to advance a `file://` remote between fetches.
    fn advance_remote(dir: &Path) -> git2::Oid {
        let repo = git2::Repository::open(dir).unwrap();
        let sig = git2::Signature::now("Remote", "remote@example.com").unwrap();
        std::fs::write(dir.join("r2.txt"), "v2\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("r2.txt")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "remote-second", &tree, &[&parent])
            .unwrap()
    }

    #[test]
    fn clone_from_file_remote_ok() {
        let remote_dir = tempdir().unwrap();
        let (url, head_oid) = init_remote(remote_dir.path());

        let ws = tempdir().unwrap();
        let net = GitNetConfig::default(); // no token / CA for file://
        let res = clone(&net, ws.path(), &url, "cloned").expect("file:// clone should succeed");

        assert_eq!(res.head, head_oid.to_string(), "cloned HEAD == remote HEAD");
        // The cloned working tree exists under the workspace with the file.
        let cloned = ws.path().join("cloned");
        assert!(cloned.join("r.txt").exists(), "cloned working file present");
        assert_eq!(
            std::fs::read_to_string(cloned.join("r.txt")).unwrap(),
            "v1\n"
        );
    }

    #[test]
    fn fetch_then_pull_ff_advances() {
        let remote_dir = tempdir().unwrap();
        let (url, _first) = init_remote(remote_dir.path());

        let ws = tempdir().unwrap();
        let net = GitNetConfig::default();
        clone(&net, ws.path(), &url, "cloned").unwrap();

        // Advance the remote with a new commit.
        let second = advance_remote(remote_dir.path());

        // Re-open the clone and fetch + pull (ff) -> local advances to `second`.
        let repo = open_repo(ws.path(), "cloned").unwrap();
        let f = fetch(&net, &repo, "origin").unwrap();
        assert_eq!(f.remote, "origin");

        let p = pull(&net, &repo, "origin", "").unwrap();
        assert!(p.fast_forward, "pull should be a fast-forward");
        assert_eq!(p.oid, second.to_string(), "local advanced to remote second");
        // Worktree synced: r2.txt now exists locally.
        assert!(
            ws.path().join("cloned").join("r2.txt").exists(),
            "ff pull synced the new file into the worktree"
        );
    }

    #[test]
    fn pull_non_ff_named_error() {
        let remote_dir = tempdir().unwrap();
        let (url, _first) = init_remote(remote_dir.path());

        let ws = tempdir().unwrap();
        let net = GitNetConfig::default();
        clone(&net, ws.path(), &url, "cloned").unwrap();

        // Advance the remote (so origin/<branch> is ahead) ...
        advance_remote(remote_dir.path());

        // ... and ALSO create a divergent local commit so the local branch is
        // not an ancestor of the remote tip -> non-ff.
        let repo = open_repo(ws.path(), "cloned").unwrap();
        std::fs::write(ws.path().join("cloned").join("local.txt"), "local\n").unwrap();
        add(&repo, &["local.txt".to_owned()]).unwrap();
        commit(&repo, "local-divergent").unwrap();

        let err = match pull(&net, &repo, "origin", "") {
            Ok(_) => panic!("diverged pull must be non-ff error"),
            Err(e) => e,
        };
        assert!(
            matches!(err, GitOpError::NonFastForward(_)),
            "diverged pull must produce GitOpError::NonFastForward, got: {err:?}"
        );
    }

    #[test]
    fn ssh_url_rejected() {
        let ws = tempdir().unwrap();
        let net = GitNetConfig::default();
        let err = match clone(&net, ws.path(), "git@github.com:x/y.git", "dest") {
            Ok(_) => panic!("ssh clone must be rejected"),
            Err(e) => e,
        };
        match &err {
            GitOpError::InvalidInput(msg) => {
                assert!(
                    msg.contains("ssh") && msg.contains("HTTPS"),
                    "ssh rejection should name ssh + HTTPS, got: {msg}"
                );
            }
            other => panic!("ssh clone must be InvalidInput, got: {other:?}"),
        }

        // ssh:// scheme is rejected too.
        assert!(matches!(
            clone(&net, ws.path(), "ssh://git@host/x.git", "dest"),
            Err(GitOpError::InvalidInput(_))
        ));
    }

    // ===== Push (G2) — file:// bare remotes, no real network =====

    /// Init a bare repo at `dir` to act as a `file://` "remote".
    fn init_bare_remote(dir: &Path) -> git2::Repository {
        git2::Repository::init_bare(dir).unwrap()
    }

    /// Short name of the repo's current HEAD branch (e.g. "master").
    fn current_branch(repo: &git2::Repository) -> String {
        repo.head().unwrap().shorthand().unwrap().to_owned()
    }

    #[test]
    fn push_advances_remote_ref_and_sets_upstream() {
        let work = tempdir().unwrap();
        let (repo, _f, second) = init_history(work.path());
        let bare = tempdir().unwrap();
        let remote_repo = init_bare_remote(bare.path());
        repo.remote("origin", &format!("file://{}", bare.path().display()))
            .unwrap();

        let branch = current_branch(&repo);
        let net = GitNetConfig::default(); // file:// needs no token
        let res = push(&net, &repo, "origin", "").expect("push ok");

        assert_eq!(res.remote, "origin");
        assert_eq!(res.branch, branch);
        assert_eq!(res.pushed_oid, second.to_string());
        assert!(res.set_upstream, "first push sets upstream");
        let remote_ref = remote_repo
            .find_reference(&format!("refs/heads/{branch}"))
            .unwrap();
        assert_eq!(remote_ref.target().unwrap(), second);
        let cfg = repo.config().unwrap();
        assert_eq!(
            cfg.get_string(&format!("branch.{branch}.remote")).unwrap(),
            "origin"
        );
        assert_eq!(
            cfg.get_string(&format!("branch.{branch}.merge")).unwrap(),
            format!("refs/heads/{branch}")
        );
    }

    #[test]
    fn push_is_idempotent_when_nothing_new() {
        let work = tempdir().unwrap();
        let (repo, _f, _second) = init_history(work.path());
        let bare = tempdir().unwrap();
        init_bare_remote(bare.path());
        repo.remote("origin", &format!("file://{}", bare.path().display()))
            .unwrap();
        let net = GitNetConfig::default();
        push(&net, &repo, "origin", "").expect("first push");
        let res = push(&net, &repo, "origin", "").expect("re-push ok");
        assert!(!res.set_upstream, "upstream already configured");
    }

    #[test]
    fn push_detached_head_without_branch_errors() {
        let work = tempdir().unwrap();
        let (repo, _f, second) = init_history(work.path());
        let bare = tempdir().unwrap();
        init_bare_remote(bare.path());
        repo.remote("origin", &format!("file://{}", bare.path().display()))
            .unwrap();
        repo.set_head_detached(second).unwrap();
        let net = GitNetConfig::default();
        let err = push(&net, &repo, "origin", "").unwrap_err();
        assert!(matches!(err, GitOpError::InvalidInput(ref m) if m.contains("detached")));
    }

    #[test]
    fn push_non_fast_forward_is_rejected() {
        let work = tempdir().unwrap();
        let (repo_a, _f, _second) = init_history(work.path());
        let bare = tempdir().unwrap();
        init_bare_remote(bare.path());
        repo_a
            .remote("origin", &format!("file://{}", bare.path().display()))
            .unwrap();
        let net = GitNetConfig::default();
        let branch = current_branch(&repo_a);
        push(&net, &repo_a, "origin", "").expect("A initial push");

        let clone_b = tempdir().unwrap();
        let repo_b =
            git2::Repository::clone(&format!("file://{}", bare.path().display()), clone_b.path())
                .unwrap();
        {
            let sig = git2::Signature::now("B", "b@example.com").unwrap();
            std::fs::write(clone_b.path().join("c.txt"), "gamma\n").unwrap();
            let mut idx = repo_b.index().unwrap();
            idx.add_path(Path::new("c.txt")).unwrap();
            idx.write().unwrap();
            let tree = repo_b.find_tree(idx.write_tree().unwrap()).unwrap();
            let head = repo_b.head().unwrap().peel_to_commit().unwrap();
            repo_b
                .commit(Some("HEAD"), &sig, &sig, "B commit", &tree, &[&head])
                .unwrap();
        }
        push(&net, &repo_b, "origin", &branch).expect("B ff push");

        {
            let sig = git2::Signature::now("A", "a@example.com").unwrap();
            std::fs::write(work.path().join("d.txt"), "delta\n").unwrap();
            let mut idx = repo_a.index().unwrap();
            idx.add_path(Path::new("d.txt")).unwrap();
            idx.write().unwrap();
            let tree = repo_a.find_tree(idx.write_tree().unwrap()).unwrap();
            let head = repo_a.head().unwrap().peel_to_commit().unwrap();
            repo_a
                .commit(Some("HEAD"), &sig, &sig, "A commit", &tree, &[&head])
                .unwrap();
        }
        let err = push(&net, &repo_a, "origin", &branch).unwrap_err();
        assert!(
            matches!(err, GitOpError::NonFastForward(_)),
            "stale push must be non-fast-forward, got {err:?}"
        );
    }

    #[test]
    fn push_rejects_ssh_remote() {
        let work = tempdir().unwrap();
        let (repo, _f, _s) = init_history(work.path());
        repo.remote("origin", "git@github.com:owner/repo.git")
            .unwrap();
        let net = GitNetConfig::default();
        let err = push(&net, &repo, "origin", "").unwrap_err();
        assert!(matches!(err, GitOpError::InvalidInput(ref m) if m.contains("ssh")));
    }
}
