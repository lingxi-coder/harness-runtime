//! Append-only thread-name sidecar — faithful port of codex's
//! `session_index.rs`.
//!
//! Thread names live in `session_index.jsonl` under `codex_home`. Updates are
//! append-only; the most recent entry for an id wins. Resolution scans the
//! file from the end so the newest name is found first without rewriting.

use crate::rollout::record::ThreadId;
use chrono::SecondsFormat;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use tokio::io::AsyncBufReadExt;

const SESSION_INDEX_FILE: &str = "session_index.jsonl";
const READ_CHUNK_SIZE: usize = 8192;
static SESSION_INDEX_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// One row in `session_index.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionIndexEntry {
    pub id: ThreadId,
    pub thread_name: String,
    pub updated_at: String,
}

/// Append a thread-name update. The most recent entry wins when resolving.
pub async fn append_thread_name(
    codex_home: &Path,
    thread_id: ThreadId,
    name: &str,
) -> std::io::Result<()> {
    let updated_at = chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    let entry = SessionIndexEntry {
        id: thread_id,
        thread_name: name.to_string(),
        updated_at,
    };
    append_session_index_entry(codex_home, &entry).await
}

/// Append a raw entry to `session_index.jsonl`.
pub async fn append_session_index_entry(
    codex_home: &Path,
    entry: &SessionIndexEntry,
) -> std::io::Result<()> {
    let _guard = SESSION_INDEX_LOCK
        .lock()
        .map_err(|err| std::io::Error::other(err.to_string()))?;
    let path = session_index_path(codex_home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    let mut line = serde_json::to_string(entry).map_err(std::io::Error::other)?;
    line.push('\n');
    file.write_all(line.as_bytes())?;
    file.flush()?;
    Ok(())
}

/// Remove all recorded names for a thread from the index.
pub async fn remove_thread_name_entries(
    codex_home: &Path,
    thread_id: ThreadId,
) -> std::io::Result<()> {
    let _guard = SESSION_INDEX_LOCK
        .lock()
        .map_err(|err| std::io::Error::other(err.to_string()))?;
    let path = session_index_path(codex_home);
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    let mut removed = false;
    let mut remaining = String::with_capacity(contents.len());
    for line in contents.lines() {
        let should_remove = serde_json::from_str::<SessionIndexEntry>(line.trim())
            .is_ok_and(|entry| entry.id == thread_id);
        if should_remove {
            removed = true;
        } else {
            remaining.push_str(line);
            remaining.push('\n');
        }
    }
    if !removed {
        return Ok(());
    }
    let temp_path = path.with_extension("jsonl.tmp");
    std::fs::write(&temp_path, remaining)?;
    std::fs::rename(temp_path, path)
}

/// Find the latest thread name for a thread id, if any.
pub async fn find_thread_name_by_id(
    codex_home: &Path,
    thread_id: &ThreadId,
) -> std::io::Result<Option<String>> {
    let path = session_index_path(codex_home);
    if !path.exists() {
        return Ok(None);
    }
    let id = *thread_id;
    let entry = tokio::task::spawn_blocking(move || scan_index_from_end_by_id(&path, &id))
        .await
        .map_err(std::io::Error::other)??;
    Ok(entry.map(|entry| entry.thread_name))
}

/// Find the latest thread names for a batch of thread ids.
pub async fn find_thread_names_by_ids(
    codex_home: &Path,
    thread_ids: &HashSet<ThreadId>,
) -> std::io::Result<HashMap<ThreadId, String>> {
    let path = session_index_path(codex_home);
    if thread_ids.is_empty() || !path.exists() {
        return Ok(HashMap::new());
    }

    let file = tokio::fs::File::open(&path).await?;
    let reader = tokio::io::BufReader::new(file);
    let mut lines = reader.lines();
    let mut names = HashMap::with_capacity(thread_ids.len());

    while let Some(line) = lines.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<SessionIndexEntry>(trimmed) else {
            continue;
        };
        let name = entry.thread_name.trim();
        // Scanning forward, the LAST entry for an id wins — overwrite on hit.
        if !name.is_empty() && thread_ids.contains(&entry.id) {
            names.insert(entry.id, name.to_string());
        }
    }

    Ok(names)
}

fn session_index_path(codex_home: &Path) -> PathBuf {
    codex_home.join(SESSION_INDEX_FILE)
}

fn scan_index_from_end_by_id(
    path: &Path,
    thread_id: &ThreadId,
) -> std::io::Result<Option<SessionIndexEntry>> {
    scan_index_from_end(path, |entry| entry.id == *thread_id)
}

fn scan_index_from_end<F>(
    path: &Path,
    mut predicate: F,
) -> std::io::Result<Option<SessionIndexEntry>>
where
    F: FnMut(&SessionIndexEntry) -> bool,
{
    scan_index_from_end_for_each(path, |entry| {
        if predicate(entry) {
            return Ok(Some(entry.clone()));
        }
        Ok(None)
    })
}

fn scan_index_from_end_for_each<F>(
    path: &Path,
    mut visit_entry: F,
) -> std::io::Result<Option<SessionIndexEntry>>
where
    F: FnMut(&SessionIndexEntry) -> std::io::Result<Option<SessionIndexEntry>>,
{
    let mut file = File::open(path)?;
    let mut remaining = file.metadata()?.len();
    let mut line_rev: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; READ_CHUNK_SIZE];

    while remaining > 0 {
        let read_size = usize::try_from(remaining.min(READ_CHUNK_SIZE as u64))
            .map_err(std::io::Error::other)?;
        remaining -= read_size as u64;
        file.seek(SeekFrom::Start(remaining))?;
        file.read_exact(&mut buf[..read_size])?;

        for &byte in buf[..read_size].iter().rev() {
            if byte == b'\n' {
                if let Some(entry) = parse_line_from_rev(&mut line_rev, &mut visit_entry)? {
                    return Ok(Some(entry));
                }
                continue;
            }
            line_rev.push(byte);
        }
    }

    if let Some(entry) = parse_line_from_rev(&mut line_rev, &mut visit_entry)? {
        return Ok(Some(entry));
    }

    Ok(None)
}

fn parse_line_from_rev<F>(
    line_rev: &mut Vec<u8>,
    visit_entry: &mut F,
) -> std::io::Result<Option<SessionIndexEntry>>
where
    F: FnMut(&SessionIndexEntry) -> std::io::Result<Option<SessionIndexEntry>>,
{
    if line_rev.is_empty() {
        return Ok(None);
    }
    line_rev.reverse();
    let line = std::mem::take(line_rev);
    let Ok(mut line) = String::from_utf8(line) else {
        return Ok(None);
    };
    if line.ends_with('\r') {
        line.pop();
    }
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let Ok(entry) = serde_json::from_str::<SessionIndexEntry>(trimmed) else {
        return Ok(None);
    };
    visit_entry(&entry)
}
