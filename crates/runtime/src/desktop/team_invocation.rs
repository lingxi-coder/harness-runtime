//! Trusted Team execution profiles. No wire, settings or renderer decoder can
//! construct this capability. A host still owns authenticated start, routing,
//! wall-clock supervision and durable recovery; this module is not an executor.

use super::{CustomizationGates, DesktopConfig, DesktopSessionComposition};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tool_api::isolated_shell::IsolatedShellPolicy;
use tool_api::{SessionToolGrantLease, SessionToolPolicy};

pub use super::session_state::team_chat::BotChatScope;

/// Finite host ceilings. Input validation must precede the product host's run
/// call; duration and reliable process termination remain host responsibilities.
#[derive(Clone, Copy, Debug)]
pub struct TeamInvocationLimits {
    pub max_turns: NonZeroU32,
    pub context_window_tokens: NonZeroU32,
    pub output_tokens_per_request: NonZeroU32,
    pub physical_dispatches: NonZeroU32,
}
impl Default for TeamInvocationLimits {
    fn default() -> Self {
        Self {
            max_turns: NonZeroU32::new(12).unwrap(),
            context_window_tokens: NonZeroU32::new(128_000).unwrap(),
            output_tokens_per_request: NonZeroU32::new(4_096).unwrap(),
            physical_dispatches: NonZeroU32::new(24).unwrap(),
        }
    }
}

/// Frozen, revocable authority for one admitted invocation. Clones share the
/// same tool lease and generation quota; enforcing the config never resets them.
#[derive(Clone)]
pub struct TeamInvocationProfile {
    scope: BotChatScope,
    scope_root: PathBuf,
    invocation_id: uuid::Uuid,
    inputs_path: PathBuf,
    instructions: Arc<str>,
    limits: TeamInvocationLimits,
    shell: Arc<IsolatedShellPolicy>,
    lease: SessionToolGrantLease,
    quota: Arc<llm_runtime::ScopeOutputQuota>,
    policy_cache: super::settings_watch::ManagedPolicySnapshotCache,
    selected_model: Option<Arc<super::team_process::TeamProcessModel>>,
    pub(crate) read_tools: Option<Arc<super::team_process::read_tools::TeamReadTools>>,
}

impl std::fmt::Debug for TeamInvocationProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TeamInvocationProfile")
            .field("scope_root", &self.scope_root)
            .field("invocation_id", &self.invocation_id)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TeamInvocationError {
    #[error("invalid Team invocation principal")]
    Identity,
    #[error("Team invocation limits exceed host ceilings")]
    Limits,
    #[error("Team invocation requires an explicit qualified model")]
    Model,
    #[error("Team and Local App profiles cannot be combined")]
    MixedProfile,
    #[error("Team persona/skills must be bounded immutable text")]
    Instructions,
    #[error("Team invocation has been revoked")]
    Revoked,
    #[error("Team credentials must use an explicit isolated memory store")]
    Credentials,
    #[error(
        "Team scope must use canonical private host-owned directories and regular immutable inputs"
    )]
    Storage,
    #[error("Team sandbox profile is unsupported on this platform")]
    Platform,
    #[error("Team scope storage: {0}")]
    Io(#[from] std::io::Error),
}

impl TeamInvocationProfile {
    /// Materialize only host-supplied text in a private scope. `storage_root`
    /// must already be canonical, owner-only and host-controlled. No Bot owner
    /// home, filesystem link, credential or live Skill directory is copied.
    /// `instructions` contains the owner's selected persona/Skill snapshot.
    ///
    /// This creates fresh authority, so the product host may call it only for
    /// a one-shot authenticated start permit, never for ambiguous restart replay.
    pub fn prepare(
        storage_root: &Path,
        scope: BotChatScope,
        invocation_id: uuid::Uuid,
        instructions: String,
        limits: TeamInvocationLimits,
    ) -> Result<Self, TeamInvocationError> {
        if !cfg!(target_os = "macos") {
            return Err(TeamInvocationError::Platform);
        }
        let root_text = storage_root.to_str().ok_or(TeamInvocationError::Storage)?;
        // The platform profile generator accepts glob patterns. Scope paths
        // are literal capabilities and must never widen into sibling grants.
        if root_text.chars().any(char::is_control)
            || sandbox_runtime::path_utils::contains_glob_chars(root_text)
        {
            return Err(TeamInvocationError::Storage);
        }
        let key = scope
            .session_id()
            .map_err(|_| TeamInvocationError::Identity)?
            .as_uuid()
            .to_string();
        if invocation_id.is_nil()
            || limits.max_turns.get() > 20
            || !(8_192..=2_000_000).contains(&limits.context_window_tokens.get())
            || limits.output_tokens_per_request.get() > 4_096
            || limits.physical_dispatches.get() > 32
        {
            return Err(TeamInvocationError::Limits);
        }
        if instructions.trim().is_empty()
            || instructions.len() > 65_536
            || instructions.contains('\0')
        {
            return Err(TeamInvocationError::Instructions);
        }
        check_private_directory(storage_root)?;
        let scope_root = private_child(&private_child(storage_root, "scopes")?, &key)?;
        for name in ["home", "workspace", "tmp", "shell-home", "inputs"] {
            private_child(&scope_root, name)?;
        }
        private_child(
            &private_child(&scope_root.join("home"), "tool-output")?,
            &invocation_id.to_string(),
        )?;
        let snapshot = format!("{:x}.md", Sha256::digest(instructions.as_bytes()));
        let inputs_path = scope_root.join("inputs").join(snapshot);
        write_immutable(&inputs_path, instructions.as_bytes())?;
        let shell = Arc::new(IsolatedShellPolicy::new(&scope_root));
        let lease = SessionToolGrantLease::new(SessionToolPolicy {
            builtin_tools: HashSet::from(["TeamChatHistory".into(), "TeamKnowledge".into()]),
            mcp_tools: HashSet::new(),
        });
        let quota = Arc::new(llm_runtime::ScopeOutputQuota::new(
            limits.output_tokens_per_request,
            limits.physical_dispatches,
        ));
        Ok(Self {
            scope,
            scope_root,
            invocation_id,
            inputs_path,
            instructions: instructions.into(),
            limits,
            shell,
            lease,
            quota,
            policy_cache: Default::default(),
            selected_model: None,
            read_tools: None,
        })
    }

    #[must_use]
    pub fn scope(&self) -> &BotChatScope {
        &self.scope
    }
    #[must_use]
    pub fn scope_root(&self) -> &Path {
        &self.scope_root
    }
    #[must_use]
    pub fn shell_policy(&self) -> Arc<IsolatedShellPolicy> {
        self.shell.clone()
    }
    /// All output spools stay under the host-writable private home.
    #[must_use]
    pub fn task_output_dir(&self) -> PathBuf {
        self.scope_root
            .join("home")
            .join("tool-output")
            .join(self.invocation_id.to_string())
    }
    /// Prepare a host config without exposing a settings/wire constructor.
    pub fn apply_to(&self, config: &mut DesktopConfig) -> Result<(), TeamInvocationError> {
        config.team_invocation = Some(self.clone());
        enforce_team_config(config)
    }
    #[must_use]
    pub fn output_quota(&self) -> Arc<llm_runtime::ScopeOutputQuota> {
        self.quota.clone()
    }

    /// Freeze one host-selected registry route before private runtime assembly.
    #[must_use]
    pub fn with_selected_model(mut self, model: super::team_process::TeamProcessModel) -> Self {
        self.selected_model = Some(Arc::new(model));
        self
    }

    pub(crate) fn selected_model(&self) -> Option<&super::team_process::TeamProcessModel> {
        self.selected_model.as_deref()
    }

    pub(crate) fn with_read_tools(
        mut self,
        tools: Arc<super::team_process::read_tools::TeamReadTools>,
    ) -> Self {
        self.read_tools = Some(tools);
        self
    }

    /// All captured tools and future model dispatches lose authority. This is
    /// not proof that an already-admitted OS process has stopped.
    pub fn revoke(&self) {
        self.lease.revoke();
        self.quota.revoke();
    }

    pub fn session_id(&self) -> lingxi_core::types::SessionId {
        self.scope.session_id().expect("validated scope")
    }
    pub fn context_window_tokens(&self) -> u32 {
        self.limits.context_window_tokens.get()
    }

    fn check_storage(&self) -> Result<(), TeamInvocationError> {
        check_private_directory(&self.scope_root)?;
        for name in [
            "home",
            "workspace",
            "tmp",
            "shell-home",
            "inputs",
            "home/tool-output",
        ] {
            check_private_directory(&self.scope_root.join(name))?;
        }
        check_private_directory(&self.task_output_dir())?;
        check_immutable(&self.inputs_path, self.instructions.as_bytes())
    }
}

/// Apply before credentials or settings are opened, on every assembly entry.
/// Starting from defaults deliberately discards owner callbacks, injected MCP,
/// plugins, resume state, workspaces and capabilities as the config evolves.
pub(super) fn enforce_team_config(config: &mut DesktopConfig) -> Result<(), TeamInvocationError> {
    let Some(profile) = config.team_invocation.clone() else {
        return Ok(());
    };
    if config.app_invocation.is_some() {
        return Err(TeamInvocationError::MixedProfile);
    }
    profile.check_storage()?;
    if profile.lease.is_revoked() || profile.quota.is_revoked() {
        return Err(TeamInvocationError::Revoked);
    }
    if !config
        .default_model
        .split_once('/')
        .is_some_and(|(provider, model)| !provider.is_empty() && !model.is_empty())
        || config.default_model.trim() != config.default_model
        || config.default_model.chars().any(char::is_control)
    {
        return Err(TeamInvocationError::Model);
    }
    let mut scoped = DesktopConfig::default();
    scoped.build_info = config.build_info.clone();
    scoped.managed_policy_snapshot_cache = profile.policy_cache.clone();
    scoped.default_model = config.default_model.clone();
    scoped.default_model_explicit = true;
    // Only the explicitly selected provider profile may carry credentials or
    // endpoint configuration. Never copy the owner's entire provider catalog.
    let selected_profile = config.default_model.split_once('/').unwrap().0;
    if selected_profile == "anthropic" {
        scoped.api_base = config.api_base.clone();
    }
    scoped.provider_profiles = config.provider_profiles.as_ref().map(|profiles| {
        profiles
            .iter()
            .filter(|(name, _)| name.as_str() == selected_profile)
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect()
    });
    if let Some(model) = profile.selected_model() {
        model.validate().map_err(|_| TeamInvocationError::Model)?;
        scoped.default_model = format!(
            "{}/{}",
            model.route().model_provider,
            model.route().model_id
        );
        scoped.provider_profiles = None;
    }
    scoped.credential_storage_policy = lingxi_core::host::CredentialStoragePolicy::NativeOrMemory;
    scoped.isolated_credential_storage = true;
    // M1 has no waiting/question/owner approval continuation capability.
    scoped.ask_user_question_tx = None;
    scoped.team_invocation = Some(profile.clone());
    scoped.session_tool_grant_lease = Some(profile.lease.clone());
    scoped.scope_output_quota = Some(profile.quota.clone());
    scoped.restricted = true;
    scoped.restricted_tools = Some(vec!["TeamChatHistory".into(), "TeamKnowledge".into()]);
    scoped.setting_source_scope = (false, false);
    scoped.customization_gates = CustomizationGates {
        safe_mode: true,
        bare: false,
    };
    scoped.composition = Some(DesktopSessionComposition::Transport);
    scoped.use_noop_permission_gate = false;
    scoped.deny_unresolved_ask = true;
    scoped.host_workspace_trusted = Some(false);
    scoped.permission_mode = permission::PermissionMode::Default;
    scoped.permission_mode_cli = Some("default".into());
    scoped.permission_mode_cli_explicit = true;
    scoped.allow_dangerously_skip_permissions = false;
    scoped.strict_mcp_config = true;
    scoped.connect_mcp_in_background = false;
    scoped.mcp_paths.clear();
    scoped.cli_mcp_servers.clear();
    scoped.initial_prompt_directories.clear();
    scoped.add_dir.clear();
    scoped.session_skill_allowlist = Some(Vec::new());
    scoped.disable_slash_commands = true;
    scoped.inline_visualization = false;
    scoped.private_pages = false;
    scoped.exclude_dynamic_system_prompt_sections = true;
    scoped.insights_policy_gate = Some(false);
    scoped.team_onboarding_policy_gate = Some(false);
    scoped.local_apps = false;
    scoped.idle_compaction_enabled = false;
    scoped.enable_automation_scheduler = false;
    scoped.session_persistence = true;
    scoped.max_turns = Some(profile.limits.max_turns.get());
    scoped.max_structured_output_retries = 1;
    scoped.lingxi_home = profile.scope_root.join("home");
    scoped.cwd = profile.scope_root.join("workspace");
    scoped.session_id_override = Some(profile.session_id().to_string());
    // The ordinary canonical projects path is required by captured resume.
    // Preserve only a capture of this exact private Bot×Chat session.
    if config
        .session_id_override
        .as_deref()
        .and_then(lingxi_core::types::SessionId::parse_prefixed)
        == Some(profile.session_id())
        && config.lingxi_home == scoped.lingxi_home
        && config.cwd == scoped.cwd
    {
        scoped.session_transcript_path = config.session_transcript_path.clone();
        scoped.session_transcript_authority = config.session_transcript_authority.clone();
        scoped.session_resume_snapshot = config.session_resume_snapshot.clone();
        scoped.session_resume_target_snapshot = config.session_resume_target_snapshot.clone();
        scoped.session_writer_lease = config.session_writer_lease.clone();
        scoped.session_resume_cost = config.session_resume_cost.clone();
        scoped.defer_session_start = config.defer_session_start;
    }
    scoped.append_system_prompt = Some(profile.instructions.to_string().into());
    *config = scoped;
    Ok(())
}

/// Published read-only tools are the entire M1 capability set. A managed
/// policy may deny them but cannot restore ambient owner capabilities.
pub(super) fn enforce_team_permission_policy(policy: &mut permission::PermissionPolicy) {
    policy.allow_rules.clear();
    policy.ask_rules.clear();
    for name in ["TeamChatHistory", "TeamKnowledge"] {
        let rule = permission::PermissionRule::allow_tool_session(name);
        policy
            .allow_rules
            .entry(rule.source)
            .or_default()
            .push(rule);
    }
    policy.sandbox_runtime = None;
    policy.set_mode(permission::PermissionMode::Default);
}

fn private_child(parent: &Path, name: &str) -> Result<PathBuf, TeamInvocationError> {
    let path = parent.join(name);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        match std::fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
    }
    check_private_directory(&path)?;
    Ok(path)
}

fn check_private_directory(path: &Path) -> Result<(), TeamInvocationError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::symlink_metadata(path)?;
        if !path.is_absolute()
            || !m.is_dir()
            || m.mode() & 0o777 != 0o700
            || m.uid() != nix::unistd::geteuid().as_raw()
            || std::fs::canonicalize(path)? != path
        {
            return Err(TeamInvocationError::Storage);
        }
        return Ok(());
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(TeamInvocationError::Platform)
    }
}

fn write_immutable(path: &Path, bytes: &[u8]) -> Result<(), TeamInvocationError> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o400)
            .open(path)
        {
            Ok(mut file) => {
                file.write_all(bytes)?;
                file.sync_all()?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
    }
    check_immutable(path, bytes)
}

fn check_immutable(path: &Path, bytes: &[u8]) -> Result<(), TeamInvocationError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::symlink_metadata(path)?;
        if !m.is_file()
            || m.len() != bytes.len() as u64
            || m.mode() & 0o777 != 0o400
            || m.nlink() != 1
            || m.uid() != nix::unistd::geteuid().as_raw()
            || std::fs::canonicalize(path)? != path
            || std::fs::read(path)? != bytes
        {
            return Err(TeamInvocationError::Storage);
        }
        return Ok(());
    }
    #[cfg(not(unix))]
    {
        let _ = (path, bytes);
        Err(TeamInvocationError::Platform)
    }
}

/// Embedded prompt commands must not construct the ordinary host shell or its
/// login snapshot. Even direct host access to the public provider refuses.
pub(super) struct DisabledShellExpansion;

#[async_trait::async_trait]
impl command_api::ShellExpansionProvider for DisabledShellExpansion {
    async fn git_worktree_context(&self) -> command_api::shell_expansion::GitWorktreeContext {
        command_api::shell_expansion::GitWorktreeContext {
            cwd: "".into(),
            inside: false,
        }
    }
    fn build(
        &self,
        _: &[String],
        _: Option<command_api::model::FrontmatterShell>,
    ) -> command_api::shell_expansion::ShellExpansionCtx {
        command_api::shell_expansion::ShellExpansionCtx {
            runner: Arc::new(DisabledShellExpansion),
            permission_gate: Arc::new(DisabledShellExpansion),
        }
    }
}
impl command_api::shell_expansion::ShellPermissionGate for DisabledShellExpansion {
    fn check(
        &self,
        _: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        _: Option<command_api::model::FrontmatterShell>,
    ) -> command_api::shell_expansion::ShellPermissionDecision {
        command_api::shell_expansion::ShellPermissionDecision::Deny {
            message: Some("Prompt shell expansion is unavailable for Team invocations".into()),
        }
    }
}
#[async_trait::async_trait]
impl command_api::shell_expansion::ShellRunner for DisabledShellExpansion {
    async fn run(
        &self,
        _: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        _: Option<command_api::model::FrontmatterShell>,
        _: lingxi_core::host::CancellationToken,
    ) -> Result<command_api::shell_expansion::ShellOut, command_api::shell_expansion::ShellRunError>
    {
        Err(command_api::shell_expansion::ShellRunError {
            generic_message: Some(
                "Prompt shell expansion is unavailable for Team invocations".into(),
            ),
            ..Default::default()
        })
    }
}
