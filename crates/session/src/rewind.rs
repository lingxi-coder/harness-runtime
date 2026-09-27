//! `/rewind` conversation restore — truncate the current session's transcript
//! in place to a previous point (claude-code `rewindConversationTo`, the
//! destructive same-session variant).
//!
//! Keeps every JSONL line up to (not including) the target user message, so the
//! target turn and everything after it are dropped; the pre-truncation file is
//! backed up to `<uuid>.jsonl.rewind-bak` first so the truncation is itself
//! recoverable. The caller re-mounts the same session id afterwards (the
//! `/resume` re-mount seam) so the live runtime reloads the truncated history.

use std::path::Path;

use uuid::Uuid;

use crate::jsonl::session_path;

/// Truncate `session_id`'s transcript in place, dropping the turn keyed by
/// `target_message` and everything after it. Backs the file up first.
///
/// # Errors
/// Returns a message when the transcript is missing or cannot be read/written.
pub async fn rewind_conversation(
    lingxi_home: &Path,
    cwd: &str,
    session_id: Uuid,
    target_message: Uuid,
) -> Result<(), String> {
    let path = session_path(lingxi_home, cwd, &session_id.to_string());
    let content = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| format!("read transcript: {e}"))?;

    // Back up the pre-truncation transcript (recoverable) before rewriting.
    let backup = path.with_extension("jsonl.rewind-bak");
    let _ = tokio::fs::write(&backup, &content).await;

    // Keep every line up to (NOT including) the target user message line; the
    // target turn + everything after it is dropped.
    let target = target_message.to_string();
    let mut kept: Vec<&str> = Vec::new();
    for line in content.lines() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if v.get("uuid").and_then(serde_json::Value::as_str) == Some(target.as_str()) {
                break;
            }
        }
        kept.push(line);
    }
    let mut body = kept.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    tokio::fs::write(&path, body)
        .await
        .map_err(|e| format!("write truncated transcript: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("lingxi-rewind-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn truncates_to_the_target_message_and_backs_up() {
        let home = scratch("trunc");
        let cwd = "/tmp/proj";
        let sid = Uuid::new_v4();
        let t1 = "11111111-1111-1111-1111-111111111111";
        let t2 = "22222222-2222-2222-2222-222222222222";
        let line = |uuid: &str, text: &str| {
            json!({"type":"user","uuid":uuid,"sessionId":sid.to_string(),"message":{"role":"user","content":text}}).to_string()
        };
        let path = session_path(&home, cwd, &sid.to_string());
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        let body = format!(
            "{}\n{}\n{}\n",
            line(t1, "first"),
            line(t2, "second"),
            line("33333333-3333-3333-3333-333333333333", "third")
        );
        tokio::fs::write(&path, body).await.unwrap();

        // Rewind to t2 → keep only t1; t2 + t3 dropped.
        rewind_conversation(&home, cwd, sid, Uuid::parse_str(t2).unwrap())
            .await
            .expect("rewind");
        let after = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(after.lines().count(), 1);
        assert!(after.contains(t1) && !after.contains(t2));
        // The pre-truncation transcript is backed up.
        let backup = path.with_extension("jsonl.rewind-bak");
        assert!(backup.exists());
        assert_eq!(
            tokio::fs::read_to_string(&backup)
                .await
                .unwrap()
                .lines()
                .count(),
            3
        );
    }
}
