//! `BuiltinToolContext` — the construction-time handle every builtin tool
//! takes in its `new()` constructor.
//!
//! Moved here from `tools/src/builtin/mod.rs` in M8-P5 so the per-category
//! tool crates (`tool-file`, `tool-shell`, …) can construct their tools
//! without depending on the `tools` monolith. The design §5.1 `ToolCtx`
//! reshape (individual `Arc<dyn Platform>` handles, no god-object) will
//! eventually slim this surface.

use crate::read_file_state::ReadFileStateMap;
use crate::sandbox_runner::SandboxRunner;
use crate::session_cwd::SessionCwd;
use crate::worktree_session::WorktreeSessionCell;
use lingxi_core::host::agent_name_registry::AgentNameRegistry;
use lingxi_core::host::audio::{AudioRecordingHandle, AudioService};
use lingxi_core::host::budget::BudgetEnforcerHandle;
use lingxi_core::host::calendar::CalendarProvider;
use lingxi_core::host::camera::CameraControl;
use lingxi_core::host::clipboard::Clipboard;
use lingxi_core::host::clock::Clock;
use lingxi_core::host::computer_control::ComputerControl;
use lingxi_core::host::contacts::ContactsProvider;
use lingxi_core::host::coordinator_mode::CoordinatorModeHandle;
use lingxi_core::host::deep_link::DeepLinkOpener;
use lingxi_core::host::device_status::DeviceStatusProvider;
use lingxi_core::host::filesystem::FileSystem;
use lingxi_core::host::haptics::HapticService;
use lingxi_core::host::http::HttpTransport;
use lingxi_core::host::location::LocationProvider;
use lingxi_core::host::mailbox::MailboxRouterHandle;
use lingxi_core::host::notification::NotificationService;
use lingxi_core::host::permission_gate::PermissionGate;
use lingxi_core::host::process::ProcessRunner;
use lingxi_core::host::sandbox::Sandbox;
use lingxi_core::host::share::SharingService;
use lingxi_core::host::subagent_spawn::SubagentSpawner;
use lingxi_core::host::task_registry::TaskRegistryHandle;
use lingxi_core::host::worktree::WorktreeManager;
use permission::PermissionMode;
use sandbox::runtime_config::{Platform, SandboxRuntimeConfig};
use std::path::PathBuf;
use std::sync::Arc;
use telemetry::AnalyticsBus;

/// A shared, live "current working directory" cell — the `getCwd()` /
/// `setCwdState` analog (claude-code's single session-global `Pt.cwd`).
///
/// The desktop `BashTool` is the single writer: a foreground `cd` commits the
/// post-`cd` directory here (via its `pwd -P` readback), and the file/search/LSP
/// tools READ it as their live cwd — matching claude-code, where Glob's default
/// dir, the Glob/Grep/Read "does not exist" cwd notes, and the LSP root all read
/// live `Ct()`. Injected via each tool's `with_live_cwd` builder rather than as a
/// [`BuiltinToolContext`] field: that struct is built by a full literal in the
/// FORBIDDEN `engine-mobile` code, so a new field would break it. Tools with no
/// cell injected (mobile, tests) fall back to [`BuiltinToolContext::workspace`]
/// — byte-identical to the pre-cell behavior.
pub type LiveCwdCell = Arc<std::sync::Mutex<PathBuf>>;

/// Fallback provider-profile resolver for the current main-loop model.
///
/// Some live sessions can carry a bare `session.model` while the resolved
/// provider survives only in engine-owned routing state. Tools that need
/// provider-aware behavior can consult this when
/// [`crate::context::ToolUseOptions::model_profile`] is absent.
pub type MainLoopModelProfileProvider = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Where a Bash edit-diff snapshot keeps its private git directories.
#[derive(Debug, Clone)]
pub struct BashEditDiffSetup {
    /// Root under which one shadow git directory is kept per repository.
    /// ⛔ Never a path inside the user's own `.git`.
    pub shadow_root: std::path::PathBuf,
}

/// Static surface every builtin tool needs at construction time.
///
/// Cloning is cheap — every field is `Arc` or a small owned vec.
#[derive(Clone)]
pub struct BuiltinToolContext {
    /// Sandboxed FS access (M1).
    pub fs: Arc<dyn FileSystem>,
    /// Telemetry bus (M3-06).
    pub bus: Arc<AnalyticsBus>,
    /// Process runner — backs BashTool/PowerShellTool/REPLTool (M4-02).
    pub process: Arc<dyn ProcessRunner>,
    /// Sandbox seam — provides the `prepare`/`bypass_with_audit` constructors
    /// that turn `ProcessCommand` into `SandboxedCommand` (M4-02).
    pub sandbox: Arc<dyn Sandbox>,
    /// Wall-clock — backs SleepTool + duration measurement (M4-02).
    pub clock: Arc<dyn Clock>,
    /// Sandbox policy runtime config — drives `wrap_with_sandbox` (M4-02).
    pub sandbox_runtime: SandboxRuntimeConfig,
    /// Async sandbox-wrap seam — the shell/skill tools call `wrap` through this
    /// handle instead of `sandbox::wrap::wrap_with_sandbox` directly, so the
    /// host can inject a live `sandbox-runtime`-backed runner. Defaults to
    /// [`crate::sandbox_runner::LegacyWrapRunner`] (byte-identical to today).
    pub sandbox_runner: Arc<dyn SandboxRunner>,
    /// Active permission mode (M4-02).
    pub permission_mode: PermissionMode,
    /// The live boot permission policy (rules + mode + roots + working-dirs +
    /// sandbox-auto-allow config). Shared behind an `Arc` with the orchestrator's
    /// `PolicyPermissionGate`. The prompt shell-expansion provider
    /// (`tool_skill::prompt_shell`) reads it as the BASE policy for embedded
    /// `!`cmd`` bodies: it builds a FRESH per-command effective policy = these
    /// base rules + that command's declared `allowed_tools` (claude-code's
    /// `alwaysAllowRules.command` injection) before calling
    /// [`permission::PermissionPolicy::authorize_with_mode`] on each command —
    /// 1:1 with `hasPermissionsToUseTool(BashTool, {command})`. Defaults to an
    /// empty [`permission::PermissionMode::Default`] policy at every test /
    /// non-live construction site (read-only git + auto-safe commands still
    /// auto-allow via the rule-independent read-only layer); the two live engine
    /// roots (`engine-desktop` + `engine-mobile`) clone their real boot
    /// `Arc<PermissionPolicy>` in here.
    pub permission_policy: Arc<permission::PermissionPolicy>,
    /// Whether the host has a working sandbox backend right now (M4-02).
    pub sandbox_available: bool,
    /// Switchable session cwd + trusted-directory set (worktree parity plan,
    /// Task 2). Replaces the former frozen `workspace: PathBuf` +
    /// `trusted_dirs: Vec<PathBuf>` fields — every tool now reads the current
    /// cwd/trusted set through [`Self::cwd`] / [`Self::trusted_dirs`] /
    /// [`Self::cwd_and_trusted`] instead of a frozen field, so a later
    /// `EnterWorktree`/`ExitWorktree` swap (not yet wired) is observed by
    /// every tool without re-plumbing. Until something calls
    /// `session_cwd.swap(..)`, `cwd()`/`trusted_dirs()` return exactly the
    /// boot values this was constructed with — byte-identical to the old
    /// frozen fields.
    pub session_cwd: Arc<SessionCwd>,
    /// Id of the session this tool call belongs to.
    ///
    /// Needed for session-scoped on-disk artifacts — claude-code persists
    /// oversized tool output under
    /// `<config>/projects/<sanitized-cwd>/<session-id>/tool-results/`
    /// (`qzg()`/`xke()`, 2.1.220 @230268971), a path that cannot be built from
    /// `session_cwd` alone. Without it the web/MCP persist sites fell back to
    /// `<cwd>/.lingxi/tool-results`, dropping artifacts inside the user's repo.
    ///
    /// `None` at construction sites that have no session (tests, and the
    /// mobile/git shims that never persist); those keep the workspace-local
    /// fallback. The production desktop context sets it.
    pub session_id: Option<lingxi_core::types::SessionId>,
    /// Shared record of the single active worktree the session entered via
    /// `EnterWorktree` (worktree 206 parity plan, Task 8). `None` when no
    /// worktree is active — the INERT default at every construction site.
    /// `EnterWorktreeTool` writes `Some(WorktreeSession { .. })` here on a
    /// successful create/enter (capturing the pre-swap cwd alongside the new
    /// worktree's path/branch/base-commit); `ExitWorktreeTool` reads it to
    /// restore [`crate::worktree_session::WorktreeSession::original_cwd`] and
    /// decide keep/remove, then clears it back to `None`. Shared behind an
    /// `Arc<Mutex<..>>` (not `ArcSwap`, unlike [`Self::session_cwd`]) because
    /// both tools need the WHOLE record, not a hot-path single-field read.
    pub worktree_session: WorktreeSessionCell,
    /// Detected platform — drives `wrap_with_sandbox` branch (M4-02).
    pub platform: Platform,
    /// HTTP transport for web tools (WebFetch + WebSearch) (M4-03). M1 trait;
    /// tests inject `MockHttpTransport`.
    pub http: Arc<dyn HttpTransport>,
    /// Session-bound hosted search; provider requests are owned by llm-client.
    pub hosted_search: Option<Arc<dyn crate::HostedWebSearchClient>>,
    /// Exact token counts using the active session provider.
    pub mcp_token_counter: Option<Arc<dyn crate::McpTokenCounter>>,
    /// Model used by `WebSearch` when calling `POST /v1/messages` (M4-03).
    /// Sourced from the session's `coordinator_model` at registration time.
    pub default_model: String,
    /// Runtime `/web` config loader for provider-agnostic client-side WebSearch.
    /// `None` preserves env-only fallback behavior.
    pub web_search_config: Option<Arc<dyn lingxi_core::host::WebSearchConfigProvider>>,
    /// Worktree manager (M2-01 trait) — backs `EnterWorktree` + `ExitWorktree`
    /// (M4-04). Tests inject `MockWorktreeManager`; production uses
    /// `platform_posix::PosixWorktreeManager`.
    pub worktree: Arc<dyn WorktreeManager>,

    // ===== M4-05 wiring (Phase 3) =====
    /// Subagent spawner — `AgentTool` dispatches recursive subagent runs
    /// through this seam. `None` when the host has not wired a state-
    /// machine pool yet; in that case `AgentTool::call` surfaces a clear
    /// internal error. Production wires `agent::PoolSubagentSpawner`.
    pub subagent_spawner: Option<Arc<dyn SubagentSpawner>>,
    /// Name → agent-id registry for spawned ASYNC subagents (claude
    /// `AppState.agentNameRegistry`, `AgentTool.tsx:704-711`). `AgentTool`
    /// registers `name → agentId` for a named async spawn so a later
    /// `SendMessage({ to: name })` resolves the running agent. `None` when no
    /// host registry is wired (sync-only paths never register). Distinct from
    /// the coordinator teammate roster.
    pub agent_name_registry: Option<Arc<dyn AgentNameRegistry>>,
    /// Task registry — the 6 `Task*` tools dispatch CRUD through this seam.
    /// Production wires `tasks::TaskRegistry`.
    pub task_registry: Option<Arc<dyn TaskRegistryHandle>>,
    /// Mailbox router — `SendMessageTool` dispatches teammate routing
    /// through this seam. Production wires `coordinator::MailboxRouter`.
    pub mailbox_router: Option<Arc<dyn MailboxRouterHandle>>,
    /// Budget enforcer — `AgentTool` gates spawn calls through this seam.
    /// Production wires `cost::BudgetEnforcer`.
    pub budget_enforcer: Option<Arc<dyn BudgetEnforcerHandle>>,
    /// Optional fallback provider-profile resolver for the current
    /// `main_loop_model`. Desktop wires the same model-routing map used by
    /// `/fusion`; tests and mobile leave this `None`.
    pub main_loop_model_profile_provider: Option<MainLoopModelProfileProvider>,
    /// Coordinator-mode seam — `AgentTool` consults this LIVE to gate the
    /// fork-subagent path (mutually exclusive with coordinator mode) and to
    /// select the slim coordinator tool prompt. `None` ⇒ not coordinator (the
    /// default). Production wires `coordinator::CoordinatorMode`.
    pub coordinator_mode: Option<Arc<dyn CoordinatorModeHandle>>,
    /// Permission gate (enforcement 3b) — `AgentTool` threads this into the
    /// `RegistryToolInvoker` it hands the spawner, so a spawned subagent's tool
    /// calls are gated by the SAME policy as the main loop (closing the bypass
    /// where the inherited invoker dispatched any tool unconditionally). `None`
    /// = no enforcement wired (the default; legacy always-dispatch behavior).
    /// Production wires the boot gate (`PolicyPermissionGate` when
    /// `LINGXI_ENFORCE_PERMISSIONS` is set, else the no-op gate).
    pub permission_gate: Option<Arc<dyn PermissionGate>>,

    /// Bash edit-diff setup (CLI-5), or `None` when the feature is off.
    ///
    /// PRESENCE is the gate: the composition root evaluates
    /// `tool_shell::bash_edit_diff::enabled` — which needs the settings TIERS,
    /// not just the merged value — and fills this only when the answer is yes.
    /// The tool therefore carries no settings plumbing, and a host that never
    /// sets it behaves exactly as before.
    pub bash_edit_diff: Option<Arc<BashEditDiffSetup>>,

    // ===== M4-07 wiring (Phase 7) =====
    /// MCP registry — the 4 MCP builtin tools (`MCPTool`, `McpAuthTool`,
    /// `ListMcpResourcesTool`, `ReadMcpResourceTool`) dispatch through this
    /// seam. `None` when the host has not wired an MCP layer; tools surface
    /// a "MCP registry not configured" error in that case. Production wires
    /// `mcp::McpRegistry` populated by the platform.
    pub mcp_registry: Option<Arc<::mcp::registry::McpRegistry>>,
    /// LSP registry — `LSPTool` dispatches through this seam. `None` when
    /// the host has not wired an LSP layer. Production wires
    /// `lsp::registry::LspRegistry` populated by plugin registration.
    pub lsp_registry: Option<Arc<::lsp::registry::LspRegistry>>,

    // ===== M8-P11 mobile / device-control capabilities =====
    /// Native camera — `tool-camera`'s `CameraTool` routes here. `None` on
    /// desktop; mobile composition roots wire `platform.camera()` (a Swift /
    /// Kotlin impl via UniFFI).
    pub camera: Option<Arc<dyn CameraControl>>,
    /// Unified per-device audio service. The audio tools, the app adapter,
    /// and Computer Use audio routes share this app-scoped service.
    pub audio: Option<Arc<dyn AudioService>>,
    /// Host-managed recording handles for public voice start/stop actions.
    /// Keyed by stable owner so a later call from another tool-use identity
    /// can stop only that owner's recording.
    pub audio_recording_handles: Arc<
        tokio::sync::Mutex<
            std::collections::HashMap<lingxi_core::host::audio::AudioOwner, AudioRecordingHandle>,
        >,
    >,
    /// Native share sheet — `tool-share`'s `ShareTool` routes here. `None` on
    /// desktop.
    pub share: Option<Arc<dyn SharingService>>,
    /// Native system notifications — `tool-notification`'s `NotificationTool`
    /// routes here. `None` on desktop; mobile wires `platform.notifications()`.
    pub notifications: Option<Arc<dyn NotificationService>>,
    /// Native system clipboard — `tool-clipboard`'s `ClipboardTool` routes
    /// here. `None` on desktop; mobile wires `platform.clipboard()`.
    pub clipboard: Option<Arc<dyn Clipboard>>,
    /// One-shot native location for the location tool.
    pub location: Option<Arc<dyn LocationProvider>>,
    /// Bounded battery and connectivity status for the device_status tool.
    pub device_status: Option<Arc<dyn DeviceStatusProvider>>,
    /// Native bounded feedback for the haptics tool.
    pub haptics: Option<Arc<dyn HapticService>>,
    /// Validated external URL launching for the open_url tool.
    pub deep_link: Option<Arc<dyn DeepLinkOpener>>,
    /// Read-only native calendar for bounded calendar queries.
    pub calendar: Option<Arc<dyn CalendarProvider>>,
    /// Read-only native contacts for bounded contacts searches.
    pub contacts: Option<Arc<dyn ContactsProvider>>,
    /// Screen-capture + input automation — the device-control tools
    /// (`computer`/`android_use`/`ios_use`) route here. `None` unless a
    /// desktop automation backend or a mobile UniFFI impl is wired.
    pub computer_control: Option<Arc<dyn ComputerControl>>,

    // ===== file-tools-remainder Batch B: read-state registry =====
    /// Per-path read-state map (`path → {content, mtime_ms, offset, limit}`).
    /// 1:1 port of claude-code's `readFileState`
    /// (`FileReadTool.ts:1032`): a successful `Read` records the file's
    /// content, floor-truncated mtime (ms), and the `offset`/`limit` it read
    /// with. The orchestrator shares ONE `Arc<Mutex<HashMap<…>>>` across the
    /// file tools it constructs so a write from one tool is visible to the
    /// others (and to the future staleness guards / Read dedup that will read
    /// this map). Cheap to clone (`Arc`).
    pub read_file_state: ReadFileStateMap,

    // ===== Read(deny) search-exclusion seam =====
    /// Ripgrep `--glob` exclude strings (WITHOUT the leading `!`) derived from
    /// the active `Read`-`deny` permission rules — 1:1 with claude-code
    /// `F4e(U4e(toolPermissionContext), cwd)`, used as a construction-time
    /// fallback when no live rule-evaluating permission gate is wired. The
    /// `Grep` and `Glob` tools turn
    /// each entry into a negated `ignore`-crate override so a denied/sensitive
    /// path never appears in search results (`GrepTool.ts:417-427`, `glob.ts`
    /// `lLa()`). Empty by default — every non-live construction site (tests,
    /// other tools) leaves it `vec![]`, so behavior is unchanged when no
    /// `Read`-deny rule applies. A live [`Self::permission_gate`] supersedes it
    /// on every search call so runtime `updatedPermissions` are observed.
    pub read_deny_exclude_globs: Vec<String>,

    // ===== Mobile Linux shell / git seams =====
    /// Stable compatibility carrier for the mobile `Shell` tool wiring. The
    /// type is now [`MobileShellToolCtx`]; the field name stays
    /// `android_shell` so existing Android call sites keep compiling while iOS
    /// can reuse the same carrier via the new generic type alias surface.
    pub android_shell: Option<MobileShellToolCtx>,

    /// Stable compatibility carrier for the mobile structured `Git` tool
    /// wiring. The type is now [`MobileGitToolCtx`]; the field name stays
    /// `android_git` so existing Android call sites keep compiling while iOS
    /// can reuse the same carrier via the generic type alias surface.
    pub android_git: Option<MobileGitToolCtx>,

    /// Mobile Git **secret** seam (spec §G3 auth). Carries the HTTPS token +
    /// CA directory used by the network ops (clone/fetch/pull). Held
    /// SEPARATELY from the public [`MobileGitToolCtx`] so the token never
    /// enters the broadly-cloned public carrier (which only exposes
    /// `has_token: bool`). `None` on desktop / iOS and whenever no token /
    /// CA dir is configured. Populated by `android-aar` (T10) and consumed by
    /// `tool-git-mobile`'s network ops. Never logged or persisted.
    pub android_git_secret: Option<MobileGitSecret>,

    // ===== V2 task lifecycle BLOCKING hooks (TaskCreate/TaskUpdate tool path) =====
    /// BLOCKING `TaskCreated` / `TaskCompleted` lifecycle-hook firer for the V2
    /// `Task*` tool path. `None` (the default) → the tool never fires these
    /// hooks (non-blocking); the desktop composition root wires
    /// `Some(orchestrator::OrchestratorTaskLifecycleHookFirer)` over the shared
    /// `Arc<hooks::HookExecutorImpl>`.
    ///
    /// This is a SEPARATE seam from the fire-and-forget
    /// `hooks::TaskCreatedFirer` / `hooks::TaskCompletedFirer` the `TaskRegistry`
    /// holds (which "MUST NOT propagate hook failures"). It reproduces
    /// claude-code's BLOCKING paths — `executeTaskCreatedHooks`
    /// (`TaskCreateTool.ts:93-113`, on a blocking error `deleteTask` + throw) and
    /// `executeTaskCompletedHooks` (`TaskUpdateTool.ts:232-265`, on a blocking
    /// error return `success:false` and DO NOT apply the status). `tool-api`
    /// deliberately does NOT depend on `hooks`, so the trait is defined here and
    /// the orchestrator implements it.
    pub task_lifecycle_hooks: Option<Arc<dyn TaskLifecycleHookFirer>>,
    /// Live session override for `sandbox_runtime.enabled` (the `/sandbox`
    /// toggle). `None` at every non-live construction site (frozen behavior).
    /// The desktop root wires a shared `Arc<AtomicBool>` here that is ALSO held
    /// by the TUI, so `/sandbox` flips sandboxing for the live session and the
    /// bash tool's next command observes it via [`Self::effective_sandbox_runtime`].
    pub sandbox_enabled_override: Option<Arc<std::sync::atomic::AtomicBool>>,

    /// Mirror of `settings.skipWebFetchPreflight` (CC 2.1.207 zod:
    /// `skipWebFetchPreflight:E.boolean().optional().describe("Skip the WebFetch
    /// blocklist check for enterprise environments with restrictive security
    /// policies")`). When `true`, `WebFetchTool` skips the domain-blocklist
    /// preflight entirely — the enterprise escape hatch for hosts whose network
    /// policy blocks outbound connections to `claude.ai`/`api.anthropic.com`
    /// (binary `if(!Mi().skipWebFetchPreflight)switch((await DSd(g)).status){…}`;
    /// leaked `utils.ts:423-424`). Populated at the two live composition roots
    /// (`engine-desktop` + `engine-mobile`) from the merged `settings.json`;
    /// `false` at every non-live / test construction site (frozen behavior).
    ///
    /// RESIDUAL: CC re-reads the live merged setting on every fetch (hot-reloads
    /// on a settings change); this bool is frozen at tool registration. A
    /// provider closure would be needed to close that minor divergence.
    pub skip_web_fetch_preflight: bool,

    /// Mirror of `settings.askUserQuestionTimeout` (CC 2.1.201 zod:
    /// `askUserQuestionTimeout:E.enum(["60s","5m","10m","never"]).catch(void 0)`,
    /// default `never`). The idle window before an unanswered `AskUserQuestion`
    /// prompt auto-continues with the answers selected so far. Threaded through
    /// to `tool_ui`'s `DefaultTimeoutResolver` at registration
    /// (`tool_ui::register_with_options` parses it via
    /// `AskUserQuestionTimeout::parse_or_default` — so an absent / unparsable
    /// value falls back to `never`, mirroring the zod `.catch`). Held as the raw
    /// settings string (like [`Self::skip_web_fetch_preflight`] mirrors its bool)
    /// because `tool-api` must NOT depend on `tool_ui` (the dependency runs the
    /// other way). Populated at the two live composition roots
    /// (`engine-desktop` + `engine-mobile`) from the merged `settings.json`;
    /// `None` at every non-live / test construction site (frozen `never`
    /// behavior).
    pub ask_user_question_timeout: Option<String>,
}

impl BuiltinToolContext {
    /// Directory for persisted oversized tool output.
    ///
    /// claude-code's `xke()` — `vas.join(qzg(), "tool-results")` where
    /// `qzg()` is `<projects>/<sessionId>` (2.1.220 @230268971) — i.e.
    /// `<config>/projects/<sanitized-cwd>/<session-id>/tool-results/`. Verified
    /// on disk against real transcripts.
    ///
    /// Falls back to the workspace-local `<cwd>/<DOT_DIR>/tool-results` only
    /// when [`Self::session_id`] is `None`. That is the inert case (tests, and
    /// the mobile shims that never persist); the live desktop context sets the
    /// id, so the fallback is not the production path. It is kept rather than
    /// inventing a placeholder session segment — but note it writes inside the
    /// user's workspace, which is exactly what the session-scoped path fixes.
    #[must_use]
    pub fn tool_results_dir(&self) -> PathBuf {
        let cwd = self.cwd();
        let Some(session_id) = self.session_id.as_ref() else {
            return cwd.join(branding::DOT_DIR).join("tool-results");
        };
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map_or_else(PathBuf::new, PathBuf::from);
        let lingxi_home = branding::config_home(&home, std::env::var_os(branding::CONFIG_DIR_ENV));
        session::jsonl::tool_results_dir(
            &lingxi_home,
            &cwd.to_string_lossy(),
            &session_id.as_uuid().to_string(),
        )
    }

    /// Current session cwd (worktree parity plan, Task 2). Reads through
    /// [`SessionCwd::cwd`] — byte-identical to the old frozen `workspace`
    /// field until a worktree tool calls `session_cwd.swap(..)`.
    #[must_use]
    pub fn cwd(&self) -> PathBuf {
        self.session_cwd.cwd()
    }

    /// Current trusted-directory set (worktree parity plan, Task 2). Reads
    /// through [`SessionCwd::trusted_dirs`] — byte-identical to the old
    /// frozen `trusted_dirs` field until a worktree tool calls
    /// `session_cwd.swap(..)`.
    #[must_use]
    pub fn trusted_dirs(&self) -> Vec<PathBuf> {
        self.session_cwd.trusted_dirs()
    }

    /// Current `(cwd, trusted_dirs)` pair from a SINGLE `load()` of the
    /// shared [`SessionCwd`] cell (worktree parity plan, Task 2). Prefer this
    /// over calling [`Self::cwd`] and [`Self::trusted_dirs`] back to back at
    /// any call site that needs BOTH in the same operation — it guarantees
    /// the pair comes from the same swap generation, never straddling a
    /// concurrent swap.
    #[must_use]
    pub fn cwd_and_trusted(&self) -> (PathBuf, Vec<PathBuf>) {
        self.session_cwd.snapshot()
    }

    /// Current `Read`-deny search exclusions rebased to `cwd`. A live policy
    /// gate wins; static contexts and tests retain the boot-time vector.
    #[must_use]
    pub fn effective_read_deny_exclude_globs(&self, cwd: &std::path::Path) -> Vec<String> {
        self.permission_gate
            .as_ref()
            .and_then(|gate| gate.read_deny_exclude_globs(cwd))
            .unwrap_or_else(|| self.read_deny_exclude_globs.clone())
    }

    /// Best-effort Edit/Write → LSP document synchronization.
    ///
    /// No registry or no matching plugin server is a normal inert case. Other
    /// failures are logged without changing the file tool's successful result.
    pub async fn sync_lsp_after_file_write(&self, path: &std::path::Path, text: &str) {
        let Some(registry) = &self.lsp_registry else {
            return;
        };
        let workspace_cwd = self.cwd();
        match registry
            .sync_file_after_edit_in_workspace(path, text, &workspace_cwd)
            .await
        {
            Ok(()) => {
                let _ = registry
                    .settle_diagnostics_under_host_root(path, std::time::Duration::from_millis(500))
                    .await;
            }
            Err(lingxi_core::host::LspError::Unavailable) => {}
            Err(error) => tracing::warn!(
                target: "lingxi_lsp::file_sync",
                path = %path.display(),
                %error,
                "failed to synchronize edited file with LSP"
            ),
        }
    }

    /// The sandbox config the shell tools should actually use: the frozen
    /// [`Self::sandbox_runtime`] with its `enabled` flag overridden live by the
    /// `/sandbox` toggle cell when one is wired, plus a per-call reconcile of
    /// deny-write symlink seeds so mid-session symlink replacements are
    /// hardened before the next sandboxed command runs.
    ///
    /// Only `enabled` is overridden — every other field (excluded commands,
    /// allow-unsandboxed, platform, …) rides the frozen config, and the cell is
    /// seeded from `sandbox_runtime.enabled`, so the result is byte-identical to
    /// the frozen config until `/sandbox` actually flips the toggle.
    #[must_use]
    pub fn effective_sandbox_runtime(&self) -> SandboxRuntimeConfig {
        let mut cfg = self.toggled_sandbox_runtime();
        sandbox::policy_convert::reconcile_deny_write_symlinks(&mut cfg);
        cfg
    }

    /// [`Self::effective_sandbox_runtime`] for a command whose sandbox is
    /// rooted at `root` — the agent's own directory (`isolation: "worktree"`
    /// or an explicit `cwd`) or else the session workspace, never the host
    /// process's cwd or the shell's current `cd`. Relative entries such as the
    /// `.` seed resolve against `root`; see [`sandbox::root::rooted_at`].
    /// Pass the same `root` as the `cwd` of [`crate::SandboxRunner::wrap`].
    #[must_use]
    pub fn sandbox_runtime_at(
        &self,
        root: &std::path::Path,
        scope: sandbox::root::SandboxRootScope,
    ) -> SandboxRuntimeConfig {
        let mut cfg = sandbox::root::rooted_at(&self.toggled_sandbox_runtime(), root, scope);
        sandbox::policy_convert::reconcile_deny_write_symlinks(&mut cfg);
        cfg
    }

    fn toggled_sandbox_runtime(&self) -> SandboxRuntimeConfig {
        match &self.sandbox_enabled_override {
            Some(cell) => {
                let mut cfg = self.sandbox_runtime.clone();
                cfg.enabled = cell.load(std::sync::atomic::Ordering::Relaxed);
                cfg
            }
            None => self.sandbox_runtime.clone(),
        }
    }

    /// Mobile shell carrier, independent of platform naming. The stable
    /// backing field remains [`Self::android_shell`].
    #[must_use]
    pub fn mobile_shell(&self) -> Option<&MobileShellToolCtx> {
        self.android_shell.as_ref()
    }

    /// Mobile git carrier, independent of platform naming. The stable backing
    /// field remains [`Self::android_git`].
    #[must_use]
    pub fn mobile_git(&self) -> Option<&MobileGitToolCtx> {
        self.android_git.as_ref()
    }

    /// Mobile git secret carrier, independent of platform naming. The stable
    /// backing field remains [`Self::android_git_secret`].
    #[must_use]
    pub fn mobile_git_secret(&self) -> Option<&MobileGitSecret> {
        self.android_git_secret.as_ref()
    }
}

/// BLOCKING `TaskCreated` / `TaskCompleted` lifecycle-hook firer for the V2
/// `Task*` TOOL path.
///
/// Distinct from the fire-and-forget `hooks::TaskCreatedFirer` /
/// `hooks::TaskCompletedFirer` the `TaskRegistry` holds (whose contract is "MUST
/// NOT propagate hook failures"). This seam REPORTS a blocking decision so the
/// tool can roll back / refuse, reproducing claude-code's blocking paths:
///
/// - [`fire_task_created`](Self::fire_task_created) mirrors `executeTaskCreatedHooks`
///   (`TaskCreateTool.ts:93-113`): on a blocking error claude-code `deleteTask`s
///   the just-created task and throws.
/// - [`fire_task_completed`](Self::fire_task_completed) mirrors
///   `executeTaskCompletedHooks` (`TaskUpdateTool.ts:232-265`): on a blocking
///   error claude-code returns `success:false` and does NOT apply the status.
///
/// Both methods default to a no-op `Ok(())` (allow) so unwired contexts never
/// block — an absent firer (`None`) and a wired-but-defaulted impl behave
/// identically: creation/completion proceeds. `tool-api` must NOT depend on
/// `hooks`, so the trait lives here and the orchestrator (which depends on both
/// `tool-api` and `hooks`) provides the real impl over its
/// `Arc<hooks::HookExecutorImpl>`.
#[async_trait::async_trait]
pub trait TaskLifecycleHookFirer: Send + Sync {
    /// claude-code `executeTaskCreatedHooks` BLOCKING path. `Ok(())` = allow
    /// creation; `Err(reason)` = a hook BLOCKED creation (the tool rolls the
    /// just-created task back and surfaces `reason`).
    ///
    /// `teammate_name` / `team_name` carry the creating teammate's identity
    /// (claude-code `getAgentName()` / `getTeamName()`, `TaskCreateTool.ts:97-98`)
    /// into the `TaskCreated` hook payload; `None` for the main thread / leader.
    async fn fire_task_created(
        &self,
        task_id: &str,
        subject: &str,
        description: Option<&str>,
        teammate_name: Option<&str>,
        team_name: Option<&str>,
    ) -> Result<(), String> {
        let _ = (task_id, subject, description, teammate_name, team_name);
        Ok(())
    }
    /// claude-code `executeTaskCompletedHooks` BLOCKING path. `Ok(())` = allow
    /// completion; `Err(reason)` = a hook BLOCKED completion (the tool returns
    /// `success:false` carrying `reason` and does NOT apply the status).
    /// `teammate_name` / `team_name` identify the completing teammate, matching
    /// TaskUpdateTool's live `getAgentName()` / `getTeamName()` arguments.
    async fn fire_task_completed(
        &self,
        task_id: &str,
        status: &str,
        subject: &str,
        description: Option<&str>,
        teammate_name: Option<&str>,
        team_name: Option<&str>,
    ) -> Result<(), String> {
        let _ = (
            task_id,
            status,
            subject,
            description,
            teammate_name,
            team_name,
        );
        Ok(())
    }
}

/// Mobile `Shell` tool wiring (shared by Android legacy shell and future
/// mobile-linux backends). `None` on builds that do not expose a mobile shell.
#[derive(Debug, Clone)]
pub struct MobileShellToolCtx {
    /// The full registration gate result: capability-probe OK + `enable_shell`
    /// + D11 secrets gate all satisfied.
    ///
    /// When `false`, the Shell tool is NOT registered (absent, not erroring).
    pub enabled: bool,
    /// Probed toybox applet inventory (for the tool prompt; may be empty).
    pub applets: Vec<String>,
    /// Shell executable path the mobile shell tool must invoke. Examples:
    /// Android legacy `/system/bin/sh`, mobile-linux guest `/bin/sh`.
    pub shell_path: String,
    /// Human-readable runtime label used in prompts/diagnostics. Examples:
    /// `system mksh`, `Alpine BusyBox sh`.
    pub runtime_label: String,
    /// Route embedded prompt-shell commands through the mobile platform
    /// sandbox/process adapters. This is enabled only for the mobile-linux
    /// guest carrier; Android legacy and all desktop contexts remain unchanged.
    pub force_platform_sandbox: bool,
    /// System sh version string (`KSH_VERSION`) when probed, for the prompt.
    pub sh_version: Option<String>,
    /// When true, the Shell runs the BUNDLED version-locked mksh + a fixed
    /// locked toybox applet inventory (not the device's system sh). Drives the
    /// tool prompt wording; the actual exec switch is in
    /// `AndroidMinijailSandbox::prepare`.
    pub bundled: bool,
}

/// Mobile structured `Git` tool wiring (shared by Android legacy git and
/// future mobile-linux backends). `None` on builds that do not expose mobile
/// git.
#[derive(Debug, Clone)]
pub struct MobileGitToolCtx {
    /// The full registration gate result: `enable_git` + workspace-ready +
    /// CA-store-reachable all satisfied.
    ///
    /// When `false`, the Git tool is NOT registered (absent, not erroring).
    pub enabled: bool,
    /// Whether a credential provider is configured for the host. When `false`,
    /// network operations (clone/fetch/pull) are unavailable; the tool prompt
    /// notes that credential configuration is required for network ops. This
    /// drives the prompt only — the registration gate does not use it.
    pub has_token: bool,
    /// App-private repository root (absolute path). All git operations are
    /// anchored to this directory; paths escaping it are rejected.
    pub workspace_root: String,
}

/// Mobile Git **secret** seam (spec §G3 auth). Carries the in-memory HTTPS
/// token + CA-certificate directory used by the network git ops
/// (clone/fetch/pull). Deliberately held outside the public
/// [`MobileGitToolCtx`] — which only exposes `has_token: bool` — so the token
/// never enters the broadly-cloned public tool carrier. `tool-git-mobile`
/// converts this into its own `GitNetConfig` at call time.
///
/// The token is never written to disk, an env var, or a child-process argv
/// (libgit2 is in-process), and is never logged. `android-aar` (T10) builds
/// this from the Keystore-backed host token + the system cacerts dir.
#[derive(Clone, Default)]
pub struct MobileGitSecret {
    /// Per-op secret provider (HTTPS token + SSH passphrase). `None` → no secrets
    /// available (anonymous/public remotes only). Replaces the former resident
    /// `token`/`ssh_passphrase` fields — secrets are no longer held resident.
    pub credential_provider: Option<std::sync::Arc<dyn GitCredentialProvider>>,
    /// CA-certificate directory for TLS verification (Android system cacerts),
    /// or `None` to use the libgit2/OpenSSL defaults.
    pub ca_dir: Option<String>,
    /// Filesystem path to the SSH private key (spec §G7), or `None` for
    /// HTTPS-only. Host-supplied and validated to stay inside the app sandbox by
    /// `android-aar` before reaching this seam. A non-secret path.
    pub ssh_private_key_path: Option<String>,
    /// Optional path to the matching SSH public key (libssh2 can derive it from
    /// the private key when `None`). A non-secret path.
    pub ssh_public_key_path: Option<String>,
    /// Pinned SSH host-key fingerprints (lowercase-hex SHA-256). The remote's
    /// host key is accepted only if its SHA-256 is a member; an empty list
    /// rejects every host key (fail-closed). Non-secret hashes.
    pub ssh_known_hosts_sha256_hex: Vec<String>,
}

/// Per-operation Git credential provider (spec: per-op credential FFI). Supplies
/// the two true Git secrets — the HTTPS token and the SSH key passphrase —
/// fetched lazily by `tool-git-mobile` inside libgit2's credentials callback,
/// once per network op. Implemented by the host (android-aar bridges a UniFFI
/// `AndroidGitCredentialProvider` onto this); the secrets are never held resident
/// between ops. Sync (libgit2's cred callback is synchronous).
pub trait GitCredentialProvider: Send + Sync {
    /// The HTTPS token (PAT) for `userpass_plaintext`, or `None` for anonymous.
    fn https_token(&self) -> Option<String>;
    /// The SSH private-key passphrase, or `None` if the key is unencrypted.
    fn ssh_passphrase(&self) -> Option<String>;
}

// A manual `Debug` that omits the secret-bearing credential provider so secrets
// can never leak via a debug print of the context. The key/public-key paths and
// pinned host hashes are non-secret and shown normally.
impl std::fmt::Debug for MobileGitSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MobileGitSecret")
            .field(
                "credential_provider",
                &self.credential_provider.as_ref().map(|_| "<provider>"),
            )
            .field("ca_dir", &self.ca_dir)
            .field("ssh_private_key_path", &self.ssh_private_key_path)
            .field("ssh_public_key_path", &self.ssh_public_key_path)
            .field(
                "ssh_known_hosts_sha256_hex",
                &self.ssh_known_hosts_sha256_hex,
            )
            .finish()
    }
}

/// Stable compatibility alias for existing Android mobile-shell call sites.
pub type AndroidShellToolCtx = MobileShellToolCtx;
/// Stable compatibility alias for existing Android mobile-git call sites.
pub type AndroidGitToolCtx = MobileGitToolCtx;
/// Stable compatibility alias for existing Android mobile-git secret call sites.
pub type AndroidGitSecret = MobileGitSecret;

impl MobileShellToolCtx {
    /// Android legacy shell carrier.
    #[must_use]
    pub fn android_legacy(
        enabled: bool,
        applets: Vec<String>,
        sh_version: Option<String>,
        bundled: bool,
    ) -> Self {
        Self {
            enabled,
            applets,
            shell_path: "/system/bin/sh".into(),
            runtime_label: if bundled {
                "bundled mksh".into()
            } else {
                "system mksh".into()
            },
            force_platform_sandbox: false,
            sh_version,
            bundled,
        }
    }

    /// Mobile-linux guest shell carrier.
    #[must_use]
    pub fn mobile_linux_guest(
        enabled: bool,
        applets: Vec<String>,
        sh_version: Option<String>,
    ) -> Self {
        Self {
            enabled,
            applets,
            shell_path: "/bin/sh".into(),
            runtime_label: "Alpine BusyBox sh".into(),
            force_platform_sandbox: true,
            sh_version,
            bundled: true,
        }
    }
}

/// Forward low-level shell-discovery diagnostics through the host's tracing sink.
/// Both Bash and standalone Monitor construct through this shared boundary.
pub fn install_shell_discovery_logging() {
    lingxi_core::host::shell_support::set_shell_discovery_logger(|warning, message| {
        if warning {
            tracing::warn!("{message}");
        } else {
            tracing::info!("{message}");
        }
    });
}

// =============================================================================
// Tests
// =============================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{ctx_for_file_tools, make_dummy_fs};
    use lingxi_core::host::permission_gate::PermissionDecision;
    use serde_json::Value;
    use std::sync::Arc;
    use telemetry::AnalyticsBus;

    struct LiveReadDenyGate;

    #[async_trait::async_trait]
    impl PermissionGate for LiveReadDenyGate {
        async fn check(&self, _name: &str, _input: &Value) -> PermissionDecision {
            PermissionDecision::Allow
        }

        fn read_deny_exclude_globs(&self, _cwd: &std::path::Path) -> Option<Vec<String>> {
            Some(vec!["/live/**".to_string()])
        }
    }

    #[test]
    fn effective_read_deny_globs_prefer_live_gate_with_static_fallback() {
        let mut ctx =
            ctx_for_file_tools(make_dummy_fs(), Arc::new(AnalyticsBus::new()), Vec::new());
        ctx.read_deny_exclude_globs = vec!["/boot/**".to_string()];
        assert_eq!(
            ctx.effective_read_deny_exclude_globs(std::path::Path::new("/proj")),
            vec!["/boot/**".to_string()]
        );

        ctx.permission_gate = Some(Arc::new(LiveReadDenyGate));
        assert_eq!(
            ctx.effective_read_deny_exclude_globs(std::path::Path::new("/proj")),
            vec!["/live/**".to_string()]
        );
    }

    /// `/sandbox`: `effective_sandbox_runtime` overrides ONLY `enabled` from the
    /// live toggle cell, observing later flips, and is byte-identical to the
    /// frozen config when no cell is wired (or the cell matches the seed).
    #[test]
    fn effective_sandbox_runtime_reflects_the_toggle_cell() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let mut ctx = ctx_for_file_tools(make_dummy_fs(), Arc::new(AnalyticsBus::new()), vec![]);
        let frozen_enabled = ctx.sandbox_runtime.enabled;

        // No cell wired ⇒ frozen behavior (byte-identical to `sandbox_runtime`).
        assert_eq!(ctx.effective_sandbox_runtime().enabled, frozen_enabled);

        // Wire a cell set to the OPPOSITE ⇒ `enabled` flips, other fields ride
        // the frozen config.
        let cell = Arc::new(AtomicBool::new(!frozen_enabled));
        ctx.sandbox_enabled_override = Some(cell.clone());
        let eff = ctx.effective_sandbox_runtime();
        assert_eq!(eff.enabled, !frozen_enabled, "toggle must override enabled");
        assert_eq!(
            eff.excluded_commands, ctx.sandbox_runtime.excluded_commands,
            "only `enabled` is overridden"
        );

        // A later flip of the SAME cell is observed live (next command sees it).
        cell.store(frozen_enabled, Ordering::Relaxed);
        assert_eq!(ctx.effective_sandbox_runtime().enabled, frozen_enabled);
    }

    #[cfg(unix)]
    #[test]
    fn effective_sandbox_runtime_reconciles_mid_session_deny_write_symlinks() {
        let mut ctx = ctx_for_file_tools(make_dummy_fs(), Arc::new(AnalyticsBus::new()), vec![]);
        let tmp = tempfile::tempdir().unwrap();
        let escaped = tmp.path().join("escaped");
        let retargeted = tmp.path().join("retargeted");
        let seeded = tmp.path().join("seeded");
        std::fs::create_dir_all(&escaped).unwrap();
        std::fs::create_dir_all(&retargeted).unwrap();
        std::os::unix::fs::symlink(&escaped, &seeded).unwrap();
        ctx.sandbox_runtime.filesystem.deny_write = vec![seeded.to_string_lossy().into_owned()];

        let eff = ctx.effective_sandbox_runtime();
        let resolved = std::fs::canonicalize(&escaped).unwrap();
        assert_eq!(
            eff.filesystem.deny_write,
            vec![resolved.to_string_lossy().into_owned()],
            "live sandbox config must re-resolve deny-write symlink seeds"
        );

        std::fs::remove_file(&seeded).unwrap();
        std::os::unix::fs::symlink(&retargeted, &seeded).unwrap();
        let retargeted = std::fs::canonicalize(&retargeted).unwrap();
        assert_eq!(
            ctx.effective_sandbox_runtime().filesystem.deny_write,
            vec![retargeted.to_string_lossy().into_owned()],
            "the lexical seed must survive so a later symlink retarget is observed"
        );
    }

    /// A mock provider for tests — counts calls and returns fixed secrets.
    struct MockProvider {
        token: Option<String>,
        passphrase: Option<String>,
        token_calls: std::sync::atomic::AtomicUsize,
        pass_calls: std::sync::atomic::AtomicUsize,
    }
    impl GitCredentialProvider for MockProvider {
        fn https_token(&self) -> Option<String> {
            self.token_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.token.clone()
        }
        fn ssh_passphrase(&self) -> Option<String> {
            self.pass_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.passphrase.clone()
        }
    }

    #[test]
    #[allow(clippy::default_trait_access)] // prescribed test body uses Default::default()
    fn android_git_secret_carries_provider_and_debug_has_no_secret() {
        let provider: std::sync::Arc<dyn GitCredentialProvider> =
            std::sync::Arc::new(MockProvider {
                token: Some("tok".into()),
                passphrase: Some("pp".into()),
                token_calls: Default::default(),
                pass_calls: Default::default(),
            });
        let s = AndroidGitSecret {
            credential_provider: Some(provider.clone()),
            ca_dir: Some("/system/etc/security/cacerts".into()),
            ssh_private_key_path: Some("/data/k".into()),
            ..Default::default()
        };
        // The provider is reachable and returns the secret on demand.
        assert_eq!(
            s.credential_provider
                .as_ref()
                .unwrap()
                .https_token()
                .as_deref(),
            Some("tok")
        );
        // Debug shows NO secret value and an opaque provider marker.
        let dbg = format!("{s:?}");
        assert!(
            !dbg.contains("tok") && !dbg.contains("pp"),
            "no secret in Debug: {dbg}"
        );
        assert!(dbg.contains("ca_dir"), "non-secrets still shown: {dbg}");
    }

    /// TDD anchor for Task 7 (P4).
    ///
    /// Asserts:
    /// 1. `AndroidGitToolCtx` constructs with all three fields.
    /// 2. The test-builder `ctx_for_file_tools` defaults `android_git` to
    ///    `None` (i.e. the field exists on `BuiltinToolContext`).
    #[test]
    fn android_git_tool_ctx_constructs_and_defaults_to_none() {
        // Construct the carrier type — enabled with token.
        let carrier = AndroidGitToolCtx {
            enabled: true,
            has_token: true,
            workspace_root: "/x".into(),
        };
        assert!(carrier.enabled);
        assert!(carrier.has_token);
        assert_eq!(carrier.workspace_root, "/x");

        // Disabled variant without token.
        let disabled = AndroidGitToolCtx {
            enabled: false,
            has_token: false,
            workspace_root: String::new(),
        };
        assert!(!disabled.enabled);
        assert!(!disabled.has_token);

        // The test-support builder must produce a ctx with `android_git: None`.
        let ctx = ctx_for_file_tools(
            make_dummy_fs(),
            Arc::new(AnalyticsBus::new()),
            vec![std::path::PathBuf::from("/tmp")],
        );
        assert!(
            ctx.android_git.is_none(),
            "android_git must default to None in test builder"
        );
    }

    /// TDD anchor for Task 1 (P3).
    ///
    /// Asserts:
    /// 1. `AndroidShellToolCtx` constructs with the legacy Android routing.
    /// 2. The test-builder `ctx_for_file_tools` defaults `android_shell` to
    ///    `None` (i.e. the field exists on `BuiltinToolContext`).
    #[test]
    fn android_shell_tool_ctx_constructs_and_defaults_to_none() {
        // Construct the carrier type — enabled.
        let carrier = AndroidShellToolCtx {
            enabled: true,
            applets: vec!["grep".into(), "ls".into()],
            shell_path: "/system/bin/sh".into(),
            runtime_label: "system mksh".into(),
            force_platform_sandbox: false,
            sh_version: Some("@(#)MIRBSD KSH R59 2020/01/19".into()),
            bundled: false,
        };
        assert!(carrier.enabled);
        assert_eq!(carrier.applets, vec!["grep", "ls"]);
        assert!(carrier.sh_version.is_some());

        // Disabled variant.
        let disabled = AndroidShellToolCtx {
            enabled: false,
            applets: vec![],
            shell_path: "/system/bin/sh".into(),
            runtime_label: "system mksh".into(),
            force_platform_sandbox: false,
            sh_version: None,
            bundled: false,
        };
        assert!(!disabled.enabled);
        assert!(disabled.applets.is_empty());
        assert!(disabled.sh_version.is_none());

        // The test-support builder must produce a ctx with `android_shell: None`.
        let ctx = ctx_for_file_tools(
            make_dummy_fs(),
            Arc::new(AnalyticsBus::new()),
            vec![std::path::PathBuf::from("/tmp")],
        );
        assert!(
            ctx.android_shell.is_none(),
            "android_shell must default to None in test builder"
        );
    }

    /// With a session, persisted output goes to claude-code's session-scoped
    /// `<projects>/<session-id>/tool-results` — and crucially NOT under `cwd`.
    ///
    /// Asserts the SHAPE rather than an absolute path so the test needs no
    /// `HOME` manipulation (which races across parallel tests). The exact
    /// layout is pinned by `session::jsonl::path`'s own tests.
    #[test]
    fn tool_results_dir_is_session_scoped_and_outside_the_workspace() {
        let mut ctx = crate::test_support::ctx_for_file_tools(
            crate::test_support::make_dummy_fs(),
            std::sync::Arc::new(telemetry::AnalyticsBus::new()),
            vec![],
        );
        let sid = lingxi_core::types::SessionId::new();
        ctx.session_id = Some(sid);
        let dir = ctx.tool_results_dir();
        let text = dir.to_string_lossy().to_string();

        assert!(text.contains("projects"), "under the projects dir: {text}");
        assert!(
            text.contains(&sid.as_uuid().to_string()),
            "carries the session id: {text}"
        );
        assert!(
            text.ends_with("tool-results"),
            "leaf is tool-results: {text}"
        );
        assert!(
            !dir.starts_with(ctx.cwd()),
            "must NOT write inside the user's workspace: {text}"
        );
    }

    /// Without a session there is no session-scoped location to use, so the
    /// workspace-local fallback stands. This is the INERT path (tests + the
    /// mobile shims), not production — the desktop context sets the id.
    #[test]
    fn tool_results_dir_falls_back_under_cwd_when_there_is_no_session() {
        let ctx = crate::test_support::ctx_for_file_tools(
            crate::test_support::make_dummy_fs(),
            std::sync::Arc::new(telemetry::AnalyticsBus::new()),
            vec![],
        );
        assert!(ctx.session_id.is_none(), "builder leaves it unset");
        let dir = ctx.tool_results_dir();
        assert!(
            dir.starts_with(ctx.cwd()),
            "fallback is workspace-local: {dir:?}"
        );
        assert!(dir.ends_with("tool-results"), "{dir:?}");
    }
}
