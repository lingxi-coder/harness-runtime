//! Project-dir + session-path resolver parity with
//! claude-code/src/utils/sessionStoragePortable.ts:293-331.

use session::jsonl::path::{project_dir_name, session_path, MAX_SANITIZED_LENGTH};
use std::path::Path;

#[test]
fn short_path_is_hyphenated_only() {
    assert_eq!(project_dir_name("/Users/foo/proj"), "-Users-foo-proj");
}

#[test]
fn windows_drive_letter_keeps_alnum_only() {
    assert_eq!(project_dir_name("C:\\Users\\foo"), "C--Users-foo");
}

#[test]
fn dot_and_space_become_hyphens() {
    assert_eq!(project_dir_name("/a b.c/d"), "-a-b-c-d");
}

#[test]
fn empty_cwd_is_empty_string() {
    assert_eq!(project_dir_name(""), "");
}

#[test]
fn at_max_length_no_suffix() {
    // 200 alnum chars -> sanitized unchanged -> no suffix.
    let cwd: String = "a".repeat(MAX_SANITIZED_LENGTH);
    assert_eq!(project_dir_name(&cwd), cwd);
}

#[test]
fn over_max_length_gets_djb2_suffix() {
    // 250 alnum chars -> first 200 kept, then "-<base36(abs(djb2(cwd)))>".
    let cwd: String = "a".repeat(250);
    let out = project_dir_name(&cwd);
    let (head, sep_and_suffix) = out.split_at(MAX_SANITIZED_LENGTH);
    assert_eq!(head, &"a".repeat(MAX_SANITIZED_LENGTH));
    assert!(
        sep_and_suffix.starts_with('-'),
        "expected '-' separator before suffix, got {out:?}"
    );
    let suffix = &sep_and_suffix[1..];
    assert!(
        suffix
            .chars()
            .all(|c: char| c.is_ascii_digit() || c.is_ascii_lowercase()),
        "suffix must be base36 lowercase: {suffix:?}"
    );
    assert!(!suffix.is_empty(), "suffix must be non-empty");
}

#[test]
fn session_path_layout_matches_claude_code() {
    let home = Path::new("/home/user/.lingxi");
    let p = session_path(
        home,
        "/Users/foo/proj",
        "0a1b2c3d-4e5f-6789-abcd-ef0123456789",
    );
    assert_eq!(
        p,
        Path::new(
            "/home/user/.lingxi/projects/-Users-foo-proj/0a1b2c3d-4e5f-6789-abcd-ef0123456789.jsonl"
        )
    );
}
