//! M4-04 parity driver — cross-checks the workflow tool implementations
//! against `workflow_tools.json`. Every byte-locked literal from spec §7
//! lines 482-489 is asserted here AND in the tool's own unit tests.

use lingxi_core::TodoState;
use serde_json::Value;
use telemetry::tengu::tool::{
    ENTER_PLAN_MODE_COMPLETED, ENTER_PLAN_MODE_FAILED, ENTER_PLAN_MODE_STARTED,
    ENTER_WORKTREE_COMPLETED, ENTER_WORKTREE_FAILED, ENTER_WORKTREE_STARTED,
    EXIT_PLAN_MODE_COMPLETED, EXIT_PLAN_MODE_FAILED, EXIT_PLAN_MODE_STARTED,
    EXIT_WORKTREE_COMPLETED, EXIT_WORKTREE_FAILED, EXIT_WORKTREE_STARTED,
};
use test_harness::parity::load_fixture;
use tool_task::todo_write::{TODO_STATE_COMPLETED, TODO_STATE_IN_PROGRESS, TODO_STATE_PENDING};
use tool_worktree::worktree::{
    flatten_slug, validate_worktree_slug, MAX_WORKTREE_SLUG_LENGTH, WORKTREE_BRANCH_PREFIX,
    WORKTREE_FLATTEN_CHAR, WORKTREE_PATH_SEGMENT,
};

fn fixture() -> Value {
    load_fixture::<Value>("workflow_tools")
}

#[test]
fn todo_states_match_fixture() {
    let f = fixture();
    assert_eq!(f["todo_states"]["pending"], TODO_STATE_PENDING);
    assert_eq!(f["todo_states"]["in_progress"], TODO_STATE_IN_PROGRESS);
    assert_eq!(f["todo_states"]["completed"], TODO_STATE_COMPLETED);
}

#[test]
fn todo_aliases_are_rejected_by_serde() {
    let f = fixture();
    for alias in f["todo_aliases_rejected"].as_array().unwrap() {
        let s = alias.as_str().unwrap();
        let literal = format!(r#""{s}""#);
        let result = serde_json::from_str::<TodoState>(&literal);
        let err = result
            .err()
            .unwrap_or_else(|| panic!("alias {s} must be rejected"));
        let msg = format!("{err}");
        assert!(
            msg.contains("unknown variant"),
            "alias {s} should produce unknown-variant error; got: {msg}"
        );
    }
}

// (todo_max_content_chars_matches_fixture removed: claude-code's TodoItemSchema
// `BWd` has `.min(1)` but NO `.max()` on content, so the 4096-char cap and its
// TODO_MAX_CONTENT_CHARS const were removed — #76.)

#[test]
fn worktree_constants_match_fixture() {
    let f = fixture();
    assert_eq!(f["worktree"]["branch_prefix"], WORKTREE_BRANCH_PREFIX);
    assert_eq!(f["worktree"]["path_segment"], WORKTREE_PATH_SEGMENT);
    assert_eq!(
        f["worktree"]["flatten_char"]
            .as_str()
            .unwrap()
            .chars()
            .next()
            .unwrap(),
        WORKTREE_FLATTEN_CHAR
    );
    assert_eq!(
        usize::try_from(f["worktree"]["max_slug_length"].as_u64().unwrap()).unwrap(),
        MAX_WORKTREE_SLUG_LENGTH
    );
}

#[test]
fn worktree_valid_slugs_flatten_correctly() {
    let f = fixture();
    for case in f["worktree"]["valid_slugs"].as_array().unwrap() {
        let slug = case["slug"].as_str().unwrap();
        validate_worktree_slug(slug).unwrap_or_else(|_| panic!("valid slug must pass: {slug}"));
        let flat = flatten_slug(slug);
        let full_branch = format!("{WORKTREE_BRANCH_PREFIX}{flat}");
        assert_eq!(case["expected_branch"], full_branch, "slug={slug}");
        let path_suffix = format!("{WORKTREE_PATH_SEGMENT}/{flat}");
        assert_eq!(case["expected_path_suffix"], path_suffix, "slug={slug}");
    }
}

#[test]
fn worktree_invalid_slugs_all_reject() {
    let f = fixture();
    for bad in f["worktree"]["invalid_slugs"].as_array().unwrap() {
        let s = bad.as_str().unwrap();
        assert!(
            validate_worktree_slug(s).is_err(),
            "invalid slug must reject: {s:?}"
        );
    }
}

#[test]
fn event_names_match_fixture() {
    let f = fixture();
    // (TodoWrite per-tool events removed for 2.1.195 parity — claude's TodoWrite
    // emits no per-tool telemetry.)
    let pairs: &[(&str, [&str; 3])] = &[
        (
            "enter_plan_mode",
            [
                ENTER_PLAN_MODE_STARTED,
                ENTER_PLAN_MODE_COMPLETED,
                ENTER_PLAN_MODE_FAILED,
            ],
        ),
        (
            "exit_plan_mode",
            [
                EXIT_PLAN_MODE_STARTED,
                EXIT_PLAN_MODE_COMPLETED,
                EXIT_PLAN_MODE_FAILED,
            ],
        ),
        (
            "enter_worktree",
            [
                ENTER_WORKTREE_STARTED,
                ENTER_WORKTREE_COMPLETED,
                ENTER_WORKTREE_FAILED,
            ],
        ),
        (
            "exit_worktree",
            [
                EXIT_WORKTREE_STARTED,
                EXIT_WORKTREE_COMPLETED,
                EXIT_WORKTREE_FAILED,
            ],
        ),
    ];
    for (key, consts) in pairs {
        let arr = f["tengu_events"][key].as_array().unwrap();
        assert_eq!(arr.len(), 3, "{key} must have exactly 3 events");
        assert_eq!(arr[0], consts[0], "{key} started");
        assert_eq!(arr[1], consts[1], "{key} completed");
        assert_eq!(arr[2], consts[2], "{key} failed");
    }
}

#[test]
fn event_suffix_is_completed_not_succeeded() {
    // Spec §1 line 27 uses `started/succeeded/failed` as a family name, but
    // the locked tengu schema (M3-06 commit cc05dc0) uses
    // `started/completed/failed`. Guard against future drift.
    for name in [
        ENTER_PLAN_MODE_COMPLETED,
        EXIT_PLAN_MODE_COMPLETED,
        ENTER_WORKTREE_COMPLETED,
        EXIT_WORKTREE_COMPLETED,
    ] {
        assert!(name.ends_with("_completed"), "drift: {name}");
        assert!(!name.ends_with("_succeeded"), "drift: {name}");
    }
}
