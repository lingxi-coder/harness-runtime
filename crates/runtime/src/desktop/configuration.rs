use lingxi_core::host::CredentialStoragePolicy;
use orchestrator::{QUERY_SOURCE_REPL_MAIN_THREAD, QUERY_SOURCE_SDK};
use permission::gate::PermissionGate;
#[cfg(windows)]
use platform_windows::WindowsMcpTransport;
#[cfg(windows)]
use platform_windows::process::supervisor as shell_supervisor;
use std::sync::Arc;

use super::{DesktopAudio, RecentModelRef};

/// API provider, mirroring `APIProvider` (`utils/model/providers.ts:4`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum ApiProvider {
    FirstParty,
    Bedrock,
    Vertex,
    Foundry,
}

/// Port of `isEnvTruthy` (`envUtils.ts:32-37`); value test delegated to
/// [`lingxi_core::host::env::is_env_truthy`].
pub(super) fn is_env_truthy(key: &str) -> bool {
    lingxi_core::host::env::is_env_truthy(std::env::var(key).ok().as_deref())
}

/// Session-memory writes persist into the same memdir-backed Session tier that
/// per-turn prefetch reads from, so enabling writes must also enable the read
/// path. Prefetch-only remains independently available.
pub(super) fn resolve_memory_feature_gates(
    memdir_prefetch_enabled: bool,
    session_memory_enabled: bool,
) -> (bool, bool) {
    (
        memdir_prefetch_enabled || session_memory_enabled,
        session_memory_enabled,
    )
}

/// Port of `getAPIProvider()` (`utils/model/providers.ts:6-14`).
pub(super) fn api_provider() -> ApiProvider {
    if is_env_truthy("CLAUDE_CODE_USE_BEDROCK") {
        ApiProvider::Bedrock
    } else if is_env_truthy("CLAUDE_CODE_USE_VERTEX") {
        ApiProvider::Vertex
    } else if is_env_truthy("CLAUDE_CODE_USE_FOUNDRY") {
        ApiProvider::Foundry
    } else {
        ApiProvider::FirstParty
    }
}

/// A deprecated model's display name and its per-provider retirement dates.
pub(super) struct DeprecationEntry {
    pub(super) model_name: &'static str,
    pub(super) first_party: Option<&'static str>,
    pub(super) bedrock: Option<&'static str>,
    pub(super) vertex: Option<&'static str>,
    pub(super) foundry: Option<&'static str>,
}

impl DeprecationEntry {
    pub(super) fn retirement_date(&self, provider: ApiProvider) -> Option<&'static str> {
        match provider {
            ApiProvider::FirstParty => self.first_party,
            ApiProvider::Bedrock => self.bedrock,
            ApiProvider::Vertex => self.vertex,
            ApiProvider::Foundry => self.foundry,
        }
    }
}

/// Deprecated models and their retirement dates by provider.
/// Byte-locked to `DEPRECATED_MODELS` (`utils/model/deprecation.ts:33-61`).
pub(super) const DEPRECATED_MODELS: &[(&str, DeprecationEntry)] = &[
    (
        "claude-3-opus",
        DeprecationEntry {
            model_name: "Claude 3 Opus",
            first_party: Some("January 5, 2026"),
            bedrock: Some("January 15, 2026"),
            vertex: Some("January 5, 2026"),
            foundry: Some("January 5, 2026"),
        },
    ),
    (
        "claude-3-7-sonnet",
        DeprecationEntry {
            model_name: "Claude 3.7 Sonnet",
            first_party: Some("February 19, 2026"),
            bedrock: Some("April 28, 2026"),
            vertex: Some("May 11, 2026"),
            foundry: Some("February 19, 2026"),
        },
    ),
    (
        "claude-3-5-haiku",
        DeprecationEntry {
            model_name: "Claude 3.5 Haiku",
            first_party: Some("February 19, 2026"),
            bedrock: None,
            vertex: None,
            foundry: None,
        },
    ),
];

pub(super) struct DeprecatedModelInfo {
    pub(super) model_name: &'static str,
    pub(super) retirement_date: &'static str,
}

pub(super) fn deprecated_model_info(model_id: &str) -> Option<DeprecatedModelInfo> {
    let lowercase = model_id.to_lowercase();
    let provider = api_provider();
    for (key, value) in DEPRECATED_MODELS {
        let Some(retirement_date) = value.retirement_date(provider) else {
            continue;
        };
        if !lowercase.contains(key) {
            continue;
        }
        return Some(DeprecatedModelInfo {
            model_name: value.model_name,
            retirement_date,
        });
    }
    None
}

/// Get a deprecation warning message for a model, or `None` if not deprecated.
///
/// Direct port of `getModelDeprecationWarning` (`deprecation.ts:88-101`).
/// Moved here from `providers::deprecation` (Plan 3b Task 3) so the CLI host
/// can call it without a direct `providers` dependency; re-exported at this
/// composition-root surface for backward compatibility.
///
/// With any current (Claude 4-generation) default model the lookup returns
/// `None`, so the startup output stays byte-identical until a user configures
/// one of the deprecated Claude 3 ids.
#[must_use]
pub fn model_deprecation_warning(model_id: Option<&str>) -> Option<String> {
    let model_id = model_id.filter(|m| !m.is_empty())?;
    let info = deprecated_model_info(model_id)?;
    Some(format!(
        "⚠ {} will be retired on {}. Consider switching to a newer model.",
        info.model_name, info.retirement_date
    ))
}

/// Desktop engine knobs.
///
/// Intentionally small in P6 — it grows as P8/P9 fold skills + commands and a
/// full `build()` entrypoint into the composition root.
#[derive(Clone, Debug)]
pub struct DesktopEngineConfig {
    /// Model reference the desktop build defaults to when argv omits `--model`.
    pub default_model: String,
}

impl Default for DesktopEngineConfig {
    fn default() -> Self {
        Self {
            // The catalog owns both parts of its automatic boot selection.
            // Shared model ids on other providers must not make that declared
            // default ambiguous; explicit user choices keep their own routing.
            default_model: lingxi_core::host::qualified_model_ref(
                lingxi_core::host::provider_default_model("anthropic")
                    .expect("the catalog defines the Anthropic boot default"),
                Some("anthropic"),
            ),
        }
    }
}

/// Deterministic, env/argv-free recipe for building a desktop runtime.
///
/// F2-00 (deliverable-zero): every value the ~270-line `build_runtime`
/// (`apps/cli/src/init.rs:148`) currently reads from `std::env`/`Argv` becomes
/// an explicit field here, so both the CLI host **and** the bridge-server (and
/// the F2 end-to-end test) can construct an identical runtime *without*
/// touching the process environment. F2-01 lifts the actual wiring into
/// `harness_runtime::desktop::build(DesktopConfig) -> DesktopRuntime`; this task only
/// freezes the field set.
///
/// Field provenance (each mirrors a concrete read in `build_runtime`):
/// - `api_base` — `resolve_api_base()` (env `LINGXI_API_BASE_URL`, init.rs:95).
/// - `api_key` — env `ANTHROPIC_API_KEY` (init.rs:153).
/// - `cwd` — `std::env::current_dir()` (init.rs:248).
/// - `lingxi_home` — the `~/.claude` (and platform config-dir) root the hook /
///   agents / global-MCP loaders walk (init.rs:253-317); made explicit so the
///   bridge-server can point it at a sandbox in tests.
/// - `default_model` — `Argv::model` ⟶ `OrchestratorConfig.model` (init.rs:213).
/// - `provider_profiles` — the settings `providers` block as raw JSON, fed
///   verbatim to `llm_runtime::ClientConfig` via `build()`. `None` ⟶ built-in
///   profiles only.
/// - `mcp_paths` — the precedence-ordered `.mcp.json` paths (project then
///   global) handed to `mcp::load_mcp_json_with_precedence` (init.rs:253-258).
/// - `use_noop_permission_gate` — lets the CLI opt into `NoOpPermissionGate`
///   (init.rs:245) while the bridge-server binds `AdapterPermissionGate`.
///
/// # Examples
///
/// Field-by-field construction (no env / argv reads — fully deterministic):
///
/// ```
/// use harness_runtime::desktop::DesktopConfig;
/// use std::collections::BTreeMap;
/// use std::path::PathBuf;
///
/// let cfg = DesktopConfig {
///     build_info: harness_runtime::desktop::BuildInfo::new("1.0.0", "host123"),
///     enable_automation_scheduler: true,
///     host_workspace_trusted: None,
///     api_base: "https://api.anthropic.com".to_string(),
///     api_key: "sk-test".to_string(),
///     isolated_credential_storage: false,
///     credential_storage_policy: lingxi_core::host::CredentialStoragePolicy::NativePreferred,
///     injected_plugin_secrets: BTreeMap::new(),
///     api_key_helper: None,
///     managed_oauth_only: false,
///     anthropic_key_fd_present: false,
///     cwd: PathBuf::from("/tmp/project"),
///     lingxi_home: PathBuf::from("/tmp/home/.lingxi"),
///     default_model: harness_runtime::desktop::DesktopEngineConfig::default().default_model,
///     default_model_explicit: false,
///     recent_models: Vec::new(),
///     fallback_model: None,
///     custom_betas: Vec::new(),
///     flag_settings: None,
///     provider_profiles: Some(BTreeMap::new()),
///     routing: None,
///     mcp_paths: vec![PathBuf::from("/tmp/project/.mcp.json")],
///     use_noop_permission_gate: false,
///     deny_unresolved_ask: false,
///     restricted: false,
///     restricted_tools: None,
///     is_tty: false,
///     max_turns: None,
///     max_budget_usd: None,
///     json_schema: None,
///     injected_permission_gate: None,
///     session_started_as_coordinator: false,
///     initial_teammate_team_name: None,
///     // `None` ⟶ empty memory (deterministic). A production host injects
///     // `Some(orchestrator::prompt::real_provider())` to load real LINGXI.md.
///     memory_provider: None,
///     permission_mode: permission::PermissionMode::Default,
///     // Kept beside the resolved mode: `build()` needs the CLI's own value
///     // and whether it was given explicitly to apply the oracle's precedence
///     // (CLI/dangerous-skip > agent frontmatter > settings defaultMode).
///     permission_mode_cli: None,
///     permission_mode_preference: None,
///     permission_mode_cli_explicit: false,
///     allow_dangerously_skip_permissions: false,
///     connect_prompt: None,
///     system_prompt_override: None,
///     append_system_prompt: None,
///     session_id_override: None,
///     // `None` ⟶ the engine opens its own writer claim rather than
///     // consuming one the host already acquired.
///     session_writer_lease: None,
///     parent_session_id: None,
///     disable_slash_commands: false,
///     add_dir: Vec::new(),
///     cli_mcp_servers: Vec::new(),
///     strict_mcp_config: false,
///     exclude_dynamic_system_prompt_sections: false,
///     setting_source_scope: (true, true),
///     customization_gates: harness_runtime::desktop::CustomizationGates::default(),
///     session_persistence: true,
///     cli_agents_json: None,
///     cli_agent: None,
///     cli_plugin_dirs: Vec::new(),
///     initial_effort: None,
///     plan_mode_instructions: None,
///     plans_directory: None,
///     default_model_env_pinned: false,
///     session_thinking: Default::default(),
///     // `None` ⟶ inert: no `-w`/`--worktree` boot launch.
///     worktree_launch: None,
///     // `None` ⟶ inert: no `--tmux` worktree tmux session.
///     tmux_launch: None,
///     // `None` ⟶ background session forking is unavailable to this host.
///     bg_session_forker: None,
///     // `None` ⟶ AskUserQuestion uses the non-TUI fallback path.
///     ask_user_question_tx: None,
///     // `None` ⟶ `request_access` uses the fail-closed DenyAllResolver.
///     computer_access_tx: None,
///     verified_computer_profiles: Vec::new(),
///     session_agent_observer: None,
///     // `None` ⟶ no device audio: the `voice`/`speech` tools are not
///     // registered at all (see `register_desktop_tools`).
///     audio: None,
/// };
///
/// assert_eq!(cfg.cwd, PathBuf::from("/tmp/project"));
/// assert!(!cfg.use_noop_permission_gate);
/// // `DesktopConfig` is `Clone` so a host can fan it out to multiple builders.
/// let _clone = cfg.clone();
/// ```
#[derive(Clone)]
pub struct DesktopConfig {
    /// Explicit host identity, independent of the available permission transport.
    pub composition: Option<DesktopSessionComposition>,
    /// Restore the mounted session before the host fires startup lifecycle hooks.
    pub defer_session_start: bool,
    /// Host package identity used by `/version`.
    pub build_info: command_api::builtins::BuildInfo,
    /// Whether this runtime owns versioned automation dispatch. CLI defaults to
    /// true; desktop hosts enable it only for their persistent scope controller.
    pub enable_automation_scheduler: bool,
    /// Explicit trust decision from the host for this workspace. `None` keeps
    /// CLI trust resolution; `Some(false)` must override any persisted grant.
    pub host_workspace_trusted: Option<bool>,
    /// The live client surface attached to this session, if one is known.
    pub mod_render_surface: Option<orchestrator::config::ModRenderSurface>,
    /// API base URL (default `https://api.anthropic.com`); env override
    /// `LINGXI_API_BASE_URL` is resolved by the host *before* it fills this.
    pub api_base: String,
    /// Anthropic API key. Empty string is valid — the orchestrator builds
    /// successfully and only fails at `run_turn` with a 401, so slash-command
    /// dispatch still works with no key configured.
    pub api_key: String,
    /// Redacted origin of the explicitly supplied API key. Hosts provide this
    /// alongside the value rather than inferring it from ambient environment.
    pub api_key_source: llm_runtime::CredentialSource,
    /// Inherit NO ambient credentials from the machine.
    ///
    /// The native backends are keyed by OS USER, not by [`Self::lingxi_home`],
    /// so a boot that points `lingxi_home` at a temp directory still reads the
    /// machine's real login keychain. That makes credential-dependent behaviour
    /// answer differently on a developer's logged-in machine than on a clean
    /// one — which is a test-isolation hazard, not a preference. Hosts that
    /// need a deterministic credential picture (sandboxed boots, e2e tests) set
    /// this; production leaves it `false`.
    ///
    /// Covers BOTH ambient sources: the OS keychain (used instead of the
    /// file-backed store) and the process environment (a bare
    /// `DEEPSEEK_API_KEY` in the developer's shell otherwise marks a provider
    /// connected, which is what made `bridge-server`'s credential-required e2e
    /// assertions machine-dependent).
    pub isolated_credential_storage: bool,
    /// Fallback policy for the shared credential store once native storage is
    /// unavailable. Production CLI/TUI/Desktop keep the owner-only plaintext
    /// sidecar; packaged bridge-server can switch to memory-only fallback.
    pub credential_storage_policy: CredentialStoragePolicy,
    /// Sensitive plugin `userConfig` values injected by the packaged Desktop
    /// parent. They are installed into the process-local credential cache
    /// before plugin discovery and are never included in debug output.
    pub injected_plugin_secrets:
        std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
    /// Settings `apiKeyHelper`: shell command/path that prints the Anthropic
    /// auth value. Used only when no higher-priority API key/OAuth source wins.
    pub api_key_helper: Option<String>,
    /// (M13) The HOST launcher forces Claude.ai OAuth as the effective auth
    /// source: with a stored OAuth session it then outranks even an env
    /// `ANTHROPIC_API_KEY` in the auth resolver
    /// (`llm_runtime::auth::anthropic::resolver`). claude-code derives this
    /// from `KWr()` (@228931361), a pure env predicate that
    /// `resolve_llm_stack` reads itself via
    /// [`llm_runtime::auth::anthropic::resolver::host_managed_oauth_only`], so
    /// every host gets it for free; this field only lets an embedding host
    /// declare the same forcing without the launcher env. A managed
    /// `forceLoginMethod` policy must NEVER be fed here — it has no place in
    /// credential precedence (`zb()` @228933355).
    pub managed_oauth_only: bool,
    /// (M13) `true` when the launcher advertised an FD-inherited Anthropic API
    /// key (`CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR` in claude-code's managed /
    /// remote launches). In the auth resolver an FD key outranks stored OAuth,
    /// so a stored session must NOT report as the Claude.ai-subscriber auth
    /// source. LingXi ships no FD-passing launcher of its own; the CLI fills
    /// this from env presence so the seam is closed for hosts that do.
    pub anthropic_key_fd_present: bool,
    /// Working directory the orchestrator + tool context are rooted at.
    pub cwd: std::path::PathBuf,
    /// The `~/.claude` root the hook / agents / global-MCP / settings loaders
    /// walk. Explicit so a host can redirect it to a sandbox.
    pub lingxi_home: std::path::PathBuf,
    /// Initial model reference; a `profile/model` qualifier is resolved before
    /// the provider-local id is passed to `OrchestratorConfig.model`.
    pub default_model: String,
    /// `true` when [`Self::default_model`] is an EXPLICIT per-session choice
    /// (`--model` flag) rather than the built-in default or the persisted
    /// `settings.model`. An explicit choice is never overridden by the
    /// boot-time connected-provider fallback ([`connected_provider_fallback`]).
    pub default_model_explicit: bool,
    /// `settings.recentModels` (most-recent-first), read by the host — `build()`
    /// itself stays off host config files (F2-01). Feeds the connected-provider
    /// fallback's preference pass. Empty ⟶ no recents (fallback uses the static
    /// provider order only).
    pub recent_models: Vec<RecentModelRef>,
    /// Fallback model id (`OrchestratorConfig.fallback_model`). `None` ⟶ no
    /// fallback, so the 529-overload interception in `turn_loop` stays a strict
    /// no-op. Mirrors `Argv::fallback_model`, which claude-code only HONORS in
    /// `--print`/non-interactive mode ("only works with --print"); the CLI host
    /// applies that gate before filling this field (`resolve_desktop_config`).
    pub fallback_model: Option<String>,
    /// Host-validated custom Anthropic beta header additions for this session.
    /// Empty keeps request headers unchanged.
    pub custom_betas: Vec<String>,
    /// Parsed CLI `--settings` / `flagSettings` layer. The CLI host fills this
    /// from an inline JSON object or file; non-CLI hosts leave it absent.
    pub flag_settings: Option<lingxi_core::settings::SettingsJson>,
    /// Settings-declared `providers` block as raw JSON, fed verbatim to
    /// `llm_runtime::ClientConfig` via `build()`. `None` ⟶ built-in profiles only.
    pub provider_profiles: Option<std::collections::BTreeMap<String, serde_json::Value>>,
    /// Settings-declared `routing` block as raw JSON, fed verbatim to
    /// `llm_runtime::ClientConfig` (model aliases / fallback / retry). `None` ⟶
    /// the default (empty) routing config. Mirrors `load_routing()` in
    /// `apps/cli/src/init.rs` — additive to the F2-00 field set so the lift
    /// preserves byte-equivalent engine behavior (no silent routing drop).
    pub routing: Option<serde_json::Value>,
    /// Precedence-ordered `.mcp.json` paths (project preferred over global)
    /// handed to `mcp::load_mcp_json_with_precedence`.
    pub mcp_paths: Vec<std::path::PathBuf>,
    /// When `true`, bind `NoOpPermissionGate` (the CLI default); when `false`,
    /// the host binds the connection-scoped `AdapterPermissionGate`.
    pub use_noop_permission_gate: bool,
    /// HEADLESS deny-on-ask (claude-code `--print` parity). When `true` AND
    /// `use_noop_permission_gate` is set, the inner gate becomes
    /// [`permission::DenyOnAskGate`] instead of the always-allow
    /// `NoOpPermissionGate`: a tool whose policy outcome is an unresolved `Ask`
    /// (a mutating tool with no matching allow rule, in a session with no
    /// interactive prompt) is DENIED rather than allowed. Allow/deny rules, the
    /// active mode, and read-only auto-allow are still resolved by
    /// `PolicyPermissionGate` first. Defaults to `false` → the prior always-allow
    /// inner, so interactive/transport builds and every existing caller stay
    /// byte-identical. The CLI sets this from `argv.print`.
    pub deny_unresolved_ask: bool,
    /// Claude Code 2.1.245 `process.stdout.isTTY??!1` (`tengu_api_success.isTTY`).
    /// Distinct from [`Self::deny_unresolved_ask`]: `-p` in a terminal is print
    /// and TTY. CLI fills this from stdout; SDK / bridge / mobile / tests leave
    /// the default `false`.
    pub is_tty: bool,
    /// CLI `--max-turns N`: cap on agent turns, mapped to
    /// [`orchestrator::OrchestratorConfig::max_turns`] in `build()`. `None` (the
    /// default) = unbounded.
    pub max_turns: Option<u32>,
    /// CLI `--plan-mode-instructions <instructions>` (print-only): custom plan-mode
    /// workflow body, mapped to
    /// [`orchestrator::OrchestratorConfig::plan_mode_instructions`] in `build()`.
    /// `None` (the default) = the default 5-phase plan reminder.
    pub plan_mode_instructions: Option<String>,
    /// `settings.json` `plansDirectory` (206 `iT`): custom directory for plan
    /// files, relative to the project root, mapped to
    /// [`orchestrator::OrchestratorConfig::plans_directory`] in `build()`.
    /// `None` (the default) = the default `<project-root>/.lingxi/plans/`.
    pub plans_directory: Option<String>,
    /// CLI `--max-budget USD`: cost ceiling in USD, mapped to
    /// [`orchestrator::OrchestratorConfig::max_budget_nano_usd`] (× 1e9) in
    /// `build()`. `None` (the default) = no cap.
    pub max_budget_usd: Option<f64>,
    /// CLI `--json-schema <schema>`: when set, `build()` forces structured
    /// output — it registers a `StructuredOutput` tool whose `input_schema` is
    /// this schema, forces `tool_choice` to it, and surfaces a capture slot on
    /// [`DesktopRuntime`] for the print path to validate + retry. `None` (the
    /// default) leaves every turn unconstrained (byte-identical to before).
    pub json_schema: Option<serde_json::Value>,
    /// Host-injected base permission gate (the INTERACTIVE prompt transport).
    /// When `Some`, `build()` uses it as the base gate instead of the
    /// `NoOpPermissionGate`/`DenyOnAskGate`/`AdapterPermissionGate` it would
    /// otherwise select — still wrapped by `PolicyPermissionGate` when
    /// enforcement is on (the CLI default), so rules + the active mode +
    /// read-only auto-allow resolve first and only an unresolved `Ask` reaches
    /// the injected prompt. The interactive TUI injects a
    /// `tui::permission_gate::TuiPermissionGate` here so an `Ask` surfaces as a dialog; `None`
    /// (the default + every headless/transport caller) keeps the prior
    /// selection, byte-identical.
    pub injected_permission_gate: Option<Arc<dyn PermissionGate>>,
    /// Enter coordinator mode at session startup. Teammate availability is
    /// controlled independently by the experimental agent-teams setting.
    pub session_started_as_coordinator: bool,
    /// Parent-owned implicit team when this host is a terminal teammate.
    pub initial_teammate_team_name: Option<String>,
    /// The LINGXI.md hierarchy provider the orchestrator loads project/user
    /// memory from. `None` (the default) ⟶ the empty
    /// [`StaticMemoryProvider::empty`], so a default build loads NO memory and
    /// is fully deterministic (the boot tests rely on this). A production host
    /// injects `Some(orchestrator::prompt::real_provider())` to load the real
    /// `<cwd>/LINGXI.md`, `<cwd>/LINGXI.local.md`, and `~/.lingxi/LINGXI.md`
    /// into the system prompt (claude-code parity), which also makes the
    /// session-start `fire_instructions_loaded()` fire over those files. The
    /// field is injectable (not a `bool` flag) so tests can supply a CONTROLLED
    /// in-memory [`StaticMemoryProvider::with_files`] and never touch the real
    /// filesystem.
    pub memory_provider: Option<Arc<dyn orchestrator::prompt::MemoryHierarchyProvider>>,
    /// CLI-resolved session permission mode (claude-code
    /// `initialPermissionModeFromCLI`). Replaces the previously hardwired
    /// `BuiltinToolContext.permission_mode = Default`. When
    /// `LINGXI_ENFORCE_PERMISSIONS` enforcement is ON, this OVERRIDES the
    /// settings `defaultMode` as the highest-priority source; `BypassPermissions`
    /// makes the policy allow-all (unless the bypass killswitch is set).
    ///
    /// EXECUTION-SEMANTICS NOTE: enforcement is default-ON for every mode
    /// (including `BypassPermissions` — the `PolicyPermissionGate` wrap is what
    /// PROVIDES the bypass auto-allow; see `should_enforce_permissions`). Only
    /// the `LINGXI_ENFORCE_PERMISSIONS=0` escape hatch disables it, in which
    /// case this field is execution-neutral but still drives
    /// `BuiltinToolContext.permission_mode` state.
    pub permission_mode: permission::PermissionMode,
    /// Raw CLI `--permission-mode` selection, when the user explicitly passed
    /// it. Preserved so `build()` can combine the final selected `--agent`
    /// frontmatter permission mode with the oracle's precedence
    /// (CLI/dangerous-skip > agent frontmatter > settings defaultMode).
    pub permission_mode_cli: Option<String>,
    /// Last user-selected mode; explicit flags, agent configuration and policy win.
    pub permission_mode_preference: Option<permission::PermissionMode>,
    /// Whether a CLI-surface permission override was explicitly requested via
    /// `--permission-mode` or `--dangerously-skip-permissions`. This differs
    /// from the resolved [`Self::permission_mode`]: an explicit
    /// `--permission-mode default` must still suppress an agent frontmatter
    /// override.
    pub permission_mode_cli_explicit: bool,
    /// Make `BypassPermissions` mode available in the session permission
    /// mode cycle (claude-code `--allow-dangerously-skip-permissions`).
    /// When `true`, `Plan` mode bypasses permissions, and the runtime
    /// `set_permission_mode("bypassPermissions")` gate accepts the mode.
    /// Default `false`.
    pub allow_dangerously_skip_permissions: bool,
    /// Plan 3c: host secure-input port for `/connect <api-key-provider>`. The tui
    /// supplies its masked-input widget; `None` → a headless no-op prompt
    /// (`crate::desktop::connect::NoopKeyPrompt`) that cancels.
    pub connect_prompt: Option<Arc<dyn crate::desktop::connect::SecureKeyPrompt>>,
    /// CLI `--system-prompt <prompt>` / `--system-prompt-file <file>`: override
    /// the assembled system prompt for the session. When `Some`, replaces the
    /// default memory-hierarchy prompt entirely (`OrchestratorConfig.system_prompt_override`).
    /// `None` (the default) keeps the assembled LINGXI.md hierarchy prompt
    /// (byte-identical to before this field was added).
    pub system_prompt_override: Option<String>,
    /// CLI `--append-system-prompt <prompt>` / `--append-system-prompt-file <file>`:
    /// text to append to the assembled system prompt for the session. When `Some`,
    /// appended after the memory-hierarchy prompt (or after `system_prompt_override`
    /// when both are set). `None` (the default) keeps the assembled prompt unchanged.
    pub append_system_prompt: Option<String>,
    /// CLI `--session-id <uuid>`: use this specific session ID for the
    /// conversation instead of minting a fresh one. `Some` ⟶ the boot-canonical
    /// MAIN session id is parsed from this string (a bare UUID, validated by the
    /// host before it fills this); `None` (the default) ⟶ a fresh random id.
    /// claude-code `--session-id`. The host (`apps/cli` / `apps/bridge-server`)
    /// validates UUID-ness + the cross-flag rules before setting this.
    pub session_id_override: Option<String>,
    /// Construction-only writer claim acquired by the host. When present the
    /// engine consumes this exact Arc instead of opening a second OS lock;
    /// None keeps standalone desktop/test hosts on the local claim path.
    pub session_writer_lease: Option<lingxi_core::host::live_sessions::SharedSessionWriterLease>,
    /// Source session id for a forked transcript. When present it is appended
    /// to Anthropic's JSON-string `metadata.user_id` as `parent_session_id`.
    /// Ordinary fresh/resumed sessions leave this unset.
    pub parent_session_id: Option<String>,
    /// CLI `--disable-slash-commands` (claude-code "Disable all skills"). When
    /// `true`, the shared command registry is emptied AFTER all builtin + plugin
    /// + skill registration, so the slash dispatcher and the `Skill` tool's
    /// loader (which share the registry `Arc`) observe zero commands/skills.
    /// `false` (the default) keeps the full command set.
    pub disable_slash_commands: bool,
    /// Optional host-owned names the model may load through `Skill`. `None`
    /// offers all eligible skills; an empty list offers none. Also inherited by
    /// subagents through the shared session command catalog.
    pub session_skill_allowlist: Option<Vec<String>>,
    /// CLI `--add-dir <directories...>` (claude-code "Additional directories to
    /// allow tool access to"). Unioned into the permission policy's
    /// working-directory set exactly like a settings-tier
    /// `permissions.additionalDirectories` entry, so file tools (Read/Edit/Bash)
    /// may operate outside `cwd`. Empty (the default) ⟶ no extra dirs.
    pub add_dir: Vec<std::path::PathBuf>,
    /// CLI `--mcp-config <configs...>` servers (claude-code: "Load MCP servers
    /// from JSON files or strings"). The host parses each file/inline-JSON entry
    /// into server configs; `build()` merges them OVER the discovered
    /// project/global servers (CLI wins on name collision). With
    /// `--strict-mcp-config` the host nulls the discovered paths, so these are
    /// the ONLY servers. Empty (the default) ⟶ none.
    pub cli_mcp_servers: Vec<mcp::McpServerConfig>,
    /// CLI `--strict-mcp-config` ("Only use MCP servers from --mcp-config,
    /// ignoring all other MCP configurations"). The host already nulls the
    /// discovered `.mcp.json` paths when set; the flag itself is threaded here
    /// because the agent-frontmatter MCP merge (claude `FWt`, cc2.1.220) skips
    /// frontmatter servers under strict mode UNLESS the agent came from the
    /// `--agents` flag (`r?.strictMcpConfig && t.source !== "flagSettings"`).
    /// `false` (the default) ⟶ no strict gating.
    pub strict_mcp_config: bool,
    /// Claude Code 2.1.251 restricted-session bit. Separate from permission
    /// mode: it narrows settings sources and protected-write handling without
    /// changing the selected mode.
    pub restricted: bool,
    /// Explicit `--tools` names carried by a restricted session.
    pub restricted_tools: Option<Vec<String>>,
    /// CLI `--exclude-dynamic-system-prompt-sections`. Threaded into
    /// `OrchestratorConfig::exclude_dynamic_system_prompt_sections`: moves the
    /// per-machine env block out of the (cacheable) system prompt and into the
    /// first user message. `false` (the default) ⟶ unchanged.
    pub exclude_dynamic_system_prompt_sections: bool,
    /// CLI `--setting-sources <user,project,local>` scope as `(include_user,
    /// include_project)`. Gates which on-disk settings TIERS `build()` reads for
    /// hook registration and permission rules (defaultMode / allow-deny rules /
    /// additionalDirectories): skip the user tier when `!include_user`, skip the
    /// project + local tiers when `!include_project`. This mirrors the
    /// `Settings::load_scoped` gating the CLI already applies to provider /
    /// routing / lingxiMdExcludes loaders, so `--setting-sources project` no
    /// longer loads user-level hooks or permission rules. `(true, true)` (the
    /// default, also the absent-flag case) ⟶ all tiers load, byte-identical to
    /// before this field.
    pub setting_source_scope: (bool, bool),
    /// CLI `--safe-mode` / `--bare` customization gates (M3, cc 2.1.198).
    /// Consumed at each registration site in `build()` (hooks / agents /
    /// plugins / custom commands + skills); the CLI also gates the
    /// memory-provider + discovered-MCP-path config fields it resolves itself.
    /// `CustomizationGates::default()` (both false) ⟶ byte-identical to before
    /// this field.
    pub customization_gates: CustomizationGates,
    /// CLI `--no-session-persistence` (print-mode only; the CLI validates the
    /// cross-flag rule): `false` ⟶ `build()` wires NO session `JsonlWriter`, so
    /// nothing is saved under `projects/` and the session cannot be resumed
    /// (claude-code "Disable session persistence - sessions will not be saved
    /// to disk and cannot be resumed"). `true` (the default) ⟶ unchanged.
    pub session_persistence: bool,
    /// (M4 cc2.1.198) CLI `--agents <json>` raw payload ("JSON object defining
    /// custom agents"). `build()` parses it with the strict flag-record schema
    /// (`agent::parse_agents_from_flag_json`, the `QXt` port) and merges the
    /// result into the agent catalog with `flagSettings` precedence — flag
    /// agents OVERRIDE same-named user/project dir agents (binary `XXt`
    /// tier order `[built-in, plugin, userSettings, projectSettings,
    /// flagSettings, policySettings]`, later wins). Ignored (warn) in safe
    /// mode; SURVIVES bare (`Hc("agents",{explicitlyRequested:!0})`
    /// @223080769). `None` (the default) ⟶ unchanged.
    pub cli_agents_json: Option<String>,
    /// (M4 cc2.1.198) CLI `--agent <agent>` ("Agent for the current session.
    /// Overrides the 'agent' setting."). `build()` resolves it against the
    /// final catalog with the `dts` lookup (exact `agentType`, else FQN
    /// `…:{name}` suffix) and logs the binary's `Warning: agent "X" not
    /// found …` line when absent. (P2-02 cc2.1.207) On a HIT it APPLIES the
    /// agent to the MAIN thread (`bde`/`mainThreadAgentDefinition`): agentType,
    /// system prompt (`nre`), `tools:`/`disallowedTools` pool filter (`HJ`), and
    /// `model` override (`jb(Zo(model))`, unless `--model` was given). RESIDUAL:
    /// frontmatter `hooks`/`mcpServers` swap + resume restoration (`rVe`).
    pub cli_agent: Option<String>,
    /// (M4 cc2.1.198) CLI `--plugin-dir <path>` entries ("Load a plugin from a
    /// directory or .zip for this session only", repeatable). Each entry feeds
    /// the plugin bootstrap AFTER the marketplace-installed discovery, like the
    /// binary's inline-plugin load (`EBm`): a missing path warns
    /// (`Plugin path does not exist: … , skipping`) without failing boot; a
    /// `.zip` is extracted to a temp dir (wrapper-dir detection like `Yor`)
    /// before the normal dir load. Empty (the default) ⟶ none.
    pub cli_plugin_dirs: Vec<std::path::PathBuf>,
    /// (M4 cc2.1.198) CLI `--effort <level>` — the session's initial effort
    /// level, already validated/normalized by the CLI (`u4i` argParser port:
    /// trim+lowercase, `med`→`medium`, must be one of low/medium/high/xhigh/
    /// max; an invalid value warned on stderr and arrives here as `None`).
    /// `build()` threads it to the main-loop `ProviderApiAdapter` so every
    /// main-session request carries `output_config.effort` (+ the
    /// `effort-2025-11-24` beta the service adds when the body has effort).
    /// `None` (the default) ⟶ requests unchanged (no effort field).
    pub initial_effort: Option<String>,
    /// `true` when [`Self::default_model`] was pinned by the `ANTHROPIC_MODEL`
    /// env var (claude-code D4 `process.env.ANTHROPIC_MODEL`) rather than by the
    /// built-in default or the persisted `settings.model`. Kept SEPARATE from
    /// [`Self::default_model_explicit`] (which stays `--model`-only, matching the
    /// binary's `userSpecifiedModel` = the `--model` flag) so the `--agent`
    /// model-override gate is unaffected; it ONLY exempts an env-pinned model
    /// from the boot connected-provider fallback (the user pinned exactly that
    /// model via env, so a reroute would defeat the pin). `false` (the default).
    pub default_model_env_pinned: bool,
    /// Boot SESSION thinking configuration, resolved host-side from the
    /// `MAX_THINKING_TOKENS` env var + the `--max-thinking-tokens` flag +
    /// the `alwaysThinkingEnabled` setting (claude-code `qIe()` + the `wn`
    /// request-build arm; see `llm_runtime::model::thinking::
    /// session_thinking_from_env`). Applied to BOTH the main-loop `ApiService`
    /// (`.with_thinking`) and the compaction/side-query `ForkedAgentRunner`
    /// (`.with_session_thinking`), so the summarizer inherits the same intent.
    /// The env read is host-side (F2-01: `build()` must not read env), so this
    /// carries the already-resolved config. Defaults to
    /// [`ThinkingConfig::Adaptive`] — byte-identical to the pre-resolver boot.
    pub session_thinking: llm_runtime::model::thinking::ThinkingConfig,
    /// CLI `-w`/`--worktree [name]` (worktree-tmux-launch plan, Task 3):
    /// create + enter a git worktree at boot. `None` (the default, and the
    /// only value every host but `apps/cli` currently supplies) ⟶ INERT — no
    /// worktree is created, `BuiltinToolContext.worktree_session` stays
    /// `None`, and boot is byte-identical to before this field existed.
    /// `Some("")` (a bare `-w`, `argv.worktree`'s empty-string sentinel) ⟶
    /// `build()` mints a random slug via the same
    /// [`tool_worktree::worktree::gen_random_slug`] helper `EnterWorktree`
    /// uses for a name-less create. `Some(name)` ⟶ that name is the slug.
    /// `--tmux` (`WorktreeSession.tmux_session_name`) is threaded separately
    /// via [`Self::tmux_launch`] (Task 4).
    pub worktree_launch: Option<String>,
    /// CLI `--tmux[=mode]` (worktree-tmux-launch plan, Task 4): create a
    /// detached tmux session (`tmux new-session -d -s <name> -c <path>`) for
    /// the worktree `worktree_launch` creates, recording the session name
    /// into `WorktreeSession.tmux_session_name`. `None` (the default, and the
    /// only value every host but `apps/cli` currently supplies, and every
    /// `apps/cli` session that omits `--tmux`) ⟶ INERT — no tmux session is
    /// created, `tmux_session_name` stays `None`, boot is byte-identical to
    /// before this field existed. `Some(mode)` ⟶ `apply_worktree_launch`
    /// creates the session AFTER the worktree itself is created+swapped+
    /// recorded; a tmux failure is logged and does NOT fail boot (the
    /// worktree launch itself already succeeded). `--tmux` requires
    /// `worktree_launch.is_some()` — `Some` here with `worktree_launch ==
    /// None` is a hard boot failure ([`BuildError::TmuxRequiresWorktree`]),
    /// mirroring the 206 constraint "Create a tmux session for the worktree
    /// (requires --worktree)".
    pub tmux_launch: Option<String>,
    /// 2.1.212 `/fork` (`vAd`) background-session forker seam. When `Some`,
    /// `build()` wires it onto the orchestrator via `with_bg_session_forker`, so
    /// `OrchestratorHandle::fork_to_background_session` copies the live
    /// conversation into a new background session (the `--bg`/daemon session-copy
    /// path). `None` (the default, and every host but `apps/cli`) ⟶ that `/fork`
    /// variant fails with a clear `ActionFailed` — INERT boot. The concrete impl
    /// lives in `apps/cli` (which owns the daemon dispatch machinery); injecting
    /// it here keeps the leaf `orchestrator` crate off an `apps/cli` dependency.
    pub bg_session_forker: Option<Arc<dyn lingxi_core::host::bg_session_forker::BgSessionForker>>,
    /// Optional per-runtime TUI AskUserQuestion bridge sender. Interactive TUI
    /// hosts fill this so questionnaire tools open the mounted bottom-pane
    /// view; non-TUI hosts leave it `None`.
    pub ask_user_question_tx:
        Option<tokio::sync::mpsc::Sender<tool_api::ask_user_question::AskUserQuestionExchange>>,
    /// Optional per-runtime TUI `computer` tool `request_access` bridge
    /// sender. Interactive TUI hosts fill this so the approval dialog opens
    /// in the mounted bottom-pane view; non-TUI hosts (and hosts without a
    /// computer-control backend) leave it `None`, which keeps
    /// `request_access` on the fail-closed `DenyAllResolver` default.
    pub computer_access_tx:
        Option<tokio::sync::mpsc::Sender<permission::computer_access::ComputerAccessExchange>>,
    /// Native Computer profiles accepted against a real provider and desktop.
    /// The host supplies evidence; an empty list uses the ordinary `computer` tool.
    pub verified_computer_profiles: Vec<orchestrator::native_computer::VerifiedComputerProfile>,
    /// Optional connection-scoped observer for real subagent lifecycle and
    /// message events. The bridge supplies this after it creates its outbound
    /// event sink; CLI/TUI hosts leave it unset so their behavior is unchanged.
    pub session_agent_observer:
        Option<Arc<dyn lingxi_core::host::subagent_spawn::SubagentSpawnObserver>>,
    /// Optional app-scoped device-audio service (capture / live recognition /
    /// synthesis / playback).
    ///
    /// `Some` only on the bridge path: `bridge_server::boot::assemble` builds an
    /// `AudioBridge` over the connection it is assembling and fills this. The
    /// service's live support snapshot controls the model-facing audio action
    /// schemas. Every other desktop host (CLI, TUI, offline factories) leaves it
    /// `None`, so audio tools are absent.
    pub audio: Option<DesktopAudio>,
}

/// Desktop session composition, independent from the permission-gate transport.
///
/// This is the session-mode signal the orchestrator uses for prompt/session
/// semantics such as "interactive CLI" vs "headless/SDK". It is intentionally
/// derived from the host composition shape, not from an arbitrary injected gate
/// alone: a transport host can bind a live permission surface while still
/// needing non-interactive session semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopSessionComposition {
    /// CLI TUI or interactive stdio REPL.
    InteractiveCli,
    /// Headless CLI execution (`--print` and other non-interactive CLI boots).
    HeadlessCli,
    /// Bridge/SDK transport session semantics.
    Transport,
}

impl DesktopSessionComposition {
    /// Whether this composition should expose interactive CLI prompt/request
    /// semantics to the orchestrator.
    #[must_use]
    pub fn is_interactive_session(self) -> bool {
        matches!(self, Self::InteractiveCli)
    }

    /// Whether the host can resolve permission prompts, including over the
    /// bridge transport without adopting interactive CLI session semantics.
    #[must_use]
    pub fn supports_interactive_permissions(self) -> bool {
        matches!(self, Self::InteractiveCli | Self::Transport)
    }

    /// Claude Code 2.1.245 main-query identity: `(querySource, print)`.
    #[must_use]
    pub fn query_source_and_print(
        self,
        output_style: Option<&str>,
        print_mode: bool,
    ) -> (String, bool) {
        match self {
            Self::Transport => (QUERY_SOURCE_SDK.to_string(), false),
            Self::InteractiveCli | Self::HeadlessCli => {
                let query_source =
                    match output_style.filter(|style| !style.is_empty() && *style != "default") {
                        Some("Concise") => {
                            format!("{QUERY_SOURCE_REPL_MAIN_THREAD}:outputStyle:Concise")
                        }
                        Some("Proactive") => {
                            format!("{QUERY_SOURCE_REPL_MAIN_THREAD}:outputStyle:Proactive")
                        }
                        Some("Explanatory") => {
                            format!("{QUERY_SOURCE_REPL_MAIN_THREAD}:outputStyle:Explanatory")
                        }
                        Some("Learning") => {
                            format!("{QUERY_SOURCE_REPL_MAIN_THREAD}:outputStyle:Learning")
                        }
                        Some(_) => format!("{QUERY_SOURCE_REPL_MAIN_THREAD}:outputStyle:custom"),
                        None => QUERY_SOURCE_REPL_MAIN_THREAD.to_string(),
                    };
                (
                    query_source,
                    matches!(self, Self::HeadlessCli) && print_mode,
                )
            }
        }
    }
}

/// `--safe-mode` / `--bare` reduced-mode customization gates (M3, cc 2.1.198).
///
/// Port of the binary's `Hc(feature, opts)` check (@209090235-ish minified:
/// `function Hc(e,t){if(Ql()&&!K5d[e])return!0;if(xd()&&!t?.explicitlyRequested)
/// return V5d[e];return!1}` with the two verdict maps
/// `V5d`(bare)/`K5d`(safe-allowlist) @209090400), where `Ql()` = env
/// `CLAUDE_CODE_SAFE_MODE` truthy OR argv `--safe-mode`, and `xd()` = env
/// `CLAUDE_CODE_SIMPLE` truthy OR argv `--bare`. The per-feature helpers below
/// bake in the map entries for exactly the features this composition root
/// registers; each cites its map values.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CustomizationGates {
    /// `--safe-mode` (or env): "Start with all customizations (CLAUDE.md,
    /// skills, plugins, hooks, MCP servers, custom commands and agents, …)
    /// disabled — useful for troubleshooting a broken configuration."
    pub safe_mode: bool,
    /// `--bare` (or env): "Minimal mode: skip hooks, LSP, plugin sync, …,
    /// and CLAUDE.md auto-discovery."
    pub bare: bool,
}

impl CustomizationGates {
    /// Settings-file hooks. Binary: bare disables outright (`V5d.hooks:!0`);
    /// safe mode passes `Hc` (`K5d.hooks:!0`) but the hooks-config merge
    /// collapses to the POLICY tier only (`UQr()` @209090550:
    /// `if(e?.allowManagedHooksOnly===!0||Ql())return e?.hooks??{}`). lingxi
    /// loads no policySettings hook tier (user + project only), so the safe-mode
    /// "policy hooks still run" residue is the empty set here — both modes skip
    /// the settings-hook loop.
    #[must_use]
    pub fn disables_settings_hooks(&self) -> bool {
        self.safe_mode || self.bare
    }

    /// Plugin discovery + materialisation (`V5d.plugins:!0`,
    /// `K5d.plugins:!1`; safe-mode log @211049652 "Skipping plugin hooks -
    /// safe mode disables plugins"). Skipping the plugin bootstrap also skips
    /// plugin LSP servers — lingxi's only LSP-server source — matching
    /// `Hc("lspServers")` gating `initializeLspServerManager` (@213275452;
    /// `V5d.lspServers:!0`, `K5d.lspServers:!1`).
    #[must_use]
    pub fn disables_plugins(&self) -> bool {
        self.safe_mode || self.bare
    }

    /// Skill + custom-command dir discovery (`V5d.skills:!0`,
    /// `K5d.skills:!1`; the user commands-dir loader `cWa` @213449557 bails on
    /// `xd()||Hc("skills")`). RESIDUAL: in bare mode the binary still loads
    /// skills from `--add-dir` roots (`aGe` @213453497 `if(xd())return …
    /// o.map(S=>jht(join(S,".claude","skills")…))`) so `/skill-name` keeps
    /// resolving; lingxi's registry loader has no add-dir root wiring yet, so
    /// bare loads none (seam: `desktop_command_registry`'s skill-roots arg).
    #[must_use]
    pub fn disables_skills(&self) -> bool {
        self.safe_mode || self.bare
    }

    /// Custom agent definitions from `agents/` dirs (`V5d.agents:!0`,
    /// `K5d.agents:!1`). In the binary a `--agents` FLAG payload is an
    /// explicit request that survives bare (`Hc("agents",{explicitlyRequested:
    /// !0})` @223080769) but not safe mode ("--agents: ignored in safe mode");
    /// lingxi's `--agents` flag is still parse-and-carry, so only the dir scan
    /// is gated here.
    #[must_use]
    pub fn disables_custom_agents(&self) -> bool {
        self.safe_mode || self.bare
    }

    /// Ambient (project/user `.mcp.json`) MCP discovery. SAFE MODE ONLY:
    /// `fQ` @212967619 `if(Hc("mcpAutoDiscovered"))return{servers:L2(),…}` —
    /// flag-supplied (`--mcp-config`) servers survive; `K5d.mcpAutoDiscovered:
    /// !1` but `V5d.mcpAutoDiscovered:!1` too, i.e. bare does NOT disable
    /// ambient MCP (its help text never lists MCP among the skips).
    #[must_use]
    pub fn disables_mcp_discovery(&self) -> bool {
        self.safe_mode
    }

    /// CLAUDE/LINGXI.md memory hierarchy (`V5d.claudeMd:!0`, `K5d.claudeMd:
    /// !1`). `explicitly_requested` mirrors `eue()` @209090235's
    /// `{explicitlyRequested:cI().length>0}` — `cI()` is the `--add-dir` list
    /// (`additionalDirectoriesForClaudeMd` @205673399) — so bare keeps the
    /// hierarchy when `--add-dir` supplies CLAUDE.md dirs; safe mode never does
    /// (and additionally exports `CLAUDE_CODE_DISABLE_CLAUDE_MDS=1`
    /// @223917313).
    #[must_use]
    pub fn disables_claude_md(&self, explicitly_requested: bool) -> bool {
        if self.safe_mode {
            return true;
        }
        self.bare && !explicitly_requested
    }
}

impl std::fmt::Debug for DesktopConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Secret-, prompt-, and executable-config-bearing fields are rendered
        // only as presence/count markers so `{cfg:?}` remains safe for host
        // diagnostics. Trait objects use the same presence-only convention.
        f.debug_struct("DesktopConfig")
            .field("composition", &self.composition)
            .field("defer_session_start", &self.defer_session_start)
            .field("build_info", &self.build_info)
            .field(
                "enable_automation_scheduler",
                &self.enable_automation_scheduler,
            )
            .field("host_workspace_trusted", &self.host_workspace_trusted)
            .field("mod_render_surface", &self.mod_render_surface)
            .field(
                "api_base",
                &if self.api_base.is_empty() {
                    "<empty>"
                } else {
                    "<configured>"
                },
            )
            .field(
                "api_key",
                &if self.api_key.is_empty() {
                    "<empty>"
                } else {
                    "<redacted>"
                },
            )
            .field(
                "api_key_helper",
                &self.api_key_helper.as_ref().map(|_| "<redacted>"),
            )
            .field("credential_storage_policy", &self.credential_storage_policy)
            .field(
                "injected_plugin_secret_count",
                &self
                    .injected_plugin_secrets
                    .values()
                    .map(std::collections::BTreeMap::len)
                    .sum::<usize>(),
            )
            .field("cwd", &self.cwd)
            .field("lingxi_home", &self.lingxi_home)
            .field("default_model", &self.default_model)
            .field("fallback_model", &self.fallback_model)
            .field("flag_settings_configured", &self.flag_settings.is_some())
            .field(
                "provider_profile_count",
                &self
                    .provider_profiles
                    .as_ref()
                    .map(|profiles| profiles.len()),
            )
            .field("routing_configured", &self.routing.is_some())
            .field("mcp_paths", &self.mcp_paths)
            .field("use_noop_permission_gate", &self.use_noop_permission_gate)
            .field("deny_unresolved_ask", &self.deny_unresolved_ask)
            .field("is_tty", &self.is_tty)
            .field(
                "injected_permission_gate",
                if self.injected_permission_gate.is_some() {
                    &"Some(<gate>)"
                } else {
                    &"None"
                },
            )
            .field(
                "session_started_as_coordinator",
                &self.session_started_as_coordinator,
            )
            .field(
                "memory_provider",
                if self.memory_provider.is_some() {
                    &"Some(<provider>)"
                } else {
                    &"None"
                },
            )
            .field("permission_mode", &self.permission_mode)
            .field("permission_mode_cli", &self.permission_mode_cli)
            .field(
                "permission_mode_cli_explicit",
                &self.permission_mode_cli_explicit,
            )
            .field(
                "connect_prompt",
                if self.connect_prompt.is_some() {
                    &"Some(<prompt>)"
                } else {
                    &"None"
                },
            )
            .field(
                "system_prompt_override_configured",
                &self.system_prompt_override.is_some(),
            )
            .field(
                "append_system_prompt_configured",
                &self.append_system_prompt.is_some(),
            )
            .field("session_id_override", &self.session_id_override)
            .field(
                "session_writer_lease",
                &self.session_writer_lease.as_ref().map(|_| "claimed"),
            )
            .field("parent_session_id", &self.parent_session_id)
            .field("disable_slash_commands", &self.disable_slash_commands)
            .field(
                "ask_user_question_tx",
                &self.ask_user_question_tx.as_ref().map(|_| "<configured>"),
            )
            .field(
                "computer_access_tx",
                &self.computer_access_tx.as_ref().map(|_| "<configured>"),
            )
            .field("verified_computer_profiles", &self.verified_computer_profiles)
            .field(
                "session_agent_observer",
                &self.session_agent_observer.as_ref().map(|_| "<configured>"),
            )
            .field("audio", &self.audio.as_ref().map(|_| "<configured>"))
            .field("add_dir", &self.add_dir)
            .field("cli_mcp_server_count", &self.cli_mcp_servers.len())
            .field("strict_mcp_config", &self.strict_mcp_config)
            .field("restricted", &self.restricted)
            .field(
                "restricted_tools",
                &self.restricted_tools.as_ref().map(Vec::len),
            )
            .field(
                "exclude_dynamic_system_prompt_sections",
                &self.exclude_dynamic_system_prompt_sections,
            )
            .field("customization_gates", &self.customization_gates)
            .field("session_persistence", &self.session_persistence)
            .field(
                "cli_agents_json_configured",
                &self.cli_agents_json.is_some(),
            )
            .field("cli_agent", &self.cli_agent)
            .field("cli_plugin_dirs", &self.cli_plugin_dirs)
            .field("initial_effort", &self.initial_effort)
            .field("default_model_env_pinned", &self.default_model_env_pinned)
            .field("session_thinking", &self.session_thinking)
            .field(
                "bg_session_forker",
                if self.bg_session_forker.is_some() {
                    &"Some(<forker>)"
                } else {
                    &"None"
                },
            )
            .finish()
    }
}

impl Default for DesktopConfig {
    fn default() -> Self {
        Self {
            build_info: command_api::builtins::BuildInfo::default(),
            enable_automation_scheduler: true,
            composition: None,
            defer_session_start: false,
            host_workspace_trusted: None,
            mod_render_surface: None,
            api_base: "https://api.anthropic.com".to_string(),
            api_key: String::new(),
            api_key_source: llm_runtime::CredentialSource::Configured,
            // Production reads the real keychain; only isolated hosts opt out.
            isolated_credential_storage: false,
            credential_storage_policy: CredentialStoragePolicy::NativePreferred,
            injected_plugin_secrets: std::collections::BTreeMap::new(),
            api_key_helper: None,
            // (M13) Default: no managed OAuth forcing, no FD-inherited key —
            // hosts that resolve either fill them in.
            managed_oauth_only: false,
            anthropic_key_fd_present: false,
            cwd: std::path::PathBuf::from("."),
            lingxi_home: std::path::PathBuf::new(),
            default_model: DesktopEngineConfig::default().default_model,
            default_model_explicit: false,
            recent_models: Vec::new(),
            fallback_model: None,
            custom_betas: Vec::new(),
            flag_settings: None,
            provider_profiles: None,
            routing: None,
            mcp_paths: Vec::new(),
            use_noop_permission_gate: true,
            deny_unresolved_ask: false,
            is_tty: false,
            max_turns: None,
            plan_mode_instructions: None,
            plans_directory: None,
            max_budget_usd: None,
            json_schema: None,
            injected_permission_gate: None,
            session_started_as_coordinator: false,
            initial_teammate_team_name: None,
            memory_provider: None,
            permission_mode: permission::PermissionMode::Auto,
            permission_mode_cli: None,
            permission_mode_preference: None,
            permission_mode_cli_explicit: false,
            allow_dangerously_skip_permissions: false,
            connect_prompt: None,
            system_prompt_override: None,
            append_system_prompt: None,
            session_id_override: None,
            session_writer_lease: None,
            parent_session_id: None,
            disable_slash_commands: false,
            session_skill_allowlist: None,
            add_dir: Vec::new(),
            cli_mcp_servers: Vec::new(),
            // Default: no `--strict-mcp-config` (ambient MCP configs load).
            strict_mcp_config: false,
            restricted: false,
            restricted_tools: None,
            exclude_dynamic_system_prompt_sections: false,
            // Default: all setting tiers load (absent `--setting-sources`).
            setting_source_scope: (true, true),
            // Default: no reduced mode (neither --safe-mode nor --bare).
            customization_gates: CustomizationGates::default(),
            // Default: persist the session JSONL (absent --no-session-persistence).
            session_persistence: true,
            // (M4 cc2.1.198) Defaults: no --agents payload, no --agent
            // selection, no --plugin-dir entries, no --effort level.
            cli_agents_json: None,
            cli_agent: None,
            cli_plugin_dirs: Vec::new(),
            initial_effort: None,
            // Default: no ANTHROPIC_MODEL env pin; the adaptive-thinking default.
            default_model_env_pinned: false,
            session_thinking: llm_runtime::model::thinking::ThinkingConfig::default(),
            // Default: no `-w`/`--worktree` flag ⟶ inert boot (no worktree).
            worktree_launch: None,
            // Default: no `--tmux` flag ⟶ inert boot (no tmux session).
            tmux_launch: None,
            // Default: no `/fork`-to-background forker ⟶ that /fork variant
            // fails with a clear ActionFailed until `apps/cli` injects one.
            bg_session_forker: None,
            ask_user_question_tx: None,
            computer_access_tx: None,
            verified_computer_profiles: Vec::new(),
            session_agent_observer: None,
            // Default: no device audio ⟶ the `voice`/`speech` tools are not
            // registered (only the bridge composition root wires an AudioBridge).
            audio: None,
        }
    }
}

impl DesktopConfig {
    /// Resolve the session composition for prompt/system-request semantics.
    ///
    /// The shared CLI base config starts headless and is promoted to
    /// [`DesktopSessionComposition::InteractiveCli`] only when the TUI / stdio
    /// REPL injects its interactive permission transport. The bridge/SDK path is
    /// modeled separately: it may surface remote permission prompts while still
    /// needing non-interactive session semantics.
    #[must_use]
    pub fn session_composition(&self) -> DesktopSessionComposition {
        if let Some(composition) = self.composition {
            return composition;
        }
        if !self.use_noop_permission_gate {
            return DesktopSessionComposition::Transport;
        }
        if self.deny_unresolved_ask {
            return DesktopSessionComposition::HeadlessCli;
        }
        if self.injected_permission_gate.is_some() {
            return DesktopSessionComposition::InteractiveCli;
        }
        DesktopSessionComposition::HeadlessCli
    }
}
