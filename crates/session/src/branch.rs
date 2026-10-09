//! `/branch` — create a fork of the current conversation at this point.
//!
//! 1:1 port of `claude-code/src/commands/branch/branch.ts`. Copies the live
//! session's transcript into a NEW `<uuid>.jsonl` under the same project dir,
//! rewriting `sessionId`, rechaining `parentUuid`, clearing `isSidechain`, and
//! stamping each entry with a `forkedFrom { sessionId, messageUuid }`
//! back-reference. Saves a `"<base> (Branch)"` custom title (collision-numbered),
//! then hands the new session id back so the CLI can switch INTO the branch via
//! the proven in-process re-mount seam (`mount_resumed_tui`).
//!
//! DISTINCT from `/fork`, which spawns a detached background agent; `/branch`
//! creates a sibling session file and moves the user into it. Mirrors claude's
//! `createFork` + `getUniqueForkName` + `deriveFirstPrompt`, including the
//! session-level content-replacement carry-over the source cold-load state
//! exposes.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use lingxi_core::host::FileSystem;
use serde_json::json;
use uuid::Uuid;

use crate::jsonl::exact_json::{
    native_message_bytes, parse_exact_json, to_vec_with_overrides, Utf16Overrides,
};
use crate::jsonl::re_append::iso_now;
#[cfg(test)]
use crate::jsonl::JsonlMessage;
use crate::jsonl::{derive_fork_name, list_recent_sessions, route_lines, session_path};

/// Outcome of a successful [`create_branch`].
#[derive(Debug, Clone)]
pub struct BranchResult {
    /// The freshly-minted branch session id (the switch target).
    pub new_session_id: Uuid,
    /// The session that was branched (for the "resume the original" hint).
    pub source_session_id: Uuid,
    /// The effective `"… (Branch[ N])"` title saved for the branch.
    pub title: String,
    /// Count of conversation messages copied into the branch.
    pub message_count: usize,
}

/// Why a branch could not be created.
#[derive(Debug)]
pub enum BranchError {
    /// The source transcript is missing or empty (`"No conversation to branch"`).
    NoConversation,
    /// The source has no main-conversation messages (`"No messages to branch"`).
    NoMessages,
    /// A filesystem read/write failed.
    Io(std::io::Error),
}

impl std::fmt::Display for BranchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoConversation => write!(f, "No conversation to branch"),
            Self::NoMessages => write!(f, "No messages to branch"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}
impl std::error::Error for BranchError {}

/// Create a branch of `source_session_id` under the same project dir.
///
/// `custom_title` is the trimmed `/branch [name]` argument (empty / `None` ⇒
/// derive the base name from the first user message). Returns the new session
/// id + saved title so the caller can switch into the branch.
///
/// # Errors
/// [`BranchError::NoConversation`] when the source file is missing/empty,
/// [`BranchError::NoMessages`] when it holds no chain messages, and
/// [`BranchError::Io`] on any read/write failure.
pub async fn create_branch(
    lingxi_home: &Path,
    cwd: &str,
    source_session_id: Uuid,
    custom_title: Option<&str>,
    fs: Arc<dyn FileSystem>,
) -> Result<BranchResult, BranchError> {
    create_branch_in(lingxi_home, cwd, cwd, source_session_id, custom_title, fs).await
}

/// Fork `source_session_id` (living under `source_cwd`'s project dir) into a
/// NEW session rooted at `target_cwd` — the v3 "create an app from this
/// chat" seam: the conversation history follows the user into the app's
/// workspace-scoped catalog while the source session stays untouched.
///
/// Differences from the same-cwd [`create_branch`]:
/// - every copied entry's `cwd` field is rewritten to `target_cwd` (the
///   fork lives there now; a resume must not leak the old root),
/// - the TARGET project dir is created if missing (the same-cwd path can
///   assume it exists because it just read the source from it; a fresh app
///   workspace has no catalog yet),
/// - title-collision numbering runs against the TARGET catalog.
///
/// Everything else is identical: main-chain messages and Claude's active fork
/// sidecars are copied, ids/parent chains are rewritten, and each entry carries
/// a `forkedFrom` back-reference. A source `relocated` record is intentionally
/// not copied across cwd roots because it would override `target_cwd` on resume.
///
/// # Errors
/// Same surface as [`create_branch`].
pub async fn create_branch_to_cwd(
    lingxi_home: &Path,
    source_cwd: &str,
    target_cwd: &str,
    source_session_id: Uuid,
    custom_title: Option<&str>,
    fs: Arc<dyn FileSystem>,
) -> Result<BranchResult, BranchError> {
    create_branch_in(
        lingxi_home,
        source_cwd,
        target_cwd,
        source_session_id,
        custom_title,
        fs,
    )
    .await
}

async fn create_branch_in(
    lingxi_home: &Path,
    source_cwd: &str,
    target_cwd: &str,
    source_session_id: Uuid,
    custom_title: Option<&str>,
    fs: Arc<dyn FileSystem>,
) -> Result<BranchResult, BranchError> {
    let cwd = source_cwd;
    // 1. Read the current transcript. Missing / empty ⇒ nothing to branch.
    let src_path = session_path(lingxi_home, cwd, &source_session_id.to_string());
    let content = match tokio::fs::read_to_string(&src_path).await {
        Ok(c) if !c.trim().is_empty() => c,
        Ok(_) => return Err(BranchError::NoConversation),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(BranchError::NoConversation);
        }
        Err(e) => return Err(BranchError::Io(e)),
    };

    // 2. Route to the linear main-conversation chain. `route_lines` yields only
    //    transcript chain participants (sidechains + metadata side-maps are
    //    filtered out), matching claude's `isTranscriptMessage && !isSidechain`.
    let routed = route_lines(&content);
    if routed.messages_in_order.is_empty() {
        return Err(BranchError::NoMessages);
    }

    // 3. Base name = provided title, else the first-prompt derivation
    //    (`deriveFirstPrompt`). Then append the collision-numbered "(Branch)".
    let base = match custom_title {
        Some(t) if !t.trim().is_empty() => t.trim().to_string(),
        _ => derive_fork_name(&routed.messages_in_order),
    };
    // Collision numbering runs against the catalog the fork will LIVE in.
    let title = unique_fork_name(lingxi_home, target_cwd, &base, &fs).await;

    // 4. Rewrite each entry: new sessionId, rechained parentUuid, cleared
    //    sidechain flag, plus a `forkedFrom` back-reference to the origin.
    let new_session_id = Uuid::new_v4();
    let new_sid = new_session_id.to_string();
    let src_sid = source_session_id.to_string();
    let carried_replacements = carried_content_replacements(&content, src_sid.as_str());
    let mut parent: Option<String> = None;
    let message_count = routed.messages_in_order.len();
    let carries_content_replacements = !carried_replacements.values.is_empty();
    let carries_relocated =
        target_cwd == source_cwd && routed.relocated_cwds.contains_key(src_sid.as_str());
    let carries_atis = routed.atis_latches.contains_key(src_sid.as_str());
    let mut lines: Vec<String> = Vec::with_capacity(
        message_count
            + usize::from(routed.session_history_suppressed)
            + usize::from(carries_content_replacements)
            + usize::from(carries_relocated)
            + usize::from(carries_atis)
            + 1,
    );

    // `createFork` stamps inherited history suppression BEFORE transcript
    // messages. The source scan writes this once on the first suppression
    // record, using the new session id and the exact `fork_inherit` cause.
    if routed.session_history_suppressed {
        lines.push(
            serde_json::to_string(&json!({
                "type": "history-suppression",
                "sessionId": new_sid,
                "cause": "fork_inherit",
                "ts": iso_now(),
            }))
            .expect("history-suppression serializes"),
        );
    }
    for mut entry in routed.messages_in_order {
        let original_uuid = entry.uuid.clone();
        entry.session_id = new_sid.clone();
        entry.parent_uuid = parent.clone();
        entry.is_sidechain = false;
        // A cross-cwd fork lives under the target root from now on; keeping
        // the source cwd would make a later resume re-anchor file context on
        // a directory the session no longer belongs to.
        if target_cwd != source_cwd {
            entry.cwd = target_cwd.to_string();
        }
        entry.extra.insert(
            "forkedFrom".to_string(),
            json!({ "sessionId": src_sid, "messageUuid": original_uuid }),
        );
        // JsonlMessage's hand-written Serialize emits claude's per-kind key order
        // and never fails for a well-formed message.
        lines.push(
            String::from_utf8(
                native_message_bytes(&entry)
                    .map_err(|error| BranchError::Io(std::io::Error::other(error.to_string())))?,
            )
            .expect("native JSON encoder emits UTF-8"),
        );
        parent = Some(original_uuid);
    }

    if !carried_replacements.values.is_empty() {
        let value = json!({
                "type": "content-replacement",
                "sessionId": new_sid,
                "replacements": carried_replacements.values,
        });
        lines.push(
            String::from_utf8(
                to_vec_with_overrides(&value, &carried_replacements.utf16_overrides)
                    .map_err(|error| BranchError::Io(std::io::Error::other(error.to_string())))?,
            )
            .expect("native JSON encoder emits UTF-8"),
        );
    }

    if target_cwd == source_cwd {
        if let Some(relocated_cwd) = routed.relocated_cwds.get(src_sid.as_str()) {
            lines.push(
                serde_json::to_string(&json!({
                    "type": "relocated",
                    "sessionId": new_sid,
                    "relocatedCwd": relocated_cwd,
                }))
                .expect("relocated serializes"),
            );
        }
    }

    if let Some(atis) = routed.atis_latches.get(src_sid.as_str()) {
        lines.push(
            serde_json::to_string(&json!({
                "type": "atis-latch",
                "sessionId": new_sid,
                "atis": atis,
            }))
            .expect("atis-latch serializes"),
        );
    }

    // 5. Append the custom-title side-map entry so /resume + /status show
    //    "<base> (Branch)" (claude `saveCustomTitle`). Shape matches the
    //    `custom-title` branch of `route_lines`: {type,sessionId,customTitle}.
    lines.push(
        serde_json::to_string(&json!({
            "type": "custom-title",
            "sessionId": new_sid,
            "customTitle": title,
        }))
        .expect("custom-title serializes"),
    );
    // 6. Write the branch file (0o600 like claude). Same-cwd: the project
    //    dir already exists (it holds the source we just read). Cross-cwd:
    //    a fresh app workspace has no catalog dir yet — create it.
    let dst_path = session_path(lingxi_home, target_cwd, &new_sid);
    if target_cwd != source_cwd {
        if let Some(parent_dir) = dst_path.parent() {
            tokio::fs::create_dir_all(parent_dir)
                .await
                .map_err(BranchError::Io)?;
        }
    }
    let body = format!("{}\n", lines.join("\n"));
    tokio::fs::write(&dst_path, body.as_bytes())
        .await
        .map_err(BranchError::Io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = tokio::fs::set_permissions(&dst_path, std::fs::Permissions::from_mode(0o600)).await;
    }

    // Inline visualizations are owned by the root session. The branch gets
    // its own copy (same ids) so widget state diverges independently from
    // here on. A failed copy degrades the branch's widgets to "unavailable"
    // cards; it must not lose the branch itself.
    let visualizations = visualization::VisualizationStore::new(fs, lingxi_home.to_path_buf());
    if let Err(error) = visualizations
        .copy_session(source_session_id, new_session_id)
        .await
    {
        tracing::warn!(%error, "branch: inline visualizations were not copied");
    }

    Ok(BranchResult {
        new_session_id,
        source_session_id,
        title,
        message_count,
    })
}

#[derive(Default)]
struct CarriedReplacements {
    values: Vec<serde_json::Value>,
    utf16_overrides: Utf16Overrides,
}

fn carried_content_replacements(content: &str, src_sid: &str) -> CarriedReplacements {
    let mut carried = CarriedReplacements::default();
    for line in content.lines() {
        let Ok(exact) = parse_exact_json(line) else {
            continue;
        };
        let value = exact.value;
        if value.get("type").and_then(serde_json::Value::as_str) != Some("content-replacement") {
            continue;
        }
        let session_id = value.get("sessionId").and_then(serde_json::Value::as_str);
        let has_agent_id = value
            .get("agentId")
            .and_then(serde_json::Value::as_str)
            .is_some();
        let belongs_to_source = session_id == Some(src_sid)
            || (has_agent_id && (session_id.is_none() || session_id == Some(src_sid)));
        if !belongs_to_source {
            continue;
        }
        if let Some(replacements) = value
            .get("replacements")
            .and_then(serde_json::Value::as_array)
        {
            let offset = carried.values.len();
            for (pointer, units) in exact.utf16_overrides {
                let Some(relative) = pointer.strip_prefix("/replacements/") else {
                    continue;
                };
                let (index, tail) = relative
                    .split_once('/')
                    .map_or((relative, ""), |(index, tail)| (index, tail));
                let Ok(index) = index.parse::<usize>() else {
                    continue;
                };
                if index < replacements.len() {
                    let suffix = if tail.is_empty() {
                        String::new()
                    } else {
                        format!("/{tail}")
                    };
                    carried
                        .utf16_overrides
                        .insert(format!("/replacements/{}{suffix}", offset + index), units);
                }
            }
            carried.values.extend(replacements.iter().cloned());
        }
    }
    carried
}

/// `getUniqueForkName` (`branch.ts:179`): `"<base> (Branch)"`, or
/// `"<base> (Branch N)"` when the plain form (or a lower N) already exists.
/// Best-effort — a session-scan failure degrades to the un-numbered name rather
/// than blocking the branch.
async fn unique_fork_name(
    lingxi_home: &Path,
    cwd: &str,
    base: &str,
    fs: &Arc<dyn FileSystem>,
) -> String {
    let plain = format!("{base} (Branch)");
    let titles: HashSet<String> = list_recent_sessions(lingxi_home, cwd, usize::MAX, fs.clone())
        .await
        .map(|rows| rows.into_iter().map(|s| s.title).collect())
        .unwrap_or_default();
    if !titles.contains(&plain) {
        return plain;
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base} (Branch {n})");
        if !titles.contains(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use platform_posix::PosixFileSystem;

    /// A unique scratch dir under the OS temp root (session has no `tempfile`
    /// dev-dep; the writer tests use the same pattern).
    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("lingxi-branch-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    fn user_line(uuid: &str, sid: &str, parent: Option<&str>, text: &str) -> String {
        json!({
            "type": "user",
            "uuid": uuid,
            "parentUuid": parent,
            "sessionId": sid,
            "timestamp": "2026-05-25T12:00:00.000Z",
            "cwd": "/tmp/proj",
            "version": "0.0.0",
            "message": { "role": "user", "content": text },
        })
        .to_string()
    }

    fn content_replacement_line(sid: &str, path: &str, agent_id: Option<&str>) -> String {
        let mut value = json!({
            "type": "content-replacement",
            "sessionId": sid,
            "replacements": [{ "path": path, "text": format!("replacement-{path}") }],
        });
        if let Some(agent_id) = agent_id {
            value["agentId"] = json!(agent_id);
        }
        value.to_string()
    }

    fn marble_line(kind: &str, sid: &str, field: &str, payload: &str) -> String {
        let mut value = json!({
            "type": kind,
            "sessionId": sid,
        });
        value[field] = json!(payload);
        value.to_string()
    }

    #[tokio::test]
    async fn branch_copies_inline_visualizations_under_the_new_root_session() {
        let home = tempfile::tempdir().unwrap();
        let cwd = "/viz-fixture";
        let src = Uuid::new_v4();
        let source = session_path(home.path(), cwd, &src.to_string());
        tokio::fs::create_dir_all(source.parent().unwrap())
            .await
            .unwrap();
        let src_sid = src.to_string();
        let line = user_line("11111111-1111-1111-1111-111111111111", &src_sid, None, "plot it");
        tokio::fs::write(&source, format!("{line}\n")).await.unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(home.path().to_path_buf()));
        let store = visualization::VisualizationStore::new(fs.clone(), home.path().to_path_buf());
        let reference = store
            .publish(
                &visualization::Publisher {
                    root_session: src,
                    agent_id: None,
                },
                None,
                "Chart",
                "<p>x</p>",
                0,
            )
            .await
            .unwrap()
            .reference;
        store.write_state(src, &reference, 0, "1", "null").await.unwrap();

        let fork = create_branch(home.path(), cwd, src, None, fs).await.unwrap();

        let copied = store.read_revision(fork.new_session_id, &reference).await.unwrap();
        assert_eq!(copied.fragment, "<p>x</p>");
        store
            .write_state(fork.new_session_id, &reference, 1, "2", "null")
            .await
            .unwrap();
        assert_eq!(store.read_state(src, &reference).await.unwrap().version, 1);
    }

    #[tokio::test]
    async fn branch_preserves_exact_native_peer_body_and_model_text() {
        use crate::jsonl::exact_json::{
            message_utf16_overrides, parse_exact_json, PRIVATE_UTF16_KEY,
        };
        let home = tempfile::tempdir().unwrap();
        let cwd = "/native-fixture";
        let src = Uuid::parse_str("11111111-2222-4333-8444-555555555555").unwrap();
        let fixture = include_str!("../tests/fixtures/handback_exact_utf16_2_1_286.jsonl");
        let source = session_path(home.path(), cwd, &src.to_string());
        tokio::fs::create_dir_all(source.parent().unwrap())
            .await
            .unwrap();
        let body = format!("{fixture}{{\"type\":\"content-replacement\",\"sessionId\":\"{src}\",\"replacements\":[{{\"text\":\"\\ud83d\"}}]}}\n{{\"type\":\"content-replacement\",\"sessionId\":\"{src}\",\"replacements\":[{{\"text\":\"\\ude00\"}}]}}\n");
        tokio::fs::write(&source, &body).await.unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(home.path().to_path_buf()));
        let fork = create_branch(home.path(), cwd, src, Some("Exact peer history"), fs)
            .await
            .unwrap();
        assert_eq!(fork.message_count, 4);
        let written = tokio::fs::read_to_string(session_path(
            home.path(),
            cwd,
            &fork.new_session_id.to_string(),
        ))
        .await
        .unwrap();
        let restored = route_lines(&written);
        assert_eq!(restored.malformed_line_count, 0);
        assert_eq!(restored.messages_in_order.len(), 4);
        for ((native, original), message) in written
            .lines()
            .take(4)
            .zip(fixture.lines())
            .zip(&restored.messages_in_order)
        {
            let native = parse_exact_json(native).unwrap();
            let original = parse_exact_json(original).unwrap();
            assert_eq!(native.utf16_overrides, original.utf16_overrides);
            assert_eq!(message_utf16_overrides(message), original.utf16_overrides);
            assert_eq!(native.value["message"], original.value["message"]);
            assert_eq!(native.value["origin"], original.value["origin"]);
            assert_eq!(native.value["origin"]["kind"], "peer");
            assert_eq!(native.value["sessionId"], fork.new_session_id.to_string());
            assert!(native.value.get(PRIVATE_UTF16_KEY).is_none());
            assert!(native.value.get("deliveryId").is_none());
        }
        let replacement = written
            .lines()
            .find_map(|line| {
                let exact = parse_exact_json(line).ok()?;
                (exact.value["type"] == "content-replacement").then_some(exact)
            })
            .unwrap();
        assert_eq!(
            replacement.utf16_overrides,
            Utf16Overrides::from([
                ("/replacements/0/text".into(), vec![0xd83d]),
                ("/replacements/1/text".into(), vec![0xde00]),
            ])
        );
        assert!(replacement.value.get(PRIVATE_UTF16_KEY).is_none());
        assert_eq!(tokio::fs::read_to_string(source).await.unwrap(), body);
    }

    #[tokio::test]
    async fn branch_rewrites_session_rechains_and_stamps_forked_from() {
        let home = scratch("rewrite");
        let cwd = "/tmp/proj";
        let src = Uuid::new_v4();
        let src_sid = src.to_string();
        let path = session_path(&home, cwd, &src_sid);
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        let body = format!(
            "{}\n{}\n",
            user_line(
                "11111111-1111-1111-1111-111111111111",
                &src_sid,
                None,
                "first prompt"
            ),
            user_line(
                "22222222-2222-2222-2222-222222222222",
                &src_sid,
                Some("11111111-1111-1111-1111-111111111111"),
                "second",
            ),
        );
        tokio::fs::write(&path, body.clone()).await.unwrap();

        let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(std::path::PathBuf::from(cwd)));
        let result = create_branch(&home, cwd, src, None, fs)
            .await
            .expect("branch");
        assert_eq!(result.message_count, 2);
        assert_ne!(result.new_session_id, src);
        assert!(result.title.contains("(Branch)"), "got {}", result.title);

        let new_sid = result.new_session_id.to_string();
        let written = tokio::fs::read_to_string(session_path(&home, cwd, &new_sid))
            .await
            .unwrap();
        let lines: Vec<&str> = written.lines().collect();
        // 2 conversation lines + 1 custom-title line.
        assert_eq!(lines.len(), 3);
        let first: JsonlMessage = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first.session_id, new_sid);
        assert_eq!(first.parent_uuid, None, "first entry re-roots the chain");
        assert_eq!(
            first.extra.get("forkedFrom").unwrap()["sessionId"]
                .as_str()
                .unwrap(),
            src_sid
        );
        let second: JsonlMessage = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(
            second.parent_uuid.as_deref(),
            Some("11111111-1111-1111-1111-111111111111"),
            "parentUuid rechained to the prior ORIGINAL uuid"
        );
        let title_line: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(title_line["type"], "custom-title");
        assert_eq!(title_line["sessionId"], new_sid);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn branch_copies_active_sidecars_in_create_fork_order() {
        let home = scratch("sidecars");
        let cwd = "/tmp/proj";
        let src = Uuid::new_v4();
        let src_sid = src.to_string();
        let path = session_path(&home, cwd, &src_sid);
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        let body = format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
            user_line(
                "11111111-1111-1111-1111-111111111111",
                &src_sid,
                None,
                "first prompt"
            ),
            user_line(
                "22222222-2222-2222-2222-222222222222",
                &src_sid,
                Some("11111111-1111-1111-1111-111111111111"),
                "second",
            ),
            content_replacement_line(&src_sid, "a.txt", None),
            content_replacement_line(&src_sid, "c.txt", None),
            marble_line("marble-origami-commit", &src_sid, "commit", "pre-reset",),
            marble_line("marble-origami-reset", &src_sid, "reason", "manual",),
            content_replacement_line(&src_sid, "b.txt", Some("agent-1")),
            marble_line(
                "marble-origami-snapshot",
                &src_sid,
                "snapshot",
                "post-reset",
            ),
            json!({
                "type": "history-suppression",
                "sessionId": src_sid,
                "cause": "manual",
                "ts": "2026-08-25T00:00:00.000Z",
            }),
            json!({
                "type": "relocated",
                "sessionId": src_sid,
                "relocatedCwd": "/tmp/old",
            }),
            json!({
                "type": "relocated",
                "sessionId": src_sid,
                "relocatedCwd": "/tmp/new",
            }),
            json!({
                "type": "atis-latch",
                "sessionId": src_sid,
                "atis": "atis-token",
            }),
            json!({
                "type": "atis-latch",
                "sessionId": src_sid,
                "atis": "invalid token",
            }),
        );
        tokio::fs::write(&path, body.clone()).await.unwrap();

        let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(std::path::PathBuf::from(cwd)));
        let result = create_branch(&home, cwd, src, None, fs)
            .await
            .expect("branch");

        let new_sid = result.new_session_id.to_string();
        let written = tokio::fs::read_to_string(session_path(&home, cwd, &new_sid))
            .await
            .unwrap();
        let lines: Vec<&str> = written.lines().collect();
        assert_eq!(lines.len(), 7);

        let suppression: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(suppression["type"], "history-suppression");
        assert_eq!(suppression["sessionId"], json!(new_sid));
        assert_eq!(suppression["cause"], "fork_inherit");
        assert!(suppression["ts"]
            .as_str()
            .is_some_and(|ts| ts.ends_with('Z') && ts.contains('T')));

        let first: JsonlMessage = serde_json::from_str(lines[1]).unwrap();
        let second: JsonlMessage = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(first.uuid, "11111111-1111-1111-1111-111111111111");
        assert_eq!(second.uuid, "22222222-2222-2222-2222-222222222222");

        let sidecar: serde_json::Value = serde_json::from_str(lines[3]).unwrap();
        assert_eq!(sidecar["type"], "content-replacement");
        assert_eq!(sidecar["sessionId"], json!(new_sid));
        assert_eq!(
            sidecar["replacements"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|value| value.get("path").and_then(|value| value.as_str()))
                .collect::<Vec<_>>(),
            vec!["a.txt", "c.txt", "b.txt"],
            "branch must flatten source session and agent replacements in source encounter order",
        );
        assert!(
            sidecar.get("agentId").is_none(),
            "agent-level replacements must not be carried onto the branch transcript",
        );

        let relocated: serde_json::Value = serde_json::from_str(lines[4]).unwrap();
        assert_eq!(relocated["type"], "relocated");
        assert_eq!(relocated["sessionId"], json!(new_sid));
        assert_eq!(relocated["relocatedCwd"], "/tmp/new");

        let atis: serde_json::Value = serde_json::from_str(lines[5]).unwrap();
        assert_eq!(atis["type"], "atis-latch");
        assert_eq!(atis["sessionId"], json!(new_sid));
        assert_eq!(atis["atis"], "atis-token");

        let title: serde_json::Value = serde_json::from_str(lines[6]).unwrap();
        assert_eq!(title["type"], "custom-title");

        let source_after = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(source_after, body, "the source session must stay untouched");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn missing_source_is_no_conversation() {
        let home = scratch("missing");
        let fs: Arc<dyn FileSystem> =
            Arc::new(PosixFileSystem::new(std::path::PathBuf::from("/tmp/proj")));
        let err = create_branch(&home, "/tmp/proj", Uuid::new_v4(), None, fs)
            .await
            .unwrap_err();
        assert!(matches!(err, BranchError::NoConversation));
        let _ = std::fs::remove_dir_all(&home);
    }

    /// v3: forking a chat INTO an app workspace — the target project dir does
    /// not exist yet (created), every copied entry re-roots on the target
    /// cwd, `forkedFrom` still points home, and the SOURCE file is untouched.
    #[tokio::test]
    async fn cross_cwd_branch_creates_the_target_catalog_and_rewrites_cwd() {
        let home = scratch("cross-cwd");
        let source_cwd = "/tmp/proj";
        let target_cwd = "/data/apps/zz9/workspace";
        let src = Uuid::new_v4();
        let src_sid = src.to_string();
        let path = session_path(&home, source_cwd, &src_sid);
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        let body = format!(
            "{}\n{}\n",
            user_line(
                "11111111-1111-1111-1111-111111111111",
                &src_sid,
                None,
                "make me an app"
            ),
            user_line(
                "22222222-2222-2222-2222-222222222222",
                &src_sid,
                Some("11111111-1111-1111-1111-111111111111"),
                "it tracks habits",
            ),
        );
        tokio::fs::write(&path, body.clone()).await.unwrap();

        let fs: Arc<dyn FileSystem> =
            Arc::new(PosixFileSystem::new(std::path::PathBuf::from(source_cwd)));
        let result = create_branch_to_cwd(&home, source_cwd, target_cwd, src, Some("习惯"), fs)
            .await
            .expect("cross-cwd branch");
        assert_eq!(result.message_count, 2);

        let new_sid = result.new_session_id.to_string();
        // The fork lives under the TARGET cwd's (freshly created) project dir.
        let dst = session_path(&home, target_cwd, &new_sid);
        let written = tokio::fs::read_to_string(&dst).await.expect("fork exists");
        let lines: Vec<&str> = written.lines().collect();
        assert_eq!(lines.len(), 3);
        for line in &lines[..2] {
            let entry: JsonlMessage = serde_json::from_str(line).unwrap();
            assert_eq!(entry.session_id, new_sid);
            assert_eq!(
                entry.cwd, target_cwd,
                "copied entries must re-root on the target cwd"
            );
            assert_eq!(
                entry.extra["forkedFrom"]["sessionId"],
                json!(src_sid),
                "the back-reference still names the source session"
            );
        }
        // Nothing was written into the SOURCE catalog, and the source file
        // is byte-identical.
        assert!(!session_path(&home, source_cwd, &new_sid).exists());
        let source_after = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(source_after, body, "the source session must not move");
        let _ = std::fs::remove_dir_all(&home);
    }
}
