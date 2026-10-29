//! Fresh app tasks over ready native host services, without parent history,
//! transcript, memory, skills, personal hooks or an application approval API.
mod lifecycle;
mod schema;
mod service;
mod stateless;
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
use serde_json::{json, Value};
pub use service::AppAgentHost;
use std::{
    collections::{BTreeMap, HashSet},
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
    pub(crate) pricing: Arc<cost::PricingCatalog>,
}
pub struct AppAgentSession {
    pub orchestrator: Arc<ConversationOrchestrator>,
    pub lease: SessionToolGrantLease,
    pub structured_output: Option<StructuredOutputSlot>,
    pub cleanup_failed: Arc<std::sync::atomic::AtomicBool>,
}
/// Private Harness identity and replay state for one persistent App Chat turn.
pub struct AppAgentChatResume {
    pub session_id: SessionId,
    pub transcript_path: PathBuf,
    pub replayed: Option<orchestrator::resume::ReplayedSession>,
    pub writer: Arc<session::jsonl::JsonlWriter>,
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

fn clear_app_media_capabilities(context: &mut BuiltinToolContext) {
    context.media = None;
    context.media_agent_handoff = None;
}

/// Generic Harness resume replays unresolved hook-deferred tools. Local App
/// recovery must leave those operations for an explicit user decision instead.
pub(super) fn validate_local_app_chat_resume(
    replayed: &orchestrator::resume::ReplayedSession,
) -> Result<(), String> {
    if replayed.runtime_metadata.deferred_tools.is_empty() {
        Ok(())
    } else {
        Err("chat_deferred_tools_pending".into())
    }
}

impl AppAgentFactory {
    pub(crate) async fn catalog(&self) -> Result<Value, String> {
        use sha2::Digest;
        let mut models = Vec::new();
        for m in self.api.model_listings().into_iter().filter(|m| !m.hidden) {
            let route = self
                .api
                .resolve_media_route(&m.request_model, Some(&m.profile_name));
            let capabilities = route
                .as_ref()
                .map(|route| stateless::app_agent_capabilities(&self.api, route))
                .unwrap_or_default();
            let ready = route.is_ok()
                && self
                    .api
                    .credential_source(&m.request_model, Some(&m.profile_name))
                    .await
                    .is_ok_and(|s| {
                        !matches!(
                            s,
                            llm_runtime::CredentialSource::None
                                | llm_runtime::CredentialSource::Unknown
                        )
                    });
            models.push(json!({
                "providerId":m.profile_name,
                "modelId":m.request_model,
                "label":m.display_model,
                "canSelect":ready,
                "supportsImages":capabilities.supports_images,
                "supportsTools":capabilities.supports_tools,
                "supportsStructuredOutput":stateless::supports_structured_output(capabilities)
            }));
        }
        let mut tools: BTreeMap<String, ToolGrant> = BUILTINS
            .iter()
            .map(|name| {
                (
                    format!("builtin:{name}"),
                    ToolGrant::Builtin {
                        name: (*name).into(),
                    },
                )
            })
            .collect();
        if self.context.mobile_shell.is_none() {
            tools.remove("builtin:Shell");
        }
        let mut labels: BTreeMap<String, String> = tools
            .keys()
            .map(|id| (id.clone(), id.trim_start_matches("builtin:").to_string()))
            .collect();
        let configured = self.configured_servers()?;
        if let Some(registry) = &self.context.mcp_registry {
            for (identity, label) in registry.tool_identity_labels().await {
                if !configured
                    .iter()
                    .any(|server| identity.matches_config(server))
                {
                    continue;
                }
                let id = format!(
                    "mcp:{:x}",
                    sha2::Sha256::digest(
                        serde_json::to_vec(&identity).map_err(|_| "invalid_tool")?
                    )
                );
                labels.insert(id.clone(), label);
                tools.insert(
                    id,
                    ToolGrant::Mcp {
                        server_config_id: identity.server_config_id,
                        config_revision: identity.config_revision,
                        raw_tool_name: identity.raw_tool_name,
                        definition_digest: identity.definition_fingerprint,
                    },
                );
            }
        }
        let servers=self.configured_servers()?.into_iter().map(|server| {
            let available=matches!(server.spec,lingxi_core::host::McpTransportSpec::Http{..}|lingxi_core::host::McpTransportSpec::Sse{..});
            json!({"id":mcp::tool_identity::McpToolIdentity::server_catalog_id(&server),"name":server.name,"available":available})
        }).collect::<Vec<_>>();
        Ok(json!({"models":models,"tools":tools,"labels":labels,"servers":servers}))
    }
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
        chat: Option<AppAgentChatResume>,
        skill_prompt: Option<String>,
    ) -> Result<AppAgentSession, String> {
        owner.validate().map_err(|_| "invalid_owner")?;
        if !admission.start || !cwd.is_absolute() {
            return Err("invalid_admission".into());
        }
        // Check before constructing/switching an orchestrator: resume_session
        // may otherwise dispatch stored deferred tools as a side effect.
        if let Some(replayed) = chat.as_ref().and_then(|chat| chat.replayed.as_ref()) {
            validate_local_app_chat_resume(replayed)?;
        }
        let model = &admission.task.model;
        let route =
            stateless::resolve_selected_route(&self.api, &model.model_id, &model.provider_id)?;
        let capabilities = stateless::app_agent_capabilities(&self.api, &route);
        if input.attachments.iter().any(|file| {
            file.media_type.starts_with("image/") || file.media_type == "application/pdf"
        }) && !capabilities.supports_images
        {
            return Err("model_vision_unsupported".into());
        }
        // Persistent Harness chats and tool-using tasks enforce schemas with
        // the internal StructuredOutput tool, so this execution path requires
        // function-tool capability even when the route supports native JSON.
        if input.output_schema.is_some()
            && !stateless::harness_structured_output_available(capabilities)
        {
            return Err("model_tools_unsupported".into());
        }
        if !admission.tools.is_empty() && !capabilities.supports_tools {
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
                    if name == "Shell" && self.context.mobile_shell.is_none() {
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
        let task_cancel = owner_cancel.clone();
        let lease = SessionToolGrantLease::new(policy)
            .with_owner_cancellation(owner_cancel)
            .with_live_mcp_guard(Arc::new(move |identity| {
                let valid = Self::configured_servers_at(&config_home).is_ok_and(|servers| {
                    servers.iter().any(|server| identity.matches_config(server))
                });
                // Wake pending native approvals as well as rejecting the RPC.
                // This is the task child token, never the app invocation root.
                if !valid {
                    task_cancel.cancel();
                }
                valid
            }));
        let session_id = chat
            .as_ref()
            .map(|chat| chat.session_id)
            .unwrap_or_else(SessionId::new);
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
        // Clear these before cloning the context into either builtins or MCP
        // tools, even if this factory came from an ordinary media-enabled host.
        clear_app_media_capabilities(&mut context);
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
        let adapter = Arc::new(ProviderApiAdapter::new(api.clone()));
        context.hosted_search = Some(adapter.clone());
        context.mcp_token_counter = Some(adapter.clone());
        let mut tools = ToolRegistry::new();
        tool_file::register_all(&mut tools, context.clone());
        tool_shell_mobile::register_all(&mut tools, context.clone());
        tool_web::register_all(&mut tools, context.clone(), None);
        // Native deferred definitions and tool_reference results belong to the
        // Anthropic protocol family. In other codecs expose only the lease's
        // authorized tools inline; never advertise an unsupported discovery hop.
        let protocol = self
            .api
            .protocol_for_model(&model.model_id, Some(&model.provider_id))
            .ok();
        if app_supports_deferred_tool_search(protocol) {
            tool_meta::register_all(&mut tools, context.clone());
        } else {
            let deferral = Arc::new(tool_api::DeferralState::disabled());
            tools.set_deferral(deferral.clone());
            tools.register_builtin(Arc::new(tool_meta::ConfigTool::new(context.clone())));
            tools.register_builtin(Arc::new(tool_meta::ToolSearchTool::with_view_and_deferral(
                context.clone(),
                tools.tool_search_view(),
                deferral,
            )));
        }
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
        let mut config = OrchestratorConfig {
            model: model.model_id.clone(),
            max_turns: admission.task.limits.max_turns,
            structured_output_enabled: structured_output.is_some(),
            // The native host owns live permission/question channels. The
            // headless defaults reject them before reaching those channels.
            interactive_session: true,
            interactive_permissions: true,
            ..OrchestratorConfig::default()
        };
        config.append_system_prompt = skill_prompt.map(Utf16JsonProjection::from);
        // A task owns its accounting session. Reading snapshot_cost without a
        // tracker returns the orchestrator's zero-shaped placeholder, even
        // after real provider requests have completed.
        let (cost_tx, mut cost_rx) = tokio::sync::mpsc::channel(8);
        tokio::spawn(async move { while cost_rx.recv().await.is_some() {} });
        let cost_tracker = Arc::new(cost::CostTracker::new(
            session_id,
            self.pricing.clone(),
            cost_tx,
        ));
        let mut builder = ConversationOrchestrator::new_side_chat(
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
        // A fresh side chat starts without a profile. Admit only this
        // host-authorized qualified route before selecting its profile;
        // otherwise the side-chat guard rejects every profiled model.
        .with_side_chat_model_refs(vec![lingxi_core::host::qualified_model_ref(
            &model.model_id,
            Some(&model.provider_id),
        )])
        .with_session_id(session_id)
        .with_cost_tracker(cost_tracker)
        .with_session_cwd(context.session_cwd.clone())
        .with_read_state_map(context.read_file_state.clone())
        .with_model_resolution_context_provider(self.model_resolution.clone());
        if let Some(chat) = &chat {
            // Persistent Chats use an exclusively owned, non-rewriting
            // writer. Compaction cannot preserve the sealed transcript proof.
            builder = builder.with_jsonl_writer(chat.writer.clone());
        }
        let orch = ConversationOrchestrator::into_shared(builder);
        orch.switch_model(&model.model_id, Some(&model.provider_id))
            .await
            .map_err(|_| "model_unavailable")?;
        if let Some(replayed) = chat.and_then(|chat| chat.replayed) {
            let mut runtime = replayed.handle_runtime_snapshot();
            runtime.model = model.model_id.clone();
            runtime.model_profile = Some(model.provider_id.clone());
            let active_goal = replayed.state.active_goal.clone().map(|goal| {
                lingxi_core::host::ActiveGoalSnapshot {
                    condition: goal.condition,
                    set_at: goal.set_at,
                    last_reason: goal.last_reason,
                    iterations: goal.iterations,
                    tokens_at_start: goal.tokens_at_start,
                }
            });
            orch.resume_session(
                session_id,
                replayed.state.history.clone(),
                replayed.last_message_uuid.map(|uuid| uuid.to_string()),
                active_goal,
                runtime,
            )
            .await
            .map_err(|_| "chat_resume_failed")?;
        }
        Ok(AppAgentSession {
            orchestrator: orch,
            lease,
            structured_output,
            cleanup_failed,
        })
    }
}

fn app_supports_deferred_tool_search(protocol: Option<llm_runtime::ProtocolFamily>) -> bool {
    use llm_runtime::ProtocolFamily::*;
    matches!(
        protocol,
        Some(AnthropicMessages | VertexClaude | BedrockClaude | FoundryClaude)
    )
}

#[cfg(test)]
mod discovery_tests {
    use super::*;

    struct TestMediaService;

    #[async_trait::async_trait]
    impl media_api::MediaService for TestMediaService {
        fn capabilities(&self) -> media_api::MediaCapabilities {
            media_api::MediaCapabilities {
                service_epoch: 1,
                support_revision: 1,
                supported_operations: vec![media_api::MediaOperationKind::Capture],
                supported_media_kinds: vec![media_api::MediaKind::Image],
                readiness: Vec::new(),
                max_payload_bytes: 1024,
            }
        }

        async fn execute(
            &self,
            _: media_api::MediaOperationContext,
            _: media_api::MediaOperation,
        ) -> Result<media_api::MediaResult, media_api::MediaError> {
            Err(media_api::MediaError::new(
                media_api::MediaErrorKind::Unavailable,
                "test",
            ))
        }

        async fn read_media(
            &self,
            _: media_api::MediaOperationContext,
            _: media_api::MediaRef,
        ) -> Result<media_api::MediaContent, media_api::MediaError> {
            Err(media_api::MediaError::new(
                media_api::MediaErrorKind::Unavailable,
                "test",
            ))
        }

        async fn cancel(
            &self,
            _: media_api::MediaOperationId,
        ) -> Result<(), media_api::MediaError> {
            Ok(())
        }
    }

    #[test]
    fn app_context_removes_media_from_builtin_and_mcp_clones() {
        let mut host = tool_api::test_support::shell_test_ctx(mobile_linux_api::ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        });
        host.media = Some(Arc::new(TestMediaService));
        host.media_agent_handoff = Some(Arc::new(
            crate::mobile::host::MobileMediaAgentHandoffQueue::default(),
        ));
        let session_id = SessionId::new();
        host.session_id = Some(session_id);
        let mut host_registry = ToolRegistry::new();
        tool_media::register_all(&mut host_registry, host.clone());
        assert!(host_registry.find_by_name("media_capture").is_some());

        let mut app = host.clone();
        clear_app_media_capabilities(&mut app);
        let mcp_context = app.clone();
        assert!(app.media.is_none() && app.media_agent_handoff.is_none());
        assert!(mcp_context.media.is_none() && mcp_context.media_agent_handoff.is_none());
        assert_eq!(app.session_id, Some(session_id));
        assert!(host.media.is_some() && host.media_agent_handoff.is_some());
        let mut app_registry = ToolRegistry::new();
        tool_media::register_all(&mut app_registry, app);
        assert!(app_registry.find_registered("media_capture").is_none());

        let identity = tool_api::McpToolIdentity {
            server_config_id: "server".into(),
            config_revision: "revision".into(),
            raw_tool_name: "lookup".into(),
            definition_fingerprint: "definition".into(),
        };
        let mcp_tool = Arc::new(
            tool_mcp::MCPTool::new_for_tool(
                mcp_context,
                "mcp__server__lookup".into(),
                "lookup".into(),
                Utf16JsonProjection::parse(r#"{"type":"object"}"#).unwrap(),
                None,
                None,
                None,
                true,
                false,
            )
            .with_capability_identity(identity.clone()),
        );
        app_registry.register_mcp_tools(lingxi_core::types::McpConnectionId::new(), vec![mcp_tool]);
        let mut policy = SessionToolPolicy::default();
        policy.mcp_tools.insert(identity.clone());
        let lease = SessionToolGrantLease::new(policy);
        app_registry.set_session_tool_policy(lease.clone());
        assert!(lease.allows_mcp(&identity));
        assert!(app_registry.find_by_name("mcp__server__lookup").is_some());
        assert!(!lease.allows_builtin("media_capture"));
        assert!(app_registry.find_by_name("media_capture").is_none());
        let advertised = app_registry.available_tools(&tool_api::ToolStaticContext::default());
        assert!(advertised
            .iter()
            .any(|tool| tool.name() == "mcp__server__lookup"));
        assert!(advertised.iter().all(|tool| tool.name() != "media_capture"));
    }

    #[test]
    fn app_discovery_is_bound_to_the_selected_wire_protocol() {
        use llm_runtime::ProtocolFamily::*;
        for protocol in [
            AnthropicMessages,
            VertexClaude,
            BedrockClaude,
            FoundryClaude,
        ] {
            assert!(app_supports_deferred_tool_search(Some(protocol)));
        }
        for protocol in [
            OpenAiChat,
            OpenAiResponses,
            GeminiGenerateContent,
            GeminiInteractions,
            VertexGemini,
            AzureOpenAi,
        ] {
            assert!(!app_supports_deferred_tool_search(Some(protocol)));
        }
        assert!(!app_supports_deferred_tool_search(None));
    }
}

#[cfg(test)]
mod resume_guard_tests {
    use super::validate_local_app_chat_resume;
    use lingxi_core::host::FileSystem;
    use orchestrator::resume::{replay_session_state_from_path, ReplayedSession};
    use serde_json::{json, Value};
    use std::{path::PathBuf, sync::Arc};
    use tempfile::TempDir;
    use uuid::Uuid;

    struct TranscriptFixture {
        _root: TempDir,
        path: PathBuf,
        original: Vec<u8>,
        replayed: ReplayedSession,
    }

    async fn transcript_fixture(deferred: bool, result_id: Option<&str>) -> TranscriptFixture {
        let root = tempfile::tempdir().unwrap();
        let session_id = Uuid::new_v4();
        let user_id = Uuid::new_v4();
        let assistant_id = Uuid::new_v4();
        let attachment_id = Uuid::new_v4();
        let result_uuid = Uuid::new_v4();
        let cwd = root.path().to_string_lossy().to_string();
        let mut rows: Vec<Value> = vec![json!({
            "type": "user", "uuid": user_id, "parentUuid": null,
            "sessionId": session_id, "timestamp": "2026-10-27T01:00:00.000Z", "cwd": cwd,
            "message": {"role": "user", "content": "inspect the saved session"}
        })];
        if deferred {
            rows.push(json!({
                "type": "assistant", "uuid": assistant_id, "parentUuid": user_id,
                "sessionId": session_id, "timestamp": "2026-10-27T01:00:01.000Z", "cwd": cwd,
                "message": {"role": "assistant", "content": [{
                    "type": "tool_use", "id": "toolu_pending", "name": "Shell", "input": {"command": "touch forbidden"}
                }]}
            }));
            rows.push(json!({
                "type": "attachment", "uuid": attachment_id, "parentUuid": assistant_id,
                "sessionId": session_id, "timestamp": "2026-10-27T01:00:02.000Z", "cwd": cwd,
                "attachment": {"type": "hook_deferred_tool", "toolUseID": "toolu_pending",
                    "toolName": "Shell", "toolInput": {"command": "touch forbidden"},
                    "hookName": "PreToolUse:Shell", "hookEvent": "PreToolUse", "permissionMode": "default"}
            }));
            if let Some(result_id) = result_id {
                rows.push(json!({
                    "type": "user", "uuid": result_uuid, "parentUuid": attachment_id,
                    "sessionId": session_id, "timestamp": "2026-10-27T01:00:03.000Z", "cwd": cwd,
                    "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": result_id, "content": "already handled"}]}
                }));
            }
        }
        let original = rows
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let path = root.path().join(format!("{session_id}.jsonl"));
        std::fs::write(&path, original.as_bytes()).unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(platform_posix_minimal::PosixFileSystem::new(
            root.path().to_path_buf(),
        ));
        let replayed = replay_session_state_from_path(&path, session_id, fs)
            .await
            .unwrap();
        TranscriptFixture {
            _root: root,
            path,
            original: original.into_bytes(),
            replayed,
        }
    }

    #[tokio::test]
    async fn unresolved_deferred_tool_is_rejected_without_changing_the_jsonl() {
        let fixture = transcript_fixture(true, None).await;
        assert_eq!(fixture.replayed.runtime_metadata.deferred_tools.len(), 1);
        assert_eq!(
            fixture.replayed.runtime_metadata.deferred_tools[0].tool_name,
            "Shell"
        );
        assert_eq!(
            fixture
                .replayed
                .handle_runtime_snapshot()
                .deferred_tools
                .len(),
            1
        );
        assert_eq!(
            validate_local_app_chat_resume(&fixture.replayed).unwrap_err(),
            "chat_deferred_tools_pending"
        );
        assert_eq!(std::fs::read(&fixture.path).unwrap(), fixture.original);
        assert!(!fixture._root.path().join("forbidden").exists());
    }

    #[tokio::test]
    async fn only_a_matching_later_tool_result_resolves_the_deferred_operation() {
        let wrong_result = transcript_fixture(true, Some("another_tool")).await;
        assert_eq!(
            validate_local_app_chat_resume(&wrong_result.replayed).unwrap_err(),
            "chat_deferred_tools_pending"
        );
        assert_eq!(
            std::fs::read(&wrong_result.path).unwrap(),
            wrong_result.original
        );
        let resolved = transcript_fixture(true, Some("toolu_pending")).await;
        assert!(resolved.replayed.runtime_metadata.deferred_tools.is_empty());
        assert!(validate_local_app_chat_resume(&resolved.replayed).is_ok());
        assert_eq!(std::fs::read(&resolved.path).unwrap(), resolved.original);
    }

    #[tokio::test]
    async fn ordinary_chat_without_deferred_tools_remains_resumable() {
        let fixture = transcript_fixture(false, None).await;
        assert!(fixture.replayed.runtime_metadata.deferred_tools.is_empty());
        assert!(validate_local_app_chat_resume(&fixture.replayed).is_ok());
        assert!(fixture
            .replayed
            .display_history
            .iter()
            .any(|row| row.text_content().contains("inspect the saved session")));
        assert_eq!(std::fs::read(&fixture.path).unwrap(), fixture.original);
    }
}
