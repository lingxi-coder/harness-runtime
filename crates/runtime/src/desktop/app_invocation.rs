//! Host-owned Local App execution profiles. This is not a renderer config API.
//!
//! One profile builds one fresh ordinary Harness runtime. Application identity,
//! storage roots, credentials and grants are resolved by the trusted host; the
//! app supplies only input and may request limits lower than its host grant.

#[cfg(test)]
use super::DesktopSessionResumeSnapshot;
use super::{CustomizationGates, DesktopConfig, DesktopSessionComposition};
#[path = "app_chat.rs"]
pub(super) mod chat;
pub use chat::AppChatCleanupReceipt;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tool_api::{SessionToolGrantLease, SessionToolPolicy};

/// Default maximum number of model iterations in an application task.
pub const DEFAULT_MAX_TURNS: u32 = 20;
/// Default wall-clock task limit. The host runner also bounds startup/shutdown.
pub const DEFAULT_TIMEOUT_MS: u64 = 600_000;
pub const MAX_MAX_TURNS: u32 = 100;
pub const MAX_TIMEOUT_MS: u64 = 3_600_000;

/// Built-ins with no unscoped child execution, extension installation or account
/// administration. Other tools can be added only after they propagate app scope.
pub const APP_AGENT_BUILTIN_TOOLS: &[&str] = &[
    "Read",
    "Write",
    "Edit",
    "Glob",
    "Grep",
    "Bash",
    "PowerShell",
    "NotebookEdit",
    "WebFetch",
    "WebSearch",
    "AskUserQuestion",
    "ToolSearch",
];

/// Identity established from the host's actual app window and running revision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AppInvocationIdentity {
    pub app_id: String,
    pub version_id: String,
    pub invocation_id: String,
}

/// Host-created identity for a persistent App Chat. The transcript lives only
/// in this App-owned directory; callers must never copy a creator transcript
/// into it or use the creator session ID.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AppChatProfile {
    pub chat_id: String,
    pub session_id: String,
    pub session_directory: PathBuf,
    pub generation_id: String,
    pub native_user_message_id: String,
    pub cleanup_nonce: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_checkpoint: Option<AppChatGenerationCheckpoint>,
}

impl AppChatProfile {
    /// Bind the incoming user row to this one host-owned task generation.
    /// Public AG-UI message identifiers are opaque and must not replace this
    /// canonical Harness identity when admitting the model-facing row.
    pub fn native_message_id(&self) -> Result<lingxi_core::types::MessageId, AppInvocationError> {
        let generation = uuid::Uuid::parse_str(&self.generation_id)
            .map_err(|_| AppInvocationError::InvalidChat)?;
        let message = lingxi_core::types::MessageId::parse_prefixed(&self.native_user_message_id)
            .ok_or(AppInvocationError::InvalidChat)?;
        if generation.to_string() != self.generation_id
            || message.as_uuid() != generation
            || message.to_string() != self.native_user_message_id
        {
            return Err(AppInvocationError::InvalidChat);
        }
        Ok(message)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AppChatGenerationCheckpoint {
    pub generation_id: String,
    pub session_id: String,
    pub byte_len: u64,
    pub sha256: String,
}

/// Validate the actual, policy-filtered tool implementations before a Chat
/// can issue its first model request. A granted but unsupported tool is an
/// explicit error, rather than a silent downgrade or an unchecked handle.
pub fn validate_app_chat_tools(
    runtime: &super::DesktopRuntime,
    policy: &SessionToolPolicy,
) -> Result<(), AppInvocationError> {
    let tools = runtime
        .tools
        .available_tools(&tool_api::tool_trait::ToolStaticContext::default());
    for tool in &tools {
        if !tool.supports_persistent_chat() {
            return Err(AppInvocationError::ChatUnsupportedTool);
        }
    }
    for name in &policy.builtin_tools {
        if !tools.iter().any(|tool| tool.name() == name) {
            return Err(AppInvocationError::ChatUnsupportedTool);
        }
    }
    for grant in &policy.mcp_tools {
        if !tools
            .iter()
            .any(|tool| tool.mcp_tool_identity() == Some(grant))
        {
            return Err(AppInvocationError::ChatUnsupportedTool);
        }
    }
    Ok(())
}

/// Seal only after the host's resource drain succeeded. This proves the
/// current generation's exact canonical history, including this admitted user
/// and the assistant identity returned over the private child transport.
pub async fn checkpoint_app_chat(
    runtime: &super::DesktopRuntime,
    chat: &AppChatProfile,
    assistant_id: &str,
) -> Result<AppChatGenerationCheckpoint, &'static str> {
    let captured = runtime
        .orchestrator
        .verify_persistent_chat_checkpoint(true)
        .await
        .map_err(|_| "chat_transcript_unavailable")?;
    let expected_path = chat
        .session_directory
        .join(&chat.generation_id)
        .join(format!("{}.jsonl", chat.session_id));
    if captured.path() != expected_path || captured.session_id().to_string() != chat.session_id {
        return Err("chat_transcript_unavailable");
    }
    let replayed =
        orchestrator::ConversationOrchestrator::replay_persistent_chat_snapshot(&captured)
            .map_err(|_| "chat_transcript_unavailable")?;
    let user_id = chat.native_message_id().map_err(|_| "invalid_chat")?;
    let assistant_id =
        lingxi_core::types::MessageId::parse_prefixed(assistant_id).ok_or("invalid_result")?;
    if replayed.state.history.iter().filter(|message| message.id() == user_id).count() != 1
        || !replayed.state.history.iter().any(|message| matches!(message, lingxi_core::types::ConversationMessage::User { id, .. } if *id == user_id))
        || replayed.state.history.iter().rev().find(|message| matches!(message, lingxi_core::types::ConversationMessage::Assistant { .. })).map(|message| message.id()) != Some(assistant_id)
    {
        return Err("chat_transcript_unavailable");
    }
    Ok(AppChatGenerationCheckpoint {
        generation_id: chat.generation_id.clone(),
        session_id: chat.session_id.clone(),
        byte_len: captured.checkpoint().byte_len,
        sha256: captured.checkpoint().sha256.clone(),
    })
}

/// Trusted host-approved capability snapshot for exactly one task.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AppInvocationProfile {
    pub identity: AppInvocationIdentity,
    pub tool_policy: SessionToolPolicy,
    /// Present only when the native host is starting a turn in an App-owned
    /// persistent Harness session. One-shot tasks leave this unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_chat: Option<AppChatProfile>,
    /// Verified instructions returned by app_agent_skills::materialize.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_instructions: Option<String>,
    /// Native cleanup evidence shared with the actual tool contexts. This is
    /// not part of the child DTO and cannot be supplied by an App page.
    #[serde(skip)]
    pub tool_cleanup_failed: Option<Arc<std::sync::atomic::AtomicBool>>,
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

const fn default_max_turns() -> u32 {
    DEFAULT_MAX_TURNS
}
const fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

impl AppInvocationProfile {
    /// Construct the conservative default host profile.
    #[must_use]
    pub fn new(identity: AppInvocationIdentity, tool_policy: SessionToolPolicy) -> Self {
        Self {
            identity,
            tool_policy,
            app_chat: None,
            skill_instructions: None,
            tool_cleanup_failed: None,
            max_turns: DEFAULT_MAX_TURNS,
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }

    fn validate(&self) -> Result<(), AppInvocationError> {
        for value in [
            &self.identity.app_id,
            &self.identity.version_id,
            &self.identity.invocation_id,
        ] {
            if value.trim().is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
                return Err(AppInvocationError::InvalidIdentity);
            }
        }
        if self.max_turns == 0
            || self.max_turns > MAX_MAX_TURNS
            || self.timeout_ms == 0
            || self.timeout_ms > MAX_TIMEOUT_MS
        {
            return Err(AppInvocationError::InvalidLimits);
        }
        if self
            .skill_instructions
            .as_ref()
            .is_some_and(|instructions| {
                instructions.is_empty()
                    || instructions.len() > 1024 * 1024
                    || instructions.contains('\0')
            })
        {
            return Err(AppInvocationError::InvalidSkills);
        }
        if let Some(chat) = self.app_chat.as_ref() {
            if uuid::Uuid::parse_str(&chat.chat_id).is_err()
                || !uuid::Uuid::parse_str(&chat.session_id)
                    .is_ok_and(|id| id.to_string() == chat.session_id)
                || chat.native_message_id().is_err()
                || chat.cleanup_nonce.len() != 64
                || !chat
                    .cleanup_nonce
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                || chat.base_checkpoint.as_ref().is_some_and(|checkpoint| {
                    uuid::Uuid::parse_str(&checkpoint.generation_id).is_err()
                        || checkpoint.generation_id == chat.generation_id
                        || checkpoint.session_id != chat.session_id
                        || checkpoint.byte_len == 0
                        || checkpoint.byte_len > 64 * 1024 * 1024
                        || checkpoint.sha256.len() != 64
                        || !checkpoint
                            .sha256
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                })
                || !chat.session_directory.is_absolute()
                || chat.session_directory.components().any(|part| {
                    matches!(
                        part,
                        std::path::Component::ParentDir | std::path::Component::CurDir
                    )
                })
            {
                return Err(AppInvocationError::InvalidChat);
            }
        }
        for tool in &self.tool_policy.builtin_tools {
            if !APP_AGENT_BUILTIN_TOOLS.contains(&tool.as_str()) {
                return Err(AppInvocationError::UnsupportedTool(tool.clone()));
            }
        }
        Ok(())
    }
}

/// An invalid profile fails before constructing providers or running any tool.
#[derive(Debug, thiserror::Error)]
pub enum AppInvocationError {
    #[error("app, version and invocation identities must be nonempty bounded identifiers")]
    InvalidIdentity,
    #[error("task turn and timeout limits must be greater than zero")]
    InvalidLimits,
    #[error("this tool does not support isolated app invocations: {0}")]
    UnsupportedTool(String),
    #[error("a requested tool cannot be preserved safely in App Chat")]
    ChatUnsupportedTool,
    #[error("app invocations require a trusted host permission gate")]
    MissingPermissionGate,
    #[error("app invocations require absolute, host-owned working and config directories")]
    InvalidDirectories,
    #[error("app invocations require an explicit configured model")]
    MissingModel,
    #[error("App Chat requires a host-owned UUID and an isolated app chat directory")]
    InvalidChat,
    #[error("App Chat transcript could not be opened under its exclusive generation owner")]
    ChatTranscriptUnavailable,
    #[error("App Skills must be unique, bounded, and host-copied into the isolated workspace")]
    InvalidSkills,
}

/// Prepare a fresh application runtime from host-owned provider and storage
/// inputs. Selected `cli_mcp_servers` and model credentials are retained; ambient
/// discovery, conversation reuse and personal customizations are stripped.
///
/// The host retains `session_tool_grant_lease` for immediate revocation. Normal
/// desktop builds repeat the isolation gate at assembly, preserving that lease.
pub fn prepare_app_config(
    mut config: DesktopConfig,
    profile: AppInvocationProfile,
) -> Result<DesktopConfig, AppInvocationError> {
    profile.validate()?;
    config.app_invocation = Some(profile);
    config.session_tool_grant_lease = None;
    enforce_app_config(&mut config)?;
    Ok(config)
}

/// Build the deliberately stateless request shape used by a plain App task.
/// No history, tool declarations or engine system prompt can be inherited.
#[must_use]
pub fn side_query_request(
    model: String,
    profile: Option<String>,
    messages: Vec<lingxi_core::types::ConversationMessage>,
    output_format: Option<serde_json::Value>,
) -> sidequery::SideQueryRequest {
    sidequery::SideQueryRequest {
        model_attempt: None,
        api_attempts_left: None,
        model,
        model_source: None,
        profile,
        system_prompt: None,
        typed_attachment_ids: Default::default(),
        task_deliveries: Default::default(),
        native_prompt_context: None,
        system_prompt_input: None,
        enable_prompt_caching: false,
        messages,
        tools: Vec::new(),
        tool_choice: None,
        output_format,
        max_tokens: None,
        max_retries: 2,
        temperature: None,
        thinking: None,
        effort: None,
        stop_sequences: Vec::new(),
        query_source: sidequery::QuerySource::Custom("local_app_agent".into()),
        skip_system_prompt_prefix: true,
    }
}

/// Build the strict JSON Schema request used by isolated one-shot App Agent
/// calls. Keeping this constructor beside the SideQuery adapter avoids a
/// direct sidequery dependency in the bridge protocol crate.
#[must_use]
pub fn strict_side_query_request(
    model: String,
    profile: Option<String>,
    messages: Vec<lingxi_core::types::ConversationMessage>,
    schema: serde_json::Value,
) -> sidequery::StrictStructuredQueryRequest {
    sidequery::StrictStructuredQueryRequest {
        model_attempt: None,
        model,
        profile,
        system_prompt: None,
        messages,
        schema,
        max_tokens: Some(8192),
        temperature: None,
        query_source: sidequery::QuerySource::Custom("local_app_agent".into()),
        skip_system_prompt_prefix: true,
    }
}

/// Execute a one-shot App query through the configured Desktop provider stack.
/// The supplied stack is the host-seeded stack from the private bridge; the
/// operation creates no Harness session, transcript, tools, hooks or memory.
pub async fn run_side_query(
    config: DesktopConfig,
    shared: super::SharedCredentialStack,
    request: sidequery::SideQueryRequest,
) -> Result<sidequery::SideQueryResponse, &'static str> {
    run_side_query_accounted(config, shared, request)
        .await
        .map_err(|error| error.code)
}

/// Bounded failure information for the private Desktop App Agent adapter.
/// Provider diagnostics are intentionally excluded while partial usage remains
/// available for the durable task record.
#[derive(Debug)]
pub struct AppSideQueryFailure {
    pub code: &'static str,
    pub usage: Option<cost::Usage>,
    pub turns: Option<u32>,
}

pub async fn run_side_query_accounted(
    config: DesktopConfig,
    shared: super::SharedCredentialStack,
    request: sidequery::SideQueryRequest,
) -> Result<sidequery::SideQueryResponse, AppSideQueryFailure> {
    if !request.tools.is_empty() {
        return Err(AppSideQueryFailure {
            code: "unsupported_tool_request",
            usage: None,
            turns: None,
        });
    }
    let stack = super::credentials::resolve_llm_stack_with_credentials(&config, shared, None)
        .await
        .map_err(|_| AppSideQueryFailure {
            code: "runtime_initialization_failed",
            usage: None,
            turns: None,
        })?;
    let service = Arc::new(
        super::api_service_from_stack(&config, &config.cwd, stack).map_err(|_| {
            AppSideQueryFailure {
                code: "runtime_initialization_failed",
                usage: None,
                turns: None,
            }
        })?,
    );
    let client = sidequery::ProviderSideQueryClient::from_service(service);
    sidequery::SideQueryClient::query(&client, request)
        .await
        .map_err(app_side_query_failure)
}

/// Strict structured-output form of [`run_side_query`]. Provider response
/// format is enforced and decoded as JSON Schema before returning to the host.
pub async fn run_side_query_json_schema(
    config: DesktopConfig,
    shared: super::SharedCredentialStack,
    request: sidequery::StrictStructuredQueryRequest,
) -> Result<sidequery::StrictStructuredQueryResponse, &'static str> {
    run_side_query_json_schema_accounted(config, shared, request)
        .await
        .map_err(|error| error.code)
}

pub async fn run_side_query_json_schema_accounted(
    config: DesktopConfig,
    shared: super::SharedCredentialStack,
    request: sidequery::StrictStructuredQueryRequest,
) -> Result<sidequery::StrictStructuredQueryResponse, AppSideQueryFailure> {
    let stack = super::credentials::resolve_llm_stack_with_credentials(&config, shared, None)
        .await
        .map_err(|_| AppSideQueryFailure {
            code: "runtime_initialization_failed",
            usage: None,
            turns: None,
        })?;
    let service = Arc::new(
        super::api_service_from_stack(&config, &config.cwd, stack).map_err(|_| {
            AppSideQueryFailure {
                code: "runtime_initialization_failed",
                usage: None,
                turns: None,
            }
        })?,
    );
    let client = sidequery::ProviderSideQueryClient::from_service(service);
    sidequery::SideQueryClient::query_json_schema(&client, request)
        .await
        .map_err(app_side_query_failure)
}

fn app_side_query_failure(error: sidequery::SideQueryError) -> AppSideQueryFailure {
    match error {
        sidequery::SideQueryError::StructuredOutputUnsupported => AppSideQueryFailure {
            code: "model_structured_output_unsupported",
            usage: None,
            turns: None,
        },
        sidequery::SideQueryError::Partial {
            source,
            usage,
            api_calls,
            ..
        } => {
            let code = match *source {
                sidequery::SideQueryError::StructuredOutputUnsupported => {
                    "model_structured_output_unsupported"
                }
                _ => "model_execution_failed",
            };
            AppSideQueryFailure {
                code,
                usage: Some(usage),
                turns: Some(api_calls),
            }
        }
        _ => AppSideQueryFailure {
            code: "model_execution_failed",
            usage: None,
            turns: None,
        },
    }
}

pub(super) fn enforce_app_config(config: &mut DesktopConfig) -> Result<(), AppInvocationError> {
    let Some(profile) = config.app_invocation.clone() else {
        return Ok(());
    };
    profile.validate()?;
    if config.injected_permission_gate.is_none() {
        return Err(AppInvocationError::MissingPermissionGate);
    }
    if !config.cwd.is_absolute() || !config.lingxi_home.is_absolute() {
        return Err(AppInvocationError::InvalidDirectories);
    }
    if config.default_model.trim().is_empty() {
        return Err(AppInvocationError::MissingModel);
    }
    let mut tool_policy = profile.tool_policy.clone();
    if config.json_schema.is_some() {
        tool_policy
            .builtin_tools
            .insert(orchestrator::structured_output::STRUCTURED_OUTPUT_TOOL_NAME.into());
    }
    if config.session_tool_grant_lease.is_none() {
        config.session_tool_grant_lease = Some(SessionToolGrantLease::new(tool_policy));
    }
    config.restricted_tools = Some(profile.tool_policy.builtin_tools.iter().cloned().collect());
    config.restricted = true; // Managed policy still participates in every gate.
    config.setting_source_scope = (false, false);
    config.customization_gates = CustomizationGates {
        safe_mode: true,
        bare: false,
    };
    config.composition = Some(DesktopSessionComposition::Transport);
    config.use_noop_permission_gate = false;
    config.deny_unresolved_ask = true;
    config.permission_mode = permission::PermissionMode::Default;
    config.permission_mode_cli = Some("default".into());
    config.permission_mode_preference = None;
    config.permission_mode_cli_explicit = true;
    config.allow_dangerously_skip_permissions = false;
    config.tool_aliases = None;
    config.flag_settings = None;
    config.flag_settings_source = None;
    config.memory_provider = None;
    config.system_prompt_override = None;
    config.append_system_prompt = None;
    config.plan_mode_instructions = None;
    config.plans_directory = None;
    config.session_resume_snapshot = None;
    config.session_resume_cost = None;
    config.session_transcript_path = None;
    config.session_writer_lease = None;
    config.session_id_override = None;
    config.parent_session_id = None;
    config.session_persistence = false;
    config.idle_compaction_enabled = false;
    config.defer_session_start = false;
    config.initial_prompt_directories.clear();
    config.add_dir.clear();
    config.initial_project_root = None;
    config.disable_slash_commands = true;
    config.session_skill_allowlist = Some(Vec::new());
    config.cli_agent = None;
    config.cli_agents_json = None;
    config.cli_plugin_dirs.clear();
    config.injected_plugin_secrets.clear();
    config.mcp_paths.clear();
    config.strict_mcp_config = true;
    config.connect_mcp_in_background = false;
    config.worktree_launch = None;
    config.tmux_launch = None;
    config.session_started_as_coordinator = false;
    config.initial_teammate_team_name = None;
    config.bg_session_forker = None;
    config.enable_automation_scheduler = false;
    config.idle_compaction_enabled = false;
    config.inline_visualization = false;
    config.local_apps = false;
    config.audio = None;
    config.media_service = None;
    config.media_conversation_attachment = None;
    config.media_agent_handoffs = None;
    config.default_model_explicit = true;
    config.default_model_env_pinned = false;
    config.fallback_model = None;
    // The host has resolved an explicit qualified model. Ambient routing rules
    // must not silently send an app's input to another provider on failure.
    config.routing = None;
    config.recent_models.clear();
    config.max_turns = Some(profile.max_turns);
    config.max_structured_output_retries = config.max_structured_output_retries.clamp(1, 5);
    if let Some(chat) = profile.app_chat.as_ref() {
        apply_app_chat(config, &profile.identity, chat)?;
    }
    if let Some(instructions) = profile.skill_instructions.as_ref() {
        config.append_system_prompt = Some(instructions.clone().into());
    }
    Ok(())
}

fn apply_app_chat(
    config: &mut DesktopConfig,
    identity: &AppInvocationIdentity,
    chat: &AppChatProfile,
) -> Result<(), AppInvocationError> {
    // This exact tree is also used by Local App v2 durable chat metadata.
    // Lexical equality prevents a wire caller from redirecting the transcript
    // into the creator workspace or another application's storage.
    let expected = config
        .lingxi_home
        .join("local-apps/v2/apps")
        .join(&identity.app_id)
        .join("agent/agui-v1/chats")
        .join(&chat.chat_id);
    let workspace = config
        .lingxi_home
        .join("local-apps/v2/apps")
        .join(&identity.app_id)
        .join("agent/agui-v1/workspaces")
        .join(&chat.chat_id);
    if chat.session_directory != expected || config.cwd != workspace {
        return Err(AppInvocationError::InvalidChat);
    }
    lingxi_core::host::rooted_fs::root_identity(&chat.session_directory)
        .map_err(|_| AppInvocationError::InvalidChat)?;
    lingxi_core::host::rooted_fs::root_identity(&config.cwd)
        .map_err(|_| AppInvocationError::InvalidDirectories)?;
    config.session_id_override = Some(chat.session_id.clone());
    config.session_transcript_path = Some(
        chat.session_directory
            .join(&chat.generation_id)
            .join(format!("{}.jsonl", chat.session_id)),
    );
    config.session_persistence = true;
    // App Chat restores only its strict capture below, before any model turn.
    // Generic startup hooks and ordinary resume loaders have no authority here.
    config.defer_session_start = true;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::OrchestratorHandle as _;
    use std::sync::Arc;

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

    struct TestMediaAttachment;

    #[async_trait::async_trait]
    impl orchestrator::MediaConversationAttachment for TestMediaAttachment {
        async fn open(
            &self,
            _: tool_api::MediaAgentHandoffRequest,
            _: orchestrator::RealtimeAgentContext,
            _: Arc<dyn media_api::MediaAudioChannel>,
        ) -> Result<orchestrator::RealtimeAgentSessionIo, String> {
            Err("test attachment is never opened".into())
        }
    }

    fn profile() -> AppInvocationProfile {
        AppInvocationProfile::new(
            AppInvocationIdentity {
                app_id: "food".into(),
                version_id: "v1".into(),
                invocation_id: "run-1".into(),
            },
            SessionToolPolicy::default(),
        )
    }
    fn config() -> DesktopConfig {
        DesktopConfig {
            cwd: "/tmp/app-run".into(),
            lingxi_home: "/tmp/app-runtime".into(),
            injected_permission_gate: Some(Arc::new(permission::DenyOnAskGate)),
            ..Default::default()
        }
    }

    #[test]
    fn strips_history_and_personal_customizations_but_retains_host_route() {
        let mut config = config();
        config.session_id_override = Some("creating-chat".into());
        config.session_resume_cost = Some(cost::CostState::default());
        config.session_resume_snapshot = Some(DesktopSessionResumeSnapshot {
            source_session_id: "old".into(),
            content: Arc::from("private history"),
        });
        config.append_system_prompt = Some("private instruction".into());
        config.mcp_paths.push("/private/mcp.json".into());
        config.add_dir.push("/private".into());
        config.cli_agents_json = Some("{}".into());
        config.cli_plugin_dirs.push("/private/plugin".into());
        config.session_started_as_coordinator = true;
        config.fallback_model = Some("other-provider/model".into());
        config.routing =
            Some(serde_json::json!({"fallback_chains": {"openai/test": ["other/model"]}}));
        config.default_model = "openai/test".into();
        let config = prepare_app_config(config, profile()).unwrap();
        assert_eq!(config.default_model, "openai/test");
        assert!(config.default_model_explicit && config.restricted);
        assert_eq!(config.max_turns, Some(DEFAULT_MAX_TURNS));
        assert_eq!(config.setting_source_scope, (false, false));
        assert!(config.customization_gates.safe_mode && !config.customization_gates.bare);
        assert!(config.session_resume_snapshot.is_none() && config.session_resume_cost.is_none());
        assert!(config.session_id_override.is_none() && config.append_system_prompt.is_none());
        assert!(config.mcp_paths.is_empty() && config.add_dir.is_empty());
        assert!(config.cli_agents_json.is_none() && config.cli_plugin_dirs.is_empty());
        assert!(!config.session_started_as_coordinator && !config.session_persistence);
        assert!(!config.enable_automation_scheduler && config.fallback_model.is_none());
        assert!(config.routing.is_none());
        assert!(config.disable_slash_commands && config.strict_mcp_config);
    }

    #[test]
    fn app_isolation_clears_media_while_preserving_granted_mcp_lease() {
        let media: Arc<dyn media_api::MediaService> = Arc::new(TestMediaService);
        let attachment: Arc<dyn orchestrator::MediaConversationAttachment> =
            Arc::new(TestMediaAttachment);
        let queue = Arc::new(super::super::DesktopMediaAgentHandoffQueue::default());
        let mut host = config();
        host.media_service = Some(media.clone());
        host.media_conversation_attachment = Some(attachment.clone());
        host.media_agent_handoffs = Some(queue.clone());
        enforce_app_config(&mut host).unwrap();
        assert!(host.media_service.is_some());
        assert!(host.media_conversation_attachment.is_some());
        assert!(host.media_agent_handoffs.is_some());

        let mcp = tool_api::McpToolIdentity {
            server_config_id: "server".into(),
            config_revision: "revision".into(),
            raw_tool_name: "lookup".into(),
            definition_fingerprint: "definition".into(),
        };
        let mut profile = profile();
        profile.tool_policy.builtin_tools.insert("Read".into());
        profile.tool_policy.mcp_tools.insert(mcp.clone());
        let mut isolated = prepare_app_config(host, profile).unwrap();
        assert!(isolated.media_service.is_none());
        assert!(isolated.media_conversation_attachment.is_none());
        assert!(isolated.media_agent_handoffs.is_none());
        let lease = isolated.session_tool_grant_lease.clone().unwrap();
        assert!(lease.allows_builtin("Read"));
        assert!(lease.allows_mcp(&mcp));
        assert!(!lease.allows_builtin("media_capture"));

        // Even a captured media tool from the host cannot bypass a lease that
        // separately authorizes an MCP tool.
        let mut ctx = tool_api::test_support::shell_test_ctx(mobile_linux_api::ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        });
        ctx.media = Some(media.clone());
        let mut registry = tool_api::ToolRegistry::new();
        tool_media::register_all(&mut registry, ctx);
        let captured = registry.find_by_name("media_capture").unwrap();
        registry.set_session_tool_policy(lease.clone());
        assert!(registry.find_by_name("media_capture").is_none());
        assert!(
            registry
                .check_session_tool_access(captured.as_ref())
                .is_err()
        );
        assert!(
            registry
                .available_tools(&tool_api::ToolStaticContext::default())
                .iter()
                .all(|tool| tool.name() != "media_capture")
        );

        // Assembly re-enforces isolation without replacing the revocable lease.
        isolated.media_service = Some(media);
        isolated.media_conversation_attachment = Some(attachment);
        isolated.media_agent_handoffs = Some(queue);
        enforce_app_config(&mut isolated).unwrap();
        assert!(isolated.media_service.is_none());
        assert!(isolated.media_conversation_attachment.is_none());
        assert!(isolated.media_agent_handoffs.is_none());
        lease.revoke();
        assert!(
            isolated
                .session_tool_grant_lease
                .as_ref()
                .unwrap()
                .is_revoked()
        );
    }

    #[test]
    fn required_schema_tool_is_internal_and_revocation_survives_assembly() {
        let mut config = config();
        config.json_schema = Some(
            lingxi_core::types::utf16_json::Utf16JsonProjection::parse(r#"{"type":"object"}"#)
                .unwrap(),
        );
        let mut config = prepare_app_config(config, profile()).unwrap();
        let lease = config.session_tool_grant_lease.clone().unwrap();
        assert!(lease.allows_builtin("StructuredOutput"));
        assert!(!lease.allows_builtin("Bash"));
        lease.revoke();
        enforce_app_config(&mut config).unwrap();
        assert!(
            config
                .session_tool_grant_lease
                .as_ref()
                .unwrap()
                .is_revoked()
        );
    }

    #[test]
    fn invalid_authority_and_unscoped_delegation_fail_closed() {
        let mut no_gate = config();
        no_gate.injected_permission_gate = None;
        assert!(matches!(
            prepare_app_config(no_gate, profile()),
            Err(AppInvocationError::MissingPermissionGate)
        ));
        for tool in [
            "Agent",
            "Skill",
            "Workflow",
            "McpAuth",
            "LocalApp",
            "StructuredOutput",
        ] {
            let mut profile = profile();
            profile.tool_policy.builtin_tools.insert(tool.into());
            assert!(matches!(
                prepare_app_config(config(), profile),
                Err(AppInvocationError::UnsupportedTool(_))
            ));
        }
        let mut zero = profile();
        zero.max_turns = 0;
        assert!(matches!(
            prepare_app_config(config(), zero),
            Err(AppInvocationError::InvalidLimits)
        ));
    }

    #[test]
    fn selected_mcp_config_identity_is_preserved() {
        let mut config = config();
        let server = mcp::build_server_from_json_entry(
            "food-source",
            &serde_json::json!({"command":"/host/mcp-food"}),
            mcp::ConfigScope::Enterprise,
        )
        .unwrap();
        let expected = serde_json::to_value(&server).unwrap();
        config.cli_mcp_servers.push(server);
        let config = prepare_app_config(config, profile()).unwrap();
        assert_eq!(
            serde_json::to_value(&config.cli_mcp_servers[0]).unwrap(),
            expected
        );
        assert!(super::super::settings_environment_for_config(&config).is_empty());
    }

    #[test]
    fn app_chat_selects_current_generation_without_importing_ambient_history() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("config");
        let chat_id = uuid::Uuid::new_v4().to_string();
        let session_id = uuid::Uuid::new_v4().to_string();
        let chat_directory = home
            .join("local-apps/v2/apps/food/agent/agui-v1/chats")
            .join(&chat_id);
        std::fs::create_dir_all(&chat_directory).unwrap();
        let transcript = chat_directory.join(format!("{session_id}.jsonl"));
        std::fs::write(&transcript, "app-owned chat transcript\n").unwrap();
        let mut config = config();
        config.cwd = home
            .join("local-apps/v2/apps/food/agent/agui-v1/workspaces")
            .join(&chat_id);
        std::fs::create_dir_all(&config.cwd).unwrap();
        config.lingxi_home = home;
        config.session_id_override = Some("creator-session-must-not-survive".into());
        config.session_transcript_path = Some(directory.path().join("creator.jsonl"));
        config.session_persistence = false;
        config.idle_compaction_enabled = false;
        let mut profile = profile();
        profile.app_chat = Some(AppChatProfile {
            chat_id,
            session_id: session_id.clone(),
            session_directory: chat_directory.clone(),
            generation_id: session_id.clone(),
            cleanup_nonce: "b".repeat(64),
            native_user_message_id: format!("msg:{session_id}"),
            base_checkpoint: None,
        });
        let config = prepare_app_config(config, profile).unwrap();
        assert!(config.session_persistence && config.defer_session_start);
        assert_eq!(
            config.session_id_override.as_deref(),
            Some(session_id.as_str())
        );
        assert_eq!(
            config.session_transcript_path,
            Some(
                chat_directory
                    .join(&session_id)
                    .join(format!("{session_id}.jsonl"))
            )
        );
        assert!(config.session_resume_snapshot.is_none() && config.session_resume_cost.is_none());
        assert_eq!(
            std::fs::read_to_string(transcript).unwrap(),
            "app-owned chat transcript\n"
        );
    }

    #[test]
    fn app_chat_rejects_paths_outside_its_app_owned_directory() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("config");
        let chat_id = uuid::Uuid::new_v4().to_string();
        let session_id = uuid::Uuid::new_v4().to_string();
        let chat_directory = directory.path().join("creator-workspace");
        std::fs::create_dir_all(&chat_directory).unwrap();
        let mut config = config();
        config.lingxi_home = home;
        let mut profile = profile();
        profile.app_chat = Some(AppChatProfile {
            chat_id,
            session_id,
            session_directory: chat_directory,
            generation_id: uuid::Uuid::nil().to_string(),
            cleanup_nonce: "b".repeat(64),
            native_user_message_id: format!("msg:{}", uuid::Uuid::nil()),
            base_checkpoint: None,
        });
        assert!(matches!(
            prepare_app_config(config, profile),
            Err(AppInvocationError::InvalidChat)
        ));
    }

    #[test]
    fn chat_native_message_identity_is_canonical_and_belongs_to_its_generation() {
        let generation = uuid::Uuid::new_v4();
        let mut chat = AppChatProfile {
            chat_id: uuid::Uuid::new_v4().to_string(),
            session_id: uuid::Uuid::new_v4().to_string(),
            session_directory: PathBuf::from("/host-owned-chat"),
            generation_id: generation.to_string(),
            cleanup_nonce: "b".repeat(64),
            native_user_message_id: format!("msg:{generation}"),
            base_checkpoint: None,
        };
        assert_eq!(chat.native_message_id().unwrap().as_uuid(), generation);
        for invalid in [
            format!("msg_{generation}"),
            generation.to_string(),
            format!("msg:{}", uuid::Uuid::new_v4()),
        ] {
            chat.native_user_message_id = invalid;
            assert!(matches!(
                chat.native_message_id(),
                Err(AppInvocationError::InvalidChat)
            ));
        }
    }

    struct SilentPermissions;

    #[async_trait::async_trait]
    impl client::adapter::PermissionRequestSink for SilentPermissions {
        async fn emit_request(&self, _: client::protocol::permission::PermissionRequest) {
            panic!("app runtime must use the injected permission gate");
        }
    }

    #[tokio::test]
    async fn isolated_build_advertises_only_granted_tools_and_validated_capture() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = config();
        config.cwd = directory.path().join("working");
        config.lingxi_home = directory.path().join("config");
        std::fs::create_dir_all(&config.cwd).unwrap();
        std::fs::create_dir_all(&config.lingxi_home).unwrap();
        std::fs::write(
            config.cwd.join("LINGXI.md"),
            "DO_NOT_INHERIT_PRIVATE_CONTEXT",
        )
        .unwrap();
        std::fs::write(
            config.lingxi_home.join("settings.json"),
            r#"{"language":"DO_NOT_INHERIT","autoMemoryEnabled":true}"#,
        )
        .unwrap();
        config.isolated_credential_storage = true;
        config.json_schema = Some(
            lingxi_core::types::utf16_json::Utf16JsonProjection::parse(
                r#"{"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"]}"#,
            )
            .unwrap(),
        );
        let config = prepare_app_config(config, profile()).unwrap();
        let lease = config.session_tool_grant_lease.clone().unwrap();
        let output = Arc::new(orchestrator::test_support::MockOutputStream::new());
        let runtime = super::super::build(config, output, Arc::new(SilentPermissions))
            .await
            .unwrap();
        assert_eq!(
            runtime.orchestrator.tool_names(),
            vec!["StructuredOutput".to_string()]
        );
        assert!(
            runtime
                .structured_output_slot
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .is_none()
        );
        assert!(runtime.plugin_runtime.is_none());
        assert!(!runtime.coordinator_mode.is_enabled());
        assert!(
            runtime
                .orchestrator
                .conversation_transcript()
                .await
                .is_empty()
        );
        assert_eq!(runtime.orchestrator.snapshot_cost().await.api_calls, 0);
        let context = runtime
            .orchestrator
            .additional_context_preview()
            .await
            .unwrap()
            .unwrap_or_default();
        assert!(!context.contains("DO_NOT_INHERIT"));
        assert!(!context.contains("userEmail"));
        lease.revoke();
        assert!(runtime.orchestrator.tool_names().is_empty());
        runtime.session_lifecycle.shutdown_and_drain().await;
    }

    #[tokio::test]
    async fn invalid_schema_fails_before_provider_request() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = config();
        config.cwd = directory.path().to_path_buf();
        config.lingxi_home = directory.path().join("config");
        config.isolated_credential_storage = true;
        config.json_schema = Some(
            lingxi_core::types::utf16_json::Utf16JsonProjection::parse(
                r#"{"type":"not-a-json-schema-type"}"#,
            )
            .unwrap(),
        );
        let config = prepare_app_config(config, profile()).unwrap();
        let output = Arc::new(orchestrator::test_support::MockOutputStream::new());
        let result = super::super::build(config, output, Arc::new(SilentPermissions)).await;
        assert!(matches!(
            result,
            Err(super::super::BuildError::StructuredOutputSchema(_))
        ));
    }
}
