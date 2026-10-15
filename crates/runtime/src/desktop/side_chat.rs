//! Lightweight Desktop composition from already-resolved host inputs.
use crate::side_chat::{SideChatFactory, SideChatSession};
use async_trait::async_trait;
use client::adapter::{
    AdapterOutputStream, AdapterPermissionGate, AskUserQuestionBroker, ClientEventSink,
    PermissionRequestSink,
};
use command_api::{CommandRegistry, RegistrySlashDispatcher};
use lingxi_core::host::{OrchestratorHandle, PermissionGate};
use orchestrator::{ConversationOrchestrator, ProviderApiAdapter, SideChatParentSnapshot};
use std::sync::Arc;
use tokio::sync::RwLock;
use tool_api::{BuiltinToolContext, ToolRegistry};

pub(super) struct DesktopSideChatFactory {
    pub api: Arc<llm_runtime::ApiService>,
    pub context: BuiltinToolContext,
    pub commands: Arc<RwLock<CommandRegistry>>,
    pub availability: std::collections::BTreeMap<String, bool>,
}
#[async_trait]
impl SideChatFactory for DesktopSideChatFactory {
    async fn build(
        &self,
        snapshot: SideChatParentSnapshot,
        sink: Arc<dyn ClientEventSink>,
        permission_sink: Arc<dyn PermissionRequestSink>,
    ) -> Result<SideChatSession, String> {
        let id = lingxi_core::types::SessionId::new();
        let ready_models = crate::side_chat::composition::ready_model_refs(
            &self.api,
            &self.availability,
            &snapshot,
        );
        let api = Arc::new(self.api.fork_independent());
        let request_session_id = id.to_string();
        api.set_request_session_id_source(Arc::new(move || {
            let id = request_session_id.clone();
            Box::pin(async move { Some(id) })
        }));
        let adapter = Arc::new(ProviderApiAdapter::new(api.clone()));
        let permission_gate =
            Arc::new(AdapterPermissionGate::new(permission_sink).with_event_sink(sink.clone()));
        permission_gate.set_session_id(Some(id.to_string()));
        let mut ctx = self.context.clone();
        ctx.side_chat = true;
        ctx.bus = Arc::new(telemetry::AnalyticsBus::disabled());
        let cleanup_failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        ctx.side_chat_cleanup_failed = Some(cleanup_failed.clone());
        ctx.session_id = Some(id);
        ctx.default_model = snapshot.config.model.clone();
        ctx.session_cwd =
            tool_api::SessionCwd::new(snapshot.cwd.clone(), self.context.trusted_dirs());
        ctx.read_file_state = tool_api::read_file_state::new_read_file_state_map();
        ctx.worktree_session = Arc::new(std::sync::Mutex::new(None));
        ctx.fs = Arc::new(super::platform::DesktopFileSystem::new(
            snapshot.cwd.clone(),
        ));
        ctx.process = Arc::new(super::platform::DesktopProcess::new());
        ctx.permission_mode =
            permission::permission_mode_from_cli_string(&snapshot.permission_mode);
        ctx.permission_policy = Arc::new(
            self.context
                .permission_policy
                .fork_independent(ctx.permission_mode),
        );
        let perms: Arc<dyn PermissionGate> = Arc::new(permission::PolicyPermissionGate::new(
            ctx.permission_policy.clone(),
            permission_gate.clone(),
        ));
        ctx.permission_gate = Some(perms.clone());
        ctx.sandbox_runner = super::new_live_sandbox_runner_with_permission_gate(perms.clone());
        ctx.sandbox_enabled_override =
            self.context.sandbox_enabled_override.as_ref().map(|value| {
                Arc::new(std::sync::atomic::AtomicBool::new(
                    value.load(std::sync::atomic::Ordering::Acquire),
                ))
            });
        ctx.subagent_spawner = None;
        ctx.task_registry = None;
        ctx.mailbox_router = None;
        ctx.budget_enforcer = None;
        ctx.coordinator_mode = None;
        ctx.bash_edit_diff = None;
        ctx.mcp_registry = None;
        ctx.lsp_registry = None;
        ctx.task_lifecycle_hooks = None;
        ctx.camera = None;
        ctx.audio = None;
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
        ctx.hosted_search = Some(adapter.clone());
        ctx.mcp_token_counter = None;
        let mut commands = CommandRegistry::new();
        commands.set_side_chat(true);
        // Copy descriptors only: host-bound handlers, Mods and loaders are not inherited.
        for command in self.commands.read().await.list_all() {
            if commands.execution_allowed(command) {
                commands.register_command(command.clone());
            }
        }
        let commands = Arc::new(RwLock::new(commands));
        let skill_loader = Arc::new(
            super::skill_loader::CommandRegistrySkillLoader::with_session_id(
                commands.clone(),
                id.to_string(),
            ),
        );
        let question_broker = Arc::new(AskUserQuestionBroker::new(sink.clone()));
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
        let live_cwd = Arc::new(std::sync::Mutex::new(snapshot.cwd.clone()));
        tool_file::register_all_with_live_cwd(&mut tools, ctx.clone(), Some(live_cwd.clone()));
        tool_shell::register_all_with_cwd_firer(
            &mut tools,
            ctx.clone(),
            None,
            Some(live_cwd.clone()),
        );
        tool_web::register_all(&mut tools, ctx.clone(), None);
        tools.register_builtin(Arc::new(
            tool_ui::ask_user_question::AskUserQuestionTool::with_resolver(ctx.clone(), resolver),
        ));
        tools.register_builtin(Arc::new(tool_skill::skill::SkillTool::with_loader(
            ctx.clone(),
            skill_loader,
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
                "Bash",
            ]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>(),
        );
        let hooks = Arc::new(
            hooks::HookExecutorImpl::new(
                Arc::new(RwLock::new(hooks::HookRegistry::new())),
                ctx.http.clone(),
                Arc::new(platform_posix::PosixRuntime::new()),
            )
            .with_policy_disable_all_hooks(true),
        );
        let output = Arc::new(AdapterOutputStream::new(sink));
        let (compactor, cache_safe_slot) =
            crate::side_chat::composition::compaction(api, &snapshot.config.model)
                .map_err(|error| error.to_string())?;
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
            .with_side_chat_model_refs(ready_models)
            .with_session_id(id)
            .with_session_cwd(ctx.session_cwd.clone())
            .with_current_cwd(live_cwd)
            .with_read_state_map(ctx.read_file_state.clone())
            .with_compaction(compactor)
            .with_cache_safe_slot(cache_safe_slot)
            .with_skill_listing(skill_listing),
        );
        orch.seed_side_chat(&snapshot).await?;
        {
            let mut commands = commands.write().await;
            commands.register_builtin_handler(Arc::new(command_api::builtins::HelpHandler::new()));
            commands.register_builtin_handler(Arc::new(
                command_api::builtins::CompactHandler::new(
                    orch.clone() as Arc<dyn OrchestratorHandle>
                ),
            ));
            commands.register_builtin_handler(Arc::new(command_api::builtins::ModelHandler::new(
                orch.clone() as Arc<dyn OrchestratorHandle>,
            )));
        }
        let dispatcher = Arc::new(
            RegistrySlashDispatcher::new(commands)
                .with_shell_expansion(tool_skill::prompt_shell::build_prompt_shell_provider(&ctx)),
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
