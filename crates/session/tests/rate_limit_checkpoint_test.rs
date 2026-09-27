//! SC-02 — the rate-limit resume checkpoint, end to end against real `git`.
//!
//! `performRateLimitCheckpoint` (cc-238.js @292197753) latches its result for
//! the owning session, which makes it a once-per-session event while allowing
//! LingXi's embedded multi-session hosts to run independently. Every case here
//! still lives in one sequential test because it exercises one session id.
//!
//! The oracle's live trigger is the REPL rate-limit callback (@306528240); the
//! port's twin is `orchestrator::turn_loop::maybe_checkpoint_on_rate_limit`,
//! fired from the `is_carveout_propagated` arm when a turn ends on
//! `LlmError::RateLimited`.

use lingxi_core::session::{TodoItem, TodoState};
use session::{
    clear_last_checkpoint_result, last_checkpoint_result, perform_rate_limit_checkpoint,
    CheckpointGates, CheckpointRequest, CheckpointResult, CheckpointSkipReason, CheckpointTrigger,
    CHECKPOINT_REF_PREFIX, RESUME_MD_REPO_PATH,
};
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

const SESSION_ID: &str = "abcd1234-5678-4abc-9def-000000000000";

fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .output()
        .expect("run git")
}

fn todos() -> Vec<TodoItem> {
    vec![
        TodoItem {
            id: "1".into(),
            content: "port the executor".into(),
            status: TodoState::Completed,
            active_form: "Porting the executor".into(),
        },
        TodoItem {
            id: "2".into(),
            content: "wire the trigger".into(),
            status: TodoState::InProgress,
            active_form: "Wiring the trigger".into(),
        },
        TodoItem {
            id: "3".into(),
            content: "write the test".into(),
            status: TodoState::Pending,
            active_form: "Writing the test".into(),
        },
    ]
}

fn request<'a>(
    cwd: &'a Path,
    todos: &'a [TodoItem],
    gates: CheckpointGates,
) -> CheckpointRequest<'a> {
    CheckpointRequest {
        session_id: SESSION_ID,
        trigger: CheckpointTrigger::RateLimited,
        todos,
        cwd,
        gates,
    }
}

/// Seed a repository with one commit, one modified tracked file and one
/// untracked file — the three states the snapshot has to get right.
fn seed_repo(dir: &Path) {
    assert!(git(dir, &["init", "-q"]).status.success(), "git init");
    std::fs::write(dir.join("tracked.txt"), "v1\n").expect("write tracked");
    std::fs::write(dir.join("gone.txt"), "bye\n").expect("write gone");
    assert!(git(dir, &["add", "-A"]).status.success(), "git add");
    assert!(
        git(dir, &["commit", "-q", "-m", "seed"]).status.success(),
        "git commit"
    );
    // Working-tree state that differs from HEAD in all three ways.
    std::fs::write(dir.join("tracked.txt"), "v2-uncommitted\n").expect("modify tracked");
    std::fs::write(dir.join("untracked.txt"), "brand new\n").expect("write untracked");
    std::fs::remove_file(dir.join("gone.txt")).expect("delete tracked");
}

#[test]
fn rate_limit_checkpoint_snapshots_the_working_tree() {
    let todos = todos();

    // ── the three caller-supplied gates, in the oracle's order ──────────────
    let empty = TempDir::new().expect("tempdir");
    for (gates, expected) in [
        (
            CheckpointGates {
                non_interactive: true,
                ..CheckpointGates::default()
            },
            CheckpointSkipReason::NonInteractive,
        ),
        (
            CheckpointGates {
                remote_workspace: true,
                ..CheckpointGates::default()
            },
            CheckpointSkipReason::RemoteWorkspace,
        ),
        (
            CheckpointGates {
                policy_allows: false,
                ..CheckpointGates::default()
            },
            CheckpointSkipReason::Policy,
        ),
    ] {
        clear_last_checkpoint_result();
        assert_eq!(
            perform_rate_limit_checkpoint(&request(empty.path(), &todos, gates)),
            CheckpointResult::Skipped(expected)
        );
    }

    // ── not a git work tree ─────────────────────────────────────────────────
    clear_last_checkpoint_result();
    assert_eq!(
        perform_rate_limit_checkpoint(&request(empty.path(), &todos, CheckpointGates::default())),
        CheckpointResult::Skipped(CheckpointSkipReason::NotGit),
        "a non-repo must skip, not error"
    );

    // ── an in-progress sequencer is refused ─────────────────────────────────
    let repo = TempDir::new().expect("tempdir");
    seed_repo(repo.path());
    let git_dir = repo.path().join(".git");
    std::fs::write(git_dir.join("MERGE_HEAD"), "deadbeef\n").expect("fake sequencer");
    clear_last_checkpoint_result();
    assert_eq!(
        perform_rate_limit_checkpoint(&request(repo.path(), &todos, CheckpointGates::default())),
        CheckpointResult::Skipped(CheckpointSkipReason::SequencerInProgress)
    );
    std::fs::remove_file(git_dir.join("MERGE_HEAD")).expect("clear sequencer");

    // ── the real thing ──────────────────────────────────────────────────────
    clear_last_checkpoint_result();
    let result =
        perform_rate_limit_checkpoint(&request(repo.path(), &todos, CheckpointGates::default()));
    let CheckpointResult::Committed {
        ref_name,
        resume_path,
        commit_sha,
    } = result.clone()
    else {
        panic!("expected a checkpoint commit, got {result:?}");
    };
    assert_eq!(ref_name, format!("{CHECKPOINT_REF_PREFIX}abcd1234"));
    assert_eq!(resume_path, RESUME_MD_REPO_PATH);
    assert_eq!(commit_sha.len(), 40, "a full object id: {commit_sha}");

    // The ref resolves, and its commit is a CHILD of HEAD (a detached WIP
    // commit, not a branch move).
    let head_before = String::from_utf8(git(repo.path(), &["rev-parse", "HEAD"]).stdout)
        .expect("utf8")
        .trim()
        .to_string();
    let parent =
        String::from_utf8(git(repo.path(), &["rev-parse", &format!("{ref_name}^")]).stdout)
            .expect("utf8")
            .trim()
            .to_string();
    assert_eq!(parent, head_before, "checkpoint is parented on HEAD");

    // The snapshot holds the WORKING TREE, not HEAD.
    let show = |path: &str| {
        String::from_utf8(git(repo.path(), &["show", &format!("{ref_name}:{path}")]).stdout)
            .expect("utf8")
    };
    assert_eq!(show("tracked.txt"), "v2-uncommitted\n");
    assert_eq!(show("untracked.txt"), "brand new\n");
    let gone = git(repo.path(), &["show", &format!("{ref_name}:gone.txt")]);
    assert!(
        !gone.status.success(),
        "a tracked file deleted on disk must be force-removed from the snapshot"
    );

    // `RESUME.md` is in the snapshot AND on disk, and reads the way the oracle
    // renders it.
    let doc = show(RESUME_MD_REPO_PATH);
    assert!(doc.starts_with("# "), "document header: {doc}");
    assert!(doc.contains(&format!("Session: {SESSION_ID}")));
    assert!(doc.contains("Trigger: rate-limited"));
    assert!(doc.contains(&format!("Ref with your in-progress files: {ref_name}")));
    assert!(doc.contains("- [x] port the executor"));
    assert!(doc.contains("- [>] Wiring the trigger    \u{2190} current step"));
    assert!(doc.contains("- [ ] write the test"));
    assert!(
        doc.contains("## What's next\n\nwrite the test"),
        "the first PENDING content is the answer: {doc}"
    );
    let on_disk =
        std::fs::read_to_string(repo.path().join(RESUME_MD_REPO_PATH)).expect("RESUME.md on disk");
    assert_eq!(
        on_disk, doc,
        "disk copy and snapshot copy are the same bytes"
    );

    // The document is excluded, so it does not haunt `git status`.
    let exclude = std::fs::read_to_string(git_dir.join("info").join("exclude")).expect("exclude");
    assert!(
        exclude
            .lines()
            .any(|l| l == format!("/{RESUME_MD_REPO_PATH}")),
        "info/exclude must carry the resume path: {exclude}"
    );

    // The private index file must not survive.
    let leftovers: Vec<String> = std::fs::read_dir(&git_dir)
        .expect("readdir")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("lingxi-checkpoint-index."))
        .collect();
    assert!(
        leftovers.is_empty(),
        "private index left behind: {leftovers:?}"
    );

    // The user's own index is untouched: `gone.txt` is still staged-as-present
    // in the real index, i.e. `git status` still reports a deletion.
    let status =
        String::from_utf8(git(repo.path(), &["status", "--porcelain"]).stdout).expect("utf8");
    assert!(
        status.contains("gone.txt"),
        "the user's index must be untouched: {status}"
    );

    // ── the latch: a second call does no work and returns the same value ────
    assert_eq!(last_checkpoint_result(), Some(result.clone()));
    let again =
        perform_rate_limit_checkpoint(&request(repo.path(), &todos, CheckpointGates::default()));
    assert_eq!(again, result, "the latch makes this once-per-session");
    let refs = String::from_utf8(
        git(
            repo.path(),
            &[
                "for-each-ref",
                "--format=%(refname)",
                &format!("{CHECKPOINT_REF_PREFIX}*"),
            ],
        )
        .stdout,
    )
    .expect("utf8");
    assert_eq!(
        refs.lines().count(),
        1,
        "exactly one checkpoint ref: {refs}"
    );
}
