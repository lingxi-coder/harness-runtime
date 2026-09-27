//! Session agent-color persistence — 1:1 port of
//! `claude-code/src/utils/sessionStorage.ts:2838-2854` (`saveAgentColor`).
//!
//! claude-code persists the `/color` choice by appending a lightweight
//! metadata entry to the session transcript JSONL:
//!
//! ```json
//! {"type":"agent-color","agentColor":"cyan","sessionId":"<uuid>"}
//! ```
//!
//! It is NOT a full conversation [`crate::jsonl::JsonlMessage`] (no `uuid` /
//! `parentUuid` / `cwd` / `timestamp` / `message`) — it is a sidecar metadata
//! record the resume loader (`agentColors.set(entry.sessionId, …)`) scans for.
//! The reset path persists the literal `"default"` sentinel (not an empty
//! string) so truthiness guards on resume re-apply the reset across restarts.
//!
//! The append mechanism mirrors [`crate::jsonl::JsonlWriter::append`]: one
//! `serde_json::to_string` line terminated with a single `\n`, written through
//! the [`FileSystem`] trait (parent dir created on first write).

use platform_api::{FileSystem, FsError};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;

/// Build the byte-locked `agent-color` JSONL entry value for `session_id` /
/// `color`. Pure (no I/O) so the wire shape is unit-testable in isolation —
/// `claude-code/src/utils/sessionStorage.ts:2844-2848`.
#[must_use]
pub fn agent_color_entry(session_id: &str, color: &str) -> Value {
    json!({
        "type": "agent-color",
        "agentColor": color,
        "sessionId": session_id,
    })
}

/// Append an `agent-color` metadata entry to the session transcript at `path`.
///
/// Mirrors [`crate::jsonl::JsonlWriter::append`]: serialize with no whitespace,
/// terminate with a single `\n`, create the parent directory on first write.
/// `color` is the agent-color name (or the `"default"` reset sentinel). 1:1
/// with claude-code `saveAgentColor` → `appendEntryToFile`.
///
/// # Errors
///
/// Returns [`FsError`] if the parent directory cannot be created or the append
/// fails.
pub async fn save_agent_color(
    fs: &Arc<dyn FileSystem>,
    path: &Path,
    session_id: &str,
    color: &str,
) -> Result<(), FsError> {
    let entry = agent_color_entry(session_id, color);
    // `to_string` on a `serde_json::Value` is infallible for finite data; map
    // the (unreachable) error into `FsError` rather than panic.
    let line = serde_json::to_string(&entry).map_err(|e| FsError::Io(e.to_string()))?;
    let path_str = path
        .to_str()
        .ok_or_else(|| FsError::Io("non-UTF-8 path".into()))?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent).map_err(|e| FsError::Io(e.to_string()))?;
        }
    }
    let mut payload = String::with_capacity(line.len() + 1);
    payload.push_str(&line);
    payload.push('\n');
    fs.append_file(path_str, &payload).await
}

/// Scan a transcript file for the LAST `agent-color` entry and return its
/// `agentColor`. Mirrors the resume loader's last-writer-wins fold
/// (`agentColors.set` overwriting on each later entry). Returns `None` when no
/// `agent-color` entry is present or the file is unreadable. Test/round-trip
/// helper for [`save_agent_color`]; not on the live resume path (the engine
/// owns that).
#[must_use]
pub fn last_agent_color(transcript: &str) -> Option<String> {
    let mut found: Option<String> = None;
    for line in transcript.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(trimmed) {
            if map.get("type").and_then(Value::as_str) == Some("agent-color") {
                if let Some(c) = map.get("agentColor").and_then(Value::as_str) {
                    found = Some(c.to_string());
                }
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use platform_posix::fs::PosixFileSystem;
    use tempfile::tempdir;

    #[test]
    fn entry_shape_is_byte_locked() {
        let v = agent_color_entry("11111111-2222-3333-4444-555555555555", "cyan");
        // Field set + values mirror claude-code saveAgentColor exactly.
        assert_eq!(v["type"], "agent-color");
        assert_eq!(v["agentColor"], "cyan");
        assert_eq!(v["sessionId"], "11111111-2222-3333-4444-555555555555");
        // No stray fields.
        let obj = v.as_object().expect("object");
        assert_eq!(obj.len(), 3);
    }

    #[test]
    fn reset_persists_default_sentinel_not_empty() {
        let v = agent_color_entry("sid", "default");
        assert_eq!(v["agentColor"], "default");
    }

    #[test]
    fn last_agent_color_takes_last_writer_wins() {
        let transcript = concat!(
            "{\"type\":\"agent-color\",\"agentColor\":\"red\",\"sessionId\":\"s\"}\n",
            "{\"type\":\"user\",\"uuid\":\"u\"}\n",
            "{\"type\":\"agent-color\",\"agentColor\":\"teal\",\"sessionId\":\"s\"}\n",
        );
        assert_eq!(last_agent_color(transcript).as_deref(), Some("teal"));
    }

    #[test]
    fn last_agent_color_none_when_absent_or_malformed() {
        assert_eq!(last_agent_color(""), None);
        assert_eq!(last_agent_color("{\"type\":\"user\"}\n"), None);
        // Malformed JSON line is skipped, not panicked on.
        assert_eq!(last_agent_color("{not json\n"), None);
    }

    #[tokio::test]
    async fn save_then_read_round_trips_through_disk() {
        let dir = tempdir().expect("tempdir");
        let path = dir
            .path()
            .join("projects")
            .join("proj")
            .join("11111111-2222-3333-4444-555555555555.jsonl");
        let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));

        save_agent_color(&fs, &path, "11111111-2222-3333-4444-555555555555", "cyan")
            .await
            .expect("save cyan");
        // A later write wins (set then reset to default).
        save_agent_color(
            &fs,
            &path,
            "11111111-2222-3333-4444-555555555555",
            "default",
        )
        .await
        .expect("save default");

        let raw = std::fs::read_to_string(&path).expect("file written");
        // Two lines, each terminated by exactly one LF (no extra whitespace).
        let lines: Vec<&str> = raw.split('\n').collect();
        assert_eq!(lines.len(), 3, "2 entries + trailing empty: {lines:?}");
        assert_eq!(lines[2], "");
        assert!(!raw.contains("\r\n"), "LF only");
        // Last-writer-wins fold recovers the reset sentinel.
        assert_eq!(last_agent_color(&raw).as_deref(), Some("default"));
    }
}
