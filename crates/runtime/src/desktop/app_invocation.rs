//! Host-owned Local App execution profiles. This is not a renderer config API.
//!
//! One profile builds one fresh ordinary Harness runtime. Application identity,
//! storage roots, credentials and grants are resolved by the trusted host; the
//! app supplies only input and may request limits lower than its host grant.

use super::{CustomizationGates, DesktopConfig, DesktopSessionComposition};
use serde::{Deserialize, Serialize};
use tool_api::{SessionToolGrantLease, SessionToolPolicy};

/// Default maximum number of model iterations in an application task.
pub const DEFAULT_MAX_TURNS: u32 = 20;
/// Default wall-clock task limit. The host runner also bounds startup/shutdown.
pub const DEFAULT_TIMEOUT_MS: u64 = 600_000;

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

/// Trusted host-approved capability snapshot for exactly one task.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AppInvocationProfile {
    pub identity: AppInvocationIdentity,
    pub tool_policy: SessionToolPolicy,
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
        if self.max_turns == 0 || self.timeout_ms == 0 {
            return Err(AppInvocationError::InvalidLimits);
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
    #[error("app invocations require a trusted host permission gate")]
    MissingPermissionGate,
    #[error("app invocations require absolute, host-owned working and config directories")]
    InvalidDirectories,
    #[error("app invocations require an explicit configured model")]
    MissingModel,
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
    config.default_model_explicit = true;
    config.default_model_env_pinned = false;
    config.fallback_model = None;
    // The host has resolved an explicit qualified model. Ambient routing rules
    // must not silently send an app's input to another provider on failure.
    config.routing = None;
    config.recent_models.clear();
    config.max_turns = Some(profile.max_turns);
    config.max_structured_output_retries = config.max_structured_output_retries.clamp(1, 5);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::OrchestratorHandle as _;
    use std::sync::Arc;

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
        config.session_resume_snapshot = Some(super::super::DesktopSessionResumeSnapshot {
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
        assert!(config
            .session_tool_grant_lease
            .as_ref()
            .unwrap()
            .is_revoked());
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
        assert!(runtime
            .structured_output_slot
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .is_none());
        assert!(runtime.plugin_runtime.is_none());
        assert!(!runtime.coordinator_mode.is_enabled());
        assert!(runtime
            .orchestrator
            .conversation_transcript()
            .await
            .is_empty());
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
