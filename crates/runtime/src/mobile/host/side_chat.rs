//! Mobile sides reuse ready Host services without the durable turn/session path.

use super::*;
use crate::side_chat::{SideChatFactory, SideChatInfoDto, SideChatSession};
use client::adapter::{ClientEventSink, ListenerSink, PermissionRequestSink};
use command_api::CommandRegistry;
use orchestrator::{ProviderApiAdapter, SideChatParentSnapshot};
use tool_api::BuiltinToolContext;

pub(super) struct MobileSideChatFactory {
    pub api: Arc<llm_runtime::ApiService>,
    pub context: BuiltinToolContext,
    pub commands: Arc<RwLock<CommandRegistry>>,
}

#[async_trait]
impl SideChatFactory for MobileSideChatFactory {
    async fn build(
        &self,
        snapshot: SideChatParentSnapshot,
        sink: Arc<dyn ClientEventSink>,
        permission_sink: Arc<dyn PermissionRequestSink>,
    ) -> Result<SideChatSession, String> {
        let session_id = lingxi_core::types::SessionId::new();
        let api = Arc::new(self.api.fork_independent());
        let api_session_id = session_id.to_string();
        api.set_request_session_id_source(Arc::new(move || {
            let id = api_session_id.clone();
            Box::pin(async move { Some(id) })
        }));
        let ready_models =
            crate::side_chat::composition::credential_ready_model_refs(&api, &snapshot).await;
        let adapter = Arc::new(ProviderApiAdapter::new(api.clone()));
        let permission_gate =
            Arc::new(AdapterPermissionGate::new(permission_sink).with_event_sink(sink.clone()));
        permission_gate.set_session_id(Some(session_id.to_string()));
        let mut ctx = self.context.clone();
        let cleanup_failed = Arc::new(AtomicBool::new(false));
        ctx.side_chat = true;
        ctx.bus = Arc::new(telemetry::AnalyticsBus::disabled());
        ctx.side_chat_cleanup_failed = Some(cleanup_failed.clone());
        ctx.session_id = Some(session_id);
        ctx.default_model = snapshot.config.model.clone();
        ctx.session_cwd =
            tool_api::SessionCwd::new(snapshot.cwd.clone(), self.context.trusted_dirs());
        ctx.read_file_state = tool_api::read_file_state::new_read_file_state_map();
        ctx.worktree_session = tool_api::worktree_session::new_worktree_session_cell();
        ctx.permission_mode =
            permission::permission_mode_from_cli_string(&snapshot.permission_mode);
        ctx.permission_policy = Arc::new(
            self.context
                .permission_policy
                .fork_independent(ctx.permission_mode),
        );
        let policy_gate = permission::PolicyPermissionGate::new(
            ctx.permission_policy.clone(),
            permission_gate.clone(),
        )
        .with_path_translator(Arc::new(permission::FileSystemPathTranslator(
            ctx.fs.clone(),
        )));
        let perms: Arc<dyn PermissionGate> = Arc::new(policy_gate);
        ctx.permission_gate = Some(perms.clone());
        ctx.sandbox_enabled_override = None;
        ctx.subagent_spawner = None;
        ctx.agent_name_registry = None;
        ctx.task_registry = None;
        ctx.mailbox_router = None;
        ctx.budget_enforcer = None;
        ctx.main_loop_model_profile_provider = None;
        ctx.coordinator_mode = None;
        ctx.bash_edit_diff = None;
        ctx.mcp_registry = None;
        ctx.lsp_registry = None;
        ctx.task_lifecycle_hooks = None;
        ctx.hosted_search = Some(adapter.clone());
        ctx.mcp_token_counter = None;
        ctx.camera = None;
        ctx.audio = None;
        ctx.audio_recording_handles = Arc::new(Mutex::new(HashMap::new()));
        ctx.share = None;
        ctx.notifications = None;
        ctx.clipboard = None;
        ctx.location = None;
        ctx.device_status = None;
        ctx.haptics = None;
        ctx.deep_link = None;
        ctx.calendar = None;
        ctx.contacts = None;
        ctx.computer_control = None;
        ctx.android_git = None;
        ctx.android_git_secret = None;

        let mut commands = CommandRegistry::new();
        commands.set_side_chat(true);
        for command in self.commands.read().await.list_all() {
            if commands.execution_allowed(command) {
                commands.register_command(command.clone());
            }
        }
        let commands = Arc::new(RwLock::new(commands));
        let loader = Arc::new(
            crate::mobile::skill_loader::MobileDiskSkillLoader::with_session_id(
                commands.clone(),
                session_id.to_string(),
            )
            .with_prompt_cwd(ctx.session_cwd.clone()),
        );
        let question_broker = Arc::new(client::adapter::AskUserQuestionBroker::new(sink.clone()));
        let (question_tx, question_rx) = tokio::sync::mpsc::channel(8);
        let broker = question_broker.clone();
        tokio::spawn(async move {
            broker.run(question_rx).await;
        });
        let timeout = tool_ui::ask_user_question::AskUserQuestionTimeout::parse_or_default(
            ctx.ask_user_question_timeout.as_deref(),
        );
        let resolver = Arc::new(tool_ui::ask_user_question::TuiBridgeResolver::new(
            timeout,
            question_tx,
        ));
        let mut tools = ToolRegistry::new();
        tool_file::register_all(&mut tools, ctx.clone());
        tool_shell_mobile::register_all(&mut tools, ctx.clone());
        tool_web::register_all(&mut tools, ctx.clone(), None);
        tools.register_builtin(Arc::new(
            tool_ui::ask_user_question::AskUserQuestionTool::with_resolver(ctx.clone(), resolver),
        ));
        tools.register_builtin(Arc::new(tool_skill::skill::SkillTool::with_loader(
            ctx.clone(),
            loader,
        )));
        tools.set_session_tool_allowlist(
            &[
                "Read",
                "Edit",
                "Write",
                "Glob",
                "Grep",
                "NotebookEdit",
                "WebSearch",
                "WebFetch",
                "AskUserQuestion",
                "Skill",
                "Shell",
            ]
            .map(str::to_owned),
        );
        let hooks = Arc::new(
            hooks::HookExecutorImpl::new(
                Arc::new(RwLock::new(hooks::HookRegistry::new())),
                ctx.http.clone(),
                Arc::new(platform_posix_minimal::runtime::PosixRuntime::new()),
            )
            .with_policy_disable_all_hooks(true),
        );
        let output = Arc::new(AdapterOutputStream::new(sink));
        let (compactor, cache_slot) =
            crate::side_chat::composition::compaction(api, &snapshot.config.model);
        let skill_listing = crate::side_chat::composition::skill_listing(
            commands.clone(),
            ctx.read_file_state.clone(),
            snapshot.cwd.clone(),
        );
        let orch = ConversationOrchestrator::into_shared(
            ConversationOrchestrator::new_side_chat(
                snapshot.config.clone(),
                adapter.clone(),
                adapter,
                Arc::new(tools),
                hooks,
                perms,
                output,
                Arc::new(
                    orchestrator::test_support::StaticMemoryProvider::with_files(
                        snapshot.memory_files.clone(),
                    ),
                ),
                snapshot.cwd.clone(),
            )
            .with_session_id(session_id)
            .with_session_cwd(ctx.session_cwd.clone())
            .with_read_state_map(ctx.read_file_state.clone())
            .with_compaction(compactor)
            .with_cache_safe_slot(cache_slot)
            .with_skill_listing(skill_listing)
            .with_side_chat_model_refs(ready_models),
        );
        orch.seed_side_chat(&snapshot).await?;
        {
            let mut registry = commands.write().await;
            registry.register_builtin_handler(Arc::new(command_api::builtins::HelpHandler::new()));
            registry.register_builtin_handler(Arc::new(
                command_api::builtins::CompactHandler::new(
                    orch.clone() as Arc<dyn OrchestratorHandle>
                ),
            ));
            registry.register_builtin_handler(Arc::new(command_api::builtins::ModelHandler::new(
                orch.clone() as Arc<dyn OrchestratorHandle>,
            )));
        }
        let dispatcher = Arc::new(
            RegistrySlashDispatcher::new(commands)
                .with_shell_expansion(tool_skill::build_prompt_shell_provider(&ctx)),
        );
        Ok(SideChatSession {
            orchestrator: orch,
            dispatcher,
            permission_gate,
            question_broker,
            cleanup_failed,
        })
    }
}

impl MobileEngineHandle {
    /// Platform packagers bridge their existing typed native callbacks here.
    pub async fn create_side_chat(
        &self,
        request_id: String,
        initial_input: Option<client::protocol::text::ClientTextDto>,
        listener: Arc<dyn ClientEventListener>,
        permission_sink: Arc<dyn PermissionRequestSink>,
    ) -> Result<SideChatInfoDto, MobileEngineError> {
        let manager = self.side_chats.clone();
        let side_id = self
            .runtime()
            .spawn(async move {
                manager
                    .create(
                        request_id,
                        initial_input,
                        Some(ListenerSink::arc(listener)),
                        Some(permission_sink),
                    )
                    .await
            })
            .await
            .map_err(|error| MobileEngineError::Internal(error.to_string()))?
            .map_err(MobileEngineError::Internal)?;
        self.side_chats
            .info(&side_id)
            .await
            .map_err(MobileEngineError::Internal)
    }
}

#[cfg_attr(feature = "uniffi", uniffi::export(async_runtime = "tokio"))]
impl MobileEngineHandle {
    /// Dispatch existing commands into the side's own turn and permission state.
    pub async fn submit_side_chat(
        &self,
        side_id: String,
        command: ClientCommand,
    ) -> Result<(), ClientError> {
        let manager = self.side_chats.clone();
        self.runtime()
            .spawn(async move {
                manager
                    .submit(&side_id, uuid::Uuid::new_v4().to_string(), command)
                    .await
            })
            .await
            .map_err(|error| ClientError::Rejected {
                message: error.to_string(),
            })?
            .map_err(|message| ClientError::Rejected { message })
    }

    /// Live RAM conversations only; there is no disk restore path.
    pub async fn list_side_chats(&self) -> Vec<SideChatInfoDto> {
        let mut sides = Vec::new();
        for id in self.side_chats.list().await {
            if let Ok(info) = self.side_chats.info(&id).await {
                sides.push(info);
            }
        }
        sides
    }

    /// Read the current lifecycle state without renewing the idle deadline.
    pub async fn get_side_chat(&self, side_id: String) -> Result<SideChatInfoDto, ClientError> {
        self.side_chats
            .info(&side_id)
            .await
            .map_err(|message| ClientError::Rejected { message })
    }

    /// End closes admission and waits for foreground work before releasing RAM.
    pub async fn end_side_chat(&self, side_id: String) -> Result<(), ClientError> {
        let manager = self.side_chats.clone();
        self.runtime()
            .spawn(async move { manager.end(&side_id).await })
            .await
            .map_err(|error| ClientError::Rejected {
                message: error.to_string(),
            })?
            .map_err(|message| ClientError::Rejected { message })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mobile::test_support::{
        CollectingPermissionSink, FakeListener, HostFakePlatform, test_config,
    };

    #[test]
    fn mobile_side_chat_uses_ram_only_and_end_preserves_user_files() {
        let root = tempfile::tempdir().unwrap();
        let mut cfg = test_config(root.path());
        cfg.api_key = "side-test-key".into();
        let home = cfg.lingxi_home.clone();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_stack_size(crate::RUNTIME_THREAD_STACK_SIZE)
            .build()
            .unwrap();
        rt.block_on(async {
            let parent_listener = Arc::new(FakeListener::default());
            let parent = super::super::build_mobile(
                cfg,
                Arc::new(HostFakePlatform::new(root.path().into())),
                parent_listener.clone(),
                Arc::new(CollectingPermissionSink::default()),
            )
            .await
            .unwrap();
            assert!(parent.orchestrator.has_jsonl_writer());
            let snapshot = parent.orchestrator.side_chat_parent_snapshot().await;
            let session = parent
                .side_chat_factory
                .build(
                    snapshot,
                    ListenerSink::arc(Arc::new(FakeListener::default())),
                    Arc::new(CollectingPermissionSink::default()),
                )
                .await
                .unwrap();
            assert!(!session.orchestrator.has_jsonl_writer());
            assert!(session.orchestrator.has_compaction());
            let names = session.orchestrator.advertised_tool_names().await;
            for allowed in [
                "Read",
                "Write",
                "Edit",
                "Glob",
                "Grep",
                "Skill",
                "AskUserQuestion",
            ] {
                assert!(
                    names.iter().any(|name| name == allowed),
                    "missing {allowed}: {names:?}"
                );
            }
            for denied in [
                "Agent",
                "Workflow",
                "EnterWorktree",
                "TaskCreate",
                "ScheduleWakeup",
                "camera",
                "voice",
            ] {
                assert!(!names.iter().any(|name| name == denied), "exposed {denied}");
            }
            let engine = Arc::downgrade(&session.orchestrator);
            drop(session);
            assert!(
                engine.upgrade().is_none(),
                "side handler cycle retained the engine"
            );
            let child_listener = Arc::new(FakeListener::default());
            let manager = crate::side_chat::SideChatManager::new(
                parent.orchestrator.clone(),
                parent.side_chat_factory.clone(),
                ListenerSink::arc(parent_listener),
            );
            let id = manager
                .create(
                    "native-create".into(),
                    None,
                    Some(ListenerSink::arc(child_listener.clone())),
                    Some(Arc::new(CollectingPermissionSink::default())),
                )
                .await
                .unwrap();
            assert!(manager.snapshot(&id).await.unwrap().is_empty());
            manager
                .submit(
                    &id,
                    "catalog".into(),
                    ClientCommand::RefreshListings {
                        which: vec![ProtocolListingKind::SlashCommands],
                    },
                )
                .await
                .unwrap();
            let replies = child_listener.received.lock().await;
            assert!(
                replies
                    .iter()
                    .any(|event| matches!(event, ClientEvent::SlashCommandCatalog { .. }))
            );
            drop(replies);
            // Ending a memory conversation never performs file rollback.
            let edited = root.path().join("edited.txt");
            std::fs::write(&edited, "user modification").unwrap();
            manager.end(&id).await.unwrap();
            assert_eq!(
                std::fs::read_to_string(edited).unwrap(),
                "user modification"
            );
            assert!(manager.info(&id).await.is_err());
            assert!(!home.join("mobile-turns").exists());
            assert!(child_listener.received.lock().await.iter().any(
                |event| matches!(event, ClientEvent::SideChatEnded { side_id } if side_id == &id)
            ));
            manager.shutdown().await.unwrap();
        });
    }
}
