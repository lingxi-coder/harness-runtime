//! Fresh app tasks over ready native host services, without parent history,
//! transcript, memory, skills, personal hooks or an application approval API.
mod lifecycle;
mod schema;
mod service;
use lingxi_core::{
    host::permission_gate::PermissionGate,
    host::{OrchestratorHandle, OutputStream},
    types::{utf16_json::Utf16JsonProjection, SessionId},
};
use local_app::v2::agent::{Admission, Input, Owner, ToolGrant};
use orchestrator::{
    structured_output::{StructuredOutputSlot, StructuredOutputTool, STRUCTURED_OUTPUT_TOOL_NAME},
    ConversationOrchestrator, OrchestratorConfig, ProviderApiAdapter,
};
pub use service::AppAgentHost;
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tool_api::{BuiltinToolContext, SessionToolGrantLease, SessionToolPolicy, ToolRegistry};

/// Ready services captured by the trusted host assembly, never a chat snapshot.
pub struct AppAgentFactory {
    pub(crate) config_home: PathBuf,
    pub(crate) api: Arc<llm_runtime::ApiService>,
    pub(crate) context: BuiltinToolContext,
    pub(crate) model_resolution: Arc<dyn agent::model_resolution::ModelResolutionContextProvider>,
    pub(crate) host_hooks: Arc<tokio::sync::RwLock<hooks::HookRegistry>>,
}
pub struct AppAgentSession {
    pub orchestrator: Arc<ConversationOrchestrator>,
    pub lease: SessionToolGrantLease,
    pub structured_output: Option<StructuredOutputSlot>,
    pub cleanup_failed: Arc<std::sync::atomic::AtomicBool>,
}
const BUILTINS: &[&str] = &[
    "Read",
    "Write",
    "Edit",
    "Glob",
    "Grep",
    "NotebookEdit",
    "Shell",
    "WebFetch",
    "WebSearch",
    "AskUserQuestion",
    "ToolSearch",
];
impl AppAgentFactory {
    fn configured_servers(&self) -> Result<Vec<mcp::McpServerConfig>, String> {
        Self::configured_servers_at(&self.config_home)
    }
    fn configured_servers_at(config_home: &PathBuf) -> Result<Vec<mcp::McpServerConfig>, String> {
        let private_cwd = config_home.join("local-apps/v2/agent/host");
        let mut servers = match std::fs::read_to_string(config_home.join("mcp-config.json")) {
            Ok(raw) if raw.len() <= 1024 * 1024 => mcp::parse_global_config_mcp_servers(
                &raw,
                mcp::ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
            )
            .map_err(|_| "invalid_mcp_configuration")?,
            Ok(_) => return Err("mcp_configuration_too_large".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(_) => return Err("mcp_configuration_unavailable".into()),
        };
        let policy = mcp::enterprise_policy::read_effective_mcp_policy_with_scope(
            config_home,
            &private_cwd,
            None,
            (true, false),
        );
        mcp::enterprise_policy::apply_enterprise_mcp_policy_with(&mut servers, &policy);
        servers.retain(|server| !server.disabled);
        Ok(servers)
    }
    /// Call after durable admission and current-run validation. Only native host
    /// UI can answer the separate permission and questionnaire channels.
    #[allow(clippy::too_many_arguments)]
    pub async fn build(
        &self,
        owner: &Owner,
        admission: &Admission,
        input: &Input,
        cwd: PathBuf,
        output: Arc<dyn OutputStream>,
        permissions: Arc<dyn PermissionGate>,
        owner_cancel: tokio_util::sync::CancellationToken,
        questions: tokio::sync::mpsc::Sender<tool_api::ask_user_question::AskUserQuestionExchange>,
    ) -> Result<AppAgentSession, String> {
        owner.validate().map_err(|_| "invalid_owner")?;
        if !admission.start || !cwd.is_absolute() {
            return Err("invalid_admission".into());
        }
        let model = &admission.task.model;
        let listing = self
            .api
            .model_listings()
            .into_iter()
            .find(|m| {
                !m.hidden
                    && m.profile_name == model.provider_id
                    && m.request_model == model.model_id
            })
            .ok_or("model_unavailable")?;
        if !input.images.is_empty() && !listing.capabilities.vision {
            return Err("model_vision_unsupported".into());
        }
        if (input.output_schema.is_some() || !admission.tools.is_empty())
            && !listing.capabilities.tools
        {
            return Err("model_tools_unsupported".into());
        }
        if !self
            .api
            .credential_source(&model.model_id, Some(&model.provider_id))
            .await
            .is_ok_and(|source| {
                !matches!(
                    source,
                    llm_runtime::CredentialSource::None | llm_runtime::CredentialSource::Unknown
                )
            })
        {
            return Err("model_credentials_unavailable".into());
        }
        let compiled = input
            .output_schema
            .as_ref()
            .map(schema::NativeSchema::compile)
            .transpose()?;
        let mut policy = SessionToolPolicy::default();
        let mut mcp = HashSet::new();
        for grant in admission.tools.values() {
            match grant {
                ToolGrant::Builtin { name } => {
                    if !BUILTINS.contains(&name.as_str()) {
                        return Err("unsupported_app_tool".into());
                    }
                    policy.builtin_tools.insert(name.clone());
                }
                ToolGrant::Mcp {
                    server_config_id,
                    config_revision,
                    raw_tool_name,
                    definition_digest,
                } => {
                    mcp.insert(tool_api::McpToolIdentity {
                        server_config_id: server_config_id.clone(),
                        config_revision: config_revision.clone(),
                        raw_tool_name: raw_tool_name.clone(),
                        definition_fingerprint: definition_digest.clone(),
                    });
                }
            }
        }
        if !mcp.is_empty() {
            let registry = self
                .context
                .mcp_registry
                .as_ref()
                .ok_or("mcp_unavailable")?;
            let catalog = registry.tool_identity_catalog().await;
            let configured = self.configured_servers()?;
            if mcp.iter().any(|identity| {
                !catalog.contains(identity)
                    || !configured
                        .iter()
                        .any(|server| identity.matches_config(server))
            }) {
                return Err("mcp_definition_changed".into());
            }
        }
        policy.mcp_tools = mcp;
        if compiled.is_some() {
            policy
                .builtin_tools
                .insert(STRUCTURED_OUTPUT_TOOL_NAME.into());
        }
        let config_home = self.config_home.clone();
        let lease = SessionToolGrantLease::new(policy)
            .with_owner_cancellation(owner_cancel)
            .with_live_mcp_guard(Arc::new(move |identity| {
                Self::configured_servers_at(&config_home).is_ok_and(|servers| {
                    servers.iter().any(|server| identity.matches_config(server))
                })
            }));
        let session_id = SessionId::new();
        let mut context = self.context.clone();
        let cleanup_failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        context.side_chat_cleanup_failed = Some(cleanup_failed.clone());
        context.session_id = Some(session_id);
        context.side_chat = true;
        context.session_cwd = tool_api::SessionCwd::new(cwd.clone(), vec![cwd.clone()]);
        context.read_file_state = tool_api::read_file_state::new_read_file_state_map();
        context.worktree_session = tool_api::worktree_session::new_worktree_session_cell();
        context.default_model = model.model_id.clone();
        context.bus = Arc::new(telemetry::AnalyticsBus::disabled());
        context.permission_mode = permission::PermissionMode::Default;
        context.permission_policy = Arc::new(self.context.permission_policy.fork_app_invocation(
            permission::FsRoots {
                cwd: cwd.clone(),
                home: None,
                lingxi_home: cwd.join(".lingxi"),
            },
        ));
        let gate: Arc<dyn PermissionGate> = Arc::new(
            permission::PolicyPermissionGate::new(context.permission_policy.clone(), permissions)
                .with_path_translator(Arc::new(permission::FileSystemPathTranslator(
                    context.fs.clone(),
                ))),
        );
        context.permission_gate = Some(gate.clone());
        context.subagent_spawner = None;
        context.agent_name_registry = None;
        context.task_registry = None;
        context.mailbox_router = None;
        context.budget_enforcer = None;
        context.coordinator_mode = None;
        context.main_loop_model_profile_provider = None;
        context.bash_edit_diff = None;
        context.task_lifecycle_hooks = None;
        context.lsp_registry = None;
        context.camera = None;
        context.audio = None;
        context.share = None;
        context.notifications = None;
        context.clipboard = None;
        context.location = None;
        context.device_status = None;
        context.haptics = None;
        context.deep_link = None;
        context.calendar = None;
        context.contacts = None;
        context.computer_control = None;
        context.mobile_git = None;
        context.mobile_git_secret = None;
        let api = Arc::new(self.api.fork_app_invocation());
        let adapter = Arc::new(ProviderApiAdapter::new(api));
        context.hosted_search = Some(adapter.clone());
        context.mcp_token_counter = Some(adapter.clone());
        let mut tools = ToolRegistry::new();
        tool_file::register_all(&mut tools, context.clone());
        tool_shell_mobile::register_all(&mut tools, context.clone());
        tool_web::register_all(&mut tools, context.clone(), None);
        tool_meta::register_all(&mut tools, context.clone());
        let resolver = Arc::new(tool_ui::ask_user_question::TuiBridgeResolver::new(
            tool_ui::ask_user_question::AskUserQuestionTimeout::parse_or_default(None),
            questions,
        ));
        tools.register_builtin(Arc::new(
            tool_ui::ask_user_question::AskUserQuestionTool::with_resolver(
                context.clone(),
                resolver,
            ),
        ));
        if let Some(registry) = &context.mcp_registry {
            for (connection, entries) in
                tool_mcp::build_registered_mcp_tools(registry, context.clone()).await
            {
                tools.register_mcp_tools(connection, entries);
            }
        }
        let structured_output =
            if let (Some(compiled), Some(source)) = (compiled, &input.output_schema) {
                let slot = Arc::new(Mutex::new(None));
                let source = Utf16JsonProjection::parse(&source.to_string())
                    .map_err(|_| "invalid_output_schema")?;
                tools.register_required_builtin(Arc::new(StructuredOutputTool::new(
                    source,
                    compiled,
                    slot.clone(),
                )));
                Some(slot)
            } else {
                None
            };
        tools.set_session_tool_policy(lease.clone());
        tools.refresh_tool_search_view();
        let mut managed_hooks = hooks::HookRegistry::new();
        for definition in self.host_hooks.read().await.all_hooks() {
            if definition.source
                == hooks::definition::HookSource::Settings(
                    lingxi_core::types::SettingsScope::Managed,
                )
            {
                managed_hooks.register(definition.clone());
            }
        }
        let hooks = Arc::new(
            hooks::HookExecutorImpl::new(
                Arc::new(tokio::sync::RwLock::new(managed_hooks)),
                context.http.clone(),
                Arc::new(platform_posix_minimal::runtime::PosixRuntime::new()),
            )
            .with_policy_disable_all_hooks(false),
        );
        let config = OrchestratorConfig {
            model: model.model_id.clone(),
            max_turns: admission.task.limits.max_turns,
            structured_output_enabled: structured_output.is_some(),
            ..OrchestratorConfig::default()
        };
        let orch = ConversationOrchestrator::into_shared(
            ConversationOrchestrator::new_side_chat(
                config,
                adapter.clone(),
                adapter,
                Arc::new(tools),
                hooks,
                gate,
                output,
                Arc::new(orchestrator::test_support::StaticMemoryProvider::empty()),
                cwd,
            )
            .with_session_id(session_id)
            .with_session_cwd(context.session_cwd.clone())
            .with_read_state_map(context.read_file_state.clone())
            .with_model_resolution_context_provider(self.model_resolution.clone()),
        );
        orch.switch_model(&model.model_id, Some(&model.provider_id))
            .await
            .map_err(|_| "model_unavailable")?;
        Ok(AppAgentSession {
            orchestrator: orch,
            lease,
            structured_output,
            cleanup_failed,
        })
    }
}
