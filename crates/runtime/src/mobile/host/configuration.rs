use std::sync::Arc;

use super::MobileEngineError;

/// Deterministic, env/argv-free recipe for building a mobile runtime.
///
/// The mobile analog of [`harness_runtime::desktop::DesktopConfig`]: every value the host
/// would otherwise read from the process environment becomes an explicit field,
/// so the FFI entry point (and the off-device host test) can build an identical
/// runtime without touching `std::env`. The OS handles themselves arrive
/// separately, through the `Arc<dyn Platform>` passed to [`build_mobile`].
///
/// Mobile deliberately omits the desktop-only `mcp_paths` and
/// `use_noop_permission_gate` knobs: MCP discovery uses the app-private
/// settings path plus the active project's `.mcp.json`, and a mobile client
/// ALWAYS binds the connection-scoped
/// [`AdapterPermissionGate`] (a phone has no always-allow CLI mode).
// P0.2: `Clone` only — `Debug` is implemented manually below because the new
// `memory_provider` field (`Arc<dyn MemoryHierarchyProvider>`) is not `Debug`.
// Mirrors the `DesktopConfig` pattern (harness-runtime::desktop/src/lib.rs:799-846).
#[derive(Clone)]
pub struct MobileConfig {
    /// Host package identity used by `/version` (never part of the FFI DTO).
    pub build_info: command_api::builtins::BuildInfo,
    /// API base URL (default `https://api.anthropic.com`).
    pub api_base: String,
    /// Explicit startup Projects-session ingress URL, captured by the native
    /// host (for example from its `SESSION_INGRESS_URL` startup fact). This is
    /// not inferred from `api_base`, and `build_mobile` never reads the process
    /// environment. `None` is a known host input with no startup ingress URL.
    pub projects_session_startup_url: Option<String>,
    /// Anthropic API key. Empty string is valid — the orchestrator builds and
    /// only fails at `run_turn` with a 401, so slash-command dispatch still
    /// works with no key configured (mirrors the desktop config contract).
    pub api_key: String,
    /// Working directory the orchestrator + tool context are rooted at. On a
    /// device this is the app-sandbox container root.
    pub cwd: std::path::PathBuf,
    /// The `~/.claude`-equivalent root the settings / agents loaders walk. On a
    /// device this is inside the app sandbox.
    pub lingxi_home: std::path::PathBuf,
    /// Model id the build defaults to (`OrchestratorConfig.model`).
    pub default_model: String,
    /// Capability profile the mobile conversation runs under.
    pub session_mode: session::jsonl::SessionMode,
    /// Optional host-owned model-invocable skill grants for this session.
    /// `None` offers every eligible skill; an empty list offers none.
    pub session_skill_allowlist: Option<Vec<String>>,
    /// Whether the selected mobile workspace has passed the host trust flow.
    /// Defaults false so `/goal` and other hook-backed persistent behaviors fail
    /// closed until the Android/iOS host explicitly records trust.
    pub workspace_trusted: bool,
    /// The native host renders inline visualizations. Registers the
    /// `Visualization` tool and the `visualize` skill.
    pub inline_visualization: bool,
    /// Settings-declared `providers` block as raw JSON, fed verbatim to
    /// `llm_runtime::ClientConfig` via `build()`. `None` ⟶ built-in profiles only.
    pub provider_profiles: Option<std::collections::BTreeMap<String, serde_json::Value>>,
    /// Settings-declared `routing` block as raw JSON, fed verbatim to
    /// `llm_runtime::ClientConfig`. `None` ⟶ the default (empty) routing config.
    pub routing: Option<serde_json::Value>,
    /// Stable compatibility carrier for the mobile `Shell` tool gate + prompt
    /// metadata. Existing Android call sites still populate this field; iOS can
    /// reuse the same carrier type once its runtime bridge enables shell/git.
    pub android_shell: Option<tool_api::AndroidShellToolCtx>,
    /// Stable compatibility carrier for the mobile structured `Git` tool gate +
    /// workspace metadata. Existing Android call sites still populate this
    /// field; iOS can reuse the same carrier type once its runtime bridge
    /// enables git.
    pub android_git: Option<tool_api::AndroidGitToolCtx>,
    /// Mobile Git network secret (HTTPS token + CA dir, spec §G3, P4). Held
    /// separately from the public [`MobileConfig::android_git`] carrier so the
    /// token never enters the broadly-cloned public ctx. `tool-git-mobile`
    /// reads it at call time.
    pub android_git_secret: Option<tool_api::AndroidGitSecret>,
    /// P0.2 (mobile LINGXI.md hierarchy): the memory hierarchy provider the
    /// orchestrator loads its instruction files from. The production FFI entry
    /// points (`ios-framework` / `android-aar`) inject
    /// `Some(orchestrator::prompt::real_provider())` so the orchestrator loads
    /// the real `<cwd>/LINGXI.md` + `<lingxi_home>/LINGXI.md` hierarchy into the
    /// system prompt (claude-code parity) and the session-start
    /// `fire_instructions_loaded()` fires over those files. `None` (the default +
    /// every off-device host test) falls back to the empty
    /// [`StaticMemoryProvider`], so a default build loads NO memory and the host
    /// tests stay deterministic (they never touch the real filesystem). Mirrors
    /// `harness_runtime::desktop::DesktopConfig::memory_provider`.
    pub memory_provider: Option<Arc<dyn orchestrator::prompt::MemoryHierarchyProvider>>,
    /// Physical memory reported by the native host; zero is the conservative
    /// fallback.
    pub physical_memory_bytes: u64,
    /// Stable native host facts used to render the fixed mobile runtime
    /// reminder. `None` keeps desktop-style prompt assembly semantics for host
    /// tests and non-mobile embedder scenarios.
    pub host_environment: Option<lingxi_core::host::MobileHostEnvironment>,
    /// Whether non-vision primary models may delegate image analysis to an
    /// internal vision model. Defaults to `true` across mobile hosts.
    pub vision_delegation_enabled: bool,
}

impl std::fmt::Debug for MobileConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `dyn MemoryHierarchyProvider` is not `Debug`, so render the
        // `memory_provider` field as a Some(<provider>)/None presence marker.
        // Every other field is printed verbatim so `{cfg:?}` stays useful for
        // host logging (copies the `DesktopConfig` Debug pattern at
        // harness-runtime::desktop/src/lib.rs:799-846).
        f.debug_struct("MobileConfig")
            .field("build_info", &self.build_info)
            .field("api_base", &self.api_base)
            .field("api_key", &self.api_key)
            .field(
                "projects_session_startup_url",
                &self
                    .projects_session_startup_url
                    .as_ref()
                    .map(|_| "<provided>"),
            )
            .field("cwd", &self.cwd)
            .field("lingxi_home", &self.lingxi_home)
            .field("default_model", &self.default_model)
            .field("session_mode", &self.session_mode.as_str())
            .field("session_skill_allowlist", &self.session_skill_allowlist)
            .field("workspace_trusted", &self.workspace_trusted)
            .field("inline_visualization", &self.inline_visualization)
            .field("provider_profiles", &self.provider_profiles)
            .field("routing", &self.routing)
            .field("android_shell", &self.android_shell)
            .field("android_git", &self.android_git)
            .field("android_git_secret", &self.android_git_secret)
            .field(
                "memory_provider",
                if self.memory_provider.is_some() {
                    &"Some(<provider>)"
                } else {
                    &"None"
                },
            )
            .field("physical_memory_bytes", &self.physical_memory_bytes)
            .field("host_environment", &self.host_environment)
            .field("vision_delegation_enabled", &self.vision_delegation_enabled)
            .finish()
    }
}

impl Default for MobileConfig {
    fn default() -> Self {
        Self {
            build_info: command_api::builtins::BuildInfo::default(),
            api_base: "https://api.anthropic.com".to_string(),
            projects_session_startup_url: None,
            api_key: String::new(),
            cwd: std::path::PathBuf::from("."),
            lingxi_home: std::path::PathBuf::new(),
            default_model: crate::mobile::MobileEngineConfig::default().default_model,
            session_mode: session::jsonl::SessionMode::Code,
            session_skill_allowlist: None,
            workspace_trusted: false,
            inline_visualization: false,
            provider_profiles: None,
            routing: None,
            android_shell: None,
            android_git: None,
            android_git_secret: None,
            // P0.2: default to NO memory provider (empty, deterministic). The
            // production FFI entry points inject `Some(real_provider())`.
            memory_provider: None,
            physical_memory_bytes: 0,
            host_environment: None,
            vision_delegation_enabled: true,
        }
    }
}

impl MobileConfig {
    /// Expose the sandboxed Mobile Linux shell to the tool registry.
    ///
    /// Platform composition roots call this only when they also install a
    /// Mobile Linux runtime. The capability probe in [`build_mobile_engine`]
    /// remains authoritative and disables the carrier if that runtime cannot
    /// actually execute.
    pub fn enable_mobile_linux_shell(&mut self) {
        self.android_shell = Some(tool_api::MobileShellToolCtx::mobile_linux_guest(
            true,
            Vec::new(),
            None,
        ));
    }

    #[must_use]
    pub fn mobile_shell(&self) -> Option<&tool_api::MobileShellToolCtx> {
        self.android_shell.as_ref()
    }

    #[must_use]
    pub fn mobile_git(&self) -> Option<&tool_api::MobileGitToolCtx> {
        self.android_git.as_ref()
    }

    #[must_use]
    pub fn mobile_git_secret(&self) -> Option<&tool_api::MobileGitSecret> {
        self.android_git_secret.as_ref()
    }
}

/// Parse the non-secret provider configuration supplied by a mobile host.
///
/// Keeping JSON decoding in `harness-runtime::mobile` avoids making the Android/iOS
/// packager crates depend on `serde_json` on host builds. Secrets deliberately
/// travel through `SetProviderCredential` instead of either JSON document.
pub fn parse_mobile_provider_config_json(
    provider_profiles_json: &str,
    routing_json: Option<&str>,
) -> Result<
    (
        Option<std::collections::BTreeMap<String, serde_json::Value>>,
        Option<serde_json::Value>,
    ),
    MobileEngineError,
> {
    const MAX_CONFIG_BYTES: usize = 512 * 1024;
    if provider_profiles_json.len() > MAX_CONFIG_BYTES
        || routing_json.is_some_and(|value| value.len() > MAX_CONFIG_BYTES)
    {
        return Err(MobileEngineError::Internal(
            "invalid provider config: payload too large".to_string(),
        ));
    }

    let profiles = if provider_profiles_json.trim().is_empty() {
        None
    } else {
        Some(
            serde_json::from_str::<std::collections::BTreeMap<String, serde_json::Value>>(
                provider_profiles_json,
            )
            .map_err(|error| {
                MobileEngineError::Internal(format!("invalid provider profiles JSON: {error}"))
            })?,
        )
    };
    let routing = routing_json
        .filter(|value| !value.trim().is_empty())
        .map(serde_json::from_str)
        .transpose()
        .map_err(|error| {
            MobileEngineError::Internal(format!("invalid provider routing JSON: {error}"))
        })?;
    Ok((profiles, routing))
}

/// Errors surfaced while building a [`MobileRuntime`].
///
/// Mirrors `harness_runtime::desktop::BuildError`. Construction is effectively infallible
/// today (the orchestrator constructor cannot fail), but the typed error is kept
/// so a future real OAuth bootstrap can surface a cause without changing call
/// sites.
#[derive(Debug, thiserror::Error)]
pub enum MobileBuildError {
    /// api-client construction failed.
    #[error("api base resolution failed: {0}")]
    ApiBase(String),
    /// Orchestrator construction failed.
    #[error("orchestrator construction failed: {0}")]
    Orchestrator(String),
}
