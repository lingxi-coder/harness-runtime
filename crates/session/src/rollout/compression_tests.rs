//! Compression tests — ported from codex's `rollout/src/compression_tests.rs`
//! for the cases that map to the LingXi-merged API.
//!
//! Cases that depend on codex-only surface (`search_rollout_matches`,
//! `find_thread_path_by_id_str`, the SQLite `RolloutConfig { sqlite_home, .. }`)
//! are omitted. The on-disk behavior they would exercise — transparent
//! plain/`.zst` reads, `existing_rollout_path`, `plain_rollout_path`,
//! materialization, the worker, and the run marker — is covered here.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::fs::FileTimes;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;
use std::time::SystemTime;

use pretty_assertions::assert_eq;
use tempfile::TempDir;
use uuid::Uuid;

use super::*;
use crate::rollout::initial_history::InitialHistory;
use crate::rollout::record::{
    RolloutItem, RolloutLine, SessionMeta, SessionMetaLine, SessionSource, ThreadId,
};
use crate::rollout::recorder::{
    append_rollout_item_to_path, RolloutConfig, RolloutRecorder, RolloutRecorderParams,
};

#[tokio::test]
async fn load_rollout_items_reads_compressed_rollout() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(1);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "hello compressed")?;
    compress_now(&rollout_path)?;

    let (items, loaded_thread_id, parse_errors) =
        RolloutRecorder::load_rollout_items(&rollout_path).await?;

    assert_eq!(loaded_thread_id, Some(thread_id));
    assert_eq!(parse_errors, 0);
    assert_eq!(items.len(), 2);
    // The plain file does not exist; only the compressed sibling does, and the
    // reader resolved it transparently.
    assert!(!rollout_path.exists());
    assert!(compressed_rollout_path(&rollout_path).exists());
    Ok(())
}

#[tokio::test]
async fn load_rollout_items_reads_plain_rollout() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(20);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "hello plain")?;

    let (items, loaded_thread_id, parse_errors) =
        RolloutRecorder::load_rollout_items(&rollout_path).await?;

    assert_eq!(loaded_thread_id, Some(thread_id));
    assert_eq!(parse_errors, 0);
    assert_eq!(items.len(), 2);
    assert!(rollout_path.exists());
    Ok(())
}

#[test]
fn rollout_file_from_path_normalizes_compressed_file_names() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(7);
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    let compressed_path = compressed_rollout_path(&rollout_path);

    let rollout_file = RolloutFile::from_path(compressed_path.clone()).expect("rollout file");
    assert_eq!(rollout_file.path(), compressed_path.as_path());
    assert_eq!(
        rollout_file.plain_file_name(),
        format!("rollout-2025-01-03T12-00-00-{uuid}.jsonl")
    );
    assert!(rollout_file.is_compressed());
    Ok(())
}

#[test]
fn rollout_file_from_path_hides_compressed_sibling_when_plain_exists() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(8);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "plain wins")?;

    assert_eq!(
        RolloutFile::from_path(compressed_rollout_path(&rollout_path)),
        None
    );
    Ok(())
}

#[test]
fn plain_rollout_path_strips_compressed_suffix() {
    let plain = std::path::Path::new("/x/rollout-2025-01-03T12-00-00-aaaa.jsonl");
    let compressed = std::path::Path::new("/x/rollout-2025-01-03T12-00-00-aaaa.jsonl.zst");
    assert_eq!(plain_rollout_path(compressed), plain.to_path_buf());
    assert_eq!(plain_rollout_path(plain), plain.to_path_buf());
}

#[tokio::test]
async fn existing_rollout_path_prefers_plain_over_compressed() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(21);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);

    // Nothing on disk yet.
    assert_eq!(existing_rollout_path(&rollout_path).await, None);

    // Only the compressed sibling exists -> resolves to it.
    write_rollout(&rollout_path, thread_id, "term")?;
    compress_now(&rollout_path)?;
    let compressed_path = compressed_rollout_path(&rollout_path);
    assert_eq!(
        existing_rollout_path(&rollout_path).await,
        Some(compressed_path.clone())
    );
    // Queried with the compressed path -> still resolves to the compressed file.
    assert_eq!(
        existing_rollout_path(&compressed_path).await,
        Some(compressed_path.clone())
    );

    // Once a plain file exists, plain wins even when the compressed sibling is
    // also present.
    write_rollout(&rollout_path, thread_id, "term")?;
    assert_eq!(
        existing_rollout_path(&rollout_path).await,
        Some(rollout_path.clone())
    );
    Ok(())
}

#[tokio::test]
async fn append_rollout_item_materializes_compressed_rollout() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(2);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "hello before append")?;
    compress_now(&rollout_path)?;

    append_rollout_item_to_path(
        &rollout_path,
        &RolloutItem::EventMsg(serde_json::json!({
            "type": "user_message",
            "message": "hello after append",
        })),
    )
    .await?;

    assert!(rollout_path.exists());
    assert!(!compressed_rollout_path(&rollout_path).exists());
    let (items, loaded_thread_id, parse_errors) =
        RolloutRecorder::load_rollout_items(&rollout_path).await?;
    assert_eq!(loaded_thread_id, Some(thread_id));
    assert_eq!(parse_errors, 0);
    assert_eq!(items.len(), 3);
    Ok(())
}

#[tokio::test]
async fn worker_compresses_old_active_and_archived_rollouts() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let active_uuid = Uuid::from_u128(3);
    let active_id = ThreadId::from_string(&active_uuid.to_string())?;
    let active_path = rollout_path(home.path(), "2025-01-03T12-00-00", active_uuid);
    write_rollout(&active_path, active_id, "old active")?;
    set_old_mtime(&active_path)?;

    let archived_uuid = Uuid::from_u128(4);
    let archived_id = ThreadId::from_string(&archived_uuid.to_string())?;
    let archived_path = archived_rollout_path(home.path(), "2025-01-04T12-00-00", archived_uuid);
    write_rollout(&archived_path, archived_id, "old archived")?;
    set_old_mtime(&archived_path)?;

    let fresh_uuid = Uuid::from_u128(5);
    let fresh_id = ThreadId::from_string(&fresh_uuid.to_string())?;
    let fresh_path = rollout_path(home.path(), "2025-01-05T12-00-00", fresh_uuid);
    write_rollout(&fresh_path, fresh_id, "fresh active")?;

    let stale_temp = active_path.with_file_name("rollout-stale.jsonl.zst.tmp");
    fs::write(&stale_temp, "stale temp")?;
    set_old_mtime(&stale_temp)?;

    let fresh_temp = active_path.with_file_name("rollout-fresh.jsonl.zst.tmp");
    fs::write(&fresh_temp, "fresh temp")?;

    worker::run(home.path().to_path_buf()).await?;

    assert!(!active_path.exists());
    assert!(compressed_rollout_path(&active_path).exists());
    assert!(!archived_path.exists());
    assert!(compressed_rollout_path(&archived_path).exists());
    assert!(fresh_path.exists());
    assert!(!compressed_rollout_path(&fresh_path).exists());
    assert!(!stale_temp.exists());
    assert!(fresh_temp.exists());
    assert!(home
        .path()
        .join(".tmp")
        .join("rollout-compression.lock")
        .exists());
    Ok(())
}

#[tokio::test]
async fn resume_materializes_compressed_rollout_path() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let config = RolloutConfig {
        codex_home: home.path().to_path_buf(),
        cwd: home.path().to_path_buf(),
        model_provider_id: "test-provider".to_string(),
        generate_memories: true,
    };
    let uuid = Uuid::from_u128(3);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "hello before resume")?;
    compress_now(&rollout_path)?;
    let compressed_path = compressed_rollout_path(&rollout_path);

    let InitialHistory::Resumed(history) =
        RolloutRecorder::get_rollout_history(compressed_path.as_path()).await?
    else {
        panic!("expected compressed rollout to load as resumed history");
    };
    assert_eq!(history.rollout_path, Some(rollout_path.clone()));

    let recorder = RolloutRecorder::new(
        &config,
        RolloutRecorderParams::resume(compressed_path.clone()),
    )
    .await?;

    assert_eq!(recorder.rollout_path(), rollout_path.as_path());
    assert!(rollout_path.exists());
    assert!(!compressed_path.exists());
    recorder
        .record_canonical_items(&[RolloutItem::EventMsg(serde_json::json!({
            "type": "user_message",
            "message": "hello after resume",
        }))])
        .await?;
    recorder.flush().await?;
    recorder.shutdown().await?;

    let (items, loaded_thread_id, parse_errors) =
        RolloutRecorder::load_rollout_items(&rollout_path).await?;
    assert_eq!(loaded_thread_id, Some(thread_id));
    assert_eq!(parse_errors, 0);
    assert_eq!(items.len(), 3);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn compression_preserves_rollout_permissions() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(6);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = archived_rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "restricted transcript")?;
    fs::set_permissions(&rollout_path, fs::Permissions::from_mode(0o600))?;
    set_old_mtime(&rollout_path)?;

    worker::run(home.path().to_path_buf()).await?;

    let compressed_path = compressed_rollout_path(&rollout_path);
    assert!(!rollout_path.exists());
    assert_eq!(
        fs::metadata(&compressed_path)?.permissions().mode() & 0o777,
        0o600
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn append_materialization_preserves_compressed_rollout_permissions() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(6);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "restricted transcript")?;
    compress_now(&rollout_path)?;
    let compressed_path = compressed_rollout_path(&rollout_path);
    fs::set_permissions(&compressed_path, fs::Permissions::from_mode(0o600))?;

    append_rollout_item_to_path(
        &rollout_path,
        &RolloutItem::EventMsg(serde_json::json!({
            "type": "user_message",
            "message": "materialize restricted transcript",
        })),
    )
    .await?;

    assert!(rollout_path.exists());
    assert!(!compressed_path.exists());
    assert_eq!(
        fs::metadata(&rollout_path)?.permissions().mode() & 0o777,
        0o600
    );
    Ok(())
}

#[test]
fn persist_temp_file_noclobber_installs_completed_temp() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let temp_path = home.path().join("rollout.jsonl.tmp");
    let destination = home.path().join("rollout.jsonl");
    fs::write(&temp_path, "completed rollout")?;

    persist_temp_file_noclobber(&temp_path, &destination)?;

    assert!(!temp_path.exists());
    assert_eq!(fs::read_to_string(destination)?, "completed rollout");
    Ok(())
}

#[test]
fn persist_temp_file_noclobber_does_not_replace_existing_destination() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let temp_path = home.path().join("rollout.jsonl.tmp");
    let destination = home.path().join("rollout.jsonl");
    fs::write(&temp_path, "candidate rollout")?;
    fs::write(&destination, "existing rollout")?;

    persist_temp_file_noclobber(&temp_path, &destination)?;

    assert!(!temp_path.exists());
    assert_eq!(fs::read_to_string(destination)?, "existing rollout");
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn compression_preserves_read_only_rollout_permissions() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(7);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = archived_rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "read-only transcript")?;
    set_old_mtime(&rollout_path)?;
    fs::set_permissions(&rollout_path, fs::Permissions::from_mode(0o400))?;
    let source_modified = fs::metadata(&rollout_path)?.modified()?;

    worker::run(home.path().to_path_buf()).await?;

    let compressed_path = compressed_rollout_path(&rollout_path);
    let compressed_metadata = fs::metadata(&compressed_path)?;
    assert!(!rollout_path.exists());
    assert_eq!(compressed_metadata.permissions().mode() & 0o777, 0o400);
    assert_eq!(compressed_metadata.modified()?, source_modified);
    Ok(())
}

#[tokio::test]
async fn worker_skips_existing_compressed_archived_rollouts() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(10);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = archived_rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "already compressed")?;
    compress_now(&rollout_path)?;
    let compressed_path = compressed_rollout_path(&rollout_path);
    set_old_mtime(&compressed_path)?;

    worker::run(home.path().to_path_buf()).await?;

    assert!(!rollout_path.exists());
    assert!(compressed_path.exists());
    let (items, loaded_thread_id, parse_errors) =
        RolloutRecorder::load_rollout_items(&rollout_path).await?;
    assert_eq!(loaded_thread_id, Some(thread_id));
    assert_eq!(parse_errors, 0);
    assert_eq!(items.len(), 2);
    Ok(())
}

#[tokio::test]
async fn worker_skips_when_fresh_run_marker_exists() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(11);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = archived_rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "throttled worker")?;
    set_old_mtime(&rollout_path)?;
    let marker_dir = home.path().join(".tmp");
    fs::create_dir_all(marker_dir.as_path())?;
    fs::write(marker_dir.join("rollout-compression.lock"), "recent run")?;

    worker::run(home.path().to_path_buf()).await?;

    assert!(rollout_path.exists());
    assert!(!compressed_rollout_path(&rollout_path).exists());
    Ok(())
}

#[test]
fn run_marker_is_removed_unless_persisted() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let marker_path = home.path().join(".tmp").join("rollout-compression.lock");

    {
        let marker = worker::CompressionRunMarker::try_claim(home.path())?;
        assert!(marker.is_some());
    }
    assert!(!marker_path.exists());

    let marker = worker::CompressionRunMarker::try_claim(home.path())?;
    let Some(marker) = marker else {
        panic!("expected run marker claim");
    };
    marker.persist();
    assert!(marker_path.exists());
    assert!(worker::CompressionRunMarker::try_claim(home.path())?.is_none());
    Ok(())
}

fn rollout_path(home: &std::path::Path, ts: &str, uuid: Uuid) -> std::path::PathBuf {
    home.join("sessions/2025/01/03")
        .join(format!("rollout-{ts}-{uuid}.jsonl"))
}

fn archived_rollout_path(home: &std::path::Path, ts: &str, uuid: Uuid) -> std::path::PathBuf {
    home.join("archived_sessions")
        .join(format!("rollout-{ts}-{uuid}.jsonl"))
}

fn write_rollout(path: &std::path::Path, thread_id: ThreadId, message: &str) -> anyhow::Result<()> {
    let parent = path.parent().expect("rollout path should have parent");
    fs::create_dir_all(parent)?;
    let session_meta_line = SessionMetaLine {
        meta: SessionMeta {
            session_id: thread_id,
            id: thread_id,
            forked_from_id: None,
            parent_thread_id: None,
            timestamp: "2025-01-03T12:00:00Z".to_string(),
            cwd: parent.to_path_buf(),
            originator: "test".to_string(),
            cli_version: "test".to_string(),
            source: SessionSource::Cli,
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
            model_provider: None,
            memory_mode: None,
            context_window: None,
            extra: serde_json::Map::new(),
        },
        git: None,
    };
    let lines = [
        RolloutLine {
            timestamp: "2025-01-03T12:00:00Z".to_string(),
            item: RolloutItem::SessionMeta(session_meta_line),
        },
        RolloutLine {
            timestamp: "2025-01-03T12:00:01Z".to_string(),
            item: RolloutItem::EventMsg(serde_json::json!({
                "type": "user_message",
                "message": message,
            })),
        },
    ];
    let jsonl = lines
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()?
        .join("\n");
    fs::write(path, format!("{jsonl}\n"))?;
    Ok(())
}

fn compress_now(path: &std::path::Path) -> anyhow::Result<()> {
    let compressed_path = compressed_rollout_path(path);
    let input = fs::File::open(path)?;
    let output = fs::File::create(compressed_path)?;
    let mut encoder = zstd::stream::write::Encoder::new(output, COMPRESSION_LEVEL)?;
    let mut input = std::io::BufReader::new(input);
    std::io::copy(&mut input, &mut encoder)?;
    encoder.finish()?;
    fs::remove_file(path)?;
    Ok(())
}

fn set_old_mtime(path: &std::path::Path) -> anyhow::Result<()> {
    let old = SystemTime::now()
        .checked_sub(Duration::from_secs(8 * 24 * 60 * 60))
        .expect("old timestamp should be representable");
    let times = FileTimes::new().set_modified(old);
    fs::OpenOptions::new()
        .write(true)
        .open(path)?
        .set_times(times)?;
    Ok(())
}
