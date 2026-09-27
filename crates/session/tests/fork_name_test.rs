//! cc 2.1.198: `/branch` default fork name is derived from the first REAL user
//! prompt, skipping a leading compaction summary (binary `I2l`/`n9e`,
//! @217273303 / @206703354).

#![allow(clippy::needless_pass_by_value)]

use serde_json::json;
use session::jsonl::{derive_fork_name, JsonlMessage};

fn user(content: serde_json::Value) -> JsonlMessage {
    serde_json::from_value(json!({
        "type": "user",
        "uuid": "11111111-1111-1111-1111-111111111111",
        "parentUuid": null,
        "sessionId": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
        "timestamp": "2026-05-25T12:00:00.000Z",
        "cwd": "/tmp",
        "version": "0.6.0",
        "isSidechain": false,
        "userType": "external",
        "message": {"role": "user", "content": content}
    }))
    .expect("user message")
}

fn compact_summary(content: serde_json::Value) -> JsonlMessage {
    let mut m = user(content);
    m.extra.insert("isCompactSummary".to_string(), json!(true));
    m
}

fn meta(content: serde_json::Value) -> JsonlMessage {
    let mut m = user(content);
    m.extra.insert("isMeta".to_string(), json!(true));
    m
}

/// THE FIX: a session whose history STARTS with a compaction summary names the
/// branch from the first real (non-summary) user prompt.
#[test]
fn compaction_summary_is_skipped_for_fork_name() {
    let messages = vec![
        compact_summary(json!(
            "This is a long compacted history summary of prior work"
        )),
        user(json!("fix the login bug")),
    ];
    assert_eq!(derive_fork_name(&messages), "fix the login bug");
}

/// isMeta user messages are skipped too (binary `e.isMeta===!0` guard).
#[test]
fn meta_and_summary_both_skipped() {
    let messages = vec![
        meta(json!("<ide-context>...</ide-context>")),
        compact_summary(json!("summary text")),
        user(json!("real prompt here")),
    ];
    assert_eq!(derive_fork_name(&messages), "real prompt here");
}

/// Empty / no real prompt → the `/branch`-specific fallback (NOT "(session)").
#[test]
fn empty_history_uses_branch_fallback() {
    assert_eq!(derive_fork_name(&[]), "Branched conversation");
    assert_eq!(
        derive_fork_name(&[compact_summary(json!("only a summary"))]),
        "Branched conversation"
    );
}

/// I2l caps at 100 chars (vs the title path's 200) and collapses whitespace
/// runs (`/\s+/g` → " ").
#[test]
fn caps_at_100_chars_and_collapses_whitespace() {
    let long = "a".repeat(250);
    let name = derive_fork_name(&[user(json!(long))]);
    assert_eq!(name.chars().count(), 100, "fork name caps at 100 chars");

    let spaced = derive_fork_name(&[user(json!("hello     world\t\tfoo"))]);
    assert_eq!(spaced, "hello world foo");
}

/// A normal first prompt is returned verbatim (short case).
#[test]
fn short_prompt_returned_verbatim() {
    assert_eq!(derive_fork_name(&[user(json!("just do it"))]), "just do it");
}
