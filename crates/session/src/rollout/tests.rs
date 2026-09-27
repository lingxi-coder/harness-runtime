//! Metadata / policy / session-index / record-format tests — ported from
//! codex's `rollout/src/{tests,metadata_tests,session_index_tests}.rs` for the
//! behavior that maps to the LingXi-merged API.

#![allow(clippy::unwrap_used)]

use super::metadata::{
    builder_from_items, parse_timestamp_uuid_from_filename, plain_rollout_path, rollout_date_parts,
};
use super::policy::{is_persisted_rollout_item, persisted_rollout_items};
use super::record::{
    CompactedItem, GitInfo, RolloutItem, RolloutLine, SessionMeta, SessionMetaLine, SessionSource,
    ThreadId,
};
use super::session_index::{
    append_thread_name, find_thread_name_by_id, find_thread_names_by_ids,
    remove_thread_name_entries,
};
use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use uuid::Uuid;

// ---- record format ----

#[test]
fn rollout_line_roundtrips_session_meta() {
    let thread_id = ThreadId::from_uuid(Uuid::from_u128(0x42));
    let meta = SessionMeta {
        id: thread_id,
        session_id: thread_id,
        timestamp: "2025-01-03T12:00:00.000Z".to_string(),
        cwd: PathBuf::from("/repo"),
        originator: "test".to_string(),
        cli_version: "1.0".to_string(),
        source: SessionSource::Cli,
        model_provider: Some("p".to_string()),
        ..SessionMeta::default()
    };
    let line = RolloutLine {
        timestamp: "2025-01-03T12:00:00.000Z".to_string(),
        item: RolloutItem::SessionMeta(SessionMetaLine { meta, git: None }),
    };
    let json = serde_json::to_string(&line).unwrap();
    // Envelope: {timestamp, type:"session_meta", payload:{...}}
    assert!(json.contains("\"type\":\"session_meta\""));
    assert!(json.contains("\"payload\""));
    let parsed: RolloutLine = serde_json::from_str(&json).unwrap();
    let RolloutItem::SessionMeta(meta_line) = parsed.item else {
        panic!("expected session meta");
    };
    assert_eq!(meta_line.meta.id, thread_id);
    assert_eq!(meta_line.meta.session_id, thread_id);
}

#[test]
fn session_meta_line_defaults_session_id_from_id() {
    // Legacy header without session_id should default it from `id`.
    let value = serde_json::json!({
        "id": "00000000-0000-0000-0000-000000000001",
        "timestamp": "2025-01-03T12:00:00Z",
        "cwd": ".",
        "originator": "o",
        "cli_version": "v",
        "source": "cli",
    });
    let meta_line: SessionMetaLine = serde_json::from_value(value).unwrap();
    assert_eq!(meta_line.meta.session_id, meta_line.meta.id);
}

#[test]
fn session_meta_line_parses_git_into_field_not_extra() {
    let value = serde_json::json!({
        "id": "00000000-0000-0000-0000-000000000001",
        "session_id": "00000000-0000-0000-0000-000000000001",
        "timestamp": "2025-01-03T12:00:00Z",
        "cwd": ".",
        "originator": "o",
        "cli_version": "v",
        "source": "cli",
        "git": { "branch": "main", "commit_hash": "abc123" },
    });
    let meta_line: SessionMetaLine = serde_json::from_value(value).unwrap();
    assert_eq!(
        meta_line.git,
        Some(GitInfo {
            commit_hash: Some("abc123".to_string()),
            branch: Some("main".to_string()),
            repository_url: None,
        })
    );
    assert!(!meta_line.meta.extra.contains_key("git"));
}

// ---- metadata path helpers ----

#[test]
fn parse_timestamp_uuid_from_filename_basic() {
    let uuid = Uuid::from_u128(0xABCD);
    let name = format!("rollout-2025-01-03T12-34-56-{uuid}.jsonl");
    let (ts, parsed_uuid) = parse_timestamp_uuid_from_filename(&name).expect("parse");
    assert_eq!(parsed_uuid, uuid);
    assert_eq!(
        ts.format("%Y-%m-%dT%H:%M:%S").to_string(),
        "2025-01-03T12:34:56"
    );
}

#[test]
fn parse_timestamp_uuid_from_filename_compressed_suffix() {
    let uuid = Uuid::from_u128(0x1);
    let name = format!("rollout-2025-01-03T12-34-56-{uuid}.jsonl.zst");
    let (_, parsed_uuid) = parse_timestamp_uuid_from_filename(&name).expect("parse");
    assert_eq!(parsed_uuid, uuid);
}

#[test]
fn parse_timestamp_uuid_from_filename_rejects_non_rollout() {
    assert!(parse_timestamp_uuid_from_filename("notarollout.jsonl").is_none());
    assert!(parse_timestamp_uuid_from_filename("rollout-bad.jsonl").is_none());
}

#[test]
fn plain_rollout_path_strips_zst() {
    let p = Path::new("/x/rollout-2025-01-03T12-34-56-aaaa.jsonl.zst");
    assert_eq!(
        plain_rollout_path(p),
        PathBuf::from("/x/rollout-2025-01-03T12-34-56-aaaa.jsonl")
    );
    let plain = Path::new("/x/rollout-2025-01-03T12-34-56-aaaa.jsonl");
    assert_eq!(plain_rollout_path(plain), plain.to_path_buf());
}

#[test]
fn rollout_date_parts_splits_filename() {
    let name = OsString::from("rollout-2025-01-03T12-34-56-aaaa.jsonl");
    assert_eq!(
        rollout_date_parts(&name),
        Some(("2025".to_string(), "01".to_string(), "03".to_string()))
    );
}

#[test]
fn builder_from_items_uses_session_meta() {
    let thread_id = ThreadId::from_uuid(Uuid::from_u128(0x55));
    let meta = SessionMeta {
        id: thread_id,
        session_id: thread_id,
        timestamp: "2025-01-03T12-34-56".to_string(),
        cwd: PathBuf::from("/repo"),
        originator: "o".to_string(),
        cli_version: "1.2".to_string(),
        source: SessionSource::Cli,
        model_provider: Some("prov".to_string()),
        ..SessionMeta::default()
    };
    let items = vec![RolloutItem::SessionMeta(SessionMetaLine {
        meta,
        git: Some(GitInfo {
            commit_hash: Some("sha".to_string()),
            branch: Some("br".to_string()),
            repository_url: None,
        }),
    })];
    let path = Path::new("/x/rollout-2025-01-03T12-34-56-aaaa.jsonl");
    let builder = builder_from_items(&items, path).expect("builder");
    assert_eq!(builder.id, thread_id);
    assert_eq!(builder.cwd, PathBuf::from("/repo"));
    assert_eq!(builder.git_sha.as_deref(), Some("sha"));
    let metadata = builder.build("default-prov");
    assert_eq!(metadata.model_provider.as_deref(), Some("prov"));
}

#[test]
fn builder_from_items_falls_back_to_filename() {
    let uuid = Uuid::from_u128(0x99);
    let path = PathBuf::from(format!("/x/rollout-2025-01-03T12-34-56-{uuid}.jsonl"));
    let builder = builder_from_items(&[], &path).expect("builder from filename");
    assert_eq!(builder.id, ThreadId::from_uuid(uuid));
    // No explicit provider → build() fills the default.
    let metadata = builder.build("default-prov");
    assert_eq!(metadata.model_provider.as_deref(), Some("default-prov"));
}

// ---- policy ----

#[test]
fn policy_persists_session_meta_and_compacted() {
    let meta = RolloutItem::SessionMeta(SessionMetaLine {
        meta: SessionMeta::default(),
        git: None,
    });
    assert!(is_persisted_rollout_item(&meta));
    let compacted = RolloutItem::Compacted(CompactedItem {
        message: "m".to_string(),
        replacement_history: None,
        extra: serde_json::Map::new(),
    });
    assert!(is_persisted_rollout_item(&compacted));
}

#[test]
fn policy_filters_event_msgs_by_inner_type() {
    let persisted = RolloutItem::EventMsg(serde_json::json!({ "type": "user_message" }));
    let dropped = RolloutItem::EventMsg(serde_json::json!({ "type": "exec_command_begin" }));
    assert!(is_persisted_rollout_item(&persisted));
    assert!(!is_persisted_rollout_item(&dropped));

    let filtered = persisted_rollout_items(&[persisted, dropped]);
    assert_eq!(filtered.len(), 1);
}

#[test]
fn policy_item_completed_only_for_plan_or_sleep() {
    let plan = RolloutItem::EventMsg(serde_json::json!({
        "type": "item_completed",
        "item": { "type": "plan" },
    }));
    let other = RolloutItem::EventMsg(serde_json::json!({
        "type": "item_completed",
        "item": { "type": "exec" },
    }));
    assert!(is_persisted_rollout_item(&plan));
    assert!(!is_persisted_rollout_item(&other));
}

#[test]
fn policy_drops_additional_tools_response_item() {
    let drop = RolloutItem::ResponseItem(serde_json::json!({ "type": "additional_tools" }));
    let keep = RolloutItem::ResponseItem(serde_json::json!({ "type": "message" }));
    assert!(!is_persisted_rollout_item(&drop));
    assert!(is_persisted_rollout_item(&keep));
}

// ---- session index ----

#[tokio::test]
async fn session_index_latest_name_wins() -> std::io::Result<()> {
    let temp = TempDir::new()?;
    let id = ThreadId::new();
    append_thread_name(temp.path(), id, "first").await?;
    append_thread_name(temp.path(), id, "second").await?;
    let found = find_thread_name_by_id(temp.path(), &id).await?;
    assert_eq!(found.as_deref(), Some("second"));
    Ok(())
}

#[tokio::test]
async fn session_index_names_by_ids_batch() -> std::io::Result<()> {
    let temp = TempDir::new()?;
    let id1 = ThreadId::new();
    let id2 = ThreadId::new();
    let id3 = ThreadId::new();
    append_thread_name(temp.path(), id1, "one").await?;
    append_thread_name(temp.path(), id2, "two").await?;
    // id2 renamed — newest wins.
    append_thread_name(temp.path(), id2, "two-renamed").await?;

    let mut wanted = HashSet::new();
    wanted.insert(id1);
    wanted.insert(id2);
    wanted.insert(id3);
    let names = find_thread_names_by_ids(temp.path(), &wanted).await?;
    assert_eq!(names.get(&id1).map(String::as_str), Some("one"));
    assert_eq!(names.get(&id2).map(String::as_str), Some("two-renamed"));
    assert_eq!(names.get(&id3), None);
    Ok(())
}

#[tokio::test]
async fn session_index_remove_entries() -> std::io::Result<()> {
    let temp = TempDir::new()?;
    let id1 = ThreadId::new();
    let id2 = ThreadId::new();
    append_thread_name(temp.path(), id1, "keep").await?;
    append_thread_name(temp.path(), id2, "drop").await?;
    remove_thread_name_entries(temp.path(), id2).await?;
    assert_eq!(
        find_thread_name_by_id(temp.path(), &id1).await?.as_deref(),
        Some("keep")
    );
    assert_eq!(find_thread_name_by_id(temp.path(), &id2).await?, None);
    Ok(())
}

#[tokio::test]
async fn session_index_missing_file_is_none() -> std::io::Result<()> {
    let temp = TempDir::new()?;
    let id = ThreadId::new();
    assert_eq!(find_thread_name_by_id(temp.path(), &id).await?, None);
    Ok(())
}
