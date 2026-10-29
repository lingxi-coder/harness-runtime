//! Recovery changes the model-context frontier, never the outcome of an old
//! run. All work here is filesystem validation; neither factory.build nor any
//! resume/model/tool dispatcher is called.
use super::*;

impl AppAgentHost {
    fn check_recovery_generation(
        &self,
        generation: u64,
        owner_cancel: Option<&CancellationToken>,
    ) -> Result<(), String> {
        if self.shutdown.load(Ordering::Acquire) || generation != self.host_generation {
            return Err("agent_host_replaced".into());
        }
        if owner_cancel.is_some_and(CancellationToken::is_cancelled) {
            return Err("app_invocation_closed".into());
        }
        Ok(())
    }

    pub(super) async fn recover_chat_if_needed(
        &self,
        state: &AppAgentState,
        owner: &Owner,
        chat_id: &str,
        owner_cancel: Option<&CancellationToken>,
    ) -> Result<bool, String> {
        #[cfg(test)]
        let recovery_gate = { self.recovery_before_generation.lock().unwrap().take() };
        #[cfg(test)]
        if let Some((entered, release)) = recovery_gate {
            let _ = entered.send(());
            let _ = release.await;
        }
        if self.shutdown.load(Ordering::Acquire) {
            return Err("agent_host_replaced".into());
        }
        let generation = self.generation_lock.lock().await;
        let _serial = self.admission.lock().await;
        self.check_recovery_generation(*generation, owner_cancel)?;
        // RunAgentInput.threadId also identifies ordinary stateless runs. Only
        // a canonical UUID bound in THIS App's private ChatStore selects Chat
        // recovery; another App's UUID cannot select its history or assets.
        if !uuid::Uuid::parse_str(chat_id).is_ok_and(|id| id.to_string() == chat_id) {
            return Ok(false);
        }
        let summary = match state.chats.recovery_summary(chat_id) {
            Ok(summary) => summary,
            Err(local_app::v2::AppError::NotFound(_)) => return Ok(false),
            Err(_) => return Err("chat_storage_unavailable".into()),
        };
        // Archived Chats remain readable without preparing or unarchiving.
        if summary.archived {
            return Ok(false);
        }
        if !state
            .store
            .chat_requires_recovery(&owner.app_id, chat_id)
            .map_err(|_| "agent_storage_unavailable")?
        {
            return Ok(false);
        }
        // A failed tool cleanup retains its Control. Never fork the context
        // underneath a task whose native executor still owns tool resources.
        if self
            .active
            .lock()
            .await
            .values()
            .any(|control| control.owner.app_id == owner.app_id)
        {
            return Err("agent_busy".into());
        }
        let plan = state
            .store
            .prepare_chat_recovery(owner, chat_id)
            .map_err(|_| "chat_recovery_unavailable")?;
        let (source, target) = state
            .chats
            .recovery_paths(&plan)
            .map_err(|_| "chat_storage_unavailable")?;
        state
            .chats
            .fence_recovery(chat_id)
            .map_err(|_| "chat_storage_unavailable")?;
        let proof = plan
            .checkpoint()
            .map(|checkpoint| session::jsonl::ChatTranscriptCheckpoint {
                byte_len: checkpoint.byte_len,
                sha256: checkpoint.sha256.clone(),
            });
        self.check_recovery_generation(*generation, owner_cancel)?;
        let captured =
            session::jsonl::JsonlWriter::stage_persistent_chat_recovery(source, target, proof)
                .await
                .map_err(|_| "chat_transcript_unavailable")?;
        self.check_recovery_generation(*generation, owner_cancel)?;
        if let Some(captured) = captured {
            let replayed =
                orchestrator::ConversationOrchestrator::replay_persistent_chat_snapshot(&captured)
                    .map_err(|_| "chat_transcript_unavailable")?;
            super::super::validate_local_app_chat_resume(&replayed)?;
            check_chat_message_metadata(
                &state.chats,
                chat_id,
                &replayed,
                &state.store,
                owner,
                None,
                None,
                false,
            )?;
            let base_task_id = plan.base_task_id().ok_or("chat_transcript_unavailable")?;
            let (base, checkpoint) = state
                .store
                .successful_chat_history_task(&owner.app_id, chat_id, base_task_id)
                .map_err(|_| "chat_transcript_unavailable")?;
            if checkpoint.byte_len != captured.checkpoint().byte_len
                || checkpoint.sha256 != captured.checkpoint().sha256
            {
                return Err("chat_transcript_unavailable".into());
            }
            validate_chat_result_in_transcript(
                &replayed,
                base.result.as_ref().ok_or("chat_transcript_unavailable")?,
            )?;
        } else if plan.checkpoint().is_some() || plan.base_task_id().is_some() {
            return Err("chat_transcript_unavailable".into());
        }
        state
            .chats
            .verify_recovery_assets(&plan)
            .map_err(|_| "chat_storage_unavailable")?;
        // Revalidate the current release/grant after every awaited filesystem
        // operation. Recovery carries no tool grant into the next admission.
        self.check_recovery_generation(*generation, owner_cancel)?;
        self.validate_current_release(owner).await?;
        self.check_recovery_generation(*generation, owner_cancel)?;
        if state
            .store
            .settings(owner)
            .map_err(|_| "agent_storage_unavailable")?
            .is_none()
        {
            return Err("agent_not_authorized".into());
        }
        self.check_recovery_generation(*generation, owner_cancel)?;
        state
            .store
            .commit_chat_recovery(&plan)
            .map_err(|_| "agent_storage_unavailable")?;
        let authority = state
            .store
            .committed_chat_recoveries(&owner.app_id)
            .map_err(|_| "agent_storage_unavailable")?;
        state
            .chats
            .install_recovery_authority(authority)
            .map_err(|_| "chat_storage_unavailable")?;
        state
            .chats
            .finish_recovery(chat_id)
            .map_err(|_| "chat_storage_unavailable")?;
        let states = state
            .store
            .recovery_states(&owner.app_id)
            .map_err(|_| "agent_storage_unavailable")?;
        state
            .chats
            .reconcile_startup(&states)
            .map_err(|_| "chat_storage_unavailable")?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::super::progress_tests::test_wake_host;
    use super::super::protocol_retry_tests::{chat_control, settings};
    use super::*;
    use lingxi_core::types::{MessageId, SessionId};
    use std::io::Write as _;

    async fn fixture() -> (
        tempfile::TempDir,
        Arc<AppAgentHost>,
        Arc<MobileLocalAppServiceHandle>,
        Owner,
        Arc<AppAgentState>,
        String,
        u64,
        Value,
    ) {
        let root = tempfile::tempdir().unwrap();
        let (host, service) = test_wake_host(root.path()).await;
        let source = root.path().join("recovery-app");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("index.html"), "<!doctype html>recovery").unwrap();
        std::fs::write(source.join("lingxi-app.json"), json!({
            "schema":2,"name":"Recovery","entry":{"type":"static","path":"index.html"},
            "targets":[{"platform":"ios","environment":"web"}],"capabilities":["agent"],"agent":{"tools":[]}
        }).to_string()).unwrap();
        let published = service
            .request_value(json!({"op":"publish","expectedRevision":0,"sourcePath":source}))
            .await
            .unwrap();
        let app = published["record"]["appId"].as_str().unwrap();
        let revision = published["record"]["currentRevision"].as_u64().unwrap();
        let (owner, _, _) = host
            .current_release(app, revision, "native-intent")
            .await
            .unwrap();
        let state = host.app_state(app).unwrap();
        state
            .store
            .configure(owner.clone(), Some(settings()))
            .unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        state
            .chats
            .create(&id, None, &uuid::Uuid::new_v4().to_string())
            .unwrap();
        (root, host, service, owner, state, id, revision, published)
    }

    fn failed_turn(
        state: &AppAgentState,
        owner: &Owner,
        chat_id: &str,
        request: &str,
    ) -> local_app::v2::agent::Task {
        let mut messages = state
            .chats
            .protocol_history_page(chat_id, None, 100)
            .unwrap()
            .messages;
        let user = json!({"id":format!("user-{request}"),"role":"user","content":"retained failed attempt"});
        messages.push(user.clone());
        let (normalized, _) = normalize_page_run_input(
            state,
            json!({"threadId":chat_id,"runId":request,
            "protocolVersion":"1.0","messages":messages}),
            Some(chat_id),
        )
        .unwrap();
        let task = state
            .store
            .admit_scoped(owner, &normalized.input, &[], &[], Some(chat_id), true)
            .unwrap()
            .task;
        state.chats.start_turn(chat_id, &task.run_id).unwrap();
        state
            .chats
            .record_protocol_message(chat_id, &task.run_id, &user)
            .unwrap();
        state
            .store
            .append_event(
                owner,
                &task.run_id,
                json!({"type":"RUN_STARTED","protocolVersion":"1.0",
            "threadId":chat_id,"runId":request}),
            )
            .unwrap();
        let terminal = super::super::commit_agent_terminal_for_test(
            &state.store,
            owner,
            &task.run_id,
            State::Failed,
            None,
            Some("model_execution_failed".into()),
        )
        .unwrap();
        state.chats.interrupt_turn(chat_id, &task.run_id).unwrap();
        terminal
    }

    async fn commit_next_turn(
        host: &AppAgentHost,
        state: &AppAgentState,
        owner: &Owner,
        chat_id: &str,
        revision: u64,
        request: &str,
    ) -> session::jsonl::ChatTranscriptCheckpoint {
        use orchestrator::test_support::{
            noop_hook_executor, MockApiClient, MockStreamingApiClient, NoOpPermissionGate,
            StaticMemoryProvider,
        };
        let mut messages = state
            .chats
            .protocol_history_page(chat_id, None, 100)
            .unwrap()
            .messages;
        let user =
            json!({"id":format!("ui-{request}"),"role":"user","content":"continue after recovery"});
        messages.push(user.clone());
        let (normalized, _) = normalize_page_run_input(
            state,
            json!({"threadId":chat_id,"runId":request,
            "protocolVersion":"1.0","messages":messages}),
            Some(chat_id),
        )
        .unwrap();
        let task = state
            .store
            .admit_scoped(owner, &normalized.input, &[], &[], Some(chat_id), true)
            .unwrap()
            .task;
        state
            .store
            .update(owner, &task.run_id, State::Running, None, None, None)
            .unwrap();
        state
            .store
            .append_event(
                owner,
                &task.run_id,
                json!({"type":"RUN_STARTED","protocolVersion":"1.0",
            "threadId":chat_id,"runId":request}),
            )
            .unwrap();
        state.chats.start_turn(chat_id, &task.run_id).unwrap();
        state
            .chats
            .record_protocol_message(chat_id, &task.run_id, &user)
            .unwrap();
        let user_id = MessageId::from_uuid(uuid::Uuid::parse_str(&task.run_id).unwrap());
        state
            .chats
            .record_message(
                chat_id,
                ChatMessageMetadata {
                    message_id: user_id.to_string(),
                    run_id: task.run_id.clone(),
                    role: ChatRole::User,
                    created_at: now_millis(),
                    attachments: Vec::new(),
                },
            )
            .unwrap();
        let previous = state
            .store
            .latest_chat_transcript_checkpoint(&owner.app_id, chat_id, &task.run_id)
            .unwrap()
            .map(|(_, proof)| session::jsonl::ChatTranscriptCheckpoint {
                byte_len: proof.byte_len,
                sha256: proof.sha256,
            });
        let path = state.chats.transcript_path(chat_id).unwrap();
        let writer = Arc::new(
            session::jsonl::JsonlWriter::open_persistent_chat(
                path.clone(),
                host.factory.context.fs.clone(),
                previous.clone(),
            )
            .await
            .unwrap(),
        );
        let api = Arc::new(MockApiClient::new(Vec::new()));
        let (persisted, _) = watch::channel(None);
        let control = Arc::new(chat_control(
            owner.clone(),
            revision,
            request.into(),
            task.run_id.clone(),
            chat_id.into(),
            user_id.to_string(),
            state.store.clone(),
            state.chats.clone(),
            persisted,
        ));
        let session_id = SessionId::from_uuid(
            uuid::Uuid::parse_str(&state.chats.session_id(chat_id).unwrap()).unwrap(),
        );
        let orch = Arc::new(
            ConversationOrchestrator::new_side_chat(
                OrchestratorConfig::default(),
                api.clone(),
                Arc::new(MockStreamingApiClient::empty()),
                Arc::new(ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                control.clone(),
                Arc::new(StaticMemoryProvider::empty()),
                host.apps_root.clone(),
            )
            .with_session_id(session_id)
            .with_jsonl_writer(writer.clone()),
        );
        if previous.is_some() {
            let captured = writer
                .capture_persistent_chat_snapshot(false)
                .await
                .unwrap();
            let replayed =
                ConversationOrchestrator::replay_persistent_chat_snapshot(&captured).unwrap();
            super::super::super::validate_local_app_chat_resume(&replayed).unwrap();
            orch.resume_session(
                session_id,
                replayed.state.history.clone(),
                replayed.last_message_uuid.map(|id| id.to_string()),
                None,
                replayed.handle_runtime_snapshot(),
            )
            .await
            .unwrap();
        }
        orch.append_external_history_message(ConversationMessage::user(
            user_id,
            "continue after recovery".into(),
        ))
        .await;
        let answer_id = MessageId::new();
        orch.append_external_history_message(ConversationMessage::Assistant {
            id: answer_id,
            content: vec![lingxi_core::types::ContentBlock::Text {
                text: format!("answer-{request}"),
                citations: None,
            }],
            stop_reason: Some("stop_sequence".into()),
            resumable: None,
            per_turn_effort: None,
        })
        .await;
        control.emit_assistant_message_identity(&answer_id).await;
        let captured = orch.verify_persistent_chat_checkpoint(true).await.unwrap();
        let proof = captured.checkpoint().clone();
        state
            .store
            .record_chat_transcript_checkpoint(
                owner,
                &task.run_id,
                chat_id,
                ChatTranscriptCheckpoint {
                    byte_len: proof.byte_len,
                    sha256: proof.sha256.clone(),
                },
            )
            .unwrap();
        *control.chat_orchestrator.lock().unwrap() = Some(orch);
        host.finish_execution(
            &control,
            Ok((
                TaskResult {
                    text: format!("answer-{request}"),
                    output: None,
                    attachments: Vec::new(),
                    chat_id: Some(chat_id.into()),
                    message_id: Some(answer_id.to_string()),
                },
                json!({}),
            )),
        )
        .await;
        assert_eq!(
            state.store.get(owner, &task.run_id).unwrap().state,
            State::Succeeded
        );
        assert!(
            api.captured_requests().await.is_empty(),
            "recovery and checkpoint wiring must not send a model request"
        );
        drop(writer);
        let reopened = session::jsonl::JsonlWriter::open_persistent_chat(
            path,
            host.factory.context.fs.clone(),
            Some(proof.clone()),
        )
        .await
        .unwrap();
        assert_eq!(
            reopened
                .capture_persistent_chat_snapshot(false)
                .await
                .unwrap()
                .checkpoint(),
            &proof
        );
        proof
    }

    #[tokio::test]
    async fn page_tasks_start_preserves_non_chat_threads_and_current_authorization() {
        let (_root, host, service, owner, state, _chat, revision, published) = fixture().await;
        let invocation =
            super::super::protocol_retry_tests::launch_agent_page(&service, &published).await;
        let (page_owner, _, _, _) = host.owner(&owner.app_id, &invocation).await.unwrap();
        for (index, thread_id) in [
            "plain-page-thread".to_owned(),
            uuid::Uuid::new_v4().to_string(),
        ]
        .into_iter()
        .enumerate()
        {
            let request_id = format!("page-stateless-{index}");
            if cfg!(target_os = "ios") {
                host.host(
                    json!({"op":"wake.reserve","appId":owner.app_id,"revision":revision,
                    "runId":request_id}),
                )
                .await
                .unwrap();
            }
            let response = host.page(&owner.app_id, &invocation, "agent.tasks.start", json!({"input":{
                "threadId":thread_id,"runId":request_id,"protocolVersion":"1.0",
                "messages":[{"id":format!("page-user-{index}"),"role":"user","content":"one-off request"}]
            }})).await.unwrap();
            assert_eq!(response["runId"], request_id);
            host.stop_app(&owner.app_id).await.unwrap();
            assert!(state
                .store
                .protocol_chat_id_for_request(&page_owner, &request_id)
                .unwrap()
                .is_none());
            assert_eq!(
                state
                    .store
                    .thread_id_for_request(&page_owner, &request_id)
                    .unwrap(),
                thread_id
            );
            assert!(state
                .store
                .committed_chat_recoveries(&owner.app_id)
                .unwrap()
                .is_empty());
        }
        // An explicit Chat route cannot turn a foreign App's Chat UUID into
        // a binding, even though an ordinary task may use any thread string.
        let foreign_app = format!("a{}", "8".repeat(32));
        std::fs::create_dir_all(host.apps_root.join(&foreign_app)).unwrap();
        let foreign = host.app_state(&foreign_app).unwrap();
        let foreign_chat = uuid::Uuid::new_v4().to_string();
        foreign
            .chats
            .create(&foreign_chat, None, &uuid::Uuid::new_v4().to_string())
            .unwrap();
        assert_eq!(host.page(&owner.app_id, &invocation, "agent.chats.send", json!({
            "chatId":foreign_chat,"input":{"threadId":foreign_chat,"runId":"foreign-chat-binding",
            "protocolVersion":"1.0","messages":[{"id":"foreign-binding-user","role":"user","content":"must not bind"}]}
        })).await.unwrap_err(), "chat_not_found");
        assert!(foreign
            .chats
            .protocol_history_page(&foreign_chat, None, 10)
            .unwrap()
            .messages
            .is_empty());
        state.store.configure(page_owner, None).unwrap();
        let rejected = host
            .page(
                &owner.app_id,
                &invocation,
                "agent.tasks.start",
                json!({"input":{
                    "threadId":"still-stateless","runId":"page-revoked","protocolVersion":"1.0",
                    "messages":[{"id":"revoked-user","role":"user","content":"must be rejected"}]
                }}),
            )
            .await
            .unwrap_err();
        assert_eq!(rejected, "agent_not_authorized");
        service
            .request_value(json!({"op":"stop","appId":owner.app_id}))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn native_start_preserves_non_chat_threads_without_selecting_foreign_chat_history() {
        let (_root, host, _service, owner, state, _chat, revision, _published) = fixture().await;
        let foreign_app = format!("a{}", "9".repeat(32));
        std::fs::create_dir_all(host.apps_root.join(&foreign_app)).unwrap();
        let foreign = host.app_state(&foreign_app).unwrap();
        let foreign_chat = uuid::Uuid::new_v4().to_string();
        foreign
            .chats
            .create(&foreign_chat, None, &uuid::Uuid::new_v4().to_string())
            .unwrap();
        let private_message =
            json!({"id":"foreign-private-user","role":"user","content":"foreign private history"});
        foreign
            .chats
            .record_protocol_message(&foreign_chat, "foreign-private-run", &private_message)
            .unwrap();
        for (index, thread_id) in [
            "plain-native-thread".to_owned(),
            uuid::Uuid::new_v4().to_string(),
            foreign_chat.clone(),
        ]
        .into_iter()
        .enumerate()
        {
            let request_id = format!("native-stateless-{index}");
            let response = host.host(json!({"op":"startNative","appId":owner.app_id,"revision":revision,
                "input":{"threadId":thread_id,"runId":request_id,"protocolVersion":"1.0",
                    "messages":[{"id":format!("native-user-{index}"),"role":"user","content":"one-off request"}]}
            })).await.unwrap();
            assert_eq!(response["runId"], request_id);
            host.stop_app(&owner.app_id).await.unwrap();
            assert!(state
                .store
                .protocol_chat_id_for_request(&owner, &request_id)
                .unwrap()
                .is_none());
        }
        assert!(state
            .store
            .committed_chat_recoveries(&owner.app_id)
            .unwrap()
            .is_empty());
        assert_eq!(
            foreign
                .chats
                .protocol_history_page(&foreign_chat, None, 10)
                .unwrap()
                .messages,
            vec![private_message]
        );
        assert!(state.chats.get(&foreign_chat).is_err());
        state.store.configure(owner.clone(), None).unwrap();
        assert_eq!(host.host(json!({"op":"startNative","appId":owner.app_id,"revision":revision,
            "input":{"threadId":"native-no-grant","runId":"native-revoked","protocolVersion":"1.0",
                "messages":[{"id":"revoked-native-user","role":"user","content":"must be rejected"}]}
        })).await.unwrap_err(), "agent_not_authorized");
    }

    #[tokio::test]
    async fn empty_frontier_and_consecutive_recovery_reach_new_writer_and_final_checkpoint() {
        let (_root, host, _service, owner, state, id, revision, _published) = fixture().await;
        let first = failed_turn(&state, &owner, &id, "failed-first");
        assert!(host
            .recover_chat_if_needed(&state, &owner, &id, None)
            .await
            .unwrap());
        assert!(state
            .chats
            .protocol_history_page(&id, None, 100)
            .unwrap()
            .messages
            .is_empty());
        let empty_path = state.chats.transcript_path(&id).unwrap();
        assert!(std::fs::read(&empty_path).unwrap().is_empty());
        let first_checkpoint =
            commit_next_turn(&host, &state, &owner, &id, revision, "first-success").await;
        failed_turn(&state, &owner, &id, "failed-second");
        assert!(host
            .recover_chat_if_needed(&state, &owner, &id, None)
            .await
            .unwrap());
        assert_ne!(state.chats.transcript_path(&id).unwrap(), empty_path);
        let second_checkpoint =
            commit_next_turn(&host, &state, &owner, &id, revision, "second-success").await;
        assert!(second_checkpoint.byte_len > first_checkpoint.byte_len);
        assert_eq!(
            state
                .chats
                .protocol_history_page(&id, None, 100)
                .unwrap()
                .messages
                .len(),
            4
        );
        assert_eq!(
            state
                .store
                .archived_chat_attempts(&owner, &id)
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            state.store.get(&owner, &first.run_id).unwrap().state,
            State::Failed
        );
    }

    #[tokio::test]
    async fn missing_attachment_during_a_recovery_keeps_healthy_b_available() {
        use sha2::Digest as _;
        let (_root, host, _service, owner, state, a, revision, _published) = fixture().await;
        let failed = failed_turn(&state, &owner, &a, "missing-asset");
        state.chats.record_protocol_message(&a, &failed.run_id, &json!({
            "id":"failed-attachment-user","role":"user","content":[
                {"type":"document","source":{"type":"data","value":"bWlzc2luZw==","mimeType":"text/plain"},
                 "metadata":{"com.lingxi.localApp":{"version":1,"filename":"notes.txt"}}}
            ]
        })).unwrap();
        let hash = format!("{:x}", sha2::Sha256::digest(b"missing"));
        std::fs::remove_file(
            state
                .root
                .join("chats")
                .join(&a)
                .join("assets")
                .join(format!("{hash}.bin")),
        )
        .unwrap();
        assert_eq!(
            host.recover_chat_if_needed(&state, &owner, &a, None)
                .await
                .unwrap_err(),
            "chat_storage_unavailable"
        );
        assert!(state.chats.protocol_history_page(&a, None, 10).is_err());
        let b = uuid::Uuid::new_v4().to_string();
        state
            .chats
            .create(&b, None, &uuid::Uuid::new_v4().to_string())
            .unwrap();
        assert!(state.chats.get(&b).is_ok());
        assert!(state
            .chats
            .protocol_history_page(&b, None, 10)
            .unwrap()
            .messages
            .is_empty());
        commit_next_turn(&host, &state, &owner, &b, revision, "healthy-b-success").await;
        assert_eq!(state.chats.get(&b).unwrap().state, ChatState::Idle);
        assert!(state.chats.get(&a).is_err());
    }

    #[tokio::test]
    async fn cancelled_page_owner_cannot_prepare_or_commit_recovery() {
        let (_root, host, _service, owner, state, id, _revision, _published) = fixture().await;
        failed_turn(&state, &owner, &id, "cancelled-owner");
        let cancel = CancellationToken::new();
        cancel.cancel();
        let ledger_path = state.root.join("tasks/agent-tasks.json");
        let before = std::fs::read(&ledger_path).unwrap();
        assert_eq!(
            host.recover_chat_if_needed(&state, &owner, &id, Some(&cancel))
                .await
                .unwrap_err(),
            "app_invocation_closed"
        );
        assert_eq!(std::fs::read(ledger_path).unwrap(), before);
    }

    #[tokio::test]
    async fn queued_old_host_recovery_cannot_overwrite_replacement_ledger() {
        let (_root, host, service, owner, state, id, _revision, _published) = fixture().await;
        failed_turn(&state, &owner, &id, "queued-old");
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        *host.recovery_before_generation.lock().unwrap() = Some((entered_tx, release_rx));
        let old = host.clone();
        let old_state = state.clone();
        let old_owner = owner.clone();
        let old_id = id.clone();
        let queued = tokio::spawn(async move {
            old.recover_chat_if_needed(&old_state, &old_owner, &old_id, None)
                .await
        });
        entered_rx.await.unwrap();
        host.stop_all().await.unwrap();
        let replacement =
            AppAgentHost::new(&service, host.factory.clone(), &service.config_home).unwrap();
        let replacement_state = replacement.app_state(&owner.app_id).unwrap();
        let second = uuid::Uuid::new_v4().to_string();
        replacement_state
            .chats
            .create(&second, None, &uuid::Uuid::new_v4().to_string())
            .unwrap();
        let mut changed_settings = settings();
        changed_settings.max_turns = 3;
        replacement_state
            .store
            .configure(owner.clone(), Some(changed_settings))
            .unwrap();
        let ledger_path = state.root.join("tasks/agent-tasks.json");
        let before = std::fs::read(&ledger_path).unwrap();
        release_tx.send(()).unwrap();
        assert_eq!(queued.await.unwrap().unwrap_err(), "agent_host_replaced");
        assert_eq!(std::fs::read(ledger_path).unwrap(), before);
        assert!(replacement_state
            .store
            .committed_chat_recoveries(&owner.app_id)
            .unwrap()
            .is_empty());
        assert!(replacement_state.chats.get(&second).is_ok());
    }

    #[tokio::test]
    async fn archived_interrupted_chat_remains_readable_after_restart_without_recovery_mutation() {
        let (_root, host, service, owner, state, id, _revision, published) = fixture().await;
        failed_turn(&state, &owner, &id, "archived-failure");
        state.chats.archive(&id).unwrap();
        host.stop_all().await.unwrap();
        let host = AppAgentHost::new(&service, host.factory.clone(), &service.config_home).unwrap();
        let invocation =
            super::super::protocol_retry_tests::launch_agent_page(&service, &published).await;
        let state = host.app_state(&owner.app_id).unwrap();
        let ledger_path = state.root.join("tasks/agent-tasks.json");
        let before = std::fs::read(&ledger_path).unwrap();
        let summary = host
            .page(
                &owner.app_id,
                &invocation,
                "agent.chats.get",
                json!({"chatId":id}),
            )
            .await
            .unwrap();
        assert_eq!(summary["archived"], true);
        let history = host
            .page(
                &owner.app_id,
                &invocation,
                "agent.chats.history",
                json!({"chatId":id}),
            )
            .await
            .unwrap();
        assert_eq!(history["messages"].as_array().unwrap().len(), 1);
        assert_eq!(std::fs::read(ledger_path).unwrap(), before);
        assert!(state
            .store
            .committed_chat_recoveries(&owner.app_id)
            .unwrap()
            .is_empty());
        service
            .request_value(json!({"op":"stop","appId":owner.app_id}))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn recovery_keeps_the_chat_and_committed_context_without_replaying_a_deferred_tool_tail()
    {
        let root = tempfile::tempdir().unwrap();
        let (host, service) = test_wake_host(root.path()).await;
        let source = root.path().join("recovery-fixture");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("index.html"), "<!doctype html>recovery fixture").unwrap();
        std::fs::write(source.join("lingxi-app.json"), json!({
            "schema":2,"name":"Recovery fixture","entry":{"type":"static","path":"index.html"},
            "targets":[{"platform":"ios","environment":"web"}],"capabilities":["agent"],"agent":{"tools":[]}
        }).to_string()).unwrap();
        let published = service
            .request_value(json!({"op":"publish","expectedRevision":0,"sourcePath":source}))
            .await
            .unwrap();
        let app = published["record"]["appId"].as_str().unwrap();
        let revision = published["record"]["currentRevision"].as_u64().unwrap();
        let (owner, _, _) = host
            .current_release(app, revision, "recovery-test")
            .await
            .unwrap();
        let state = host.app_state(app).unwrap();
        state
            .store
            .configure(owner.clone(), Some(settings()))
            .unwrap();
        let chat_id = uuid::Uuid::new_v4().to_string();
        let session_id = SessionId::new();
        state
            .chats
            .create(&chat_id, None, &session_id.as_uuid().to_string())
            .unwrap();
        let first_user =
            json!({"id":"first-ui-user","role":"user","content":"remember the committed answer"});
        let (first, _) = normalize_page_run_input(&state, json!({
            "threadId":chat_id,"runId":"first-run","protocolVersion":"1.0","messages":[first_user.clone()]
        }), Some(&chat_id)).unwrap();
        let task = state
            .store
            .admit_scoped(&owner, &first.input, &[], &[], Some(&chat_id), true)
            .unwrap()
            .task;
        state
            .store
            .update(&owner, &task.run_id, State::Running, None, None, None)
            .unwrap();
        state.store.append_event(&owner, &task.run_id, json!({
            "type":"RUN_STARTED","protocolVersion":"1.0","threadId":chat_id,"runId":"first-run"
        })).unwrap();
        state.chats.start_turn(&chat_id, &task.run_id).unwrap();
        let user_id = MessageId::from_uuid(uuid::Uuid::parse_str(&task.run_id).unwrap());
        state
            .chats
            .record_message(
                &chat_id,
                ChatMessageMetadata {
                    message_id: user_id.to_string(),
                    run_id: task.run_id.clone(),
                    role: ChatRole::User,
                    created_at: now_millis(),
                    attachments: Vec::new(),
                },
            )
            .unwrap();
        state
            .chats
            .record_protocol_message(&chat_id, &task.run_id, &first_user)
            .unwrap();
        let transcript = state.chats.transcript_path(&chat_id).unwrap();
        let writer = session::jsonl::JsonlWriter::open_persistent_chat(
            transcript.clone(),
            host.factory.context.fs.clone(),
            None,
        )
        .await
        .unwrap();
        let answer_id = MessageId::new();
        for row in [
            json!({"type":"user","uuid":user_id.as_uuid(),"parentUuid":null,"sessionId":session_id.as_uuid(),
                "timestamp":"2026-10-29T00:00:00.000Z","cwd":"/chat","version":"test","isSidechain":false,
                "message":{"role":"user","content":"remember the committed answer"}}),
            json!({"type":"assistant","uuid":answer_id.as_uuid(),"parentUuid":user_id.as_uuid(),"sessionId":session_id.as_uuid(),
                "timestamp":"2026-10-29T00:00:01.000Z","cwd":"/chat","version":"test","isSidechain":false,
                "message":{"id":answer_id.as_uuid(),"role":"assistant","stop_reason":"stop_sequence",
                    "content":[{"type":"text","text":"COMMITTED-42"}]}}),
        ] {
            let row: session::JsonlMessage = serde_json::from_value(row).unwrap();
            writer.append(&row).await.unwrap();
        }
        let committed = writer.capture_persistent_chat_snapshot(true).await.unwrap();
        let replayed =
            orchestrator::ConversationOrchestrator::replay_persistent_chat_snapshot(&committed)
                .unwrap();
        assert_eq!(replayed.state.history.len(), 2);
        state
            .store
            .record_chat_transcript_checkpoint(
                &owner,
                &task.run_id,
                &chat_id,
                ChatTranscriptCheckpoint {
                    byte_len: committed.checkpoint().byte_len,
                    sha256: committed.checkpoint().sha256.clone(),
                },
            )
            .unwrap();
        let (persisted, _) = watch::channel(None);
        let control = chat_control(
            owner.clone(),
            revision,
            "first-run".into(),
            task.run_id.clone(),
            chat_id.clone(),
            user_id.to_string(),
            state.store.clone(),
            state.chats.clone(),
            persisted,
        );
        control
            .commit_terminal(
                State::Succeeded,
                Some(TaskResult {
                    text: "COMMITTED-42".into(),
                    output: None,
                    attachments: Vec::new(),
                    chat_id: Some(chat_id.clone()),
                    message_id: Some(answer_id.to_string()),
                }),
                None,
                None,
            )
            .unwrap();
        state.chats.finish_turn(&chat_id, &task.run_id).unwrap();
        drop(writer);
        let committed_history = state
            .chats
            .protocol_history_page(&chat_id, None, 10)
            .unwrap()
            .messages;
        let interrupted_user =
            json!({"id":"interrupted-user","role":"user","content":"perform a side effect"});
        let mut supplied_history = committed_history.clone();
        supplied_history.push(interrupted_user.clone());
        let (interrupted, _) = normalize_page_run_input(&state, json!({
            "threadId":chat_id,"runId":"interrupted-run","protocolVersion":"1.0","messages":supplied_history
        }), Some(&chat_id)).unwrap();
        let failed = state
            .store
            .admit_scoped(&owner, &interrupted.input, &[], &[], Some(&chat_id), true)
            .unwrap()
            .task;
        state.store.append_event(&owner, &failed.run_id, json!({
            "type":"RUN_STARTED","protocolVersion":"1.0","threadId":chat_id,"runId":"interrupted-run"
        })).unwrap();
        state.chats.start_turn(&chat_id, &failed.run_id).unwrap();
        state
            .chats
            .record_protocol_message(&chat_id, &failed.run_id, &interrupted_user)
            .unwrap();
        let forbidden = root.path().join("must-never-be-replayed");
        let deferred = json!({"type":"attachment","attachment":{"type":"hook_deferred_tool",
            "toolUseID":"pending","toolName":"Shell","toolInput":{"command":format!("touch '{}'", forbidden.display())}}});
        std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap()
            .write_all(format!("{deferred}\n").as_bytes())
            .unwrap();
        let original = std::fs::read(&transcript).unwrap();
        super::super::commit_agent_terminal_for_test(
            &state.store,
            &owner,
            &failed.run_id,
            State::Failed,
            None,
            Some("timeout_exceeded".into()),
        )
        .unwrap();
        // Deliberately leave Chat metadata Running: emulate process death after
        // terminal task commit and before Chat cleanup.
        // Crash after durable generation staging but before the authority
        // switch: reopening must reuse the same pending plan and file, not
        // manufacture a new run or execute the uncommitted tool tail.
        let pending = state.store.prepare_chat_recovery(&owner, &chat_id).unwrap();
        let (old_path, target_path) = state.chats.recovery_paths(&pending).unwrap();
        state.chats.fence_recovery(&chat_id).unwrap();
        session::jsonl::JsonlWriter::stage_persistent_chat_recovery(
            old_path,
            target_path,
            pending
                .checkpoint()
                .map(|proof| session::jsonl::ChatTranscriptCheckpoint {
                    byte_len: proof.byte_len,
                    sha256: proof.sha256.clone(),
                }),
        )
        .await
        .unwrap();
        drop(control);
        host.apps.lock().unwrap().remove(app);
        drop(state);
        let state = host.app_state(app).unwrap();
        assert!(state
            .store
            .committed_chat_recoveries(app)
            .unwrap()
            .is_empty());
        assert!(host
            .recover_chat_if_needed(&state, &owner, &chat_id, None)
            .await
            .unwrap());
        assert_eq!(state.chats.get(&chat_id).unwrap().state, ChatState::Idle);
        assert_eq!(
            state
                .chats
                .protocol_history_page(&chat_id, None, 10)
                .unwrap()
                .messages,
            committed_history
        );
        assert_eq!(
            state
                .chats
                .protocol_attempt_history_page(&chat_id, &failed.run_id, None, 10)
                .unwrap()
                .messages,
            vec![interrupted_user]
        );
        assert_eq!(std::fs::read(&transcript).unwrap(), original);
        let current = state.chats.transcript_path(&chat_id).unwrap();
        assert_ne!(current, transcript);
        assert_eq!(
            std::fs::read(current).unwrap(),
            committed.source().as_bytes()
        );
        assert!(!forbidden.exists());
        assert!(host.active.lock().await.is_empty());
        assert!(!host
            .recover_chat_if_needed(&state, &owner, &chat_id, None)
            .await
            .unwrap());
        let mut next_history = committed_history;
        next_history.push(json!({"id":"next-user","role":"user","content":"continue safely"}));
        let (next, _) = normalize_page_run_input(
            &state,
            json!({"threadId":chat_id,"runId":"new-run",
            "protocolVersion":"1.0","messages":next_history}),
            Some(&chat_id),
        )
        .unwrap();
        assert!(
            state
                .store
                .admit_scoped(&owner, &next.input, &[], &[], Some(&chat_id), true)
                .unwrap()
                .start
        );
        assert!(!forbidden.exists());
    }
}
