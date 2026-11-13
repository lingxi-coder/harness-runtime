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
