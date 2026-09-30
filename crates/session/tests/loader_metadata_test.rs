//! T3 tests — confirm `SessionMetadata` `Ord` is newest-first by the logical
//! transcript timestamps carried in `modified`/`created`.

use session::jsonl::SessionMetadata;
use std::path::PathBuf;
use std::time::{Duration, UNIX_EPOCH};
use uuid::Uuid;

fn meta(uuid_byte: u8, secs: u64, created_secs: u64, name: &str) -> SessionMetadata {
    SessionMetadata {
        uuid: Uuid::from_bytes([uuid_byte; 16]),
        title: format!("title-{uuid_byte}"),
        modified: UNIX_EPOCH + Duration::from_secs(secs),
        created: UNIX_EPOCH + Duration::from_secs(created_secs),
        message_count: 1,
        mode: session::jsonl::SessionMode::Code,
        path: PathBuf::from(name),
        pr_number: None,
        custom_or_ai_title: Some(format!("title-{uuid_byte}")),
        resume_model: None,
        resume_model_profile: None,
    }
}

#[test]
fn ord_sorts_newest_first() {
    let mut v = [
        meta(1, 100, 100, "a.jsonl"),
        meta(2, 300, 300, "b.jsonl"),
        meta(3, 200, 200, "c.jsonl"),
    ];
    v.sort();
    assert_eq!(v[0].modified, UNIX_EPOCH + Duration::from_secs(300));
    assert_eq!(v[1].modified, UNIX_EPOCH + Duration::from_secs(200));
    assert_eq!(v[2].modified, UNIX_EPOCH + Duration::from_secs(100));
}

#[test]
fn ord_prefers_newer_transcript_activity_over_filesystem_mtime() {
    let older_file_newer_conversation = meta(1, 5, 1, "a.jsonl");
    let newer_file_older_conversation = meta(2, 4, 1, "b.jsonl");
    let mut v = [newer_file_older_conversation, older_file_newer_conversation];
    v.sort();
    assert_eq!(v[0].uuid, Uuid::from_bytes([1; 16]));
    assert_eq!(v[1].uuid, Uuid::from_bytes([2; 16]));
}

#[test]
fn ord_ties_break_by_created_descending() {
    // Equal `modified` → newest `created` (birthtime) first, mirroring
    // claude-code `sortLogs` (`types/logs.ts:327-328`). The filenames are
    // deliberately NOT in created order to prove the tie-break is `created`,
    // not the old filename-ascending behavior.
    let mut v = [
        meta(1, 500, 100, "z.jsonl"), // oldest created
        meta(2, 500, 300, "a.jsonl"), // newest created
        meta(3, 500, 200, "m.jsonl"),
    ];
    v.sort();
    assert_eq!(v[0].created, UNIX_EPOCH + Duration::from_secs(300));
    assert_eq!(v[1].created, UNIX_EPOCH + Duration::from_secs(200));
    assert_eq!(v[2].created, UNIX_EPOCH + Duration::from_secs(100));
    // And the matching paths follow the created order, not filename order.
    assert_eq!(v[0].path.to_string_lossy(), "a.jsonl");
    assert_eq!(v[1].path.to_string_lossy(), "m.jsonl");
    assert_eq!(v[2].path.to_string_lossy(), "z.jsonl");
}

#[test]
fn equality_requires_all_fields() {
    let a = meta(1, 100, 100, "x");
    let b = meta(1, 100, 100, "x");
    assert_eq!(a, b);
    let c = meta(1, 101, 100, "x");
    assert_ne!(a, c);
    // `created` participates in equality too.
    let d = meta(1, 100, 101, "x");
    assert_ne!(a, d);
}
