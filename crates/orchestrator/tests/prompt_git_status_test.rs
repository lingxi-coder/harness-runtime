//! Git status contributes to the prompt environment.

use orchestrator::prompt::{env_block, git_status, FileTree, SystemPromptContext};
use std::path::PathBuf;
use std::process::Command;
use tempfile::TempDir;

fn run_git(cwd: &std::path::Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git binary");
    assert!(out.status.success(), "git {args:?} failed: {out:?}");
}

fn fresh_repo() -> TempDir {
    let tmp = TempDir::new().unwrap();
    run_git(tmp.path(), &["init", "-q", "-b", "main"]);
    run_git(tmp.path(), &["config", "user.email", "t@t.io"]);
    run_git(tmp.path(), &["config", "user.name", "t"]);
    std::fs::write(tmp.path().join("a.txt"), "hi\n").unwrap();
    run_git(tmp.path(), &["add", "."]);
    run_git(tmp.path(), &["commit", "-q", "-m", "init"]);
    tmp
}

#[test]
fn probe_returns_none_outside_repo() {
    let tmp = TempDir::new().unwrap();
    assert!(git_status::probe(tmp.path()).is_none());
}

#[test]
fn probe_returns_some_inside_clean_repo() {
    let tmp = fresh_repo();
    let g = git_status::probe(tmp.path()).expect("probe");
    assert_eq!(g.branch, "main");
    assert!(g.working_dir_clean);
    assert!(g.file_changes_summary.is_empty());
}

#[test]
fn env_block_marks_git_repo_true_but_emits_no_git_status_lines() {
    let ctx = SystemPromptContext {
        cwd: PathBuf::from("/dummy"), // probe is NOT called by env_block
        platform: "darwin".into(),
        model: "claude-opus-4-7".into(),
        model_marketing_name: None,
        knowledge_cutoff: None,
        shell: "zsh".into(),
        os_version: "Darwin 25.3.0".into(),
        git_status: Some(orchestrator::prompt::GitStatus {
            branch: "feature/x".into(),
            working_dir_clean: false,
            file_changes_summary: String::new(),
        }),
        in_worktree: false,
        file_tree: FileTree::default(),
        memory_files: Vec::new(),
        tool_names: Vec::new(),
        skills_available: false,
        is_interactive: false,
        memory_dir: None,
        exclude_dynamic_sections: false,
    };
    let out = env_block::format(&ctx);
    // A repo present sets the `true` line; the env block carries NO branch /
    // status lines (those live in the separate `gitStatus` system-prompt block).
    assert!(out.contains("\n - Is a git repository: true\n"));
    assert!(!out.contains("Current branch"));
    assert!(!out.contains("Git branch"));
    assert!(!out.contains("gitStatus"));
}

#[test]
fn env_block_marks_git_repo_false_when_no_git_status() {
    let ctx = SystemPromptContext {
        cwd: PathBuf::from("/dummy"),
        platform: "darwin".into(),
        model: "claude-opus-4-7".into(),
        model_marketing_name: None,
        knowledge_cutoff: None,
        shell: "zsh".into(),
        os_version: "Darwin 25.3.0".into(),
        git_status: None,
        in_worktree: false,
        file_tree: FileTree::default(),
        memory_files: Vec::new(),
        tool_names: Vec::new(),
        skills_available: false,
        is_interactive: false,
        memory_dir: None,
        exclude_dynamic_sections: false,
    };
    let out = env_block::format(&ctx);
    assert!(out.contains("\n - Is a git repository: false\n"));
    assert!(!out.contains("Current branch"));
}

// ---- the v2.1.183 `gitStatus` system-prompt block (R-P1c) ----

#[test]
fn status_value_none_outside_repo() {
    let tmp = TempDir::new().unwrap();
    assert!(git_status::status_value(tmp.path()).is_none());
    assert!(git_status::render_git_status_block(tmp.path()).is_none());
}

#[test]
fn status_value_clean_repo_byte_structure() {
    let tmp = fresh_repo();
    let v = git_status::status_value(tmp.path()).expect("value");
    // First element + blank-line joins (claude-code `m8r` array `.join("\n\n")`).
    assert!(v.starts_with(
        "This is the git status at the start of the conversation. Note that this status is a \
snapshot in time, and will not update during the conversation.\n\n"
    ));
    assert!(v.contains("\n\nCurrent branch: main\n\n"));
    assert!(v.contains("\n\nMain branch (you will usually use this for PRs): "));
    // `user.name` is set → the `Git user:` element is present.
    assert!(v.contains("\n\nGit user: t\n\n"));
    // Clean tree → "(clean)".
    assert!(v.contains("\n\nStatus:\n(clean)\n\n"));
    // One commit ("init"); the Recent commits element is last (no trailing join).
    assert!(v.contains("\n\nRecent commits:\n"));
    assert!(v.trim_end().ends_with("init"));

    // The rendered block prefixes the `gitStatus: ` key (claude-code `WZa`).
    let block = git_status::render_git_status_block(tmp.path()).expect("block");
    assert!(
        block.starts_with("gitStatus: This is the git status at the start of the conversation.")
    );
}

#[test]
fn status_value_dirty_repo_shows_short_status() {
    let tmp = fresh_repo();
    std::fs::write(tmp.path().join("b.txt"), "new\n").unwrap();
    let v = git_status::status_value(tmp.path()).expect("value");
    // Untracked file shows up in `git status --short` as `?? b.txt`.
    assert!(v.contains("\n\nStatus:\n?? b.txt\n\n"), "got: {v}");
    assert!(!v.contains("(clean)"));
}

#[test]
fn status_value_omits_git_user_when_unset() {
    let tmp = TempDir::new().unwrap();
    run_git(tmp.path(), &["init", "-q", "-b", "main"]);
    run_git(tmp.path(), &["config", "user.email", "t@t.io"]);
    // Commit with a throwaway identity, then set the local `user.name` to EMPTY
    // so the merged `git config user.name` lookup returns "" (this overrides any
    // global user.name on the test host). `m8r` trims + filters empty → the
    // `Git user:` element is omitted.
    run_git(tmp.path(), &["config", "user.name", "tmp"]);
    std::fs::write(tmp.path().join("a.txt"), "hi\n").unwrap();
    run_git(tmp.path(), &["add", "."]);
    run_git(tmp.path(), &["commit", "-q", "-m", "init"]);
    run_git(tmp.path(), &["config", "user.name", ""]);
    let v = git_status::status_value(tmp.path()).expect("value");
    assert!(!v.contains("Git user:"), "got: {v}");
    // The Main-branch element is immediately followed by Status (no Git user).
    assert!(v.contains("for PRs): main\n\nStatus:"), "got: {v}");
}
