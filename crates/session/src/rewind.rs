//! Rewind one claimed transcript from its captured physical source.

use std::io::Write;
use std::path::Path;

use lingxi_core::host::live_sessions::SharedSessionWriterLease;
use lingxi_core::host::rooted_fs::{
    atomic_write_stream_pinned_checked, opened_file_identity, AtomicWriteOptions,
};
use lingxi_core::host::session_context::ConversationMode;
use lingxi_core::host::FsError;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::jsonl::writer::TranscriptFileAuthority;

/// The exact file and bytes published by a successful rewind.
#[derive(Debug, Clone)]
pub struct RewoundTranscript {
    pub authority: TranscriptFileAuthority,
    pub content: String,
}

/// A failed publication cannot be retried by recapturing the same path.
#[derive(Debug, thiserror::Error)]
pub enum RewindError {
    #[error("rewind rejected before transcript publication: {0}")]
    Rejected(String),
    #[error("rewind transcript publication outcome uncertain: {0}")]
    OutcomeUnknown(String),
}

pub(crate) fn source_revision(source: &str) -> [u8; 32] {
    Sha256::digest(source.as_bytes()).into()
}

/// Verify the original claimed source, fixed profile, and effective workspace.
pub(crate) fn validate_captured_source(
    authority: &TranscriptFileAuthority,
    source: &str,
    session_id: Uuid,
    mode: ConversationMode,
    cwd: &Path,
) -> Result<(), String> {
    let parts: Vec<_> = authority.relative.components().collect();
    if parts.len() != 3
        || !matches!(parts[0], std::path::Component::Normal(name) if name == "projects")
        || !matches!(parts[1], std::path::Component::Normal(_))
        || !matches!(parts[2], std::path::Component::Normal(_))
        || authority
            .path()
            .extension()
            .and_then(|value| value.to_str())
            != Some("jsonl")
        || authority
            .path()
            .file_stem()
            .and_then(|value| value.to_str())
            .and_then(|value| Uuid::parse_str(value).ok())
            != Some(session_id)
    {
        return Err("captured transcript is outside the session catalog".into());
    }
    if source.is_empty() || !source.ends_with('\n') {
        return Err("transcript has an incomplete final record".into());
    }
    if authority
        .read_checked()
        .map_err(|error| error.to_string())?
        != source.as_bytes()
    {
        return Err("captured transcript changed".into());
    }
    authority
        .verify_current()
        .map_err(|error| error.to_string())?;
    let actual =
        crate::JsonlWriter::fixed_session_mode_from_source(source, &session_id.to_string())?;
    if actual != mode {
        return Err("session_mode_mismatch: captured transcript mode changed".into());
    }
    let path = authority.path();
    let loaded = crate::jsonl::loader::route_captured_session_source(&path, session_id, source)
        .map_err(|error| error.to_string())?;
    if loaded.malformed_line_count != 0 {
        return Err("captured transcript contains malformed records".into());
    }
    let recorded = crate::jsonl::loader::transcript_workspace_path(&path, &loaded)
        .ok_or("captured transcript workspace is unavailable")?;
    if !cwd.is_absolute() || !cwd.is_dir() {
        return Err("rewind workspace is unavailable".into());
    }
    let current = std::fs::canonicalize(cwd).map_err(|error| error.to_string())?;
    let recorded = std::fs::canonicalize(recorded).map_err(|error| error.to_string())?;
    if current != recorded {
        return Err("rewind workspace does not match the captured transcript".into());
    }
    authority
        .verify_revision(&source_revision(source))
        .map_err(|error| error.to_string())
}

fn render_rewind(
    authority: &TranscriptFileAuthority,
    source: &str,
    session_id: Uuid,
    target: Uuid,
    mode: ConversationMode,
    cwd: &Path,
) -> Result<String, String> {
    let loaded_source = crate::jsonl::route_lines(source);
    let (chain, _) =
        crate::jsonl::build_conversation_chain(&loaded_source, &session_id.to_string());
    if !chain
        .iter()
        .any(|row| row.message_type == "user" && Uuid::parse_str(&row.uuid).ok() == Some(target))
    {
        return Err("rewind target is not in the selected conversation".into());
    }
    let mut offset = 0;
    let mut selected = None;
    for line in source.split_inclusive('\n') {
        let row = crate::jsonl::exact_json::parse_exact_json(line.trim_end_matches('\n'))
            .map_err(|error| format!("invalid transcript record: {error}"))?;
        if row
            .value
            .get("uuid")
            .and_then(serde_json::Value::as_str)
            .and_then(|value| Uuid::parse_str(value).ok())
            == Some(target)
        {
            let owner = row
                .value
                .get("sessionId")
                .and_then(serde_json::Value::as_str)
                .and_then(|value| Uuid::parse_str(value).ok());
            if owner != Some(session_id)
                || row.value.get("type").and_then(serde_json::Value::as_str) != Some("user")
            {
                return Err("rewind target is not an owned user message".into());
            }
            selected = Some(offset);
            break;
        }
        offset += line.len();
    }
    let offset = selected.ok_or("rewind target message was not found")?;
    let mut result = source[..offset].to_owned();
    if crate::JsonlWriter::fixed_session_mode_from_source(&result, &session_id.to_string())? != mode
    {
        return Err("rewind would discard the fixed session profile".into());
    }
    let loaded = crate::jsonl::route_lines(&result);
    if loaded.malformed_line_count != 0 {
        return Err("rewind prefix contains malformed records".into());
    }
    let (messages, _) = crate::jsonl::build_conversation_chain(&loaded, &session_id.to_string());
    let cwd = cwd.to_str().ok_or("rewind workspace is not UTF-8")?;
    if crate::jsonl::loader::transcript_workspace_path(&authority.path(), &loaded).as_deref()
        != Some(cwd)
    {
        // A later /cd may occur after the truncation point. Preserve the
        // authorized current workspace rather than remounting the old one.
        result.push_str(&format!(
            "{}\n",
            serde_json::json!({
                "type":"relocated", "sessionId":session_id, "relocatedCwd":cwd
            })
        ));
    }
    if !messages
        .iter()
        .any(|message| matches!(message.message_type.as_str(), "user" | "assistant"))
    {
        // A rewind to before the first turn is still a discoverable, fixed-mode
        // session. Carry the authorized current workspace in its draft marker.
        let title = loaded
            .custom_titles
            .get(&session_id.to_string())
            .filter(|title| !title.is_empty())
            .map_or("New conversation", String::as_str);
        result.push_str(&format!(
            "{}\n",
            serde_json::json!({
                "type":"custom-title", "sessionId":session_id, "customTitle":title,
                "mobileEmptySession":1, "cwd":cwd
            })
        ));
        if crate::jsonl::validated_empty_draft_cwd(&result, &session_id.to_string()).as_deref()
            != Some(Path::new(cwd))
        {
            return Err("rewind prefix is not a valid empty draft".into());
        }
    }
    let final_source = crate::jsonl::route_lines(&result);
    if crate::jsonl::loader::transcript_workspace_path(&authority.path(), &final_source).as_deref()
        != Some(cwd)
    {
        return Err("rewind prefix does not preserve its current workspace".into());
    }
    Ok(result)
}

fn rejected(error: impl std::fmt::Display) -> RewindError {
    RewindError::Rejected(error.to_string())
}

fn rewind_transaction<B, P, A>(
    authority: TranscriptFileAuthority,
    source: String,
    session_id: Uuid,
    mode: ConversationMode,
    cwd: std::path::PathBuf,
    target: Uuid,
    before_backup: B,
    before_publish: P,
    after_publish: A,
) -> Result<RewoundTranscript, RewindError>
where
    B: FnOnce() -> Result<(), FsError>,
    P: FnOnce() -> Result<(), FsError>,
    A: FnOnce(),
{
    validate_captured_source(&authority, &source, session_id, mode, &cwd).map_err(rejected)?;
    let content =
        render_rewind(&authority, &source, session_id, target, mode, &cwd).map_err(rejected)?;
    let revision = source_revision(&source);
    let options = AtomicWriteOptions {
        overwrite: true,
        create_parents: false,
        dir_mode: 0o700,
        file_mode: 0o600,
    };
    let backup = authority.relative.with_extension("jsonl.rewind-bak");
    let mut backup_identity = None;
    atomic_write_stream_pinned_checked(
        &authority.root,
        &backup,
        options,
        &authority.root_identity,
        |temporary| {
            backup_identity = Some(opened_file_identity(temporary)?);
            temporary
                .write_all(source.as_bytes())
                .map_err(|error| FsError::Io(error.to_string()))
        },
        || {
            before_backup()?;
            authority.verify_revision(&revision)
        },
    )
    .map_err(|error| rejected(format!("backup failed (transcript not rewritten): {error}")))?;
    let backup_identity = backup_identity.ok_or_else(|| rejected("backup identity unavailable"))?;
    let backup_authority = TranscriptFileAuthority::from_capture(
        authority.root.clone(),
        backup,
        authority.root_identity,
        backup_identity,
    )
    .map_err(rejected)?;
    backup_authority
        .verify_revision(&revision)
        .map_err(rejected)?;

    let mut published_identity = None;
    let published = atomic_write_stream_pinned_checked(
        &authority.root,
        &authority.relative,
        options,
        &authority.root_identity,
        |temporary| {
            published_identity = Some(opened_file_identity(temporary)?);
            temporary
                .write_all(content.as_bytes())
                .map_err(|error| FsError::Io(error.to_string()))
        },
        || {
            before_publish()?;
            authority.verify_revision(&revision)?;
            backup_authority.verify_revision(&revision)?;
            authority.verify_revision(&revision)
        },
    );
    if let Err(error) = published {
        return Err(if error.published {
            RewindError::OutcomeUnknown(error.to_string())
        } else {
            rejected(error)
        });
    }
    after_publish();
    let published_identity = published_identity.ok_or_else(|| {
        RewindError::OutcomeUnknown("published transcript identity unavailable".into())
    })?;
    let published = TranscriptFileAuthority::from_capture(
        authority.root.clone(),
        authority.relative.clone(),
        authority.root_identity,
        published_identity,
    )
    .map_err(|error| RewindError::OutcomeUnknown(error.to_string()))?;
    if published
        .read_checked()
        .map_err(|error| RewindError::OutcomeUnknown(error.to_string()))?
        != content.as_bytes()
    {
        return Err(RewindError::OutcomeUnknown(
            "published transcript changed".into(),
        ));
    }
    published
        .verify_current()
        .map_err(|error| RewindError::OutcomeUnknown(error.to_string()))?;
    backup_authority
        .verify_revision(&revision)
        .map_err(|error| RewindError::OutcomeUnknown(error.to_string()))?;
    published
        .verify_revision(&source_revision(&content))
        .map_err(|error| RewindError::OutcomeUnknown(error.to_string()))?;
    Ok(RewoundTranscript {
        authority: published,
        content,
    })
}

/// Back up and atomically replace the exact claimed transcript from its
/// captured bytes. The task holds the UUID lease even if the caller is dropped.
pub async fn rewind_conversation(
    authority: &TranscriptFileAuthority,
    source: &str,
    session_id: Uuid,
    mode: ConversationMode,
    cwd: &Path,
    target: Uuid,
    lease: SharedSessionWriterLease,
) -> Result<RewoundTranscript, RewindError> {
    rewind_conversation_with_hooks(
        authority,
        source,
        session_id,
        mode,
        cwd,
        target,
        lease,
        || Ok(()),
        || Ok(()),
        || {},
    )
    .await
}

async fn rewind_conversation_with_hooks<B, P, A>(
    authority: &TranscriptFileAuthority,
    source: &str,
    session_id: Uuid,
    mode: ConversationMode,
    cwd: &Path,
    target: Uuid,
    lease: SharedSessionWriterLease,
    before_backup: B,
    before_publish: P,
    after_publish: A,
) -> Result<RewoundTranscript, RewindError>
where
    B: FnOnce() -> Result<(), FsError> + Send + 'static,
    P: FnOnce() -> Result<(), FsError> + Send + 'static,
    A: FnOnce() + Send + 'static,
{
    if lease.canonical_session_id().map(|id| id.as_uuid()) != Some(session_id) {
        return Err(rejected("rewind session writer claim does not match"));
    }
    let authority = authority.clone();
    let source = source.to_owned();
    let cwd = cwd.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let _lease = lease;
        rewind_transaction(
            authority,
            source,
            session_id,
            mode,
            cwd,
            target,
            before_backup,
            before_publish,
            after_publish,
        )
    })
    .await
    .map_err(|error| RewindError::OutcomeUnknown(error.to_string()))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::live_sessions::LiveSessionDir;
    use std::path::PathBuf;

    struct Fixture {
        _root: tempfile::TempDir,
        home: PathBuf,
        cwd: PathBuf,
        path: PathBuf,
        id: Uuid,
        first: Uuid,
        second: Uuid,
        source: String,
        authority: TranscriptFileAuthority,
        lease: SharedSessionWriterLease,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let cwd = root.path().canonicalize().unwrap();
            let home = cwd.join("home");
            std::fs::create_dir(&home).unwrap();
            let id = Uuid::new_v4();
            let first = Uuid::new_v4();
            let second = Uuid::new_v4();
            let path = crate::jsonl::session_path(&home, &cwd.to_string_lossy(), &id.to_string());
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let source = format!(
                "{}\n{}\n{}\n",
                serde_json::json!({"type":"fixed-session-mode","sessionId":id,"sessionMode":"code","instructionEpoch":4}),
                serde_json::json!({"type":"user","uuid":first,"parentUuid":null,"sessionId":id,"cwd":cwd,"message":{"role":"user","content":"first"}}),
                serde_json::json!({"type":"user","uuid":second,"parentUuid":first,"sessionId":id,"cwd":cwd,"message":{"role":"user","content":"second"}})
            );
            std::fs::write(&path, &source).unwrap();
            let home = home.canonicalize().unwrap();
            let (authority, bytes) = TranscriptFileAuthority::capture(
                home.clone(),
                path.strip_prefix(&home).unwrap().to_path_buf(),
            )
            .unwrap();
            assert_eq!(bytes, source.as_bytes());
            let lease = LiveSessionDir::at_live(home.join("sessions"))
                .claim_session_id(&id.to_string(), std::process::id())
                .unwrap()
                .into_shared();
            Self {
                _root: root,
                home,
                cwd,
                path,
                id,
                first,
                second,
                source,
                authority,
                lease,
            }
        }

        fn run<B, P, A>(
            &self,
            target: Uuid,
            before_backup: B,
            before_publish: P,
            after_publish: A,
        ) -> Result<RewoundTranscript, RewindError>
        where
            B: FnOnce() -> Result<(), FsError>,
            P: FnOnce() -> Result<(), FsError>,
            A: FnOnce(),
        {
            rewind_transaction(
                self.authority.clone(),
                self.source.clone(),
                self.id,
                ConversationMode::Code,
                self.cwd.clone(),
                target,
                before_backup,
                before_publish,
                after_publish,
            )
        }
    }

    #[tokio::test]
    async fn captured_rewind_publishes_receipt_and_durable_backup() {
        let fixture = Fixture::new();
        let published = rewind_conversation(
            &fixture.authority,
            &fixture.source,
            fixture.id,
            ConversationMode::Code,
            &fixture.cwd,
            fixture.second,
            fixture.lease.clone(),
        )
        .await
        .unwrap();
        assert!(published.content.contains("first"));
        assert!(!published.content.contains("second"));
        assert_eq!(
            published.authority.read_checked().unwrap(),
            published.content.as_bytes()
        );
        assert!(!published.authority.same_source(&fixture.authority));
        assert_eq!(
            std::fs::read_to_string(fixture.path.with_extension("jsonl.rewind-bak")).unwrap(),
            fixture.source
        );
        assert_eq!(
            crate::JsonlWriter::fixed_session_mode_from_source(
                &published.content,
                &fixture.id.to_string()
            )
            .unwrap(),
            ConversationMode::Code
        );
    }

    #[tokio::test]
    async fn captured_rewind_preserves_native_exact_utf16_prefix() {
        let mut fixture = Fixture::new();
        let native = include_str!("../tests/fixtures/handback_exact_utf16_2_1_286.jsonl");
        let old = "11111111-2222-4333-8444-555555555555";
        let cwd = serde_json::to_string(&fixture.cwd).unwrap();
        let native = native
            .replace(old, &fixture.id.to_string())
            .replace("\"/native-fixture\"", &cwd);
        let header = serde_json::json!({"type":"fixed-session-mode","sessionId":fixture.id,
            "sessionMode":"code","instructionEpoch":4});
        fixture.source = format!("{header}\n{native}");
        std::fs::write(&fixture.path, &fixture.source).unwrap();
        let target = Uuid::parse_str("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaa2").unwrap();
        let published = rewind_conversation(
            &fixture.authority,
            &fixture.source,
            fixture.id,
            ConversationMode::Code,
            &fixture.cwd,
            target,
            fixture.lease.clone(),
        )
        .await
        .unwrap();
        assert_eq!(
            published.content,
            format!("{header}\n{}\n", native.lines().next().unwrap())
        );
    }

    #[tokio::test]
    async fn captured_rewind_to_first_turn_keeps_valid_anchored_profile() {
        let fixture = Fixture::new();
        let published = rewind_conversation(
            &fixture.authority,
            &fixture.source,
            fixture.id,
            ConversationMode::Code,
            &fixture.cwd,
            fixture.first,
            fixture.lease.clone(),
        )
        .await
        .unwrap();
        assert_eq!(
            crate::jsonl::validated_empty_draft_cwd(&published.content, &fixture.id.to_string()),
            Some(fixture.cwd)
        );
        assert!(!published.content.contains("\"first\""));
        assert_eq!(
            published.authority.read_checked().unwrap(),
            published.content.as_bytes()
        );
    }

    #[tokio::test]
    async fn captured_rewind_keeps_later_workspace_relocation_on_nonempty_prefix() {
        let mut fixture = Fixture::new();
        let later = fixture.cwd.join("later-workspace");
        std::fs::create_dir(&later).unwrap();
        let later = later.canonicalize().unwrap();
        fixture.source.push_str(&format!(
            "{}\n",
            serde_json::json!({
                "type":"relocated", "sessionId":fixture.id, "relocatedCwd":later
            })
        ));
        std::fs::write(&fixture.path, &fixture.source).unwrap();
        let published = rewind_conversation(
            &fixture.authority,
            &fixture.source,
            fixture.id,
            ConversationMode::Code,
            &later,
            fixture.second,
            fixture.lease.clone(),
        )
        .await
        .unwrap();
        let loaded = crate::jsonl::route_lines(&published.content);
        assert!(published.content.contains("first"));
        assert!(!published.content.contains("second"));
        assert_eq!(
            crate::jsonl::loader::transcript_workspace_path(&published.authority.path(), &loaded)
                .map(std::path::PathBuf::from),
            Some(later)
        );
        assert_eq!(published.authority.path(), fixture.path);
    }

    #[test]
    fn captured_rewind_backup_failure_never_rewrites_source() {
        let fixture = Fixture::new();
        let result = fixture.run(
            fixture.second,
            || Err(FsError::Io("injected no space".into())),
            || Ok(()),
            || {},
        );
        assert!(matches!(result, Err(RewindError::Rejected(_))));
        assert_eq!(
            std::fs::read_to_string(&fixture.path).unwrap(),
            fixture.source
        );
        assert!(!fixture.path.with_extension("jsonl.rewind-bak").exists());
    }

    #[test]
    fn captured_rewind_staging_failure_keeps_original_and_backup() {
        let fixture = Fixture::new();
        let result = fixture.run(
            fixture.second,
            || Ok(()),
            || Err(FsError::Io("injected no space".into())),
            || {},
        );
        assert!(matches!(result, Err(RewindError::Rejected(_))));
        assert_eq!(
            std::fs::read_to_string(&fixture.path).unwrap(),
            fixture.source
        );
        assert_eq!(
            std::fs::read_to_string(fixture.path.with_extension("jsonl.rewind-bak")).unwrap(),
            fixture.source
        );
    }

    #[cfg(unix)]
    #[test]
    fn captured_rewind_rejects_symlink_backup_without_touching_its_target() {
        let fixture = Fixture::new();
        let outside = fixture.home.join("outside.txt");
        std::fs::write(&outside, "outside").unwrap();
        std::os::unix::fs::symlink(&outside, fixture.path.with_extension("jsonl.rewind-bak"))
            .unwrap();
        let result = fixture.run(fixture.second, || Ok(()), || Ok(()), || {});
        assert!(matches!(result, Err(RewindError::Rejected(_))));
        assert_eq!(
            std::fs::read_to_string(&fixture.path).unwrap(),
            fixture.source
        );
        assert_eq!(std::fs::read_to_string(outside).unwrap(), "outside");
    }

    #[test]
    fn captured_rewind_rejects_missing_target_and_unterminated_source_before_backup() {
        let mut fixture = Fixture::new();
        let missing = fixture.run(Uuid::new_v4(), || Ok(()), || Ok(()), || {});
        assert!(matches!(missing, Err(RewindError::Rejected(_))));
        assert_eq!(
            std::fs::read_to_string(&fixture.path).unwrap(),
            fixture.source
        );
        assert!(!fixture.path.with_extension("jsonl.rewind-bak").exists());
        assert_eq!(fixture.source.pop(), Some('\n'));
        std::fs::write(&fixture.path, &fixture.source).unwrap();
        let incomplete = fixture.run(fixture.second, || Ok(()), || Ok(()), || {});
        assert!(matches!(incomplete, Err(RewindError::Rejected(_))));
        assert_eq!(
            std::fs::read_to_string(&fixture.path).unwrap(),
            fixture.source
        );
        assert!(!fixture.path.with_extension("jsonl.rewind-bak").exists());
    }

    #[test]
    fn captured_rewind_refuses_same_bytes_inode_replacement_before_publication() {
        for during_backup in [true, false] {
            let fixture = Fixture::new();
            let retired = fixture.home.join("retired.jsonl");
            let swap = || {
                std::fs::rename(&fixture.path, &retired).unwrap();
                std::fs::write(&fixture.path, &fixture.source).unwrap();
                Ok(())
            };
            let result = if during_backup {
                fixture.run(fixture.second, swap, || Ok(()), || {})
            } else {
                fixture.run(fixture.second, || Ok(()), swap, || {})
            };
            assert!(matches!(result, Err(RewindError::Rejected(_))));
            assert_eq!(
                std::fs::read_to_string(&fixture.path).unwrap(),
                fixture.source
            );
            assert_eq!(std::fs::read_to_string(retired).unwrap(), fixture.source);
            if !during_backup {
                assert_eq!(
                    std::fs::read_to_string(fixture.path.with_extension("jsonl.rewind-bak"))
                        .unwrap(),
                    fixture.source
                );
            }
        }
    }

    #[test]
    fn captured_rewind_post_publication_replacement_is_uncertain() {
        let fixture = Fixture::new();
        let retired = fixture.home.join("published-retired.jsonl");
        let result = fixture.run(
            fixture.second,
            || Ok(()),
            || Ok(()),
            || {
                std::fs::rename(&fixture.path, &retired).unwrap();
                std::fs::write(&fixture.path, "replacement\n").unwrap();
            },
        );
        assert!(matches!(result, Err(RewindError::OutcomeUnknown(_))));
        assert_eq!(
            std::fs::read_to_string(&fixture.path).unwrap(),
            "replacement\n"
        );
        assert!(std::fs::read_to_string(retired).unwrap().contains("first"));
        assert_eq!(
            std::fs::read_to_string(fixture.path.with_extension("jsonl.rewind-bak")).unwrap(),
            fixture.source
        );
    }

    #[test]
    fn captured_rewind_lost_backup_after_publication_is_uncertain() {
        let fixture = Fixture::new();
        let backup = fixture.path.with_extension("jsonl.rewind-bak");
        let retired = fixture.home.join("retired-backup.jsonl");
        let result = fixture.run(
            fixture.second,
            || Ok(()),
            || Ok(()),
            || {
                std::fs::rename(&backup, &retired).unwrap();
                std::fs::write(&backup, &fixture.source).unwrap();
            },
        );
        assert!(matches!(result, Err(RewindError::OutcomeUnknown(_))));
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), fixture.source);
        assert_eq!(std::fs::read_to_string(retired).unwrap(), fixture.source);
        assert!(!std::fs::read_to_string(&fixture.path)
            .unwrap()
            .contains("second"));
    }

    #[tokio::test]
    async fn captured_rewind_keeps_claim_when_waiter_is_cancelled() {
        let fixture = Fixture::new();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let authority = fixture.authority.clone();
        let source = fixture.source.clone();
        let cwd = fixture.cwd.clone();
        let id = fixture.id;
        let target = fixture.second;
        let lease = fixture.lease.clone();
        let task = tokio::spawn(async move {
            rewind_conversation_with_hooks(
                &authority,
                &source,
                id,
                ConversationMode::Code,
                &cwd,
                target,
                lease,
                move || {
                    let _ = entered_tx.send(());
                    release_rx.recv().unwrap();
                    Ok(())
                },
                || Ok(()),
                || {},
            )
            .await
        });
        entered_rx.await.unwrap();
        task.abort();
        drop(fixture.lease);
        let live = LiveSessionDir::at_live(fixture.home.join("sessions"));
        assert!(live
            .claim_session_id(&id.to_string(), std::process::id())
            .is_err());
        release_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if live
                    .claim_session_id(&id.to_string(), std::process::id())
                    .is_ok()
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("claim released after detached write settled");
    }
}
