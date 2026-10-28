//! Conversation selections, effective query state, and the request inventory.
//!
//! The original transcript is retained. Profile changes rebuild the system
//! projection and retire scoped skill instructions at the outer query gate.

use super::ConversationOrchestrator;
use lingxi_core::host::orchestrator::{HandleError, OrchestratorHandle};
use lingxi_core::host::session_context::{
    ContextBlockKind, ContextBlockManifest, ContextManifest, ConversationMode, SessionModeSnapshot,
    SessionModeTransition,
};
use lingxi_core::types::ConversationMessage;
use sha2::{Digest, Sha256};

#[derive(Default)]
pub(crate) struct ConversationContextRuntime {
    state: SessionModeSnapshot,
    active_epoch: u64,
    selected_epoch: u64,
    restored: bool,
    manifest: ContextManifest,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
        StaticMemoryProvider,
    };
    use lingxi_core::types::{utf16_json::Utf16JsonProjection as Source, MessageId};
    use std::sync::Arc;

    fn orchestrator_with_tools(tools: Arc<tool_api::ToolRegistry>) -> ConversationOrchestrator {
        ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(Vec::new())),
            tools,
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        )
    }

    fn orchestrator() -> ConversationOrchestrator {
        orchestrator_with_tools(Arc::new(tool_api::ToolRegistry::new()))
    }

    #[derive(Default)]
    struct CaptureCompactRequests(std::sync::Mutex<Vec<sidequery::SideQueryRequest>>);

    #[derive(Default)]
    struct CaptureSideRequests(std::sync::Mutex<Vec<sidequery::SideQueryRequest>>);

    #[async_trait::async_trait]
    impl sidequery::SideQueryClient for CaptureSideRequests {
        async fn query(
            &self,
            request: sidequery::SideQueryRequest,
        ) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
            self.0.lock().unwrap().push(request);
            Ok(sidequery::SideQueryResponse {
                text: Some(r#"{"name":"current-topic"}"#.into()),
                structured: None,
                tool_calls: Vec::new(),
                usage: cost::Usage::default(),
                stop_reason: Some("end_turn".into()),
                retry_count: 0,
            })
        }
    }

    #[derive(Default)]
    struct CaptureForkRequests(
        std::sync::Mutex<Vec<lingxi_core::host::subagent_spawn::SubagentSpawnRequest>>,
    );

    #[async_trait::async_trait]
    impl lingxi_core::host::subagent_spawn::SubagentSpawner for CaptureForkRequests {
        async fn spawn(
            &self,
            _: lingxi_core::host::subagent_spawn::SubagentSpawnRequest,
            _: lingxi_core::host::subagent_spawn::SubagentInheritance,
        ) -> Result<
            lingxi_core::host::subagent_spawn::SubagentResult,
            lingxi_core::host::subagent_spawn::SubagentSpawnError,
        > {
            Err(
                lingxi_core::host::subagent_spawn::SubagentSpawnError::Internal(
                    "only async forks are expected".into(),
                ),
            )
        }

        async fn spawn_async(
            &self,
            request: lingxi_core::host::subagent_spawn::SubagentSpawnRequest,
            _: lingxi_core::host::subagent_spawn::SubagentInheritance,
        ) -> Result<
            lingxi_core::host::subagent_spawn::AsyncLaunch,
            lingxi_core::host::subagent_spawn::SubagentSpawnError,
        > {
            self.0.lock().unwrap().push(request);
            Ok(lingxi_core::host::subagent_spawn::AsyncLaunch {
                agent_id: lingxi_core::types::AgentId::new(),
                output_file: String::new(),
            })
        }
    }

    struct ForkBudget;

    #[async_trait::async_trait]
    impl lingxi_core::host::budget::BudgetEnforcerHandle for ForkBudget {
        async fn check_and_charge(
            &self,
            _: u64,
        ) -> Result<(), lingxi_core::host::budget::BudgetError> {
            Ok(())
        }

        async fn snapshot_total_nano_usd(&self) -> u64 {
            0
        }
    }

    #[async_trait::async_trait]
    impl sidequery::SideQueryClient for CaptureCompactRequests {
        async fn query(
            &self,
            request: sidequery::SideQueryRequest,
        ) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
            self.0.lock().unwrap().push(request);
            Ok(sidequery::SideQueryResponse {
                text: Some("<summary>Kept the user's project context.</summary>".into()),
                structured: None,
                tool_calls: Vec::new(),
                usage: cost::Usage::default(),
                stop_reason: Some("end_turn".into()),
                retry_count: 0,
            })
        }
    }

    #[tokio::test]
    async fn profile_boundary_invalidates_cached_fork_before_manual_compaction() {
        use lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput;

        let slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
        let captured = Arc::new(CaptureCompactRequests::default());
        let runner = Arc::new(
            sidequery::ForkedAgentRunner::new()
                .with_side_query_client(captured.clone(), "test-compact-model".into()),
        );
        let compactor = Arc::new(compaction::CompactionOrchestrator::with_autocompactor(
            compaction::Autocompactor::with_forked_runner(runner, slot.clone()),
            u64::MAX,
        ));
        let orch = orchestrator()
            .with_conversation_mode(ConversationMode::Code)
            .with_cache_safe_slot(slot.clone())
            .with_compaction(compactor);
        let retired = ConversationMessage::user_meta(MessageId::new(), "old skill body".into())
            .with_origin(compaction::invoked_skills::skill_activation_origin(0));
        let answer = |text: &str| ConversationMessage::Assistant {
            per_turn_effort: None,
            id: MessageId::new(),
            content: vec![lingxi_core::types::ContentBlock::Text {
                text: text.into(),
                citations: None,
            }],
            stop_reason: Some("end_turn".into()),
            resumable: None,
        };
        orch.session.lock().await.history = vec![
            ConversationMessage::user(MessageId::new(), "first question".into()),
            retired.clone(),
            answer("first answer"),
            ConversationMessage::user(MessageId::new(), "second question".into()),
            answer("second answer"),
        ];
        let model = orch.session.lock().await.model.clone();
        let old_system = SystemPromptInput::custom_prompt("OLD PROFILE SOURCE".into());
        orch.save_cache_safe_params(Some(&old_system), &model, &[])
            .await
            .unwrap();
        orch.save_current_turn_system_prompt(Some(&old_system))
            .await;
        let old = slot.get_last().await.unwrap();
        assert!(old
            .fork_context_messages
            .iter()
            .any(|row| row.id() == retired.id()));

        // Selection during a running turn is pending and must not disturb the
        // old turn's captured context.
        let turn = orch.turn_gate.lock().await;
        let selected = orch
            .select_conversation_mode(ConversationMode::Chat)
            .await
            .unwrap();
        assert!(selected.pending);
        assert_eq!(slot.get_last().await.unwrap().generation, old.generation);
        assert!(orch.current_turn_system_prompt().await.is_some());
        drop(turn);

        // Manual compaction admits the pending profile before rebuilding its
        // system source and sending a fork to the actual side-query client.
        orch.force_compact_with_instructions_and_cancel(
            None,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(orch.current_turn_system_prompt().await.is_none());
        let calls = captured.0.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let call = &calls[0];
        let system = call.system_prompt.as_ref().unwrap().string_at("").unwrap();
        assert!(system.contains("research, writing, learning"), "{system}");
        assert!(!system.contains("OLD PROFILE SOURCE"), "{system}");
        assert!(call.messages.iter().all(|row| row.id() != retired.id()));
        drop(calls);

        assert!(slot
            .save_if_generation_matches(old.generation, old)
            .await
            .is_err());
        let current = slot.get_last().await.unwrap();
        assert!(current
            .system_prompt
            .string_at("")
            .unwrap()
            .contains("research, writing, learning"));
    }

    #[tokio::test]
    async fn profile_boundary_fork_uses_current_system_and_filtered_history() {
        use lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput;

        let captured = Arc::new(CaptureForkRequests::default());
        let orch = orchestrator()
            .with_conversation_mode(ConversationMode::Code)
            .with_fork_spawner(captured.clone())
            .with_fork_budget(Arc::new(ForkBudget));
        let retired = ConversationMessage::user_meta(MessageId::new(), "old skill body".into())
            .with_origin(compaction::invoked_skills::skill_activation_origin(0));
        let user = ConversationMessage::user(MessageId::new(), "question".into());
        let assistant = ConversationMessage::Assistant {
            per_turn_effort: None,
            id: MessageId::new(),
            content: vec![lingxi_core::types::ContentBlock::Text {
                text: "answer".into(),
                citations: None,
            }],
            stop_reason: Some("end_turn".into()),
            resumable: None,
        };
        let raw = vec![user.clone(), retired.clone(), assistant.clone()];
        orch.session.lock().await.history = raw.clone();
        orch.save_current_turn_system_prompt(Some(&SystemPromptInput::custom_prompt(
            "OLD PROFILE SOURCE".into(),
        )))
        .await;
        let turn = orch.turn_gate.lock().await;
        assert!(
            orch.select_conversation_mode(ConversationMode::Chat)
                .await
                .unwrap()
                .pending
        );
        drop(turn);
        orch.fork_conversation(&Source::from("follow up"))
            .await
            .unwrap();
        assert!(!orch.conversation_profile().await.pending);

        let requests = captured.0.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        let system = request
            .fork_parent_system_prompt
            .as_ref()
            .unwrap()
            .string_at("")
            .unwrap();
        assert!(system.contains("research, writing, learning"), "{system}");
        assert!(!system.contains("OLD PROFILE SOURCE"), "{system}");
        let history = request.fork_context_messages.as_ref().unwrap();
        assert!(history.iter().any(|row| row.id() == user.id()));
        assert!(history.iter().any(|row| row.id() == assistant.id()));
        assert!(history.iter().all(|row| row.id() != retired.id()));
        drop(requests);
        assert_eq!(orch.session.lock().await.history, raw);
    }

    #[tokio::test]
    async fn profile_boundary_rebuilds_recap_and_name_without_old_cached_context() {
        use lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput;

        let captured = Arc::new(CaptureSideRequests::default());
        let slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
        let runner = Arc::new(
            sidequery::ForkedAgentRunner::new()
                .with_side_query_client(captured.clone(), "test-model".into()),
        );
        let orch = orchestrator()
            .with_conversation_mode(ConversationMode::Code)
            .with_cache_safe_slot(slot.clone())
            .with_recap_runner(runner);
        let retired = ConversationMessage::user_meta(MessageId::new(), "old skill body".into())
            .with_origin(compaction::invoked_skills::skill_activation_origin(0));
        orch.session.lock().await.history = vec![
            ConversationMessage::user(MessageId::new(), "question".into()),
            retired.clone(),
        ];
        let model = orch.session.lock().await.model.clone();
        orch.save_cache_safe_params(
            Some(&SystemPromptInput::custom_prompt(
                "OLD PROFILE SOURCE".into(),
            )),
            &model,
            &[],
        )
        .await
        .unwrap();
        orch.select_conversation_mode(ConversationMode::Chat)
            .await
            .unwrap();
        assert!(slot.get_last().await.is_none());
        *orch
            .prompt_runtime
            .pending_prompt_source_vector
            .lock()
            .await = Some(vec![
            lingxi_llm_client::providers::anthropic::system_prompt::PromptText::from_string(
                "main request snapshot",
            ),
        ]);
        orch.generate_recap_query(tokio_util::sync::CancellationToken::new())
            .await
            .unwrap();
        assert!(orch
            .generate_session_name_query(tokio_util::sync::CancellationToken::new())
            .await
            .unwrap()
            .is_some());
        let requests = captured.0.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for request in requests.iter() {
            let system = request
                .system_prompt
                .as_ref()
                .unwrap()
                .string_at("")
                .unwrap();
            assert!(system.contains("research, writing, learning"), "{system}");
            assert!(!system.contains("OLD PROFILE SOURCE"));
            assert!(request.messages.iter().all(|row| row.id() != retired.id()));
        }
        drop(requests);
        assert!(slot.get_last().await.is_none());
        let pending = orch
            .prompt_runtime
            .pending_prompt_source_vector
            .lock()
            .await;
        assert_eq!(
            pending.as_ref().unwrap()[0].display_text(),
            "main request snapshot"
        );
    }

    /// Minimal deferred Skill whose activation follows the real Skill tool's
    /// session/epoch provenance, without loading a file or executing setup.
    struct DeferredSkill;
    #[async_trait::async_trait]
    impl tool_api::tool_trait::Tool for DeferredSkill {
        fn name(&self) -> &str {
            "Skill"
        }
        fn input_schema(&self) -> &Source {
            static SCHEMA: once_cell::sync::Lazy<Source> = once_cell::sync::Lazy::new(|| {
                tool_api::tool_trait::generated_builtin_result(
                    &serde_json::json!({"type":"object","properties":{}}),
                )
            });
            &SCHEMA
        }
        fn input_repair_metadata(&self) -> tool_api::tool_trait::ToolInputRepairMetadata {
            tool_api::tool_trait::ToolInputRepairMetadata::Known {
                schema: tool_api::tool_trait::ToolInputRepairSchema::Advertised,
                schema_provenance:
                    tool_api::tool_trait::ToolInputSchemaProvenance::GeneratedBuiltin,
                exempt_top_level_keys: None,
            }
        }
        fn is_enabled(&self, _: &tool_api::tool_trait::ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            4096
        }
        fn is_concurrency_safe(&self, _: &Source) -> bool {
            true
        }
        fn is_read_only(&self, _: &Source) -> bool {
            true
        }
        async fn validate_input(
            &self,
            _: &Source,
            _: &tool_api::context::ToolUseContext,
        ) -> Result<(), tool_api::tool_trait::ValidationError> {
            Ok(())
        }
        async fn check_permissions(
            &self,
            _: &Source,
            _: &tool_api::context::ToolUseContext,
        ) -> permission::PermissionResult {
            permission::PermissionResult::Allow {
                reason: permission::PermissionDecisionReason::Other {
                    reason: "test".into(),
                },
                updated_input: None,
                update_destination: None,
                metadata: permission::result::PermissionMetadata::default(),
            }
        }
        async fn description(
            &self,
            _: &Source,
            _: &tool_api::tool_trait::DescriptionOptions,
        ) -> String {
            "deferred skill".into()
        }
        async fn prompt(&self, _: &tool_api::tool_trait::PromptOptions) -> Source {
            "".into()
        }
        async fn call(
            &self,
            _: Source,
            ctx: tool_api::context::ToolUseContext,
            _: tool_api::progress::ToolProgressSender,
        ) -> Result<tool_api::tool_trait::ToolCallResult, tool_api::tool_trait::ToolError> {
            let session = ctx
                .session
                .expect("deferred Skill has a session")
                .lock()
                .await
                .session_id
                .to_string();
            let scope = compaction::invoked_skills::InvokedSkillScopeRef::new(Some(&session), None);
            let name: Source = "deferred".into();
            let body: Source = "guidance from old turn".into();
            let id = MessageId::new();
            let message = ConversationMessage::user(id, "guidance from old turn".to_string())
                .with_origin(
                    compaction::invoked_skills::skill_activation_origin_with_skill(
                        compaction::invoked_skills::context_epoch(scope),
                        &name,
                        std::path::Path::new("/skill"),
                    ),
                );
            compaction::invoked_skills::register_activation_with_message(
                &name,
                std::path::Path::new("/skill"),
                &body,
                None,
                scope,
                id,
            );
            Ok(tool_api::tool_trait::ToolCallResult {
                data: tool_api::tool_trait::generated_builtin_result(
                    &serde_json::json!({"model_content":"Launching skill: deferred"}),
                ),
                model_content: None,
                new_messages: vec![message],
                context_modifier: None,
                is_error: false,
                mcp_meta: None,
                subagent_hooks_fired: false,
            })
        }
    }

    fn persisted_orchestrator(
        root: &std::path::Path,
        id: lingxi_core::types::SessionId,
    ) -> (ConversationOrchestrator, Arc<session::jsonl::JsonlWriter>) {
        let fs: Arc<dyn lingxi_core::host::FileSystem> =
            Arc::new(platform_posix::fs::PosixFileSystem::new(root.to_path_buf()));
        let writer = Arc::new(session::jsonl::JsonlWriter::new(
            root.join(format!("{}.jsonl", id.as_uuid())),
            fs,
        ));
        let mut tools = tool_api::ToolRegistry::new();
        tools.register_builtin(Arc::new(DeferredSkill));
        (
            orchestrator_with_tools(Arc::new(tools))
                .with_session_id(id)
                .with_jsonl_writer(writer.clone()),
            writer,
        )
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn pending_profile_survives_cold_resume_and_expires_deferred_old_skill_at_boundary() {
        let _registry_guard = compaction::invoked_skills::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = tempfile::tempdir().unwrap();
        let id = lingxi_core::types::SessionId::new();
        let (original, _) = persisted_orchestrator(root.path(), id);
        let turn = original.turn_gate.lock().await;
        let selected = original
            .select_conversation_mode(ConversationMode::Chat)
            .await
            .unwrap();
        assert!(selected.pending);
        drop(turn);
        drop(original);

        let (recovered, writer) = persisted_orchestrator(root.path(), id);
        let restored = recovered
            .restore_conversation_profile(ConversationMode::Code)
            .await
            .unwrap();
        assert_eq!(
            (
                restored.active,
                restored.selected,
                restored.revision,
                restored.pending
            ),
            (ConversationMode::Code, ConversationMode::Chat, 1, true)
        );

        // Execute the actual deferred replay path; the test Skill stamps its
        // instruction with the profile retained by the interrupted query.
        let turn = recovered.turn_gate.lock().await;
        crate::resume::replay_deferred_tools_after_resume(
            &recovered,
            vec![lingxi_core::host::DeferredToolReplay {
                tool_use_id: "deferred-call".into(),
                tool_name: "Skill".into(),
                tool_input: Source::parse("{}").unwrap(),
                permission_mode: None,
                traceparent: None,
            }],
        )
        .await
        .unwrap();
        drop(turn);
        let skill = recovered
            .session
            .lock()
            .await
            .history
            .iter()
            .find(|row| {
                matches!(row, ConversationMessage::User { origin: Some(origin), .. }
                if origin.string_at("/type") == Some("skill_activation"))
            })
            .unwrap()
            .clone();
        let skill_id = skill.id();
        let scope_id = id.to_string();
        let scope = compaction::invoked_skills::InvokedSkillScopeRef::new(Some(&scope_id), None);
        assert_eq!(compaction::invoked_skills::context_epoch(scope), 0);
        assert_eq!(compaction::invoked_skills::filter_for_scope(scope).len(), 1);

        // Hosts can repeat restoration after the owned resume completed; that
        // must not turn this already-applied replay into a new activation.
        recovered
            .restore_conversation_profile(ConversationMode::Chat)
            .await
            .unwrap();
        assert_eq!(compaction::invoked_skills::filter_for_scope(scope).len(), 1);
        let turn = recovered.turn_gate.lock().await;
        recovered.admit_conversation_profile().await.unwrap();
        drop(turn);
        assert!(compaction::invoked_skills::filter_for_scope(scope).is_empty());
        assert!(!recovered
            .session
            .lock()
            .await
            .model_context_history()
            .iter()
            .any(|row| row.id() == skill_id));
        let applied = writer.persisted_session_mode_transition().await.unwrap();
        assert_eq!(
            (
                applied.active,
                applied.selected,
                applied.revision,
                applied.active_epoch,
                applied.pending
            ),
            (ConversationMode::Chat, ConversationMode::Chat, 1, 1, false)
        );
        drop(recovered);

        // A second crash after admission reads the applied boundary and cannot
        // make the old-epoch instruction effective again.
        let (again, _) = persisted_orchestrator(root.path(), id);
        again.session.lock().await.history.push(skill);
        let profile = again
            .restore_conversation_profile(ConversationMode::Code)
            .await
            .unwrap();
        assert_eq!(
            (profile.active, profile.pending),
            (ConversationMode::Chat, false)
        );
        assert!(!again
            .session
            .lock()
            .await
            .model_context_history()
            .iter()
            .any(|row| row.id() == skill_id));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn failed_boundary_persistence_keeps_pending_profile_and_old_skill() {
        let _registry_guard = compaction::invoked_skills::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = tempfile::tempdir().unwrap();
        let id = lingxi_core::types::SessionId::new();
        let (orch, writer) = persisted_orchestrator(root.path(), id);
        let turn = orch.turn_gate.lock().await;
        orch.select_conversation_mode(ConversationMode::Chat)
            .await
            .unwrap();
        drop(turn);
        let input = super::super::drivers_impl::skill_prompt_inputs(
            "/keep".into(),
            "keep".into(),
            Vec::new(),
        )
        .remove(1);
        let skill_id = MessageId::new();
        let mut skill = ConversationMessage::user_meta(skill_id, "keep".to_string());
        orch.stamp_skill_prompt_activation(&input, &mut skill).await;
        orch.session.lock().await.history.push(skill);

        // Make the transcript path unwritable while keeping the writer's
        // in-memory metadata and then restore it for the retry.
        let path = writer.active_path();
        let backup = path.with_extension("backup");
        std::fs::rename(&path, &backup).unwrap();
        std::fs::create_dir(&path).unwrap();
        let turn = orch.turn_gate.lock().await;
        assert!(orch.admit_conversation_profile().await.is_err());
        let current = orch.conversation_profile().await;
        assert_eq!(
            (current.active, current.selected, current.pending),
            (ConversationMode::Code, ConversationMode::Chat, true)
        );
        assert!(orch
            .session
            .lock()
            .await
            .model_context_history()
            .iter()
            .any(|row| row.id() == skill_id));
        assert!(
            writer
                .persisted_session_mode_transition()
                .await
                .unwrap()
                .pending
        );
        std::fs::remove_dir(&path).unwrap();
        std::fs::rename(&backup, &path).unwrap();
        orch.admit_conversation_profile().await.unwrap();
        drop(turn);
        assert!(
            !writer
                .persisted_session_mode_transition()
                .await
                .unwrap()
                .pending
        );
    }

    #[tokio::test]
    async fn conversation_profiles_replace_cached_engineering_instructions() {
        let orch = orchestrator();
        let model = orch.session.lock().await.model.clone();
        let code = orch.assemble_system_prompt_preview().await.unwrap();
        assert!(code.contains("command-line coding assistant"));
        let selected = orch
            .select_conversation_mode(ConversationMode::Chat)
            .await
            .unwrap();
        assert_eq!(
            selected.session_id,
            orch.current_session_id().await.as_uuid().to_string()
        );
        assert!(!selected.pending);
        let chat = orch.assemble_system_prompt_preview().await.unwrap();
        assert!(chat.contains("research, writing, learning"));
        assert!(!chat.contains("command-line coding assistant"));
        assert!(!chat.contains("# Doing tasks"));
        assert!(chat.contains("Earlier profile directives"));
        orch.select_conversation_mode(ConversationMode::Code)
            .await
            .unwrap();
        let code_again = orch.assemble_system_prompt_preview().await.unwrap();
        assert!(code_again.contains("command-line coding assistant"));
        assert!(!code_again.contains("You are LingXi, a capable assistant"));
        assert_eq!(orch.session.lock().await.model, model);
    }

    #[tokio::test]
    async fn conversation_profiles_pending_cancel_preserves_effective_epoch() {
        let orch = orchestrator();
        let turn = orch.turn_gate.lock().await;
        let pending = orch
            .select_conversation_mode(ConversationMode::Chat)
            .await
            .unwrap();
        assert!(pending.pending);
        assert_eq!(pending.active, ConversationMode::Code);
        orch.record_context_projection(None, &[], &[]).await;
        assert!(!orch.current_context_manifest().await.projected);
        let canceled = orch
            .select_conversation_mode(ConversationMode::Code)
            .await
            .unwrap();
        assert!(!canceled.pending);
        assert_eq!(canceled.revision, 2);
        assert_eq!(orch.profile_epoch().await, 0);
        drop(turn);
        let _turn = orch.turn_gate.lock().await;
        orch.admit_conversation_profile().await.unwrap();
        assert_eq!(orch.profile_epoch().await, 0);
        assert_eq!(
            orch.conversation_profile().await.active,
            ConversationMode::Code
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn captured_profile_restores_pending_epochs_without_a_writer() {
        let _registry_guard = compaction::invoked_skills::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let session_id = lingxi_core::types::SessionId::new();
        let old_id = MessageId::new();
        let current_id = MessageId::new();
        let user_id = MessageId::new();
        let excluded_id = MessageId::new();
        let sid = session_id.as_uuid();
        let rows = [
            serde_json::json!({"type":"user","uuid":old_id.as_uuid(),"sessionId":sid,"parentUuid":null,"isMeta":true,"origin":{"type":"skill_activation","context_epoch":1,"skill_name":"old"},"message":{"role":"user","content":"same text"}}),
            serde_json::json!({"type":"user","uuid":current_id.as_uuid(),"sessionId":sid,"parentUuid":old_id.as_uuid(),"isMeta":true,"origin":{"type":"skill_activation","context_epoch":2,"skill_name":"current"},"message":{"role":"user","content":"current instruction"}}),
            serde_json::json!({"type":"user","uuid":user_id.as_uuid(),"sessionId":sid,"parentUuid":current_id.as_uuid(),"message":{"role":"user","content":"same text"}}),
            serde_json::json!({"type":"user","uuid":excluded_id.as_uuid(),"sessionId":sid,"parentUuid":user_id.as_uuid(),"isModelContextExcluded":true,"message":{"role":"user","content":"excluded evidence"}}),
            serde_json::json!({"type":"session-mode","sessionId":sid,"sessionMode":"code","contextRevision":3,"profileEpoch":3,"admittedMode":"chat","admittedEpoch":2,"pending":true}),
        ];
        let source = rows
            .iter()
            .map(|row| format!("{row}\n"))
            .collect::<String>();
        let loaded = session::jsonl::route_lines(&source);
        let (messages, _) = session::jsonl::build_conversation_chain(&loaded, &sid.to_string());
        let profile =
            session::JsonlWriter::session_mode_transition_from_source(&source, &sid.to_string());
        let orch = orchestrator().with_session_id(session_id);
        orch.restore_captured_session_history(
            sid,
            &messages,
            &loaded.messages_in_order,
            false,
            permission::PermissionMode::Default,
            profile,
        )
        .await
        .unwrap();

        let mode = orch.conversation_profile().await;
        assert_eq!(
            (mode.active, mode.selected, mode.revision, mode.pending),
            (ConversationMode::Chat, ConversationMode::Code, 3, true)
        );
        assert_eq!(orch.profile_epoch().await, 2);
        let history = orch.session.lock().await;
        assert_eq!(history.history.len(), 4);
        let model = history.model_context_history();
        assert!(model.iter().any(|row| row.id() == current_id));
        assert!(model.iter().any(|row| row.id() == user_id));
        assert!(!model
            .iter()
            .any(|row| row.id() == old_id || row.id() == excluded_id));
        drop(history);
        let scoped_id = session_id.to_string();
        let active = compaction::invoked_skills::filter_for_scope(
            compaction::invoked_skills::InvokedSkillScopeRef::new(Some(&scoped_id), None),
        );
        assert!(active
            .iter()
            .any(|skill| skill.name.string_at("") == Some("current")));

        let _turn = orch.turn_gate.lock().await;
        orch.admit_conversation_profile().await.unwrap();
        assert_eq!(
            orch.conversation_profile().await.active,
            ConversationMode::Code
        );
        assert_eq!(orch.profile_epoch().await, 3);
        let model = orch.session.lock().await.model_context_history();
        assert!(!model.iter().any(|row| row.id() == current_id));
        assert!(model.iter().any(|row| row.id() == user_id));
    }

    #[tokio::test]
    async fn conversation_profiles_retire_only_host_marked_skill_instructions() {
        let orch = orchestrator();
        let old_id = MessageId::new();
        let user_id = MessageId::new();
        let mut old = ConversationMessage::user_meta(old_id, "Do the old skill action".to_string());
        if let ConversationMessage::User { origin, .. } = &mut old {
            *origin = Some(compaction::invoked_skills::skill_activation_origin(0));
        }
        let user = ConversationMessage::user_meta(user_id, "Do the old skill action".to_string());
        orch.session.lock().await.history.extend([old, user]);
        orch.select_conversation_mode(ConversationMode::Chat)
            .await
            .unwrap();
        let session = orch.session.lock().await;
        assert_eq!(session.history.len(), 2, "the transcript remains available");
        let projected = session.model_context_history();
        assert!(!projected.iter().any(|row| row.id() == old_id));
        assert!(projected.iter().any(|row| row.id() == user_id));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn conversation_profiles_initial_restore_preserves_current_epoch_skill() {
        let _registry_guard = compaction::invoked_skills::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let orch = orchestrator();
        let id = MessageId::new();
        let mut row = ConversationMessage::user_meta(id, "Current task guidance".to_string());
        if let ConversationMessage::User { origin, .. } = &mut row {
            *origin = Some(
                compaction::invoked_skills::skill_activation_origin_with_skill(
                    0,
                    &Source::from("current"),
                    std::path::Path::new("/skills/current"),
                ),
            );
        }
        orch.session.lock().await.history.push(row);
        orch.restore_conversation_profile(ConversationMode::Code)
            .await
            .unwrap();
        assert!(orch
            .session
            .lock()
            .await
            .model_context_history()
            .iter()
            .any(|row| row.id() == id));
        let scope_session = orch.current_session_id().await.to_string();
        let active = compaction::invoked_skills::filter_for_scope(
            compaction::invoked_skills::InvokedSkillScopeRef::new(Some(&scope_session), None),
        );
        assert!(active
            .iter()
            .any(|skill| skill.name.string_at("") == Some("current")));
    }
}

impl ConversationContextRuntime {
    fn bind(&mut self, session_id: String) {
        if self.state.session_id != session_id {
            self.state = SessionModeSnapshot {
                session_id,
                active: self.state.selected,
                selected: self.state.selected,
                ..Default::default()
            };
            self.active_epoch = 0;
            self.selected_epoch = 0;
            self.restored = false;
            self.manifest = ContextManifest::default();
        }
    }
}

fn block(
    id: String,
    source: &str,
    kind: ContextBlockKind,
    scope: &str,
    reason: &str,
    bytes: &[u8],
) -> ContextBlockManifest {
    ContextBlockManifest {
        id,
        source: source.into(),
        version: format!("{:x}", Sha256::digest(bytes)),
        kind,
        scope: scope.into(),
        reason: reason.into(),
        bytes: bytes.len() as u64,
    }
}

impl ConversationOrchestrator {
    /// Initialize a host's profile without changing execution permissions.
    #[must_use]
    pub fn with_conversation_mode(self, mode: ConversationMode) -> Self {
        // Builders run before the orchestrator is shared with an executor.
        let mut context = self
            .conversation_context
            .try_lock()
            .expect("unshared profile builder");
        context.state.active = mode;
        context.state.selected = mode;
        drop(context);
        self
    }

    pub(crate) async fn conversation_profile(&self) -> SessionModeSnapshot {
        // Hooks may inspect the effective profile while the session activation
        // transaction owns the administrative gate. Read the identity only
        // after obtaining the context guard, so an old read cannot rebind a
        // freshly restored snapshot to a previous session.
        let mut context = self.conversation_context.lock().await;
        let session_id = self.current_session_id().await.as_uuid().to_string();
        context.bind(session_id);
        context.state.clone()
    }

    pub(crate) async fn select_conversation_mode(
        &self,
        mode: ConversationMode,
    ) -> Result<SessionModeSnapshot, HandleError> {
        // An idle selection owns the same gate as compaction/resume/turns.
        // A busy selection changes only `selected`; the running query keeps
        // `active` and all of its already-admitted instructions and tools.
        let turn = self.turn_gate.try_lock().ok();
        let transition = self.conversation_transition.lock().await;
        let session_id = self.current_session_id().await.as_uuid().to_string();
        let mut context = self.conversation_context.lock().await;
        if self.current_session_id().await.as_uuid().to_string() != session_id {
            return Err(HandleError::ActionFailed(
                "conversation changed before profile selection".into(),
            ));
        }
        context.bind(session_id.clone());
        if context.state.selected == mode {
            return Ok(context.state.clone());
        }
        let revision = context.state.revision.checked_add(1).ok_or_else(|| {
            HandleError::ActionFailed("conversation context revision exhausted".into())
        })?;
        // Keep epochs exactly representable in the source JSON projection.
        if revision > (1_u64 << 53) - 1 {
            return Err(HandleError::ActionFailed(
                "conversation context revision exhausted".into(),
            ));
        }
        let selected_epoch = if context.state.active == mode {
            context.active_epoch
        } else {
            revision
        };
        let apply_now = turn.is_some() && context.state.active != mode;
        if apply_now
            && crate::native_computer::has_unsettled_profile_receipts(self)
                .await
                .map_err(|error| HandleError::ActionFailed(error.to_string()))?
        {
            return Err(HandleError::ActionFailed(
                "A computer receipt is still pending. Finish its current-profile recovery before changing mode.".into(),
            ));
        }
        let transition_record = SessionModeTransition {
            selected: mode,
            active: if apply_now {
                mode
            } else {
                context.state.active
            },
            revision,
            selected_epoch,
            active_epoch: if apply_now {
                selected_epoch
            } else {
                context.active_epoch
            },
            pending: !apply_now && context.state.active != mode,
        };
        if let Some(writer) = &self.transcript.jsonl_writer {
            let path = writer.active_path();
            if path.file_stem().and_then(|name| name.to_str()) != Some(session_id.as_str()) {
                return Err(HandleError::ActionFailed(
                    "conversation changed before profile persistence".into(),
                ));
            }
            writer
                .append_session_mode_transition(&session_id, &transition_record)
                .await
                .map_err(|error| {
                    HandleError::ActionFailed(format!("persist conversation profile: {error}"))
                })?;
        }
        // No live state is changed until the durable append succeeds.
        context.state.selected = mode;
        context.selected_epoch = selected_epoch;
        context.state.revision = revision;
        context.state.pending = transition_record.pending;
        if apply_now {
            self.reset_profile_projection(&session_id, selected_epoch, true)
                .await;
            context.active_epoch = selected_epoch;
            context.state.active = mode;
            context.state.pending = false;
        }
        context.manifest.projected = false;
        let snapshot = context.state.clone();
        drop(context);
        drop(transition);
        drop(turn);
        self.output.emit_conversation_mode(&snapshot).await;
        Ok(snapshot)
    }

    pub(crate) async fn restore_conversation_profile(
        &self,
        fallback: ConversationMode,
    ) -> Result<SessionModeSnapshot, HandleError> {
        let turn = self.turn_gate.lock().await;
        let transition = self.conversation_transition.lock().await;
        let snapshot = self.restore_conversation_profile_locked(fallback).await?;
        drop(transition);
        drop(turn);
        self.output.emit_conversation_mode(&snapshot).await;
        Ok(snapshot)
    }

    /// Restore a profile from the already captured transcript, including when
    /// this runtime deliberately has no transcript writer.
    pub(crate) async fn restore_captured_conversation_profile(
        &self,
        fallback: ConversationMode,
        persisted: Option<SessionModeTransition>,
    ) -> Result<SessionModeSnapshot, HandleError> {
        let turn = self.turn_gate.lock().await;
        let transition = self.conversation_transition.lock().await;
        let snapshot = self
            .restore_conversation_profile_state_locked(fallback, persisted)
            .await?;
        drop(transition);
        drop(turn);
        self.output.emit_conversation_mode(&snapshot).await;
        Ok(snapshot)
    }

    /// Restore inside the same transaction that publishes the mounted session.
    /// The caller owns turn_gate and conversation_transition; no callback runs
    /// until the owning host publishes its SessionResumed event.
    pub(crate) async fn restore_conversation_profile_locked(
        &self,
        fallback: ConversationMode,
    ) -> Result<SessionModeSnapshot, HandleError> {
        let persisted = if let Some(writer) = &self.transcript.jsonl_writer {
            writer.persisted_session_mode_transition().await
        } else {
            None
        };
        self.restore_conversation_profile_state_locked(fallback, persisted)
            .await
    }

    async fn restore_conversation_profile_state_locked(
        &self,
        fallback: ConversationMode,
        persisted: Option<SessionModeTransition>,
    ) -> Result<SessionModeSnapshot, HandleError> {
        let session_id = self.current_session_id().await.as_uuid().to_string();
        let profile = persisted.unwrap_or(SessionModeTransition {
            selected: fallback,
            active: fallback,
            revision: 0,
            selected_epoch: 0,
            active_epoch: 0,
            pending: false,
        });
        let mut context = self.conversation_context.lock().await;
        if context.restored
            && context.state.session_id == session_id
            && context.state.selected == profile.selected
            && context.state.active == profile.active
            && context.state.revision == profile.revision
            && context.state.pending == profile.pending
            && context.active_epoch == profile.active_epoch
            && context.selected_epoch == profile.selected_epoch
        {
            return Ok(context.state.clone());
        }
        let registry_session_id = self.current_session_id().await.to_string();
        let registry_scope =
            compaction::invoked_skills::InvokedSkillScopeRef::new(Some(&registry_session_id), None);
        let retire = (context.state.session_id == session_id
            && (context.state.active != profile.active
                || context.active_epoch != profile.active_epoch))
            || (!compaction::invoked_skills::filter_for_scope(registry_scope).is_empty()
                && compaction::invoked_skills::context_epoch(registry_scope)
                    != profile.active_epoch);
        self.reset_profile_projection(&session_id, profile.active_epoch, retire)
            .await;
        self.restore_effective_skill_activations(profile.active_epoch)
            .await;
        context.active_epoch = profile.active_epoch;
        context.selected_epoch = profile.selected_epoch;
        context.restored = true;
        context.state = SessionModeSnapshot {
            session_id,
            active: profile.active,
            selected: profile.selected,
            revision: profile.revision,
            pending: profile.pending,
        };
        context.manifest = ContextManifest::default();
        let snapshot = context.state.clone();
        drop(context);
        Ok(snapshot)
    }

    /// Called once per admitted outer query, before any prompt is assembled.
    pub(crate) async fn admit_conversation_profile(&self) -> Result<(), HandleError> {
        let transition = self.conversation_transition.lock().await;
        let session_id = self.current_session_id().await.as_uuid().to_string();
        let mut context = self.conversation_context.lock().await;
        context.bind(session_id.clone());
        let skill_session_id = self.current_session_id().await.to_string();
        compaction::invoked_skills::set_context_epoch(
            compaction::invoked_skills::InvokedSkillScopeRef::new(Some(&skill_session_id), None),
            context.active_epoch,
        );
        if context.state.pending {
            if crate::native_computer::has_unsettled_profile_receipts(self)
                .await
                .map_err(|error| HandleError::ActionFailed(error.to_string()))?
            {
                // Cancel the queued change durably, so the next query can
                // recover the receipt with the profile that admitted it.
                let revision = context
                    .state
                    .revision
                    .checked_add(1)
                    .filter(|revision| *revision <= (1_u64 << 53) - 1)
                    .ok_or_else(|| {
                        HandleError::ActionFailed("conversation context revision exhausted".into())
                    })?;
                if let Some(writer) = &self.transcript.jsonl_writer {
                    writer
                        .append_session_mode_transition(
                            &session_id,
                            &SessionModeTransition {
                                selected: context.state.active,
                                active: context.state.active,
                                revision,
                                selected_epoch: context.active_epoch,
                                active_epoch: context.active_epoch,
                                pending: false,
                            },
                        )
                        .await
                        .map_err(|error| {
                            HandleError::ActionFailed(format!(
                                "persist canceled conversation profile: {error}"
                            ))
                        })?;
                }
                context.state.selected = context.state.active;
                context.state.revision = revision;
                context.state.pending = false;
                context.selected_epoch = context.active_epoch;
                context.manifest.projected = false;
                let snapshot = context.state.clone();
                drop(context);
                drop(transition);
                self.output.emit_conversation_mode(&snapshot).await;
                return Err(HandleError::ActionFailed(
                    "The mode change was canceled because a computer receipt is pending. Finish its recovery, then select the mode again.".into(),
                ));
            }
            if let Some(writer) = &self.transcript.jsonl_writer {
                let applied = SessionModeTransition {
                    selected: context.state.selected,
                    active: context.state.selected,
                    revision: context.state.revision,
                    selected_epoch: context.selected_epoch,
                    active_epoch: context.selected_epoch,
                    pending: false,
                };
                writer
                    .append_session_mode_transition(&session_id, &applied)
                    .await
                    .map_err(|error| {
                        HandleError::ActionFailed(format!(
                            "persist conversation profile admission: {error}"
                        ))
                    })?;
            }
            self.reset_profile_projection(&session_id, context.selected_epoch, true)
                .await;
            context.active_epoch = context.selected_epoch;
            context.state.active = context.state.selected;
            context.state.pending = false;
            context.manifest.projected = false;
            let snapshot = context.state.clone();
            drop(context);
            drop(transition);
            self.output.emit_conversation_mode(&snapshot).await;
        }
        Ok(())
    }

    /// The caller owns turn_gate. Permission grants and existing background
    /// owners are independent of this model-context transition.
    async fn reset_profile_projection(&self, _session_id: &str, epoch: u64, retire: bool) {
        // The cached prefix contains both profile instructions and skill
        // messages. Side queries can consume it without rebuilding history, so
        // it must not outlive the profile that produced it.
        self.cancel_idle_compaction();
        if let Some(slot) = self.model_runtime.cache_safe_slot.as_ref() {
            slot.invalidate().await;
        }
        if retire {
            if let Some(handle) = self.compaction_runtime.session_memory.as_ref() {
                // A previously launched extraction owns its old request, but
                // must not publish that result into the new profile's memory.
                // The commit path holds this lock while checking generation.
                let _extractor = handle.extractor.lock().await;
                handle
                    .generation
                    .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            }
        }
        // Skill scopes use the engine's prefixed SessionId; client events and
        // transcript filenames use its canonical UUID.
        let scoped_session_id = self.current_session_id().await.to_string();
        let session_id = scoped_session_id.as_str();
        let scope = compaction::invoked_skills::InvokedSkillScopeRef::new(Some(session_id), None);
        if retire {
            compaction::invoked_skills::expire_for_mode_change(session_id);
        }
        compaction::invoked_skills::set_context_epoch(scope, epoch);
        let retired = compaction::invoked_skills::expired_message_ids_for_scope(
            compaction::invoked_skills::InvokedSkillScopeRef::new(Some(session_id), None),
        );
        let restored_skill_rows = self
            .transcript
            .post_compact_skill_attachments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .copied()
            .collect::<Vec<_>>();
        {
            let mut session = self.session.lock().await;
            session.model_context_excluded_messages.extend(retired);
            if retire {
                let legacy_restored = restored_skill_rows.into_iter().filter(|id| {
                    session.history.iter().find(|row| row.id() == *id).is_none_or(|row| !matches!(row, ConversationMessage::User { origin: Some(origin), .. } if origin.string_at("/type") == Some("skill_activation")))
                }).collect::<Vec<_>>();
                session
                    .model_context_excluded_messages
                    .extend(legacy_restored);
            }
            // Only host-owned envelope provenance is inspected. Identical text
            // quoted in an attachment or tool result is never reclassified.
            let obsolete = session
                .history
                .iter()
                .filter_map(|message| match message {
                    ConversationMessage::User {
                        id,
                        origin: Some(origin),
                        ..
                    } if origin.string_at("/type") == Some("skill_activation") => {
                        let marker = origin
                            .to_json_string()
                            .ok()
                            .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok());
                        let previous_epoch = marker
                            .as_ref()
                            .and_then(|value| value.get("context_epoch"))
                            .and_then(serde_json::Value::as_u64);
                        (previous_epoch != Some(epoch)).then_some(*id)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            session.model_context_excluded_messages.extend(obsolete);
        }
        *self.prompt_runtime.prompt_snapshot.lock().await = None;
        *self.prompt_runtime.current_turn_system_prompt.lock().await = None;
        *self
            .prompt_runtime
            .pending_prompt_source_vector
            .lock()
            .await = None;
        self.prompt_runtime
            .prompt_snapshot_resume
            .store(false, std::sync::atomic::Ordering::Release);
        let mut cache = self.prompt_runtime.mod_prompt_sections.lock().await;
        cache.generation = cache.generation.wrapping_add(1);
        cache.context = None;
        cache.answers.clear();
    }

    pub(crate) async fn current_context_manifest(&self) -> ContextManifest {
        let state = self.conversation_profile().await;
        let context = self.conversation_context.lock().await;
        let mut manifest = context.manifest.clone();
        manifest.projected &= !state.pending
            && manifest.session.session_id == state.session_id
            && manifest.session.revision == state.revision;
        manifest.session = state;
        manifest
    }

    pub(crate) async fn profile_epoch(&self) -> u64 {
        self.conversation_context.lock().await.active_epoch
    }

    pub(crate) async fn stamp_skill_prompt_activation(
        &self,
        input: &super::QueuedPromptInput,
        message: &mut ConversationMessage,
    ) {
        if !input.turn_companion {
            return;
        }
        let session_id = self.current_session_id().await.to_string();
        let scope = compaction::invoked_skills::InvokedSkillScopeRef::new(Some(&session_id), None);
        let epoch = compaction::invoked_skills::context_epoch(scope);
        let mut activation = input
            .message_origin
            .clone()
            .filter(|source| source.string_at("/type") == Some("skill_activation"))
            .unwrap_or_else(|| compaction::invoked_skills::skill_activation_origin(epoch));
        activation
            .set_field(
                "context_epoch",
                lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
                    &serde_json::json!(epoch),
                )
                .expect("epoch is exact source JSON"),
            )
            .expect("host metadata is an object");
        if let Ok(name) = activation.subprojection("/skill_name") {
            compaction::invoked_skills::register_activation_with_message(
                &name,
                std::path::Path::new(""),
                &input.text,
                Some(&input.text),
                scope,
                message.id(),
            );
        }
        if let ConversationMessage::User { origin, .. } = message {
            *origin = Some(activation);
        }
    }

    /// Passive rehydration of host-stamped instructions. No loader, hook,
    /// embedded command, setup step or user action is executed here.
    async fn restore_effective_skill_activations(&self, epoch: u64) {
        use lingxi_core::types::{utf16_json::Utf16JsonProjection as Source, ContentBlock};
        let session_id = self.current_session_id().await.to_string();
        let scope = compaction::invoked_skills::InvokedSkillScopeRef::new(Some(&session_id), None);
        let history = self.session.lock().await.model_context_history();
        let attachments = self
            .transcript
            .post_compact_skill_attachments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        for message in history {
            let ConversationMessage::User {
                id,
                content,
                origin: Some(origin),
                ..
            } = message
            else {
                continue;
            };
            if origin.string_at("/type") != Some("skill_activation") {
                continue;
            }
            let marker_epoch = origin
                .to_json_string()
                .ok()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
                .and_then(|value| {
                    value
                        .get("context_epoch")
                        .and_then(serde_json::Value::as_u64)
                });
            if marker_epoch != Some(epoch) {
                continue;
            }
            if let Ok(name) = origin.subprojection("/skill_name") {
                let units = content
                    .iter()
                    .flat_map(|block| match block {
                        ContentBlock::Text { text, .. } => text.encode_utf16().collect::<Vec<_>>(),
                        ContentBlock::TextJsUtf16 {
                            utf16_code_units, ..
                        } => utf16_code_units.clone(),
                        _ => Vec::new(),
                    })
                    .collect::<Vec<_>>();
                let body = Source::root_string(String::from_utf16_lossy(&units), units)
                    .expect("restored skill source is text");
                let path = origin.string_at("/skill_path").unwrap_or("");
                compaction::invoked_skills::register_activation_with_message(
                    &name,
                    std::path::Path::new(path),
                    &body,
                    None,
                    scope,
                    id,
                );
            } else if let Some(bodies) = attachments.get(&id) {
                for (index, body) in bodies.iter().enumerate() {
                    if let Ok(name) = origin.subprojection(&format!("/skills/{index}/name")) {
                        let path = origin
                            .string_at(&format!("/skills/{index}/path"))
                            .unwrap_or("");
                        compaction::invoked_skills::register_activation_with_message(
                            &name,
                            std::path::Path::new(path),
                            body,
                            None,
                            scope,
                            id,
                        );
                    }
                }
            }
        }
    }

    pub(crate) async fn record_context_projection(
        &self,
        system: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
        history: &[ConversationMessage],
        tools: &[lingxi_core::host::SourceToolWireDefinition],
    ) {
        let session = self.conversation_profile().await;
        let mut blocks = Vec::with_capacity(history.len() + 2);
        if let Some(system) = system {
            let source = Self::system_prompt_source_text(system);
            if let Ok(bytes) = source.to_json_string() {
                blocks.push(block(
                    "system".into(),
                    "host_prompt_assembler",
                    ContextBlockKind::Instruction,
                    "session",
                    "current behavior, user/project rules and host guidance",
                    bytes.as_bytes(),
                ));
            }
        }
        for tool in tools {
            let bytes = format!(
                "{}\n{}\n{}\n{}",
                tool.description.to_json_string().unwrap_or_default(),
                tool.input_schema.to_json_string().unwrap_or_default(),
                tool.strict,
                tool.defer_loading
            );
            blocks.push(block(
                format!("tool:{}", tool.name),
                "eligible_tool_registry",
                ContextBlockKind::Capability,
                "request",
                if tool.defer_loading {
                    "deferred schema; discover when needed"
                } else {
                    "schema present; execution still requires authorization"
                },
                bytes.as_bytes(),
            ));
        }
        let injected = self.session.lock().await.injected_message_sources.clone();
        for message in history {
            let (source, kind) = match message {
                ConversationMessage::User {
                    origin: Some(origin),
                    ..
                } if origin.string_at("/type") == Some("skill_activation") => {
                    ("active_skill", ContextBlockKind::Instruction)
                }
                ConversationMessage::User {
                    content,
                    is_meta: false,
                    id,
                    ..
                } if !injected.contains_key(id)
                    && !content.iter().any(|block| {
                        matches!(block, lingxi_core::types::ContentBlock::ToolResult { .. })
                    }) =>
                {
                    ("user_request", ContextBlockKind::Instruction)
                }
                ConversationMessage::User { is_meta: false, .. } => {
                    ("user_or_tool_input", ContextBlockKind::Evidence)
                }
                ConversationMessage::User { .. } => {
                    ("host_context_attachment", ContextBlockKind::State)
                }
                ConversationMessage::Assistant { .. } => {
                    ("model_output", ContextBlockKind::Evidence)
                }
                ConversationMessage::System { .. } => ("host_state", ContextBlockKind::State),
            };
            if let Ok(bytes) = serde_json::to_vec(message) {
                blocks.push(block(
                    format!("message:{}", message.id()),
                    source,
                    kind,
                    "conversation",
                    "retained in the prepared model projection",
                    &bytes,
                ));
            }
        }
        if blocks.len() > 384 {
            let remaining = blocks.split_off(383);
            let total_bytes = remaining.iter().map(|block| block.bytes).sum::<u64>();
            let versions = remaining
                .iter()
                .map(|block| block.version.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let mut aggregate = block(
                "additional_context".into(),
                "prepared_projection",
                ContextBlockKind::State,
                "request",
                &format!(
                    "{} additional blocks summarized by fingerprint",
                    remaining.len()
                ),
                versions.as_bytes(),
            );
            aggregate.bytes = total_bytes;
            blocks.push(aggregate);
        }
        self.conversation_context.lock().await.manifest = ContextManifest {
            projected: !session.pending,
            session,
            blocks,
        };
    }
}
