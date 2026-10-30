//! Current-format Desktop App Chat mount. A run owns a new generation; only
//! the broker's last committed byte checkpoint can seed it. No failed WAL is
//! opened, and no path supplied by an application page is accepted here.

use super::{AppChatProfile, AppInvocationProfile};
use crate::desktop::{BuildError, DesktopConfig};
use lingxi_core::host::{FileSystem, rooted_fs};
use orchestrator::ConversationOrchestrator;
use session::jsonl::{CapturedChatTranscript, ChatTranscriptCheckpoint, JsonlWriter};
use std::path::PathBuf;
use std::sync::Arc;

/// Native-only authority for one generation's durable cleanup acknowledgement.
/// It carries the original physical directory identity; a replaced generation
/// cannot receive a receipt belonging to the earlier tool owner.
pub struct AppChatCleanupReceipt {
    root: PathBuf,
    identity: rooted_fs::RootIdentity,
    bytes: Vec<u8>,
}

impl AppChatCleanupReceipt {
    pub async fn open(
        home: &std::path::Path,
        identity: &super::AppInvocationIdentity,
        chat: &AppChatProfile,
    ) -> Result<Self, &'static str> {
        let expected = home
            .join("local-apps/v2/apps")
            .join(&identity.app_id)
            .join("agent/agui-v1/chats")
            .join(&chat.chat_id);
        if chat.session_directory != expected
            || chat.native_message_id().is_err()
            || chat.cleanup_nonce.len() != 64
            || !chat
                .cleanup_nonce
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err("invalid_chat");
        }
        let bytes = serde_json::to_vec(&serde_json::json!({
            "schema": 1, "status": "complete", "appId": identity.app_id,
            "versionId": identity.version_id, "invocationId": identity.invocation_id,
            "chatId": chat.chat_id, "generationId": chat.generation_id,
            "sessionId": chat.session_id, "nonce": chat.cleanup_nonce,
        }))
        .map_err(|_| "invalid_chat")?;
        let generation = PathBuf::from(&chat.generation_id);
        let (root, identity) = tokio::task::spawn_blocking(move || {
            let chat_identity =
                rooted_fs::root_identity(&expected).map_err(|_| "chat_transcript_unavailable")?;
            let pinned = rooted_fs::ensure_private_directory(
                &expected,
                &generation,
                rooted_fs::PRIVATE_DIR_MODE,
            )
            .map_err(|_| "chat_transcript_unavailable")?;
            rooted_fs::sync_parent_pinned(&expected, &generation, Some(&chat_identity))
                .map_err(|_| "chat_transcript_unavailable")?;
            Ok::<_, &'static str>((expected.join(generation), pinned))
        })
        .await
        .map_err(|_| "chat_transcript_unavailable")??;
        Ok(Self {
            root,
            identity,
            bytes,
        })
    }

    /// The caller must first confirm every producer drain AND SkillScope
    /// cleanup. This acknowledgement is never a successful model checkpoint.
    pub async fn publish_after_cleanup(&self) -> Result<(), &'static str> {
        let root = self.root.clone();
        let identity = self.identity;
        let bytes = self.bytes.clone();
        tokio::task::spawn_blocking(move || {
            rooted_fs::atomic_write_pinned(
                &root,
                std::path::Path::new("tool-cleanup.json"),
                &bytes,
                rooted_fs::AtomicWriteOptions {
                    overwrite: false,
                    create_parents: false,
                    dir_mode: rooted_fs::PRIVATE_DIR_MODE,
                    file_mode: rooted_fs::PRIVATE_FILE_MODE,
                },
                &identity,
            )
            .map_err(|_| "tool_cleanup_incomplete")
        })
        .await
        .map_err(|_| "tool_cleanup_incomplete")?
    }
}

pub(crate) struct AppChatMount {
    pub writer: Arc<JsonlWriter>,
    pub source: Option<CapturedChatTranscript>,
    pub ledger_home: PathBuf,
    // Stable across generations. Keep this through native drain and sealing;
    // a second process must not run the same Chat while its predecessor lives.
    _owner: rooted_fs::RootedFileLock,
}

fn unavailable() -> BuildError {
    BuildError::AppInvocation(super::AppInvocationError::ChatTranscriptUnavailable)
}

pub(crate) async fn open(
    config: &DesktopConfig,
    fs: Arc<dyn FileSystem>,
) -> Result<Option<Arc<AppChatMount>>, BuildError> {
    let Some(AppInvocationProfile {
        app_chat: Some(chat),
        ..
    }) = &config.app_invocation
    else {
        return Ok(None);
    };
    let chat = chat.clone();
    let root = chat.session_directory.clone();
    let generation = chat.generation_id.clone();
    let (owner, ledger_home) = tokio::task::spawn_blocking(move || {
        let owner = rooted_fs::try_lock_exclusive(
            &root,
            std::path::Path::new(".desktop-chat-owner.lock"),
            rooted_fs::PRIVATE_DIR_MODE,
            rooted_fs::PRIVATE_FILE_MODE,
        )
        .map_err(|_| unavailable())?;
        rooted_fs::ensure_private_directory(
            &root,
            std::path::Path::new(&generation),
            rooted_fs::PRIVATE_DIR_MODE,
        )
        .map_err(|_| unavailable())?;
        let relative = PathBuf::from(&generation).join("runtime");
        let ledger_home = root.join(&relative);
        // Reusing a generation's journal could reintroduce a failed turn even
        // when its transcript was rolled back. There is no old-WAL fallback.
        match std::fs::symlink_metadata(&ledger_home) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => return Err(unavailable()),
        }
        rooted_fs::ensure_private_directory(&root, &relative, rooted_fs::PRIVATE_DIR_MODE)
            .map_err(|_| unavailable())?;
        Ok((owner, ledger_home))
    })
    .await
    .map_err(|_| unavailable())??;
    let path = chat
        .session_directory
        .join(&chat.generation_id)
        .join(format!("{}.jsonl", chat.session_id));
    let expected = if let Some(base) = &chat.base_checkpoint {
        let proof = ChatTranscriptCheckpoint {
            byte_len: base.byte_len,
            sha256: base.sha256.clone(),
        };
        JsonlWriter::stage_persistent_chat_recovery(
            chat.session_directory
                .join(&base.generation_id)
                .join(format!("{}.jsonl", chat.session_id)),
            path.clone(),
            Some(proof.clone()),
        )
        .await
        .map_err(|_| unavailable())?;
        Some(proof)
    } else {
        None
    };
    let writer = Arc::new(
        JsonlWriter::open_persistent_chat(path, fs, expected.clone())
            .await
            .map_err(|_| unavailable())?,
    );
    let source = if expected.is_some() {
        let captured = writer
            .capture_persistent_chat_snapshot(false)
            .await
            .map_err(|_| unavailable())?;
        ConversationOrchestrator::replay_persistent_chat_snapshot(&captured)
            .map_err(|_| unavailable())?;
        Some(captured)
    } else {
        None
    };
    Ok(Some(Arc::new(AppChatMount {
        writer,
        source,
        ledger_home,
        _owner: owner,
    })))
}

pub(crate) async fn restore(
    orchestrator: &ConversationOrchestrator,
    mount: &AppChatMount,
) -> Result<(), BuildError> {
    if let Some(source) = &mount.source {
        let replayed = ConversationOrchestrator::replay_persistent_chat_snapshot(source)
            .map_err(|_| BuildError::Orchestrator("chat_transcript_unavailable".into()))?;
        orchestrator
            .restore_captured_session_history(
                source.session_id(),
                &replayed.messages,
                &session::jsonl::route_lines(source.source()).messages_in_order,
                true,
                permission::PermissionMode::Default,
            )
            .await
            .map_err(|_| BuildError::Orchestrator("chat_transcript_unavailable".into()))?;
        // Prove the actual adopted engine state before the first paid request.
        orchestrator
            .verify_persistent_chat_checkpoint(false)
            .await
            .map_err(|_| BuildError::Orchestrator("chat_transcript_unavailable".into()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop::app_invocation::{
        AppChatGenerationCheckpoint, AppInvocationIdentity, prepare_app_config,
    };
    use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId, SessionId};
    use orchestrator::test_support::{
        MockApiClient, MockOutputStream, MockStreamingApiClient, NoOpPermissionGate,
        StaticMemoryProvider, noop_hook_executor,
    };

    fn fixture() -> (tempfile::TempDir, DesktopConfig) {
        let root = tempfile::tempdir().unwrap();
        let identity = AppInvocationIdentity {
            app_id: "test-app".into(),
            version_id: "version".into(),
            invocation_id: "run".into(),
        };
        let chat_id = uuid::Uuid::new_v4().to_string();
        let generation = uuid::Uuid::new_v4().to_string();
        let store = root
            .path()
            .join("local-apps/v2/apps/test-app/agent/agui-v1");
        let chat = AppChatProfile {
            chat_id: chat_id.clone(),
            session_id: uuid::Uuid::new_v4().to_string(),
            session_directory: store.join("chats").join(&chat_id),
            generation_id: generation.clone(),
            native_user_message_id: format!("msg:{generation}"),
            cleanup_nonce: "b".repeat(64),
            base_checkpoint: None,
        };
        std::fs::create_dir_all(&chat.session_directory).unwrap();
        let cwd = store.join("workspaces").join(&chat_id);
        std::fs::create_dir_all(&cwd).unwrap();
        let mut profile = AppInvocationProfile::new(identity, Default::default());
        profile.app_chat = Some(chat);
        let config = prepare_app_config(
            DesktopConfig {
                cwd,
                lingxi_home: root.path().to_path_buf(),
                default_model: "test/model".into(),
                injected_permission_gate: Some(Arc::new(permission::DenyOnAskGate)),
                ..Default::default()
            },
            profile,
        )
        .unwrap();
        (root, config)
    }

    fn fs(config: &DesktopConfig) -> Arc<dyn FileSystem> {
        Arc::new(platform_posix::fs::PosixFileSystem::new(config.cwd.clone()))
    }

    fn next(config: &DesktopConfig, base: Option<AppChatGenerationCheckpoint>) -> DesktopConfig {
        let mut next = config.clone();
        let chat = next
            .app_invocation
            .as_mut()
            .unwrap()
            .app_chat
            .as_mut()
            .unwrap();
        chat.generation_id = uuid::Uuid::new_v4().to_string();
        chat.native_user_message_id = format!("msg:{}", chat.generation_id);
        chat.base_checkpoint = base;
        // Only the profile can choose a physical target, not a creator override.
        super::super::enforce_app_config(&mut next).unwrap();
        next
    }

    #[tokio::test]
    async fn a_live_generation_blocks_a_second_owner_and_an_existing_wal_is_never_reused() {
        let (_root, config) = fixture();
        let mount = open(&config, fs(&config)).await.unwrap().unwrap();
        let next = next(&config, None);
        assert!(
            open(&next, fs(&next)).await.is_err(),
            "stable Chat ownership spans generations"
        );
        std::fs::write(mount.ledger_home.join("unknown-wal"), "never recover me").unwrap();
        drop(mount);
        assert!(
            open(&config, fs(&config)).await.is_err(),
            "the same generation cannot reopen a failed WAL"
        );
        let fresh = open(&next, fs(&next)).await.unwrap().unwrap();
        assert!(!fresh.ledger_home.join("unknown-wal").exists());
    }

    #[tokio::test]
    async fn recovery_uses_only_the_committed_canonical_prefix_and_a_fresh_journal() {
        let (_root, config) = fixture();
        let mount = open(&config, fs(&config)).await.unwrap().unwrap();
        let chat = config
            .app_invocation
            .as_ref()
            .unwrap()
            .app_chat
            .as_ref()
            .unwrap();
        let session_id = SessionId::parse_prefixed(&chat.session_id).unwrap();
        let api = Arc::new(MockApiClient::new(Vec::new()));
        let orch = ConversationOrchestrator::new_with_streaming(
            Default::default(),
            api,
            Arc::new(MockStreamingApiClient::empty()),
            Arc::new(tool_api::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            config.cwd.clone(),
        )
        .with_session_id(session_id)
        .with_persistent_app_chat()
        .with_jsonl_writer(mount.writer.clone());
        let user = ConversationMessage::user(chat.native_message_id().unwrap(), "accepted".into());
        orch.append_external_history_message(user.clone()).await;
        let answer = ConversationMessage::Assistant {
            id: MessageId::new(),
            content: vec![ContentBlock::Text {
                text: "committed".into(),
                citations: None,
            }],
            stop_reason: Some("stop_sequence".into()),
            resumable: None,
            per_turn_effort: None,
        };
        orch.append_external_history_message(answer.clone()).await;
        let captured = orch.verify_persistent_chat_checkpoint(true).await.unwrap();
        let base = AppChatGenerationCheckpoint {
            generation_id: chat.generation_id.clone(),
            session_id: chat.session_id.clone(),
            byte_len: captured.checkpoint().byte_len,
            sha256: captured.checkpoint().sha256.clone(),
        };
        let old_path = captured.path().to_path_buf();
        let mut with_tail = captured.source().as_bytes().to_vec();
        with_tail.extend_from_slice(b"{unknown failed suffix}\n");
        std::fs::write(mount.ledger_home.join("old.wal"), "failed side effects").unwrap();
        drop(orch);
        drop(mount);
        std::fs::write(&old_path, &with_tail).unwrap();
        let next = next(&config, Some(base));
        let resumed = open(&next, fs(&next)).await.unwrap().unwrap();
        let source = resumed.source.as_ref().unwrap();
        assert_eq!(source.source(), captured.source());
        assert_eq!(
            ConversationOrchestrator::replay_persistent_chat_snapshot(source)
                .unwrap()
                .state
                .history,
            vec![user, answer]
        );
        assert!(!resumed.ledger_home.join("old.wal").exists());
        assert_eq!(std::fs::read(old_path).unwrap(), with_tail);
    }

    #[tokio::test]
    async fn cleanup_receipt_is_private_and_rejects_a_replaced_generation() {
        let (_root, config) = fixture();
        let profile = config.app_invocation.as_ref().unwrap();
        let chat = profile.app_chat.as_ref().unwrap();
        let receipt = AppChatCleanupReceipt::open(&config.lingxi_home, &profile.identity, chat)
            .await
            .unwrap();
        receipt.publish_after_cleanup().await.unwrap();
        let path = chat
            .session_directory
            .join(&chat.generation_id)
            .join("tool-cleanup.json");
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["generationId"], chat.generation_id);
        assert_eq!(value["nonce"], chat.cleanup_nonce);
        assert!(value.get("checkpoint").is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let replaced = path.parent().unwrap();
        let retained = replaced.with_extension("original");
        std::fs::rename(replaced, &retained).unwrap();
        std::fs::create_dir(replaced).unwrap();
        assert_eq!(
            receipt.publish_after_cleanup().await,
            Err("tool_cleanup_incomplete")
        );
        assert!(!path.exists());
        assert!(retained.join("tool-cleanup.json").exists());
    }
}
