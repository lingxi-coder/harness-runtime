use super::*;
use crate::mobile::host::{
    is_mobile_dispatch_command, mobile_prompt_command_uuid, rebuild_mobile_prompt_dispatch_cache,
};
use crate::mobile::turn_durability::DurableTurnStore;

fn seed_child(path: &Path, text: &str) {
    let message = lingxi_core::types::ConversationMessage::user(
        lingxi_core::types::MessageId::new(),
        text.into(),
    );
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let row = serde_json::json!({
        "message_index": 7, "uuid": message.id().as_uuid().to_string(), "message": message,
    });
    std::fs::write(path, format!("{row}\n")).unwrap();
    let mut metadata = path.as_os_str().to_os_string();
    metadata.push(".meta");
    std::fs::write(
        std::path::PathBuf::from(metadata),
        r#"{"next_message_index":11}"#,
    )
    .unwrap();
}

fn text_in_rows(rows: &[client::protocol::listings::SessionAgentMessageRowDto]) -> String {
    rows.iter()
        .flat_map(|row| &row.message.blocks)
        .filter_map(|block| match block {
            client::protocol::message::MessageBlockDto::Text { text } => Some(text.display()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn mobile_existing_child_file_merges_unpersisted_observer_messages_and_api_errors() {
    let root = tempfile::tempdir().unwrap();
    let (handle, listener) = build_submit_handle(root.path());
    handle.runtime().block_on(async {
        let (owner, dir) = handle.session_agent_dir().await.unwrap();
        let child = lingxi_core::types::AgentId::new();
        let child_path = dir.join(format!("agent-{child}.jsonl"));
        seed_child(&child_path, "PERSISTED_PREFIX");
        let disk_before = std::fs::read(&child_path).unwrap();
        let mut observer = MobileSessionAgentObserver::new(
            ListenerSink::arc(listener),
            Arc::new(StdMutex::new(owner.as_uuid().to_string())),
        );
        observer.transcript_cache = handle.inner.session_agent_transcript_cache.clone();
        observer
            .on_event(SubagentObservation::Allocated {
                agent_id: child,
                agent_type: "fixture".into(),
                name: None,
                model: "fixture".into(),
                model_profile: None,
                persistent: true,
                initial_message_index: 11,
                origin_session_id: Some(owner),
            })
            .await;
        observer
            .on_event(SubagentObservation::Message {
                agent_id: child,
                message: lingxi_core::types::ConversationMessage::user(
                    lingxi_core::types::MessageId::new(),
                    "UNPERSISTED_SUFFIX".into(),
                ),
            })
            .await;
        let error = lingxi_core::host::ServerFallbackApiErrorRow::new(
            &"UNPERSISTED_API_ERROR".into(),
            "2026-11-12T00:00:00Z".into(),
        );
        observer
            .on_event(SubagentObservation::ServerFallbackApiErrorRow {
                agent_id: child,
                row: error.clone(),
                message_index: 12,
            })
            .await;
        for _ in 0..2 {
            let (rows, revision, next) = handle
                .load_session_agent_transcript_for_session(owner, &dir, &child.to_string())
                .await
                .unwrap();
            assert_eq!(
                rows.iter().map(|row| row.message_index).collect::<Vec<_>>(),
                [7, 11, 12]
            );
            assert_eq!(next, 13);
            assert!(
                revision > session_agent_transcript_revision(&disk_before),
                "clients must accept recovered rows as a newer snapshot"
            );
            assert!(text_in_rows(&rows).contains("PERSISTED_PREFIX"));
            assert!(text_in_rows(&rows).contains("UNPERSISTED_SUFFIX"));
            assert_eq!(rows[2].message_uuid, error.uuid.as_uuid().to_string());
            assert!(rows[2]
                .api_error_json
                .as_deref()
                .is_some_and(|source| source.contains("UNPERSISTED_API_ERROR")));
        }
        handle
            .submit(ClientCommand::LoadSessionAgentTranscript {
                agent_id: child.to_string(),
            })
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(&child_path).unwrap(),
            disk_before,
            "viewer recovery must not write to the child transcript"
        );
        let other = lingxi_core::types::SessionId::new();
        let (rows, _, next) = handle
            .load_session_agent_transcript_for_session(other, &dir, &child.to_string())
            .await
            .unwrap();
        assert_eq!(
            rows.len(),
            1,
            "fallback is isolated by owner, even with the same file and child UUID"
        );
        assert_eq!(next, 11);
    });
}

#[test]
fn mobile_queue_rebuild_recovers_uncertain_publication_once_in_warm_and_cold_stores() {
    let root = tempfile::tempdir().unwrap();
    let (handle, _) = build_submit_handle(root.path());
    handle.runtime().block_on(async {
        let path = root.path().join("uncertain-turns");
        let warm = DurableTurnStore::new(path.clone());
        assert!(warm.scan_pending("owner").unwrap().is_empty());
        warm.fail_next_metadata_after_publish_for_test();
        assert!(warm
            .admit_queued_prompt(
                "owner",
                Some(19),
                None,
                "RECOVER_QUEUED_BODY".into(),
                None,
                Vec::new()
            )
            .is_err());
        warm.admit_queued_prompt(
            "owner",
            Some(19),
            None,
            "RECOVER_QUEUED_BODY".into(),
            None,
            Vec::new(),
        )
        .unwrap();
        let cold = DurableTurnStore::new(path);
        for store in [&warm, &cold, &warm] {
            // NewResume and host startup share this production rebuild path.
            rebuild_mobile_prompt_dispatch_cache(&handle.message_queue, store, "owner")
                .await
                .unwrap();
            let commands = handle.message_queue.snapshot().await;
            let queued = commands
                .iter()
                .filter(|command| command.uuid == mobile_prompt_command_uuid("owner", 19))
                .collect::<Vec<_>>();
            assert_eq!(queued.len(), 1);
        }
        warm.cancel_pending("owner", 19).unwrap();
        rebuild_mobile_prompt_dispatch_cache(&handle.message_queue, &warm, "owner")
            .await
            .unwrap();
        assert!(!handle
            .message_queue
            .snapshot()
            .await
            .iter()
            .any(|command| is_mobile_dispatch_command(&command.uuid)));
    });
}

#[test]
fn mobile_transcript_viewers_keep_admitted_source_after_cd_and_new_session() {
    let root = tempfile::tempdir().unwrap();
    let (handle, _) = build_submit_handle(root.path());
    handle.runtime().block_on(async {
        let owner = handle.inner.orchestrator.current_session_id().await;
        let source = handle
            .inner
            .session_writer
            .session_target_path(owner)
            .unwrap();
        let user_id = uuid::Uuid::new_v4().to_string();
        let native = serde_json::json!({
            "type":"user", "uuid":user_id, "parentUuid":null,
            "sessionId":owner.as_uuid().to_string(), "cwd":handle.session_cwd(),
            "isSidechain":false, "timestamp":"2026-11-12T00:00:00Z",
            "message":{"role":"user","content":"ADMITTED_MAIN_SOURCE"}
        });
        let native = session::jsonl::reader::route_lines(&format!("{native}\n"))
            .messages_in_order
            .remove(0);
        handle
            .inner
            .session_writer
            .append_native_message_once(&native)
            .await
            .unwrap();
        let child = lingxi_core::types::AgentId::new();
        let child_filename = format!("agent-{child}.jsonl");
        let child_path = source
            .with_extension("")
            .join("subagents")
            .join(&child_filename);
        seed_child(&child_path, "ADMITTED_CHILD_SOURCE");

        let nested = root.path().join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        handle.inner.session_cwd.change_cwd(nested.clone());
        let sibling = orchestrator::transcript_paths::main_transcript_path(
            &handle.lingxi_home,
            &nested.to_string_lossy(),
            &owner.as_uuid().to_string(),
        );
        assert_ne!(sibling, source);
        std::fs::create_dir_all(sibling.parent().unwrap()).unwrap();
        let false_source = std::fs::read_to_string(&source)
            .unwrap()
            .replace("ADMITTED_MAIN_SOURCE", "WRONG_SIBLING_SOURCE");
        std::fs::write(&sibling, false_source).unwrap();
        seed_child(
            &sibling
                .with_extension("")
                .join("subagents")
                .join(child_filename),
            "WRONG_SIBLING_CHILD",
        );

        let (session, dir) = handle.session_agent_dir().await.unwrap();
        assert_eq!(session, owner);
        assert_eq!(dir, source.with_extension("").join("subagents"));
        let (main_rows, _, main_next) = handle
            .load_session_agent_transcript_for_session(owner, &dir, "main")
            .await
            .unwrap();
        assert!(text_in_rows(&main_rows).contains("ADMITTED_MAIN_SOURCE"));
        assert!(!text_in_rows(&main_rows).contains("WRONG_SIBLING_SOURCE"));
        assert!(main_next > 0);
        let (child_rows, _, child_next) = handle
            .load_session_agent_transcript_for_session(owner, &dir, &child.to_string())
            .await
            .unwrap();
        assert_eq!(text_in_rows(&child_rows), "ADMITTED_CHILD_SOURCE");
        assert_eq!(child_rows[0].message_index, 7);
        assert_eq!(child_next, 11);
        // Exercise the public command path too; it must retain these exact
        // paths even though a same-UUID sibling is present in the new cwd.
        handle
            .submit(ClientCommand::LoadSessionAgentTranscript {
                agent_id: "main".into(),
            })
            .await
            .unwrap();
        handle
            .submit(ClientCommand::LoadSessionAgentTranscript {
                agent_id: child.to_string(),
            })
            .await
            .unwrap();
        handle
            .submit(ClientCommand::ListSessionAgents)
            .await
            .unwrap();

        handle
            .submit(ClientCommand::NewSession {
                cwd: Some(nested.to_string_lossy().into_owned()),
                model: None,
                mode: SessionModeDto::Code,
            })
            .await
            .unwrap();
        let (new_owner, new_dir) = handle.session_agent_dir().await.unwrap();
        assert_ne!(new_owner, owner);
        assert_ne!(new_dir, dir);
        let (new_main, _, _) = handle
            .load_session_agent_transcript_for_session(new_owner, &new_dir, "main")
            .await
            .unwrap();
        assert!(new_main.is_empty());
        let (new_child, _, _) = handle
            .load_session_agent_transcript_for_session(new_owner, &new_dir, &child.to_string())
            .await
            .unwrap();
        assert!(new_child.is_empty());
        assert_eq!(
            handle.inner.session_writer.session_target_path(owner),
            Some(source)
        );
    });
}

#[test]
fn mobile_parked_restore_after_real_cd_and_source_resume_uses_admitted_directory() {
    use lingxi_core::host::Platform;

    // Keep the production task-registry/LocalAgent restore path, while every
    // HTTP boundary is synthetic even if a restored worker gets to dispatch.
    struct NoNetworkPlatform(HostFakePlatform);
    impl Platform for NoNetworkPlatform {
        fn filesystem(&self) -> Arc<dyn lingxi_core::host::FileSystem> {
            self.0.filesystem()
        }
        fn http(&self) -> Arc<dyn lingxi_core::host::HttpTransport> {
            Arc::new(test_harness::mocks::MockHttpTransport::default())
        }
        fn clock(&self) -> Arc<dyn lingxi_core::host::Clock> {
            self.0.clock()
        }
        fn process(&self) -> Arc<dyn lingxi_core::host::ProcessRunner> {
            self.0.process()
        }
        fn sandbox(&self) -> Arc<dyn lingxi_core::host::Sandbox> {
            self.0.sandbox()
        }
        fn worktree(&self) -> Arc<dyn lingxi_core::host::WorktreeManager> {
            self.0.worktree()
        }
        fn secure_storage(&self) -> Option<Arc<dyn lingxi_core::host::SecureStorage>> {
            self.0.secure_storage()
        }
    }

    async fn parked_row(
        directory: &Path,
        owner: lingxi_core::types::SessionId,
        child: lingxi_core::types::AgentId,
        alias: &str,
        cwd: &Path,
    ) {
        seed_child(&directory.join(format!("agent-{child}.jsonl")), alias);
        session::agent_rows::write_row(
            directory,
            &session::agent_rows::ParkedAgentRow {
                task_id: alias.into(),
                agent_id: child,
                description: alias.into(),
                request: lingxi_core::host::SubagentSpawnRequest {
                    subagent_type: "general-purpose".into(),
                    description: Some(alias.into()),
                    prompt: "original prompt must not replay".into(),
                    cwd: Some(cwd.display().to_string()),
                    origin_session_id: Some(owner),
                    run_in_background: true,
                    ..Default::default()
                },
                handback_opt_in: false,
                handback_state: None,
                handback_history: Vec::new(),
            },
        )
        .await
        .unwrap();
        assert!(session::agent_rows::read_row(directory, &child.to_string())
            .await
            .is_some());
    }

    let root = tempfile::tempdir().unwrap();
    let platform = NoNetworkPlatform(
        HostFakePlatform::new(root.path().to_path_buf())
            .with_secure_storage(Arc::new(FakeEncryptedStore::default())),
    );
    let (handle, _) = build_submit_handle_with_platform(root.path(), Arc::new(platform));
    handle.task_notification_watcher.abort();
    handle.runtime().block_on(async {
        let owner = handle.inner.orchestrator.current_session_id().await;
        let owner_uuid = owner.as_uuid().to_string();
        let source = handle.inner.session_writer.session_target_path(owner).unwrap();
        let native = serde_json::json!({
            "type":"user", "uuid":uuid::Uuid::new_v4().to_string(), "parentUuid":null,
            "sessionId":owner_uuid, "cwd":handle.session_cwd(), "isSidechain":false,
            "timestamp":"2026-11-12T00:00:00Z",
            "message":{"role":"user","content":"resumable original source"}
        });
        let native = session::jsonl::reader::route_lines(&format!("{native}\n"))
            .messages_in_order.remove(0);
        handle.inner.session_writer.append_native_message_once(&native).await.unwrap();
        let after_cd = Path::new(&handle.session_cwd()).join("after-cd");
        std::fs::create_dir_all(&after_cd).unwrap();
        // This is the real mobile directory-control operation: it executes the
        // owned cwd/transcript transaction and writes relocatedCwd in source A.
        handle.change_workspace(after_cd.display().to_string(), owner_uuid.clone()).await.unwrap();
        assert_eq!(handle.inner.session_writer.active_path(), source);
        assert!(std::fs::read_to_string(&source).unwrap().contains("relocatedCwd"));
        assert_eq!(Path::new(&handle.session_cwd()), after_cd);
        let admitted_dir = source.with_extension("").join("subagents");
        let recomputed_dir = orchestrator::transcript_paths::subagents_dir(
            &handle.lingxi_home, &handle.session_cwd(), &owner_uuid,
        );
        assert_ne!(admitted_dir, recomputed_dir);
        handle.submit(ClientCommand::NewSession {
            cwd: None, model: None, mode: SessionModeDto::Code,
        }).await.unwrap();
        assert_ne!(handle.inner.orchestrator.current_session_id().await, owner);

        let original_child = lingxi_core::types::AgentId::new();
        let sibling_child = lingxi_core::types::AgentId::new();
        parked_row(&admitted_dir, owner, original_child, "admitted-parked-child", &after_cd).await;
        parked_row(&recomputed_dir, owner, sibling_child, "wrong-cwd-parked-child", &after_cd).await;
        assert!(handle.inner.task_registry.resolve_task_id("admitted-parked-child").await.is_none());
        handle.resume_workspace_session(owner_uuid, source.display().to_string()).await.unwrap();
        assert_eq!(handle.inner.session_writer.active_path(), source);
        assert_eq!(Path::new(&handle.session_cwd()), after_cd);
        assert!(std::fs::symlink_metadata(&source).unwrap().file_type().is_file());
        let registry = &handle.inner.task_registry;
        let restored_id = registry.resolve_task_id("admitted-parked-child").await
            .expect("exact-source resume restores the parked .task.json from source A");
        let restored = registry.get(&restored_id).await.unwrap();
        assert!(matches!(restored, tasks::state::TaskState::LocalAgent(ref row) if row.agent_id == original_child));
        assert!(registry.resolve_task_id("wrong-cwd-parked-child").await.is_none(),
            "same-session sibling storage under relocated cwd is not the admitted source");
    });
}

#[cfg(unix)]
#[test]
fn mobile_child_viewer_rejects_a_symlinked_parent_without_mutating_foreign_rows() {
    let root = tempfile::tempdir().unwrap();
    let foreign = tempfile::tempdir().unwrap();
    let (handle, _) = build_submit_handle(root.path());
    handle.runtime().block_on(async {
        let (owner, dir) = handle.session_agent_dir().await.unwrap();
        let child = lingxi_core::types::AgentId::new();
        let foreign_path = foreign.path().join(format!("agent-{child}.jsonl"));
        seed_child(&foreign_path, "FOREIGN_PRIVATE_BODY");
        let before = std::fs::read(&foreign_path).unwrap();
        std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(foreign.path(), &dir).unwrap();
        assert!(handle
            .load_session_agent_transcript_for_session(owner, &dir, &child.to_string())
            .await
            .is_err());
        assert_eq!(std::fs::read(foreign_path).unwrap(), before);
    });
}

#[tokio::test]
async fn active_child_body_eviction_keeps_full_notifications_binding_and_message_indices() {
    let listener = Arc::new(FakeListener::default());
    let owner = lingxi_core::types::SessionId::from_uuid(uuid::Uuid::new_v4());
    let observer = MobileSessionAgentObserver::new(
        ListenerSink::arc(listener.clone()),
        Arc::new(StdMutex::new("other-session".into())),
    );
    *observer.transcript_cache.lock().await =
        super::super::session_agent_cache::Cache::with_budget(2048);
    let child = lingxi_core::types::AgentId::new();
    observer
        .on_event(SubagentObservation::Allocated {
            agent_id: child,
            agent_type: "fixture".into(),
            name: None,
            model: "fixture".into(),
            model_profile: None,
            persistent: true,
            initial_message_index: 4,
            origin_session_id: Some(owner),
        })
        .await;
    let huge = lingxi_core::types::ConversationMessage::user(
        lingxi_core::types::MessageId::new(),
        "x".repeat(100_000).into(),
    );
    observer
        .on_event(SubagentObservation::Message {
            agent_id: child,
            message: huge,
        })
        .await;
    let key = (owner.as_uuid().to_string(), child.to_string());
    {
        let mut cache = observer.transcript_cache.lock().await;
        assert!(cache.owned_bytes() <= 2048);
        let snapshot = cache.get(&key).unwrap();
        assert!(snapshot.rows.is_empty());
        assert_eq!(snapshot.next_message_index, 5);
    }
    observer
        .on_event(SubagentObservation::Message {
            agent_id: child,
            message: lingxi_core::types::ConversationMessage::user(
                lingxi_core::types::MessageId::new(),
                "still bound".into(),
            ),
        })
        .await;
    let api_error = lingxi_core::host::ServerFallbackApiErrorRow::new(
        &"fallback declined".into(),
        "2026-11-12T00:00:00Z".into(),
    );
    observer
        .on_event(SubagentObservation::ServerFallbackApiErrorRow {
            agent_id: child,
            row: api_error.clone(),
            message_index: 6,
        })
        .await;
    observer
        .on_event(SubagentObservation::ServerFallbackTombstone {
            agent_id: child,
            message: lingxi_core::host::ServerFallbackTombstoneMessage {
                uuid: api_error.uuid,
                message_type: "assistant".into(),
                timestamp: api_error.timestamp.clone(),
                request_id: api_error.request_id.clone(),
                request_ref: None,
                provider_message_id: Some(api_error.message.id.as_uuid().to_string()),
                model: Some(api_error.message.model.clone()),
                stop_reason: Some(api_error.message.stop_reason.clone()),
                stop_details: api_error.message.stop_details.clone(),
                usage: Some(source_value(&api_error.message.usage)),
                content: api_error.message.content.clone(),
                is_api_error_message: Some(true),
                supersedes_uuids: None,
            },
            display_only: true,
        })
        .await;
    observer
        .on_event(SubagentObservation::Message {
            agent_id: child,
            message: lingxi_core::types::ConversationMessage::user(
                lingxi_core::types::MessageId::new(),
                "after tombstone".into(),
            ),
        })
        .await;
    let events = listener.received.lock().await;
    let rows = events
        .iter()
        .filter_map(|event| match event {
            Ev::SessionAgentMessage {
                session_id,
                agent_id,
                row,
            } if agent_id == &child.to_string() => {
                assert_eq!(session_id, &owner.as_uuid().to_string());
                Some(row)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        rows.iter().map(|row| row.message_index).collect::<Vec<_>>(),
        vec![4, 5, 6, 7]
    );
    assert_eq!(text_in_rows(&[(*rows[0]).clone()]).len(), 100_000);
    assert_eq!(text_in_rows(&[(*rows[1]).clone()]), "still bound");
    assert!(rows[2].api_error_json.is_some());
    assert_eq!(text_in_rows(&[(*rows[3]).clone()]), "after tombstone");
    assert!(events
        .iter()
        .any(|event| matches!(event, Ev::SessionAgentTombstone {
        session_id, agent_id, message_uuid, ..
    } if session_id == &owner.as_uuid().to_string() && agent_id == &child.to_string()
        && message_uuid == &api_error.uuid.as_uuid().to_string())));
    drop(events);
    assert!(observer
        .bound_agents
        .lock()
        .await
        .contains_key(&child.to_string()));
    let mut cache = observer.transcript_cache.lock().await;
    assert!(cache.owned_bytes() <= 2048);
    let snapshot = cache.get(&key).unwrap();
    assert_eq!(snapshot.next_message_index, 8);
    assert!(!snapshot
        .rows
        .iter()
        .any(|row| row.message_uuid == api_error.uuid.as_uuid().to_string()));
    assert!(snapshot
        .rows
        .windows(2)
        .all(|rows| rows[0].message_index < rows[1].message_index));
}

fn check_claimed_coalesced_resume(cold: bool) {
    use crate::mobile::test_support::new_engine_with_streaming;
    use orchestrator::test_support_stream::*;
    let root = tempfile::tempdir().unwrap();
    let stream = Arc::new(MockStreamingApiClient::with_turns(vec![vec![
        message_start("coalesced-resume", "test"),
        content_block_start_text(0),
        text_delta(0, "recovered"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ]]));
    let build = || {
        new_engine_with_streaming(
            test_config(root.path()),
            Arc::new(HostFakePlatform::new(root.path().to_owned())),
            Arc::new(FakeListener::default()),
            Arc::new(RecordingPermissionSink::default()),
            Some(stream.clone()),
        )
        .unwrap()
    };
    let first = build();
    let (session, owner, last_id) = first.runtime().block_on(async {
        let session = first
            .inner
            .orchestrator
            .current_session_id()
            .await
            .as_uuid()
            .to_string();
        first
            .inner
            .session_writer
            .append_mobile_empty_session(&session, "coalesced recovery", &first.session_cwd())
            .await
            .unwrap();
        let mut members = Vec::new();
        for (turn, text, mime) in [(11, "first", "image/png"), (12, "second", "image/jpeg")] {
            members.push(
                first
                    .durable_turns
                    .admit_queued_prompt(
                        &session,
                        Some(turn),
                        Some(turn),
                        text.into(),
                        None,
                        vec![client::protocol::commands::ImageRefDto {
                            media_type: mime.into(),
                            base64: "aGVsbG8=".into(),
                        }],
                    )
                    .unwrap(),
            );
        }
        let batch = first
            .durable_turns
            .prepare_mid_turn_batch(&session, &[members[0].turn_id, members[1].turn_id])
            .unwrap();
        first
            .durable_turns
            .claim_pending(&session, batch.owner_turn_id)
            .unwrap();
        first
            .submit(ClientCommand::PauseTurn {
                turn_id: batch.owner_turn_id,
                reason: "cancelled_after_claim_before_dispatch".into(),
            })
            .await
            .unwrap();
        (
            session,
            batch.owner_turn_id,
            batch.aggregate_identity.message_id,
        )
    });
    let resumed = if cold {
        drop(first);
        build()
    } else {
        first
    };
    resumed.runtime().block_on(async {
        if cold {
            resumed
                .resume_seeded_test_session(session.clone())
                .await
                .unwrap();
        }
        resumed
            .submit(ClientCommand::ResumeTurn { turn_id: owner })
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if !stream.captured_calls().await.is_empty()
                    && resumed.active_cancel.lock().await.is_none()
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let calls = stream.captured_calls().await;
        assert_eq!(calls.len(), 1);
        let aggregate = calls[0]
            .messages
            .iter()
            .find(|row| row.id() == last_id)
            .expect("last member owns aggregate identity");
        assert!(aggregate.text_content().contains("first\nsecond"));
        if let lingxi_core::types::ConversationMessage::User { content, .. } = aggregate {
            let images = content
                .iter()
                .filter_map(|block| match block {
                    lingxi_core::types::ContentBlock::Image {
                        source: lingxi_core::types::ImageSource::Base64 { media_type, .. },
                    } => Some(media_type.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(images, ["image/png", "image/jpeg"]);
        } else {
            panic!("aggregate is a user row");
        }
        assert!(
            resumed
                .durable_turns
                .scan_pending(&session)
                .unwrap()
                .is_empty(),
            "retired members must not dispatch again"
        );
    });
}

#[test]
fn mobile_claimed_coalesced_cancel_then_warm_resume_preserves_every_member() {
    check_claimed_coalesced_resume(false);
}
#[test]
fn mobile_claimed_coalesced_cancel_then_cold_resume_preserves_every_member() {
    check_claimed_coalesced_resume(true);
}

fn seed_cancellable_coalesced(handle: &MobileEngineHandle, session: &str, claimed: bool) {
    for id in [811, 812] {
        handle
            .durable_turns
            .admit_queued_prompt(
                session,
                Some(id),
                Some(id),
                format!("queued-{id}").into(),
                None,
                Vec::new(),
            )
            .unwrap();
    }
    handle
        .durable_turns
        .prepare_mid_turn_batch(session, &[811, 812])
        .unwrap();
    if claimed {
        handle.durable_turns.claim_pending(session, 811).unwrap();
    }
}

#[test]
fn mobile_public_cancel_retires_cold_coalesced_owner_and_every_member() {
    for cold in [false, true] {
        for requested in [811, 812] {
            let root = tempfile::tempdir().unwrap();
            let (first, first_listener) = build_submit_handle(root.path());
            first.task_notification_watcher.abort();
            let session = first.runtime().block_on(async {
                let session = first.active_session_id();
                first
                    .inner
                    .session_writer
                    .append_mobile_empty_session(&session, "cancel recovery", &first.session_cwd())
                    .await
                    .unwrap();
                seed_cancellable_coalesced(&first, &session, false);
                session
            });
            let (handle, listener) = if cold {
                drop(first);
                let built = build_submit_handle(root.path());
                built.0.task_notification_watcher.abort();
                built
            } else {
                (first, first_listener)
            };
            handle.runtime().block_on(async {
                if cold {
                    handle
                        .resume_seeded_test_session(session.clone())
                        .await
                        .unwrap();
                }
                // Hold the worker's normal transition gate to select the
                // legitimate recovered-but-not-yet-reserved cancellation edge.
                let _transition = handle.loop_transition.lock().await;
                rebuild_mobile_prompt_dispatch_cache(
                    &handle.message_queue,
                    &handle.durable_turns,
                    &session,
                )
                .await
                .unwrap();
                handle
                    .submit(ClientCommand::Cancel {
                        turn_id: Some(requested),
                    })
                    .await
                    .unwrap();
                assert!(handle.active_cancel.lock().await.is_none());
                assert_eq!(
                    handle.durable_turns.load(&session, 811).unwrap().state,
                    Some(TurnRecoveryStateDto::Cancelled)
                );
                rebuild_mobile_prompt_dispatch_cache(
                    &handle.message_queue,
                    &handle.durable_turns,
                    &session,
                )
                .await
                .unwrap();
                assert!(handle
                    .message_queue
                    .snapshot()
                    .await
                    .iter()
                    .all(|command| !is_mobile_dispatch_command(&command.uuid)));
                let cold_store = DurableTurnStore::new(handle.lingxi_home.join("mobile-turns"));
                assert!(cold_store.scan_pending(&session).unwrap().is_empty());
                let events = listener.received.lock().await;
                for member in [811, 812] {
                    assert!(events.iter().any(|event| matches!(event,
                        Ev::TurnRecoveryState { snapshot } if snapshot.turn_id == member
                            && snapshot.state == TurnRecoveryStateDto::Cancelled)));
                }
            });
        }
    }
}

#[test]
fn mobile_public_stale_coalesced_cancel_preserves_unrelated_stream_and_followup() {
    use crate::mobile::test_support::new_engine_with_streaming;
    let root = tempfile::tempdir().unwrap();
    let (first, _) = build_submit_handle(root.path());
    first.task_notification_watcher.abort();
    let session = first.runtime().block_on(async {
        let session = first.active_session_id();
        first
            .inner
            .session_writer
            .append_mobile_empty_session(&session, "old claimed group", &first.session_cwd())
            .await
            .unwrap();
        seed_cancellable_coalesced(&first, &session, true);
        first
            .durable_turns
            .begin(
                &session,
                900,
                "new independent owner".into(),
                None,
                Vec::new(),
            )
            .unwrap();
        first
            .durable_turns
            .transition(
                &session,
                900,
                TurnRecoveryStateDto::PausedRecoverable,
                true,
                Some("explicit continuation fixture".into()),
            )
            .unwrap();
        session
    });
    drop(first);
    let (entered, entered_rx) = tokio::sync::oneshot::channel();
    let stream = Arc::new(PendingPromptStreamClient::new(entered));
    let handle = new_engine_with_streaming(
        test_config(root.path()),
        Arc::new(HostFakePlatform::new(root.path().to_owned())),
        Arc::new(FakeListener::default()),
        Arc::new(RecordingPermissionSink::default()),
        Some(stream.clone()),
    )
    .unwrap();
    handle.task_notification_watcher.abort();
    handle.runtime().block_on(async {
        handle
            .resume_seeded_test_session(session.clone())
            .await
            .unwrap();
        handle
            // A cold Running group deliberately blocks new automatic work.
            // Explicit continuation is authorized to establish a separate
            // live owner without making that stale group its own input.
            .submit(ClientCommand::ResumeTurn { turn_id: 900 })
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), entered_rx)
            .await
            .unwrap()
            .unwrap();
        let owner = handle.active_cancel.lock().await.clone().unwrap();
        assert_eq!(owner.turn_id, Some(900));
        let calls = stream.captured_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0]
                .messages
                .iter()
                .filter(|row| row.text_content() == "new independent owner")
                .count(),
            1
        );
        assert!(calls[0]
            .messages
            .iter()
            .all(|row| !row.text_content().contains("queued-811")
                && !row.text_content().contains("queued-812")));
        handle
            .submit(ClientCommand::Cancel { turn_id: Some(812) })
            .await
            .unwrap();
        assert!(!owner.cancel.is_cancelled());
        assert!(handle
            .active_cancel
            .lock()
            .await
            .as_ref()
            .is_some_and(|live| Arc::ptr_eq(live, &owner)));
        assert_eq!(
            handle.durable_turns.load(&session, 811).unwrap().state,
            Some(TurnRecoveryStateDto::Cancelled)
        );
        handle
            .submit(ClientCommand::SendPrompt {
                text: "live append remains accepted".into(),
                prompt_mode: None,
                images: Vec::new(),
                turn_id: Some(901),
                visualization_context: None,
            })
            .await
            .unwrap();
        assert_eq!(
            handle
                .durable_turns
                .scan_pending(&session)
                .unwrap()
                .iter()
                .map(|row| row.turn_id)
                .collect::<Vec<_>>(),
            [901]
        );
        handle
            .submit(ClientCommand::Cancel { turn_id: Some(901) })
            .await
            .unwrap();
        assert!(!owner.cancel.is_cancelled());
        handle
            .submit(ClientCommand::Cancel { turn_id: Some(900) })
            .await
            .unwrap();
        handle
            .submit(ClientCommand::SendPrompt {
                text: "continue after cancel".into(),
                prompt_mode: None,
                images: Vec::new(),
                turn_id: Some(902),
                visualization_context: None,
            })
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while stream.captured_calls().len() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            handle.active_cancel.lock().await.as_ref().unwrap().turn_id,
            Some(902)
        );
        handle
            .submit(ClientCommand::Cancel { turn_id: Some(902) })
            .await
            .unwrap();
    });
}

#[test]
fn mobile_public_midturn_member_cancel_unwinds_only_its_bound_executor() {
    use crate::mobile::test_support::new_engine_with_streaming;
    use orchestrator::prompt::mid_turn_input::MidTurnInputSource;
    let root = tempfile::tempdir().unwrap();
    let (entered, entered_rx) = tokio::sync::oneshot::channel();
    let handle = new_engine_with_streaming(
        test_config(root.path()),
        Arc::new(HostFakePlatform::new(root.path().to_owned())),
        Arc::new(FakeListener::default()),
        Arc::new(RecordingPermissionSink::default()),
        Some(Arc::new(PendingPromptStreamClient::new(entered))),
    )
    .unwrap();
    handle.task_notification_watcher.abort();
    handle.runtime().block_on(async {
        handle
            .submit(ClientCommand::SendPrompt {
                text: "current".into(),
                prompt_mode: None,
                images: Vec::new(),
                turn_id: Some(900),
                visualization_context: None,
            })
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), entered_rx)
            .await
            .unwrap()
            .unwrap();
        for id in [811, 812] {
            handle
                .submit(ClientCommand::SendPrompt {
                    text: format!("append-{id}").into(),
                    prompt_mode: None,
                    images: Vec::new(),
                    turn_id: Some(id),
                    visualization_context: None,
                })
                .await
                .unwrap();
        }
        let source = MobileMsgQueueInput {
            queue: handle.message_queue.clone(),
            loop_state: Arc::new(tool_cron::LoopRuntime::default()),
            durable_turns: handle.durable_turns.clone(),
            active_session_uuid: handle.inner.active_session_uuid.clone(),
            active_turn: Some(handle.active_cancel.clone()),
        };
        let inputs = source.take_mid_turn_batch().await.unwrap().unwrap();
        assert_eq!(inputs.len(), 2);
        handle
            .submit(ClientCommand::Cancel { turn_id: Some(812) })
            .await
            .unwrap();
        assert!(handle.active_cancel.lock().await.is_none());
        assert_eq!(
            handle
                .durable_turns
                .load(&handle.active_session_id(), 900)
                .unwrap()
                .state,
            Some(TurnRecoveryStateDto::Cancelled)
        );
        assert!(handle
            .durable_turns
            .scan_pending(&handle.active_session_id())
            .unwrap()
            .is_empty());
    });
}

#[derive(Default)]
struct CoalescedCancelBoundaryListener {
    recorded: FakeListener,
    terminal_entered: Notify,
    terminal_release: Notify,
}

#[async_trait]
impl ClientEventListener for CoalescedCancelBoundaryListener {
    async fn on_event(&self, event: ClientEvent) {
        let is_terminal = matches!(&event, Ev::TurnEnded { .. });
        self.recorded.on_event(event).await;
        if is_terminal {
            self.terminal_entered.notify_one();
            self.terminal_release.notified().await;
        }
    }

    async fn on_workflow_progress(
        &self,
        origin_session_id: String,
        task_id: String,
        run_id: String,
        progress: client::protocol::listings::WorkflowProgressDto,
    ) {
        self.recorded
            .on_workflow_progress(origin_session_id, task_id, run_id, progress)
            .await;
    }
}

#[test]
fn mobile_public_bound_member_cancel_retires_group_before_owner_release() {
    use crate::mobile::test_support::new_engine_with_streaming;
    use orchestrator::prompt::mid_turn_input::MidTurnInputSource;
    let wait = std::time::Duration::from_secs(10);
    for requested in [811, 812] {
        for uncertain in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let listener = Arc::new(CoalescedCancelBoundaryListener::default());
            let (entered, entered_rx) = tokio::sync::oneshot::channel();
            let handle = new_engine_with_streaming(
                test_config(root.path()),
                Arc::new(HostFakePlatform::new(root.path().to_owned())),
                listener.clone(),
                Arc::new(RecordingPermissionSink::default()),
                Some(Arc::new(PendingPromptStreamClient::new(entered))),
            )
            .unwrap();
            // Keep the production queue watcher running. The controlled
            // terminal callback prevents the real executor from releasing its
            // slot until both durable and volatile retirement are inspected.
            handle.runtime().block_on(async {
                handle
                    .submit(ClientCommand::SendPrompt {
                        text: "current".into(),
                        prompt_mode: None,
                        images: Vec::new(),
                        turn_id: Some(900),
                        visualization_context: None,
                    })
                    .await
                    .unwrap();
                tokio::time::timeout(wait, entered_rx)
                    .await
                    .unwrap()
                    .unwrap();
                let owner = handle.active_cancel.lock().await.clone().unwrap();
                let session = owner.session_id.clone();
                for id in [811, 812] {
                    handle
                        .submit(ClientCommand::SendPrompt {
                            text: format!("append-{id}").into(),
                            prompt_mode: None,
                            images: Vec::new(),
                            turn_id: Some(id),
                            visualization_context: None,
                        })
                        .await
                        .unwrap();
                }
                let source = MobileMsgQueueInput {
                    queue: handle.message_queue.clone(),
                    loop_state: Arc::new(tool_cron::LoopRuntime::default()),
                    durable_turns: handle.durable_turns.clone(),
                    active_session_uuid: handle.inner.active_session_uuid.clone(),
                    active_turn: Some(handle.active_cancel.clone()),
                };
                assert_eq!(
                    source.take_mid_turn_batch().await.unwrap().unwrap().len(),
                    2
                );
                if uncertain {
                    handle
                        .durable_turns
                        .fail_next_metadata_after_publish_for_test();
                }
                let canceller = handle.clone();
                let cancellation = tokio::spawn(async move {
                    canceller
                        .submit(ClientCommand::Cancel {
                            turn_id: Some(requested),
                        })
                        .await
                });
                tokio::time::timeout(wait, listener.terminal_entered.notified())
                    .await
                    .unwrap();
                let owns_slot = handle
                    .active_cancel
                    .lock()
                    .await
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &owner));
                let warm_pending = handle.durable_turns.scan_pending(&session).unwrap();
                let cold_pending = DurableTurnStore::new(handle.lingxi_home.join("mobile-turns"))
                    .scan_pending(&session)
                    .unwrap();
                let dispatches = handle.message_queue.snapshot().await;
                tokio::time::timeout(wait, async {
                    while owner
                        .task
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_some_and(|task| task.join.is_some())
                    {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                // The first cancellation now owns the executor join. A
                // concurrent retry must still await that live owner, even
                // though the envelope already contains its terminal state.
                let retry = handle.submit(ClientCommand::Cancel {
                    turn_id: Some(requested),
                });
                tokio::pin!(retry);
                let retry_before_release = futures_util::poll!(retry.as_mut());
                let retry_returned_before_release = retry_before_release.is_ready();
                let member_proof_before_release =
                    listener.recorded.received.lock().await.iter().any(|event| {
                        matches!(event, Ev::TurnRecoveryState { snapshot }
                        if [811, 812].contains(&snapshot.turn_id)
                            && snapshot.state == TurnRecoveryStateDto::Cancelled)
                    });
                // Always release the callback before making assertions so a
                // failing regression cannot leave engine teardown parked.
                listener.terminal_release.notify_one();
                let result = tokio::time::timeout(wait, cancellation)
                    .await
                    .unwrap()
                    .unwrap();
                let retry_result = match retry_before_release {
                    std::task::Poll::Ready(result) => result,
                    std::task::Poll::Pending => tokio::time::timeout(wait, retry).await.unwrap(),
                };
                assert!(owns_slot, "retirement must precede executor slot release");
                assert!(owner.cancel.is_cancelled());
                assert!(
                    !retry_returned_before_release,
                    "a retry must await the same live executor"
                );
                assert!(
                    !member_proof_before_release,
                    "member proof must follow executor unwind"
                );
                retry_result.unwrap();
                assert!(
                    warm_pending.is_empty(),
                    "the live store must retire the group before release"
                );
                assert!(
                    cold_pending.is_empty(),
                    "the group commit must survive a cold open before release"
                );
                assert!(
                    dispatches
                        .iter()
                        .all(|command| !is_mobile_dispatch_command(&command.uuid)),
                    "the watcher must not inherit an unacknowledged group marker"
                );
                if uncertain {
                    assert!(result.is_err());
                    handle
                        .submit(ClientCommand::Cancel {
                            turn_id: Some(requested),
                        })
                        .await
                        .unwrap();
                } else {
                    result.unwrap();
                }
                let events = listener.recorded.received.lock().await;
                for member in [811, 812] {
                    assert!(events.iter().any(|event| matches!(event,
                        Ev::TurnRecoveryState { snapshot } if snapshot.turn_id == member
                            && snapshot.state == TurnRecoveryStateDto::Cancelled)));
                }
            });
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum CoalescedClaimBoundary {
    Running,
    Started,
}

struct CoalescedWatcherClaimListener {
    boundary: CoalescedClaimBoundary,
    recorded: FakeListener,
    entered: Notify,
    release: Notify,
    release_probe: StdMutex<Option<CoalescedClaimReleaseProbe>>,
    early_proof: AtomicBool,
}

#[derive(Clone)]
struct CoalescedClaimReleaseProbe {
    owner: Arc<super::super::ActiveTurn>,
    slot: Arc<tokio::sync::Mutex<Option<Arc<super::super::ActiveTurn>>>>,
}

impl CoalescedWatcherClaimListener {
    fn new(boundary: CoalescedClaimBoundary) -> Self {
        Self {
            boundary,
            recorded: FakeListener::default(),
            entered: Notify::new(),
            release: Notify::new(),
            release_probe: StdMutex::new(None),
            early_proof: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl ClientEventListener for CoalescedWatcherClaimListener {
    async fn on_event(&self, event: ClientEvent) {
        if matches!(&event, Ev::TurnRecoveryState { snapshot }
            if [811, 812].contains(&snapshot.turn_id)
                && snapshot.state == TurnRecoveryStateDto::Cancelled)
        {
            let probe = self.release_probe.lock().unwrap().clone();
            if let Some(probe) = probe {
                let completed = probe.owner.completed.load(Ordering::Acquire);
                let released = probe.slot.lock().await.is_none();
                if !completed || !released {
                    self.early_proof.store(true, Ordering::Release);
                }
            }
        }
        let selected = match (self.boundary, &event) {
            (CoalescedClaimBoundary::Running, Ev::TurnRecoveryState { snapshot }) => {
                snapshot.turn_id == 811 && snapshot.state == TurnRecoveryStateDto::Running
            }
            (CoalescedClaimBoundary::Started, Ev::TurnStarted { turn_id }) => *turn_id == Some(811),
            _ => false,
        };
        self.recorded.on_event(event).await;
        if selected {
            self.entered.notify_one();
            self.release.notified().await;
        }
    }

    async fn on_workflow_progress(
        &self,
        origin_session_id: String,
        task_id: String,
        run_id: String,
        progress: client::protocol::listings::WorkflowProgressDto,
    ) {
        self.recorded
            .on_workflow_progress(origin_session_id, task_id, run_id, progress)
            .await;
    }
}

#[test]
fn mobile_public_watcher_claim_cancel_proves_every_unconsumed_member_after_release() {
    use crate::mobile::test_support::new_engine_with_streaming;
    let wait = std::time::Duration::from_secs(10);
    for boundary in [
        CoalescedClaimBoundary::Running,
        CoalescedClaimBoundary::Started,
    ] {
        for requested in [Some(811), None, Some(812)] {
            for uncertain in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let listener = Arc::new(CoalescedWatcherClaimListener::new(boundary));
                let (entered, _entered_rx) = tokio::sync::oneshot::channel();
                let stream = Arc::new(PendingPromptStreamClient::new(entered));
                let handle = new_engine_with_streaming(
                    test_config(root.path()),
                    Arc::new(HostFakePlatform::new(root.path().to_owned())),
                    listener.clone(),
                    Arc::new(RecordingPermissionSink::default()),
                    Some(stream.clone()),
                )
                .unwrap();
                handle.runtime().block_on(async {
                    let session = handle.active_session_id();
                    {
                        let _transition = handle.loop_transition.lock().await;
                        seed_cancellable_coalesced(&handle, &session, false);
                        rebuild_mobile_prompt_dispatch_cache(
                            &handle.message_queue,
                            &handle.durable_turns,
                            &session,
                        )
                        .await
                        .unwrap();
                    }
                    tokio::time::timeout(wait, listener.entered.notified())
                        .await
                        .unwrap();
                    let owner = handle.active_cancel.lock().await.clone().unwrap();
                    assert_eq!(owner.turn_id, Some(811));
                    *listener.release_probe.lock().unwrap() = Some(CoalescedClaimReleaseProbe {
                        owner: owner.clone(),
                        slot: handle.active_cancel.clone(),
                    });
                    assert_eq!(
                        handle.durable_turns.load(&session, 811).unwrap().queue_delivery,
                        QueueDeliveryState::Claimed
                    );
                    if uncertain {
                        handle.durable_turns.fail_next_metadata_after_publish_for_test();
                    }
                    // Primary Stop must signal before waiting on this gate.
                    let signal_gate = if requested != Some(812) {
                        Some(owner.initial_event_gate.lock().await)
                    } else {
                        None
                    };
                    let canceller = handle.clone();
                    let cancellation = tokio::spawn(async move {
                        canceller.submit(ClientCommand::Cancel { turn_id: requested }).await
                    });
                    tokio::time::timeout(wait, owner.cancel.cancelled())
                        .await
                        .unwrap();
                    drop(signal_gate);
                    tokio::time::timeout(wait, async {
                        while handle.durable_turns.load(&session, 811).unwrap().state
                            != Some(TurnRecoveryStateDto::Cancelled)
                        {
                            tokio::task::yield_now().await;
                        }
                    })
                    .await
                    .unwrap();
                    // The commit becomes visible before queue retirement's
                    // await. This gate also waits for that retirement to end.
                    drop(owner.initial_event_gate.lock().await);
                    // The foreign initial callback stays permanently parked.
                    // Cancellation may drop that callback; only the actual
                    // native owner completion and slot release are barriers.
                    let warm_pending = handle.durable_turns.scan_pending(&session).unwrap();
                    let cold_pending = DurableTurnStore::new(handle.lingxi_home.join("mobile-turns"))
                        .scan_pending(&session)
                        .unwrap();
                    let dispatches = handle.message_queue.snapshot().await;
                    let result = tokio::time::timeout(wait, cancellation).await.unwrap().unwrap();
                    assert!(owner.completed.load(Ordering::Acquire));
                    assert!(handle.active_cancel.lock().await.is_none());
                    assert!(!handle.message_queue.has_active_turn().await);
                    assert!(!listener.early_proof.load(Ordering::Acquire), "every group proof must follow native owner completion and slot release");
                    // After release, Cancel(None) has no target. Retry the
                    // exact durable member explicitly so uncertain publication
                    // still proves the whole group, with or without an owner.
                    tokio::time::timeout(wait, handle.submit(ClientCommand::Cancel { turn_id: Some(812) }))
                        .await.unwrap().unwrap();
                    assert!(warm_pending.is_empty());
                    assert!(cold_pending.is_empty());
                    assert!(dispatches.iter().all(|command| !is_mobile_dispatch_command(&command.uuid)));
                    assert!(!listener.early_proof.load(Ordering::Acquire));
                    if uncertain {
                        assert!(result.is_err());
                    } else {
                        result.unwrap();
                    }
                    assert!(stream.captured_calls().is_empty(), "pre-cancelled queue input must not reach the model");
                    let events = listener.recorded.received.lock().await;
                    assert!(!events.iter().any(|event| matches!(event,
                        Ev::UserTranscriptRowIdentity { row_token, .. } if row_token == "client-turn:811,812")));
                    for member in [811, 812] {
                        assert_eq!(events.iter().filter(|event| matches!(event,
                            Ev::TurnRecoveryState { snapshot } if snapshot.turn_id == member
                                && snapshot.state == TurnRecoveryStateDto::Cancelled)).count(),
                            if uncertain { 1 } else { 2 },
                            "each successful Cancel publishes one proof per original member; boundary={boundary:?}, requested={requested:?}, uncertain={uncertain}, events={events:?}");
                    }
                });
            }
        }
    }
}

#[test]
fn mobile_public_watcher_consumed_group_cancel_preserves_its_row_and_unrelated_pending() {
    use crate::mobile::test_support::new_engine_with_streaming;
    let wait = std::time::Duration::from_secs(10);
    for requested in [Some(811), None, Some(812)] {
        let root = tempfile::tempdir().unwrap();
        let listener = Arc::new(FakeListener::default());
        let (entered, entered_rx) = tokio::sync::oneshot::channel();
        let handle = new_engine_with_streaming(
            test_config(root.path()),
            Arc::new(HostFakePlatform::new(root.path().to_owned())),
            listener.clone(),
            Arc::new(RecordingPermissionSink::default()),
            Some(Arc::new(PendingPromptStreamClient::new(entered))),
        )
        .unwrap();
        handle.runtime().block_on(async {
            let session = handle.active_session_id();
            {
                let _transition = tokio::time::timeout(wait, handle.loop_transition.lock()).await
                    .expect("consumed-group initial transition must be available");
                seed_cancellable_coalesced(&handle, &session, false);
                tokio::time::timeout(wait, rebuild_mobile_prompt_dispatch_cache(&handle.message_queue, &handle.durable_turns, &session))
                    .await.expect("consumed-group queue rebuild must finish").unwrap();
            }
            tokio::time::timeout(wait, entered_rx).await.unwrap().unwrap();
            let aggregate_id = handle.durable_turns.load(&session, 811).unwrap()
                .coalesced_batch.unwrap().aggregate_identity.message_id;
            assert!(listener.received.lock().await.iter().any(|event| matches!(event,
                Ev::UserTranscriptRowIdentity { row_token, .. } if row_token == "client-turn:811,812")));
            tokio::time::timeout(wait, handle.submit(ClientCommand::SendPrompt {
                text: "unrelated pending".into(), prompt_mode: None, images: Vec::new(),
                turn_id: Some(821), visualization_context: None,
            })).await.expect("consumed-group busy SendPrompt must finish").unwrap();
            let _transition = tokio::time::timeout(wait, handle.loop_transition.lock()).await
                .expect("consumed-group watcher transition must be available while streaming");
            tokio::time::timeout(wait, handle.submit(ClientCommand::Cancel { turn_id: requested })).await
                .expect("consumed-group Cancel must unwind the executor").unwrap();
            assert!(handle.active_cancel.lock().await.is_none());
            let history = handle.inner.orchestrator.snapshot_history().await;
            let aggregate = history.iter().filter(|message| message.id() == aggregate_id).collect::<Vec<_>>();
            assert_eq!(aggregate.len(), 1);
            assert!(aggregate[0].text_content().contains("queued-811\nqueued-812"));
            assert_eq!(handle.durable_turns.scan_pending(&session).unwrap().iter()
                .map(|checkpoint| checkpoint.turn_id).collect::<Vec<_>>(), [821]);
            let events = listener.received.lock().await;
            assert_eq!(events.iter().filter(|event| matches!(event,
                Ev::UserTranscriptRowIdentity { row_token, .. } if row_token == "client-turn:811,812")).count(), 1);
            for member in [811, 812] {
                assert_eq!(events.iter().filter(|event| matches!(event,
                    Ev::TurnRecoveryState { snapshot } if snapshot.turn_id == member
                        && snapshot.state == TurnRecoveryStateDto::Cancelled)).count(), 1);
            }
            drop(events);
            tokio::time::timeout(wait, handle.submit(ClientCommand::Cancel { turn_id: Some(821) })).await
                .expect("consumed-group cleanup cancellation must finish").unwrap();
        });
    }
}

struct GatedCompletionThenPendingClient {
    first: orchestrator::test_support_stream::MockStreamingApiClient,
    pending: PendingPromptStreamClient,
    calls: AtomicUsize,
    first_entered: Notify,
    first_release: Notify,
}

#[async_trait]
impl orchestrator::StreamingApiClient for GatedCompletionThenPendingClient {
    async fn stream(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
        messages: Vec<lingxi_core::types::ConversationMessage>,
        tools: Vec<llm_runtime::ToolDeclaration>,
        query_source: &str,
        skip_global_cache_for_system_prompt: bool,
        request_dispatch_admission: Option<llm_runtime::RequestDispatchAdmission>,
    ) -> Result<
        futures_util::stream::BoxStream<
            'static,
            Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
        >,
        llm_runtime::LlmError,
    > {
        let client: &dyn orchestrator::StreamingApiClient =
            if self.calls.fetch_add(1, Ordering::AcqRel) == 0 {
                self.first_entered.notify_one();
                self.first_release.notified().await;
                &self.first
            } else {
                &self.pending
            };
        client
            .stream(
                model,
                profile,
                system,
                messages,
                tools,
                query_source,
                skip_global_cache_for_system_prompt,
                request_dispatch_admission,
            )
            .await
    }
}

#[test]
fn mobile_public_consumed_midturn_member_cancel_keeps_live_owner_and_later_input() {
    use crate::mobile::test_support::new_engine_with_streaming;
    use orchestrator::test_support_stream::*;
    let root = tempfile::tempdir().unwrap();
    let listener = Arc::new(FakeListener::default());
    let (entered, entered_rx) = tokio::sync::oneshot::channel();
    let stream = Arc::new(GatedCompletionThenPendingClient {
        first: MockStreamingApiClient::with_turns(vec![vec![
            message_start("initial", "test"),
            content_block_start_text(0),
            text_delta(0, "initial"),
            content_block_stop(0),
            message_delta_stop("end_turn"),
            message_stop(),
        ]]),
        pending: PendingPromptStreamClient::new(entered),
        calls: AtomicUsize::new(0),
        first_entered: Notify::new(),
        first_release: Notify::new(),
    });
    let handle = new_engine_with_streaming(
        test_config(root.path()),
        Arc::new(HostFakePlatform::new(root.path().to_owned())),
        listener.clone(),
        Arc::new(RecordingPermissionSink::default()),
        Some(stream.clone()),
    )
    .unwrap();
    handle.runtime().block_on(async {
        let wait = std::time::Duration::from_secs(10);
        handle
            .submit(ClientCommand::SendPrompt {
                text: "current".into(),
                prompt_mode: None,
                images: Vec::new(),
                turn_id: Some(900),
                visualization_context: None,
            })
            .await
            .unwrap();
        tokio::time::timeout(wait, stream.first_entered.notified())
            .await
            .unwrap();
        let owner = handle.active_cancel.lock().await.clone().unwrap();
        for id in [811, 812] {
            handle
                .submit(ClientCommand::SendPrompt {
                    text: format!("append-{id}").into(),
                    prompt_mode: None,
                    images: Vec::new(),
                    turn_id: Some(id),
                    visualization_context: None,
                })
                .await
                .unwrap();
        }
        stream.first_release.notify_one();
        tokio::time::timeout(wait, entered_rx)
            .await
            .unwrap()
            .unwrap();
        let session = owner.session_id.clone();
        assert_eq!(
            handle
                .durable_turns
                .load(&session, 811)
                .unwrap()
                .queue_delivery,
            QueueDeliveryState::Delivered
        );
        assert!(
            listener
                .received
                .lock()
                .await
                .iter()
                .any(|event| matches!(event,
            Ev::UserTranscriptRowIdentity { row_token, .. } if row_token == "client-turn:811,812"))
        );
        handle
            .submit(ClientCommand::SendPrompt {
                text: "later input".into(),
                prompt_mode: None,
                images: Vec::new(),
                turn_id: Some(821),
                visualization_context: None,
            })
            .await
            .unwrap();
        let _transition = handle.loop_transition.lock().await;
        for requested in [811, 812] {
            handle
                .submit(ClientCommand::Cancel {
                    turn_id: Some(requested),
                })
                .await
                .unwrap();
            assert!(
                !owner.cancel.is_cancelled(),
                "an acknowledged mid-turn member is no longer a live owner"
            );
        }
        assert!(handle
            .active_cancel
            .lock()
            .await
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, &owner)));
        assert_eq!(
            handle
                .durable_turns
                .scan_pending(&session)
                .unwrap()
                .iter()
                .map(|checkpoint| checkpoint.turn_id)
                .collect::<Vec<_>>(),
            [821]
        );
        handle
            .submit(ClientCommand::Cancel { turn_id: Some(900) })
            .await
            .unwrap();
        assert_eq!(
            handle
                .durable_turns
                .scan_pending(&session)
                .unwrap()
                .iter()
                .map(|checkpoint| checkpoint.turn_id)
                .collect::<Vec<_>>(),
            [821]
        );
        handle
            .submit(ClientCommand::Cancel { turn_id: Some(821) })
            .await
            .unwrap();
    });
}

#[test]
fn mobile_public_member_cancel_settles_primary_and_bound_groups_independently() {
    use crate::mobile::test_support::new_engine_with_streaming;
    use orchestrator::prompt::mid_turn_input::MidTurnInputSource;
    let wait = std::time::Duration::from_secs(10);
    for uncertain in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let listener = Arc::new(FakeListener::default());
        let (entered, entered_rx) = tokio::sync::oneshot::channel();
        let handle = new_engine_with_streaming(
            test_config(root.path()),
            Arc::new(HostFakePlatform::new(root.path().to_owned())),
            listener.clone(),
            Arc::new(RecordingPermissionSink::default()),
            Some(Arc::new(PendingPromptStreamClient::new(entered))),
        )
        .unwrap();
        handle.runtime().block_on(async {
            let session = handle.active_session_id();
            {
                let _transition = tokio::time::timeout(wait, handle.loop_transition.lock())
                    .await
                    .expect("dual-group initial transition must be available");
                seed_cancellable_coalesced(&handle, &session, false);
                tokio::time::timeout(
                    wait,
                    rebuild_mobile_prompt_dispatch_cache(
                        &handle.message_queue,
                        &handle.durable_turns,
                        &session,
                    ),
                )
                .await
                .expect("dual-group queue rebuild must finish")
                .unwrap();
            }
            tokio::time::timeout(wait, entered_rx)
                .await
                .unwrap()
                .unwrap();
            for id in [821, 822] {
                tokio::time::timeout(
                    wait,
                    handle.submit(ClientCommand::SendPrompt {
                        text: format!("append-{id}").into(),
                        prompt_mode: None,
                        images: Vec::new(),
                        turn_id: Some(id),
                        visualization_context: None,
                    }),
                )
                .await
                .expect("dual-group busy SendPrompt must complete")
                .unwrap();
            }
            let source = MobileMsgQueueInput {
                queue: handle.message_queue.clone(),
                loop_state: Arc::new(tool_cron::LoopRuntime::default()),
                durable_turns: handle.durable_turns.clone(),
                active_session_uuid: handle.inner.active_session_uuid.clone(),
                active_turn: Some(handle.active_cancel.clone()),
            };
            assert_eq!(
                tokio::time::timeout(wait, source.take_mid_turn_batch())
                    .await
                    .expect("dual-group mid-turn admission must complete")
                    .unwrap()
                    .unwrap()
                    .len(),
                2
            );
            if uncertain {
                handle
                    .durable_turns
                    .fail_next_metadata_after_publish_for_test();
            }
            let result = tokio::time::timeout(
                wait,
                handle.submit(ClientCommand::Cancel { turn_id: Some(822) }),
            )
            .await
            .expect("dual-group Cancel must unwind the executor");
            if uncertain {
                assert!(result.is_err());
            } else {
                result.unwrap();
            }
            assert!(handle.active_cancel.lock().await.is_none());
            assert!(handle
                .durable_turns
                .scan_pending(&session)
                .unwrap()
                .is_empty());
            {
                let events = listener.received.lock().await;
                for member in [811, 812] {
                    assert_eq!(
                        events
                            .iter()
                            .filter(|event| matches!(event,
                        Ev::TurnRecoveryState { snapshot } if snapshot.turn_id == member
                            && snapshot.state == TurnRecoveryStateDto::Cancelled))
                            .count(),
                        1,
                        "the primary group proof must survive a different group's storage error"
                    );
                }
            }
            if uncertain {
                tokio::time::timeout(
                    wait,
                    handle.submit(ClientCommand::Cancel { turn_id: Some(822) }),
                )
                .await
                .expect("dual-group cancellation retry must complete")
                .unwrap();
            }
            let events = listener.received.lock().await;
            for member in [821, 822] {
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| matches!(event,
                    Ev::TurnRecoveryState { snapshot } if snapshot.turn_id == member
                        && snapshot.state == TurnRecoveryStateDto::Cancelled))
                        .count(),
                    1,
                    "the bound group proof is deduplicated and recoverable on retry"
                );
            }
        });
    }
}

#[test]
fn mobile_watcher_anchor_failure_preserves_dispatch_until_same_file_is_repaired() {
    use crate::mobile::test_support::new_engine_with_streaming;
    let root = tempfile::tempdir().unwrap();
    let listener = Arc::new(FakeListener::default());
    let (entered, entered_rx) = tokio::sync::oneshot::channel();
    let stream = Arc::new(PendingPromptStreamClient::new(entered));
    let handle = new_engine_with_streaming(
        test_config(root.path()),
        Arc::new(HostFakePlatform::new(root.path().to_owned())),
        listener.clone(),
        Arc::new(RecordingPermissionSink::default()),
        Some(stream.clone()),
    )
    .unwrap();
    handle.runtime().block_on(async {
        let wait = std::time::Duration::from_secs(10);
        let session = handle.active_session_id();
        let path = handle.inner.session_writer.active_path();
        let held = path.with_extension("preflight-held");
        let original = std::fs::read(&path).unwrap();
        {
            let _transition = tokio::time::timeout(wait, handle.loop_transition.lock())
                .await
                .unwrap();
            seed_cancellable_coalesced(&handle, &session, false);
            std::fs::rename(&path, &held).unwrap();
            std::fs::create_dir(&path).unwrap();
            rebuild_mobile_prompt_dispatch_cache(
                &handle.message_queue,
                &handle.durable_turns,
                &session,
            )
            .await
            .unwrap();
        }
        tokio::time::timeout(wait, handle.queued_anchor_failure_for_test.notified())
            .await
            .expect("observe a failed anchor preflight without adding a product event");
        {
            let _transition = tokio::time::timeout(wait, handle.loop_transition.lock())
                .await
                .unwrap();
            assert!(handle.active_cancel.lock().await.is_none());
            let owner = handle.durable_turns.load(&session, 811).unwrap();
            assert_eq!(owner.state, None);
            assert_eq!(owner.queue_delivery, QueueDeliveryState::CoalescedClaimed);
            assert!(stream.captured_calls().is_empty());
            assert_eq!(
                handle
                    .message_queue
                    .snapshot()
                    .await
                    .iter()
                    .filter(|command| command.uuid == mobile_prompt_command_uuid(&session, 811))
                    .count(),
                1
            );
            std::fs::remove_dir(&path).unwrap();
            std::fs::rename(&held, &path).unwrap();
        }
        tokio::time::timeout(wait, entered_rx)
            .await
            .unwrap()
            .unwrap();
        {
            let initialized = handle.initialized_session_anchor.lock().await;
            let anchor = initialized
                .as_ref()
                .expect("the real queued activation filled the anchor cache");
            assert_eq!(anchor.path, path);
            assert_eq!(anchor.session_id.to_string(), session);
            assert_eq!(
                anchor.file_id,
                super::super::mobile_anchor_file_id(
                    &tokio::fs::symlink_metadata(&path).await.unwrap()
                )
            );
        }
        assert!(std::fs::read(&path).unwrap().starts_with(&original));
        tokio::time::timeout(
            wait,
            handle.submit(ClientCommand::SendPrompt {
                text: "append after anchor repair".into(),
                prompt_mode: None,
                images: Vec::new(),
                turn_id: Some(821),
                visualization_context: None,
            }),
        )
        .await
        .expect("busy input must use the initialized anchor cache")
        .unwrap();
        let _transition = tokio::time::timeout(wait, handle.loop_transition.lock())
            .await
            .unwrap();
        tokio::time::timeout(
            wait,
            handle.submit(ClientCommand::Cancel { turn_id: Some(811) }),
        )
        .await
        .unwrap()
        .unwrap();
        tokio::time::timeout(
            wait,
            handle.submit(ClientCommand::Cancel { turn_id: Some(821) }),
        )
        .await
        .unwrap()
        .unwrap();
    });
}

#[test]
fn mobile_cold_explicit_resume_initializes_anchor_before_busy_append() {
    use crate::mobile::test_support::new_engine_with_streaming;
    let root = tempfile::tempdir().unwrap();
    let (first, _) = build_submit_handle(root.path());
    first.task_notification_watcher.abort();
    let session = first.runtime().block_on(async {
        first.ensure_mobile_session_anchor().await.unwrap();
        let session = first.active_session_id();
        seed_cancellable_coalesced(&first, &session, true);
        first
            .submit(ClientCommand::PauseTurn {
                turn_id: 811,
                reason: "cold recovery".into(),
            })
            .await
            .unwrap();
        session
    });
    drop(first);
    let (entered, entered_rx) = tokio::sync::oneshot::channel();
    let handle = new_engine_with_streaming(
        test_config(root.path()),
        Arc::new(HostFakePlatform::new(root.path().to_owned())),
        Arc::new(FakeListener::default()),
        Arc::new(RecordingPermissionSink::default()),
        Some(Arc::new(PendingPromptStreamClient::new(entered))),
    )
    .unwrap();
    handle.runtime().block_on(async {
        let wait = std::time::Duration::from_secs(10);
        tokio::time::timeout(wait, handle.resume_seeded_test_session(session.clone()))
            .await
            .unwrap()
            .unwrap();
        assert!(handle.initialized_session_anchor.lock().await.is_none());
        tokio::time::timeout(
            wait,
            handle.submit(ClientCommand::ResumeTurn { turn_id: 811 }),
        )
        .await
        .unwrap()
        .unwrap();
        tokio::time::timeout(wait, entered_rx)
            .await
            .unwrap()
            .unwrap();
        {
            let initialized = handle.initialized_session_anchor.lock().await;
            let anchor = initialized
                .as_ref()
                .expect("cold ResumeTurn initialized the cache before the driver");
            assert_eq!(anchor.path, handle.inner.session_writer.active_path());
            assert_eq!(anchor.session_id.to_string(), session);
        }
        tokio::time::timeout(
            wait,
            handle.submit(ClientCommand::SendPrompt {
                text: "append during cold resumed stream".into(),
                prompt_mode: None,
                images: Vec::new(),
                turn_id: Some(821),
                visualization_context: None,
            }),
        )
        .await
        .expect("cold resumed busy input must not wait for the turn to end")
        .unwrap();
        let _transition = tokio::time::timeout(wait, handle.loop_transition.lock())
            .await
            .unwrap();
        tokio::time::timeout(
            wait,
            handle.submit(ClientCommand::Cancel { turn_id: Some(811) }),
        )
        .await
        .unwrap()
        .unwrap();
        tokio::time::timeout(
            wait,
            handle.submit(ClientCommand::Cancel { turn_id: Some(821) }),
        )
        .await
        .unwrap()
        .unwrap();
    });
}

#[test]
fn mobile_terminal_and_unsafe_resume_do_not_initialize_the_session_anchor() {
    for terminal in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (handle, _) = build_submit_handle(root.path());
        handle.runtime().block_on(async {
            let session = handle.active_session_id();
            let path = handle.inner.session_writer.active_path();
            let before = std::fs::read(&path).unwrap();
            handle
                .durable_turns
                .begin(&session, 811, "not resumable".into(), None, Vec::new())
                .unwrap();
            if terminal {
                handle.durable_turns.cancel(&session, 811).unwrap();
            } else {
                handle
                    .durable_turns
                    .transition(
                        &session,
                        811,
                        TurnRecoveryStateDto::WaitingForUser,
                        false,
                        Some("unsafe checkpoint".into()),
                    )
                    .unwrap();
            }
            assert!(!handle
                .durable_turns
                .load(&session, 811)
                .unwrap()
                .can_resume_without_user());
            tokio::time::timeout(
                std::time::Duration::from_secs(10),
                handle.submit(ClientCommand::ResumeTurn { turn_id: 811 }),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(handle.initialized_session_anchor.lock().await.is_none());
            assert!(handle.active_cancel.lock().await.is_none());
            assert_eq!(std::fs::read(&path).unwrap(), before);
        });
    }
}
