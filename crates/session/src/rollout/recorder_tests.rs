//! Recorder behavior tests — ported from codex's `rollout/src/recorder_tests.rs`
//! for the subset of behavior that maps to the LingXi-merged recorder API.
//!
//! Tests that depend on codex's SQLite `state_db` listing/pagination
//! (`list_threads_*`, `state_db_*`) are out of scope here — the LingXi
//! `session` crate does not host a SQLite thread store.

#![allow(clippy::unwrap_used)]

use super::initial_history::InitialHistory;
use super::record::{RolloutItem, RolloutLine, SessionSource, ThreadId};
use super::recorder::{
    append_rollout_item_to_path, RolloutConfig, RolloutRecorder, RolloutRecorderParams,
    RolloutWriterStateForTest,
};
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use tempfile::TempDir;
use uuid::Uuid;

fn test_config(codex_home: &Path) -> RolloutConfig {
    RolloutConfig {
        codex_home: codex_home.to_path_buf(),
        cwd: codex_home.to_path_buf(),
        model_provider_id: "test-provider".to_string(),
        generate_memories: true,
    }
}

#[tokio::test]
async fn load_rollout_items_defaults_legacy_session_id() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let rollout_path = home.path().join("rollout.jsonl");
    let mut file = File::create(&rollout_path)?;
    let thread_id = ThreadId::new();
    let ts = "2025-01-03T12:00:00Z";

    // Header WITHOUT `session_id` — the deserializer must default it from `id`.
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "timestamp": ts,
            "type": "session_meta",
            "payload": {
                "id": thread_id,
                "timestamp": ts,
                "cwd": ".",
                "originator": "test_originator",
                "cli_version": "test_version",
                "source": "cli",
                "model_provider": "test-provider",
            },
        })
    )?;
    // A legacy ghost_snapshot response item that must be dropped.
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "timestamp": ts,
            "type": "response_item",
            "payload": {
                "type": "ghost_snapshot",
                "ghost_commit": { "id": "deadbeef" },
            },
        })
    )?;
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "timestamp": ts,
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [ { "type": "output_text", "text": "hello" } ],
            },
        })
    )?;

    let (items, loaded_thread_id, parse_errors) =
        RolloutRecorder::load_rollout_items(&rollout_path).await?;

    assert_eq!(loaded_thread_id, Some(thread_id));
    assert_eq!(parse_errors, 0);
    assert_eq!(items.len(), 2);
    let RolloutItem::SessionMeta(session_meta) = &items[0] else {
        panic!("expected session metadata");
    };
    assert_eq!(session_meta.meta.session_id, thread_id);
    assert!(matches!(items[1], RolloutItem::ResponseItem(_)));
    Ok(())
}

#[tokio::test]
async fn load_rollout_items_preserves_legacy_event_lines() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let rollout_path = home.path().join("rollout.jsonl");
    let mut file = File::create(&rollout_path)?;
    let thread_id = ThreadId::new();
    let ts = "2025-01-03T12:00:00Z";

    writeln!(
        file,
        "{}",
        serde_json::json!({
            "timestamp": ts,
            "type": "session_meta",
            "payload": {
                "session_id": thread_id,
                "id": thread_id,
                "timestamp": ts,
                "cwd": ".",
                "originator": "test_originator",
                "cli_version": "test_version",
                "source": "cli",
                "model_provider": "test-provider",
            },
        })
    )?;
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "timestamp": ts,
            "type": "event_msg",
            "payload": {
                "type": "guardian_assessment",
                "id": "guardian-1",
                "turn_id": "turn-1",
                "status": "in_progress",
            },
        })
    )?;

    let (items, loaded_thread_id, parse_errors) =
        RolloutRecorder::load_rollout_items(&rollout_path).await?;

    assert_eq!(loaded_thread_id, Some(thread_id));
    assert_eq!(parse_errors, 0);
    assert_eq!(items.len(), 2);
    let RolloutItem::EventMsg(payload) = &items[1] else {
        panic!("expected event_msg rollout item");
    };
    assert_eq!(
        payload.get("id").and_then(|v| v.as_str()),
        Some("guardian-1")
    );
    Ok(())
}

#[tokio::test]
async fn load_rollout_items_filters_legacy_ghost_snapshots_from_compaction_history(
) -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let rollout_path = home.path().join("rollout.jsonl");
    let mut file = File::create(&rollout_path)?;
    let thread_id = ThreadId::new();
    let ts = "2025-01-03T12:00:00Z";

    writeln!(
        file,
        "{}",
        serde_json::json!({
            "timestamp": ts,
            "type": "session_meta",
            "payload": {
                "session_id": thread_id,
                "id": thread_id,
                "timestamp": ts,
                "cwd": ".",
                "originator": "test_originator",
                "cli_version": "test_version",
                "source": "cli",
                "model_provider": "test-provider",
            },
        })
    )?;
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "timestamp": ts,
            "type": "compacted",
            "payload": {
                "message": "summary",
                "replacement_history": [
                    {
                        "type": "message",
                        "role": "assistant",
                        "content": [ { "type": "output_text", "text": "kept" } ],
                    },
                    {
                        "type": "ghost_snapshot",
                        "ghost_commit": { "id": "deadbeef" },
                    }
                ],
            },
        })
    )?;

    let (items, loaded_thread_id, parse_errors) =
        RolloutRecorder::load_rollout_items(&rollout_path).await?;

    assert_eq!(loaded_thread_id, Some(thread_id));
    assert_eq!(parse_errors, 0);
    assert_eq!(items.len(), 2);
    let RolloutItem::Compacted(compacted) = &items[1] else {
        panic!("expected compacted rollout item");
    };
    let replacement_history = compacted
        .replacement_history
        .as_ref()
        .expect("replacement history");
    assert_eq!(replacement_history.len(), 1);
    assert_eq!(
        replacement_history[0].get("type").and_then(|v| v.as_str()),
        Some("message")
    );
    Ok(())
}

#[tokio::test]
async fn get_rollout_history_builds_resumed_history() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let rollout_path = home.path().join("rollout.jsonl");
    let mut file = File::create(&rollout_path)?;
    let thread_id = ThreadId::new();
    let ts = "2025-01-03T12:00:00Z";

    writeln!(
        file,
        "{}",
        serde_json::json!({
            "timestamp": ts,
            "type": "session_meta",
            "payload": {
                "id": thread_id,
                "session_id": thread_id,
                "timestamp": ts,
                "cwd": "/work",
                "originator": "test_originator",
                "cli_version": "test_version",
                "source": "cli",
            },
        })
    )?;
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "timestamp": ts,
            "type": "response_item",
            "payload": { "type": "message", "role": "assistant", "content": [] },
        })
    )?;

    let history = RolloutRecorder::get_rollout_history(&rollout_path).await?;
    let InitialHistory::Resumed(resumed) = history else {
        panic!("expected resumed history");
    };
    assert_eq!(resumed.conversation_id, thread_id);
    assert_eq!(resumed.history.len(), 2);
    assert_eq!(
        resumed.rollout_path.as_deref(),
        Some(rollout_path.as_path())
    );
    Ok(())
}

#[tokio::test]
async fn get_rollout_history_without_meta_errors() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let rollout_path = home.path().join("no-meta.jsonl");
    let mut file = File::create(&rollout_path)?;
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "timestamp": "2025-01-03T12:00:00Z",
            "type": "response_item",
            "payload": { "type": "message", "role": "user", "content": [] },
        })
    )?;
    let err = RolloutRecorder::get_rollout_history(&rollout_path)
        .await
        .expect_err("missing session_meta should error");
    assert!(err.to_string().contains("thread ID"));
    Ok(())
}

#[tokio::test]
async fn load_rollout_items_errors_on_empty_file() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let rollout_path = home.path().join("empty.jsonl");
    File::create(&rollout_path)?;
    let err = RolloutRecorder::load_rollout_items(&rollout_path)
        .await
        .expect_err("empty file should error");
    assert!(err.to_string().contains("empty session file"));
    Ok(())
}

#[tokio::test]
async fn recorder_materializes_on_flush_with_pending_items() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());
    let session_id = ThreadId::new();
    let thread_id = ThreadId::new();
    let initial_window_id = Uuid::new_v4().to_string();
    let recorder = RolloutRecorder::new(
        &config,
        RolloutRecorderParams::new(
            thread_id,
            None,
            None,
            SessionSource::Exec,
            "test_originator".to_string(),
        )
        .with_session_id(session_id)
        .with_initial_window_id(initial_window_id.clone()),
    )
    .await?;

    let rollout_path = recorder.rollout_path().to_path_buf();
    assert!(
        !rollout_path.exists(),
        "rollout file should not exist before the first recordable item"
    );

    recorder
        .record_canonical_items(&[RolloutItem::EventMsg(serde_json::json!({
            "type": "agent_message",
            "message": "buffered-event",
        }))])
        .await?;
    recorder.flush().await?;
    assert!(
        rollout_path.exists(),
        "flush with pending items should materialize the rollout"
    );

    recorder
        .record_canonical_items(&[RolloutItem::EventMsg(serde_json::json!({
            "type": "user_message",
            "message": "first-user-message",
        }))])
        .await?;
    recorder.flush().await?;

    recorder.persist().await?;
    // Second call verifies persist() is idempotent after materialization.
    recorder.persist().await?;
    assert!(rollout_path.exists(), "rollout file should be materialized");

    let text = std::fs::read_to_string(&rollout_path)?;
    let first_line = text.lines().next().expect("session metadata line");
    let session_meta: RolloutLine = serde_json::from_str(first_line)?;
    let RolloutItem::SessionMeta(session_meta) = session_meta.item else {
        panic!("expected session metadata in rollout");
    };
    assert_eq!(session_meta.meta.session_id, session_id);
    assert_eq!(
        session_meta
            .meta
            .context_window
            .as_ref()
            .and_then(|w| w.get("window_id"))
            .and_then(|v| v.as_str()),
        Some(initial_window_id.as_str())
    );
    let buffered_idx = text.find("buffered-event").expect("buffered event");
    let user_idx = text.find("first-user-message").expect("first user message");
    assert!(buffered_idx < user_idx, "buffered items preserve ordering");

    let text_after_second_persist = std::fs::read_to_string(&rollout_path)?;
    assert_eq!(text_after_second_persist, text);

    recorder.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn persist_reports_filesystem_error_and_retries_buffered_items() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());
    let thread_id = ThreadId::new();
    let recorder = RolloutRecorder::new(
        &config,
        RolloutRecorderParams::new(
            thread_id,
            None,
            None,
            SessionSource::Exec,
            "test_originator".to_string(),
        ),
    )
    .await?;
    let rollout_path = recorder.rollout_path().to_path_buf();

    recorder
        .record_canonical_items(&[RolloutItem::EventMsg(serde_json::json!({
            "type": "agent_message",
            "message": "buffered-before-persist",
        }))])
        .await?;
    // Block the sessions dir by creating a FILE where the directory should go.
    let sessions_blocker_path = home.path().join("sessions");
    File::create(&sessions_blocker_path)?;

    let err = recorder
        .persist()
        .await
        .expect_err("blocked sessions directory should fail persist");
    assert_ne!(err.kind(), std::io::ErrorKind::Interrupted);
    assert!(
        !rollout_path.exists(),
        "failed persist should keep the rollout deferred"
    );

    fs::remove_file(sessions_blocker_path)?;
    recorder.flush().await?;
    let text = std::fs::read_to_string(&rollout_path)?;
    assert!(
        text.contains("buffered-before-persist"),
        "retry should preserve items buffered before the failed persist"
    );

    recorder.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn writer_state_retries_write_error_before_reporting_flush_success() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let rollout_path = home.path().join("rollout.jsonl");
    File::create(&rollout_path)?;
    let read_only_file = std::fs::OpenOptions::new().read(true).open(&rollout_path)?;
    let mut state = RolloutWriterStateForTest::new(
        Some(tokio::fs::File::from_std(read_only_file)),
        None,
        None,
        home.path().to_path_buf(),
        rollout_path.clone(),
    );
    state.add_items(vec![RolloutItem::EventMsg(serde_json::json!({
        "type": "agent_message",
        "message": "queued-after-writer-error",
    }))]);

    state.flush().await?;
    let text_after_retry = std::fs::read_to_string(&rollout_path)?;
    assert!(
        text_after_retry.contains("queued-after-writer-error"),
        "flush should retry after reopening and write buffered items"
    );
    Ok(())
}

#[tokio::test]
async fn append_rollout_item_to_path_appends_one_line() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let rollout_path = home.path().join("rollout.jsonl");
    File::create(&rollout_path)?;

    append_rollout_item_to_path(
        &rollout_path,
        &RolloutItem::EventMsg(serde_json::json!({
            "type": "agent_message",
            "message": "appended",
        })),
    )
    .await?;

    let text = std::fs::read_to_string(&rollout_path)?;
    assert_eq!(text.lines().count(), 1);
    assert!(text.contains("appended"));
    let line: RolloutLine = serde_json::from_str(text.lines().next().unwrap())?;
    assert!(matches!(line.item, RolloutItem::EventMsg(_)));
    Ok(())
}
